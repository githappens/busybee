"""The Parallels boundary.

`query` runs only the fixed read-only commands preflight needs. Every other
method changes a VM and first checks that the controller owns it (see
registry.py): a VM that is not claimed is never started, stopped, cloned or
deleted. Tests substitute the runner, so contract and failure paths run
without Parallels installed.
"""
import json
from pathlib import Path
import re
import subprocess

# The complete set of read-only queries. A prefix match is not enough:
# `prlctl list --all --json --info` would be a different query.
READ_ONLY = (
    ("prlctl", ("--version",)),
    ("prlsrvctl", ("info", "--json")),
    ("prlctl", ("list", "--all", "--json")),
)

QUERY_TIMEOUT_S = 30
LIFECYCLE_TIMEOUT_S = 300

# PS/2 set-1 scancodes for a US layout, which is what the guest console expects.
_ROWS = {"1234567890-=": 2, "qwertyuiop[]": 16, "asdfghjkl;'": 30, "zxcvbnm,./": 44}
_SCANCODES = {c: base + i for row, base in _ROWS.items() for i, c in enumerate(row)}
_SCANCODES.update({" ": 57, "\n": 28, "\\": 43, "`": 41})
_SHIFTED = dict(zip('!@#$%^&*()_+{}:"<>?|~', "1234567890-=[];',./\\`"))
SHIFT = 42
# A short hold between press and release; separate prlctl calls per key were
# slow enough for the guest to autorepeat.
KEY_HOLD_MS = 15


class ParallelsError(RuntimeError):
    pass


class StartRefused(ParallelsError):
    """`prlctl start` failed; Parallels gives the reason only in the VM's own
    log, so it is named here (`code`) from that log."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


# What a macOS guest's parallels.log says when Apple's limit of two running
# macOS guests per host is reached; prlctl itself prints only "An unexpected
# error occurred".
GUEST_LIMIT = "maximum supported number of active virtual machines"


def is_read_only(argv):
    tool, rest = Path(argv[0]).name, tuple(argv[1:])
    if (tool, rest) in READ_ONLY:
        return True
    # `prlctl snapshot-list <vm> --json` and nothing else.
    return tool == "prlctl" and len(rest) == 3 and rest[0] == "snapshot-list" and rest[2] == "--json"


def braced(uuid):
    """`prlctl list` prints bare UUIDs; manifests and snapshots use braces."""
    return uuid if uuid.startswith("{") else "{" + uuid + "}"


def key_events(text):
    events = []
    for ch in text:
        base = ch.lower() if ch.isalpha() else _SHIFTED.get(ch, ch)
        if base not in _SCANCODES:
            raise ValueError(f"cannot type {ch!r} on the guest console")
        shifted = ch.isupper() or ch in _SHIFTED
        if shifted:
            events.append({"scancode": SHIFT, "event": "press"})
        events += [{"scancode": _SCANCODES[base], "event": "press"},
                   {"scancode": _SCANCODES[base], "event": "release", "delay": KEY_HOLD_MS}]
        if shifted:
            events.append({"scancode": SHIFT, "event": "release", "delay": KEY_HOLD_MS})
    return events


def run(argv, timeout=QUERY_TIMEOUT_S, stdin=None):
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, input=stdin)
    except (OSError, subprocess.TimeoutExpired) as err:
        raise ParallelsError(f"{Path(argv[0]).name} {argv[1]}: {err}") from err
    if done.returncode != 0:
        detail = (done.stderr or done.stdout).strip().splitlines()
        raise ParallelsError(f"{Path(argv[0]).name} {argv[1]} exited {done.returncode}: "
                             f"{detail[-1] if detail else 'no output'}")
    return done.stdout


class Parallels:
    def __init__(self, prlctl, prlsrvctl, runner=run, owned=None):
        self.tools = {"prlctl": prlctl, "prlsrvctl": prlsrvctl}
        self.runner = runner
        self.owned = owned

    def query(self, args, tool="prlctl"):
        argv = [self.tools[Path(tool).name], *args]
        if not is_read_only(argv):
            raise ParallelsError(f"refusing a non-query Parallels command: {Path(tool).name} {' '.join(args)}")
        return self.runner(argv)

    def _owned(self, name, args, timeout=LIFECYCLE_TIMEOUT_S, stdin=None):
        if self.owned is None or not self.owned.owns(name):
            raise ParallelsError(f"refusing to operate on {name!r}: the controller does not own it")
        return self.runner([self.tools["prlctl"], args[0], name, *args[1:]], timeout=timeout, stdin=stdin)

    def create(self, name, dst, disk_mib):
        # The default Linux disk is 64 GiB; the template sizes its own.
        self._owned(name, ["create", "--distribution", "linux", "--no-hdd", "--dst", str(dst)])
        self._owned(name, ["set", "--device-add", "hdd", "--type", "expand", "--size", str(disk_mib)])

    def configure(self, name, cpus, memory_mib, iso):
        self.allocate(name, cpus, memory_mib)
        # A worker needs none of the host's devices or sharing. The default
        # sound card's input alone raises a macOS microphone prompt. USB stays
        # until the install is done: the keyboard the bootstrap types on is a
        # USB device.
        for device in ("sound0", "serial0"):
            self._owned(name, ["set", "--device-del", device])
        self._owned(name, ["set", "--isolate-vm", "on", "--auto-share-gamepad", "off",
                           "--startup-view", "headless"])
        self._owned(name, ["set", "--device-set", "cdrom0", "--image", str(iso), "--connect"])
        self._owned(name, ["set", "--device-bootorder", "cdrom0 hdd0"])

    def boot_from_disk(self, name):
        """After the install: no installer, and no USB, whose controller makes
        Parallels raise a camera prompt on every start of every clone."""
        self._owned(name, ["set", "--device-set", "cdrom0", "--disconnect"])
        self._owned(name, ["set", "--device-bootorder", "hdd0"])
        self._owned(name, ["set", "--device-del", "usb"])

    def allocate(self, name, cpus, memory_mib):
        self._owned(name, ["set", "--cpus", str(cpus), "--memsize", str(memory_mib)])

    def info(self, name):
        vm = json.loads(self._owned(name, ["list", "--info", "--json"], timeout=QUERY_TIMEOUT_S))[0]
        disk = re.fullmatch(r"(\d+)Mb", vm["Hardware"].get("hdd0", {}).get("size", ""))
        return {"vm_id": braced(vm["ID"]), "state": vm["State"], "mac": vm["Hardware"]["net0"]["mac"],
                "devices": sorted(vm["Hardware"]), "disk_mib": int(disk.group(1)) if disk else None,
                "home": vm.get("Home")}

    def start(self, name):
        self._owned(name, ["start"])

    def start_reporting(self, name):
        """Start, and on failure name the reason the VM's parallels.log gives."""
        log = Path(self.info(name)["home"] or ".") / "parallels.log"
        offset = log.stat().st_size if log.is_file() else 0
        try:
            self.start(name)
        except ParallelsError as err:
            new = ""
            if log.is_file():
                with open(log, errors="replace") as f:
                    f.seek(offset)
                    new = f.read()
            if GUEST_LIMIT in new:
                raise StartRefused("macos_guest_limit", f"{name} did not start: the host already runs the two "
                                   "macOS guests Apple allows; stop one of them") from err
            lines = [line for line in new.splitlines() if " E " in line or "rror" in line]
            raise StartRefused("vm_start_failed", f"{err}; its log says: {lines[-1] if lines else 'nothing new'}") \
                from err

    def isolate(self, name, devices, host_devices):
        """A macOS clone: drop the host devices it has, and the sharing a
        prepared desktop VM comes with."""
        for device in sorted(set(devices) & set(host_devices)):
            self._owned(name, ["set", "--device-del", device])
        self._owned(name, ["set", "--isolate-vm", "on", "--auto-share-camera", "off", "--startup-view", "headless"])

    def stop(self, name, kill=False):
        self._owned(name, ["stop", "--kill"] if kill else ["stop", "--acpi"])

    def delete(self, name):
        self._owned(name, ["delete"])

    def capture(self, name, path):
        self._owned(name, ["capture", "--file", str(path)], timeout=QUERY_TIMEOUT_S)

    def type_text(self, name, text):
        self._owned(name, ["send-key-event", "--json"], timeout=QUERY_TIMEOUT_S,
                    stdin=json.dumps(key_events(text)))

    def snapshot(self, name, label):
        out = self._owned(name, ["snapshot", "--name", label])
        found = re.search(r"\{[0-9a-f-]{36}\}", out)
        if not found:
            raise ParallelsError(f"snapshot of {name} printed no snapshot id")
        return found.group(0)

    def snapshot_switch(self, name, snapshot_id):
        self._owned(name, ["snapshot-switch", "--id", snapshot_id, "--skip-resume"])

    def clone_source(self, source, name, dst):
        """A full clone of an operator-prepared VM the controller does not own.
        Only the claimed clone is created; the source is read, never changed."""
        if self.owned is None or not self.owned.owns(name):
            raise ParallelsError(f"refusing to create {name!r}: it is not claimed")
        self.runner([self.tools["prlctl"], "clone", source, "--name", name, "--dst", str(dst)],
                    timeout=LIFECYCLE_TIMEOUT_S)

    def clone(self, source, name, snapshot_id, dst, linked):
        if self.owned is None or not self.owned.owns(name):
            raise ParallelsError(f"refusing to create {name!r}: it is not claimed")
        args = ["clone", "--name", name, "--dst", str(dst)]
        args += ["--linked", "--id", snapshot_id] if linked else []
        self._owned(source, args)
