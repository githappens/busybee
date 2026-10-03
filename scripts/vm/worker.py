"""Workers: owned clones of the promoted baseline, and bounded commands in them.

See docs/design/agent-lab.md §Run an issue from reproduction to review,
§Controller interface and §Bound execution and preserve failures. A worker is
claimed in the registry, naming the baseline it depends on, before its clone
exists, and its record (contracts.worker_errors) is written before any guest
work. Reset restores the snapshot recorded at creation, never the newest one.
Reset and destroy collect first; a failed collection keeps the clone, stopped,
and reports what is missing. Guest commands are queued here and run by the
run's supervisor (supervisor.py), which enforces their deadlines whether or not
the process that queued them is still alive.
"""
from contextlib import contextmanager
from datetime import datetime, timezone
import base64
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import time

import contracts
import evidence
import guest
import lease
import macos
import parallels
import screen
import template

# One active worker until concurrency becomes a budgeted controller setting.
MAX_ACTIVE = 1
CHECKOUT = template.GUEST_CHECKOUT
# Seconds timeout(1) waits after TERM before KILL, and the host's margin on top.
KILL_GRACE_S = 10
HOST_MARGIN_S = KILL_GRACE_S + guest.SSH_CONNECT_S + 20
SIGNALS = ("TERM", "KILL", "INT", "HUP", "QUIT", "USR1", "USR2", "STOP", "CONT")
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
EXEC_NAME = re.compile(r"^\d{4}$")
BINARIES = ("busybee", "bzb", "bzbd")
STREAMS = ("stdout", "stderr")
POLL_S = 1
# Run files that are evidence; anything else there is controller bookkeeping.
EVIDENCE_FILES = ("command.json", "result.json", "stdout", "stderr", "source.json", "commits.bundle",
                  "worktree.diff")
READ_LIMIT = 16 * 1024 * 1024


class Refused(Exception):
    """A request the controller will not act on; nothing was changed."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def run_dir(state, run_id):
    return Path(state) / "runs" / run_id


def console_path(state, run_id):
    path = run_dir(state, run_id) / "console" / f"{_now().strftime('%Y%m%dT%H%M%S%fZ')}.png"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def run_deadline(record):
    """The worker's run deadline, in seconds since the epoch."""
    return datetime.strptime(record["deadline"], contracts.TIMESTAMP).replace(tzinfo=timezone.utc).timestamp()


def _now():
    return datetime.now(timezone.utc)


def _stamp(moment):
    return moment.strftime(contracts.TIMESTAMP)


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _reason(err):
    """An error for a result: an OSError's message would name a host path."""
    return err.strerror if isinstance(err, OSError) and err.strerror else str(err)


def _load(path):
    return json.loads(path.read_text()) if path.is_file() else None


def _next_dir(parent):
    parent.mkdir(parents=True, exist_ok=True)
    while True:
        taken = [int(p.name) for p in parent.iterdir() if p.name.isdigit()]
        path = parent / f"{max(taken, default=0) + 1:04d}"
        try:
            path.mkdir()
            return path
        except FileExistsError:  # another controller process took that number
            continue


def _latest_dir(parent):
    taken = sorted(p for p in parent.iterdir() if p.name.isdigit()) if parent.is_dir() else []
    return taken[-1] if taken else None


@contextmanager
def locked(path, wait=True):
    """Hold an exclusive lock on `path` for the block; yields False when
    `wait` is off and another process holds it. The kernel drops the lock
    with its holder, however that process ends."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a") as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | (0 if wait else fcntl.LOCK_NB))
        except BlockingIOError:
            yield False
            return
        yield True


def held(path):
    with locked(path, wait=False) as got:
        return not got


def finalize(edir, outcome):
    """Publish an exec's result once: the first of the supervisor, a cancelling
    destroy or a recovering supervisor to finish it wins."""
    tmp = edir / f"result.json.{os.getpid()}"
    evidence.durable(tmp, (json.dumps(outcome, indent=2) + "\n").encode())
    try:
        os.link(tmp, edir / "result.json")
        return True
    except FileExistsError:
        return False
    finally:
        tmp.unlink()


def guest_file(run_id, name, kind):
    """Where the guest records one exec's process group (`pid`) and exit status."""
    return f"/var/tmp/busybee-lab-exec-{run_id}-{name}.{kind}"


def exec_command(argv, cwd, env, timeout, status_path, pid_path):
    """The guest shell command for one exec. Every word is quoted, so spaces and
    shell metacharacters reach the program literally. The exit status is also
    written to `status_path`, which tells a command's own 255 from ssh's.
    timeout(1) leads its own process group; its pid goes to `pid_path`, so the
    controller can kill the command and all it started."""
    # timeout(1) stops parsing options at the duration, so `--` goes before it.
    words = ["sh", "-c", 'echo $$ > "$0"; exec "$@"', pid_path, "env", *(f"{k}={v}" for k, v in env.items()),
             "timeout", "-k", str(KILL_GRACE_S), "--", str(timeout), *argv]
    q = shlex.quote
    return (f"cd {q(cwd)} && {' '.join(map(q, words))}; s=$?; echo $s > {q(status_path)}; exit $s")


def kill_command(pid_path):
    """Kill one exec's process group. It always exits 0 once it ran, so a
    nonzero status means the guest never ran it."""
    return f"p=$(cat {pid_path} 2>/dev/null) && kill -s KILL -- -$p; true"


def _after_exec(status_path, pid_path, checkout=CHECKOUT):
    """Read and remove the recorded status, then report source and binary provenance."""
    binaries = " ".join(f"build/*/{b}" for b in BINARIES)
    return (f'printf "status: %s\\n" "$(cat {status_path} 2>/dev/null)"; rm -f {status_path} {pid_path}; '
            f'cd {checkout} || exit 0; printf "head: %s\\n" "$(git rev-parse HEAD)"; '
            f'printf "dirty: %s\\n" "$(git status --porcelain | wc -l)"; '
            f'for f in {binaries}; do [ -f "$f" ] && sha256sum "$f" | sed "s/^/binary: /"; done; true')


# A guest terminal's files the host keeps (tests/scenarios/terminal.py).
TERMINAL_FILES = ("state.json", "recording", "captures")


def fetch_terminal(g, timeout, gdir, hdir):
    """Copy a guest terminal's state, recording and captures into `hdir`."""
    data = g.run(f"cd {shlex.quote(gdir)} && tar -cf - {' '.join(TERMINAL_FILES)}", timeout, raw=True)[1]
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        tar.extractall(hdir, filter="data")


def parse_after_exec(text):
    facts = {"exit_code": None, "head": None, "dirty_files": None, "binaries": {}}
    for line in text.splitlines():
        kind, _, value = line.partition(": ")
        value = value.strip()
        if kind == "status" and value.lstrip("-").isdigit():
            facts["exit_code"] = int(value)
        elif kind == "head" and value:
            facts["head"] = value
        elif kind == "dirty" and value.isdigit():
            facts["dirty_files"] = int(value)
        elif kind == "binary":
            digest, _, path = value.partition("  ")
            facts["binaries"][path] = digest
    return facts


# Every process, in BSD syntax that procps and macOS both take; etime is
# [[dd-]hh:]mm:ss on both (procps's etimes, in seconds, is not on macOS).
PS = "ps axo pid=,ppid=,user=,stat=,etime=,args="


def _seconds(elapsed):
    days, _, clock = elapsed.rpartition("-")
    seconds = 0
    for part in clock.split(":"):
        seconds = seconds * 60 + int(part)
    return seconds + int(days or 0) * 86400


def parse_processes(text):
    processes = []
    for line in text.splitlines():
        fields = line.split(None, 5)
        if len(fields) == 6 and fields[0].isdigit():
            pid, ppid, user, stat, elapsed, args = fields
            processes.append({"pid": int(pid), "ppid": int(ppid), "user": user, "stat": stat,
                              "elapsed_s": _seconds(elapsed), "args": args})
    return processes


class Workers(template.Lab):
    """Worker operations against Parallels. `free_gib(path)` measures host
    storage; `connect` returns command access to a running worker;
    `supervise(run_id)` makes sure the run's supervisor is alive. Tests
    substitute them, and the clock and sleep that waits use."""

    def __init__(self, repo, config, prl, reg, free_gib, connect=None, supervise=None, clock=time.time,
                 sleep=time.sleep, slot_wait_s=None):
        super().__init__(repo, config, prl, reg)
        self.connect = connect or self._ssh
        self.free_gib = free_gib
        self.supervise = supervise
        self.clock, self.sleep = clock, sleep
        # How long `worker create macos` waits in line for the slot.
        self.slot_wait_s = config["deadlines"]["run"] if slot_wait_s is None else slot_wait_s
        self.slot = lease.Slot(self.state, clock=clock, sleep=sleep)

    # Ownership and records

    def vm_for(self, run_id):
        """The VM a run works in: its own clone, or the macOS slot while it holds the lease."""
        return next((n for n, e in self.reg.entries().items() if e["role"] == "slot" and e.get("holder") == run_id),
                    contracts.worker_name(run_id))

    def claimed(self, run_id):
        entry = self.reg.get(self.vm_for(run_id))
        return entry is not None and (entry["role"] == "worker" and entry["run_id"] == run_id
                                      or entry["role"] == "slot" and entry.get("holder") == run_id)

    def _owned(self, run_id, need_record=True):
        """The registry entry and record of a worker this controller created
        or, on macOS, of the slot this run holds."""
        if not contracts.valid_run_id(run_id):
            raise Refused("target_invalid", f"{run_id!r} is not a run id")
        vm = self.vm_for(run_id)
        entry = self.reg.get(vm)
        if not self.claimed(run_id):
            raise Refused("target_not_owned", f"no registered worker for run {run_id}")
        record = _load(run_dir(self.state, run_id) / "worker.json")
        if record is None and need_record:
            raise Refused("worker_incomplete", f"worker {run_id} was never fully created; destroy it")
        if record is not None and record["vm_id"] != entry["vm_id"]:
            raise Refused("target_not_owned", f"worker {run_id}'s record and registry disagree on its VM")
        return vm, record

    def _save(self, record):
        template._write_json(run_dir(self.state, record["run_id"]) / "worker.json", record)

    def _rel(self, path):
        return str(path.relative_to(self.state))

    def _window(self, kind="command"):
        self.deadline = time.monotonic() + self.config["deadlines"][kind]

    def _run_window(self, record):
        """Bound what follows by the worker's run deadline; returns the seconds left."""
        left = run_deadline(record) - self.clock()
        self.deadline = time.monotonic() + left
        return left

    def _lock(self, run_id):
        """Held by every operation that changes a worker, so the supervisor and
        reconciliation can tell an interrupted one from one in progress."""
        return run_dir(self.state, run_id) / "operation.lock"

    def event(self, run_id, kind, **detail):
        """Append to the run's durable log of what the controller did to it."""
        line = json.dumps({"at": _stamp(_now()), "event": kind, **detail}) + "\n"
        with open(run_dir(self.state, run_id) / "events.jsonl", "ab") as f:
            f.write(line.encode())
            f.flush()
            os.fsync(f.fileno())

    def events(self, run_id):
        path = run_dir(self.state, run_id) / "events.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.is_file() else []

    def unfinished(self, run_id):
        edir = run_dir(self.state, run_id) / "exec"
        return sorted(p.name for p in edir.iterdir() if (p / "command.json").is_file()
                      and not (p / "result.json").exists()) if edir.is_dir() else []

    def _used(self, run_id):
        return sum(f.stat().st_size for f in run_dir(self.state, run_id).rglob("*") if f.is_file())

    def artifact_overrun(self, record):
        """Why the run's logs and evidence exceed its budget, or None."""
        used = self._used(record["run_id"])
        budget = record["allocation"]["artifact_mib"]
        if used <= budget * 1024 * 1024:
            return None
        return f"run {record['run_id']} holds {used / 2 ** 20:.1f} MiB of logs and evidence; its budget is {budget} MiB"

    def _running(self, vm):
        try:
            return self.prl.info(vm)["state"] != "stopped"
        except parallels.ParallelsError:
            return False

    def _host_vms(self):
        """Every VM Parallels lists, by id, with its name: a read-only query."""
        return {parallels.braced(vm["uuid"]): vm["name"] for vm in json.loads(self.prl.query(["list", "--all", "--json"]))}

    def _gone(self, vm):
        """Whether a claimed worker's VM no longer exists, as after a destroy
        that died between the delete and the release."""
        vm_id, listed = self.reg.get(vm)["vm_id"], self._host_vms()
        return vm_id not in listed if vm_id else vm not in listed.values()

    # Guest access

    def _guest_user(self, record):
        """The account the baseline's key logs into: root on NixOS, the macOS
        baseline's lab account as its candidate recorded it."""
        if record["template"] != "macos":
            return "root"
        cdir = template.candidate_dir(self.state, record["template"], record["candidate"])
        return json.loads((cdir / "candidate.json").read_text())["guest_user"]

    def checkout(self, record):
        return macos.checkout(self._guest_user(record)) if record["template"] == "macos" else CHECKOUT

    def _ssh(self, record, info, deadline):
        cdir = template.candidate_dir(self.state, record["template"], record["candidate"])
        ip = guest.wait_for_lease(info["mac"], deadline)
        known = run_dir(self.state, record["run_id"]) / "known_hosts"
        known.write_text(f"{ip} {' '.join((cdir / 'host.pub').read_text().split()[:2])}\n")
        g = guest.Guest(ip, cdir / "access", known, user=self._guest_user(record),
                        posix=record["template"] == "macos")
        g.wait(deadline)
        if record["template"] == "macos" and not macos.wait_for_nix(g, deadline, lambda: self._bound("command"),
                                                                     self.sleep):
            raise guest.GuestError("the macOS guest answers, but its Nix store did not mount")
        return g

    def _guest(self, record, boot=False):
        vm = record["worker"]
        info = self.prl.info(vm)
        if info["state"] != "running":
            if not boot:
                raise Refused("worker_stopped", f"worker {record['run_id']} is {info['state']}")
            self.prl.start_reporting(vm)
        return self.connect(record, info, self._until(self.config["deadlines"]["command"]))

    def _transfer(self, g, record):
        """The recorded revision with its history and tags, as a guest-local checkout."""
        revision = record["source"]["revision"]
        co = self.checkout(record)
        bundle = subprocess.run(["git", "bundle", "create", "-", revision, "--tags"], cwd=self.repo,
                                capture_output=True, check=True).stdout
        # A bare revision is no ref in the bundle, so unbundle and recreate the tags.
        g.run(f"rm -rf {co} /var/tmp/source.bundle && cat > /var/tmp/source.bundle && "
              f"git init -q {co} && cd {co} && git bundle unbundle /var/tmp/source.bundle | "
              f"while read -r id ref; do git update-ref \"$ref\" \"$id\"; done && "
              f"git checkout -q --detach {revision} && rm /var/tmp/source.bundle",
              self._bound("command"), stdin=bundle)
        patch = run_dir(self.state, record["run_id"]) / "source.patch"
        if record["source"]["patch_sha256"]:
            g.run(f"cd {co} && git apply", self._bound("command"), stdin=patch.read_bytes())
        head = g.run(f"git -C {co} rev-parse HEAD", self._bound("command"))[1].strip()
        if head != revision:
            raise RuntimeError(f"guest checkout is at {head}, not {revision}")

    def halt(self, record):
        vm = record["worker"]
        if self.prl.info(vm)["state"] == "stopped":
            return
        try:
            self._remaining()  # with no time left, straight to the kill
            if record["template"] == "macos":
                # A macOS guest answers an ACPI request with a dialog: shut it down from inside.
                self._guest(record).run("sudo -n shutdown -h now", self._bound("command"), check=False)
                self._wait_state(vm, "stopped", self.config["deadlines"]["command"])
            else:
                self._shutdown(vm)
        except (template.DeadlineExceeded, parallels.ParallelsError, guest.GuestError, Refused):
            self.prl.stop(vm, kill=True)

    # Admission and reconciliation

    def _reconcile(self):
        """Account for every owned worker, and restart the supervision of any
        that a dead supervisor left unwatched. Only registered workers are
        looked at; no other VM is touched."""
        workers, findings = [], []
        for vm, entry in sorted(self.reg.entries().items()):
            if entry["role"] == "slot" and entry.get("holder"):
                run_id = entry["holder"]
            elif entry["role"] == "worker":
                run_id = entry["run_id"]
            else:
                continue
            record = _load(run_dir(self.state, run_id) / "worker.json")
            if record and (record["status"] in ("ready", "provisioning") or self.unfinished(run_id) or
                           record["status"] in contracts.FROZEN_STATES and self._running(vm)):
                self.supervise(run_id)
                record = _load(run_dir(self.state, run_id) / "worker.json")
            item = {"run_id": run_id, "status": record["status"] if record else "claimed",
                    "allocation": record["allocation"] if record else None, "deadline": entry["deadline"]}
            if record is None:
                findings.append(contracts.finding("interrupted_create", f"worker {run_id} was claimed but its "
                                                  "creation never recorded it; destroy it", "warning"))
            try:
                item["vm_state"] = self.prl.info(vm)["state"]
            except parallels.ParallelsError:
                item["vm_state"] = "missing"
                findings.append(contracts.finding("worker_vm_missing", f"Parallels does not list worker {run_id}'s "
                                                  "VM; destroy releases its claim", "warning"))
            workers.append(item)
        return workers, findings

    def status(self, run_id=None, name=None):
        """Every owned worker (after reconciling), one worker, or one exec."""
        if name is not None:
            self._owned(run_id)
            handle = self._handle(run_id, name)
            return contracts.result("status", "success", f"exec {name} is {handle['state']}", data=handle)
        if run_id is not None:
            return self._run_status(run_id)
        with locked(self.state / "controller.lock"):
            workers, findings = self._reconcile()
        allocated = {k: sum(w["allocation"][k] for w in workers if w["allocation"]) for k in contracts.BUDGET_BOUNDS}
        return contracts.result("status", "success", f"{len(workers)} owned worker(s)", findings, {
            "workers": workers, "resources": {"allocated": allocated, "budget": self.config["budget"]}})

    def _run_status(self, run_id):
        vm, record = self._owned(run_id)
        rdir = run_dir(self.state, run_id)
        if record["status"] in ("ready", "provisioning") or self.unfinished(run_id):
            self.supervise(run_id)
        execs = sorted(p.name for p in (rdir / "exec").iterdir() if EXEC_NAME.match(p.name)) \
            if (rdir / "exec").is_dir() else []
        used = self._used(run_id)
        data = {"run_id": run_id, "status": record["status"], "deadline": record["deadline"],
                "supervised": held(rdir / "supervisor.lock"), "artifact_mib_used": round(used / 2 ** 20, 1),
                "artifact_mib": record["allocation"]["artifact_mib"],
                "execs": {name: self._handle(run_id, name)["state"] for name in execs}}
        return contracts.result("status", "success", f"worker {run_id} is {record['status']}", data=data)

    # Operations

    def usable_baseline(self, name):
        """The promoted baseline workers of `name` come from and its registered
        VM, or the findings that make it unusable."""
        path = template.manifest_path(self.state, name)
        if not path.is_file():
            return None, None, [contracts.finding("baseline_missing", f"no promoted {name} baseline")]
        manifest = json.loads(path.read_text())
        errors = contracts.manifest_errors(manifest)
        strategy = contracts.clone_strategy(self.config, name)
        if not errors and manifest["os"] != name:
            errors.append(f"the {name} manifest describes a {manifest['os']} baseline")
        if not errors and strategy not in manifest["clone_modes"]:
            errors.append(f"clone_strategy {strategy} was not validated for this baseline")
        baseline_vm = self.reg.name_for(manifest["vm_id"])
        if baseline_vm is None:
            errors.append("the baseline VM is not registered to this controller")
        if errors:
            return None, None, [contracts.finding("baseline_invalid", "; ".join(errors))]
        return manifest, baseline_vm, []

    def create(self, name, revision, patch=None):
        op = "worker create"
        if name not in contracts.GUEST_OS:
            return contracts.result(op, "unsupported", f"{name} workers are not provided here", [
                contracts.finding("template_unsupported", f"workers are {', '.join(contracts.GUEST_OS)}")])
        manifest, baseline_vm, problems = self.usable_baseline(name)
        if problems:
            return contracts.result(op, "environment_failure", "baseline is not usable", problems)
        if name == "macos":
            return self._lease(manifest, baseline_vm, revision, patch)
        strategy = contracts.clone_strategy(self.config, name)
        # Admission and the claim are one step: no other controller process
        # can admit a worker between this one's check and its claim.
        with locked(self.state / "controller.lock"):
            allocation = dict(self.config["worker"])
            refused = self._admit(op)
            if refused:
                return refused
            # A clone can grow to the baseline's disk size, so that must fit the allocation.
            disk_mib = self.prl.info(baseline_vm)["disk_mib"]
            if disk_mib is None or disk_mib > allocation["storage_gib"] * 1024:
                return contracts.result(op, "environment_failure", "baseline disk exceeds the allocation", [
                    contracts.finding("storage_exhausted", f"the baseline disk is {disk_mib} MiB; a worker is "
                                      f"allotted {allocation['storage_gib']} GiB")])
            source = self._source(revision, patch)
            run_id = contracts.new_run_id()
            vm = contracts.worker_name(run_id)
            rdir, created, expires = self._open_run(run_id, patch)
            self.reg.claim(vm, "worker", name, run_id, expires, parent=manifest["vm_id"])
        record = None
        with locked(self._lock(run_id)):
            try:
                self.prl.clone(baseline_vm, vm, manifest["snapshot_id"], self.state / "workers", strategy == "linked")
                info = self.prl.info(vm)
                self.reg.bind(vm, info["vm_id"])
                present = sorted(set(info["devices"]) & set(template.HOST_DEVICES))
                if present:
                    raise RuntimeError(f"clone has host devices: {', '.join(present)}")
                self.prl.allocate(vm, allocation["cpus"], allocation["memory_mib"])
                record = {"schema": contracts.WORKER_SCHEMA, "run_id": run_id, "worker": vm, "vm_id": info["vm_id"],
                          "template": name, "candidate": manifest["candidate"], "baseline_vm_id": manifest["vm_id"],
                          "snapshot_id": manifest["snapshot_id"],
                          "reset_snapshot_id": self.prl.snapshot(vm, f"worker {run_id} baseline"),
                          "clone_strategy": strategy, "allocation": allocation, "source": source,
                          "status": "provisioning", "created_at": _stamp(created), "deadline": expires}
                self._save(record)
                # From here the run deadline holds even if this process dies.
                self.supervise(run_id)
                self._transfer(self._guest(record, boot=True), record)
                record["status"] = "ready"
                self._save(record)
            except Exception as err:  # every failure ends in owned cleanup and a recorded result
                notes = self._dispose(vm, rdir / "failure-console.png")
                if record:
                    record["status"] = "failed"
                    self._save(record)
                status = "timeout" if isinstance(err, template.DeadlineExceeded) else "environment_failure"
                return contracts.result(op, status, f"worker {run_id} was not created", [
                    contracts.finding("worker_create_failed", str(err)),
                    *(contracts.finding("cleanup_incomplete", n) for n in notes)], {"run_id": run_id})
        return contracts.result(op, "success", f"worker {run_id} is ready", data={
            "run_id": run_id, "worker": vm, "deadline": expires, "source": source, "allocation": allocation})

    # The macOS slot: one guest, leased (lease.py, §macOS workers)

    def _slot_blocked(self):
        """Why a free slot lock still cannot be granted, or None. A holder
        halted with its evidence (retained, stopped, expired) keeps the slot
        until it is destroyed; one whose lease process died has lost it."""
        for vm, entry in self.reg.entries().items():
            if entry["role"] != "slot" or not entry.get("holder"):
                continue
            holder = entry["holder"]
            record = _load(run_dir(self.state, holder) / "worker.json")
            if record is None or record["status"] in ("destroyed", "failed"):
                self.reg.hold(vm, None, None)
            elif record["status"] in contracts.FROZEN_STATES:
                return f"run {holder} is {record['status']} with uncollected evidence; destroy it to free the slot"
            else:
                # Its supervisor held the lease; the lock being free means it is gone.
                self._lose_lease(record, "its supervisor ended while it held the macOS slot")
                self.reg.hold(vm, None, None)
        return None

    def _slot_vm(self, manifest, baseline_vm, allocation):
        """The slot guest for the current baseline: kept while the baseline
        stays, replaced when another is promoted. Its reset snapshot is taken
        before it first starts."""
        known = _load(self.slot.record_path)
        if known and known["candidate"] == manifest["candidate"] and self.reg.owns(known["vm"]):
            return known
        if known and self.reg.owns(known["vm"]):
            if self._running(known["vm"]):
                self.prl.stop(known["vm"], kill=True)
            self.prl.delete(known["vm"])
            self.reg.release(known["vm"])
        vm = f"{contracts.SLOT_PREFIX}{contracts.new_run_id()}"
        self.reg.claim(vm, "slot", "macos", vm[len(contracts.SLOT_PREFIX):], None, parent=manifest["vm_id"])
        try:
            self._clone(baseline_vm, vm, manifest["snapshot_id"], "full")
            info = self.prl.info(vm)
            self.reg.bind(vm, info["vm_id"])
            present = sorted(set(info["devices"]) & set(template.HOST_DEVICES))
            if present:
                raise RuntimeError(f"the slot guest has host devices: {', '.join(present)}")
            self.prl.allocate(vm, allocation["cpus"], allocation["memory_mib"])
            known = {"vm": vm, "vm_id": info["vm_id"], "candidate": manifest["candidate"],
                     "reset_snapshot_id": self.prl.snapshot(vm, "slot baseline")}
        except Exception as err:  # a half-made slot guest is removed, or stays registered and reported
            notes = self._dispose(vm, self.slot.dir / "failure-console.png")
            raise RuntimeError(f"{_reason(err)}{'; ' + '; '.join(notes) if notes else ''}") from err
        template._write_json(self.slot.record_path, known)
        return known

    def _lease(self, manifest, baseline_vm, revision, patch):
        op = "worker create"
        source = self._source(revision, patch)
        run_id = contracts.new_run_id()

        def report(ahead, why):
            print(f"busybee-lab: {run_id} queued for the macOS slot ({ahead} ahead"
                  f"{': ' + why if why else ''})", file=sys.stderr, flush=True)
        try:
            held = self.slot.acquire(run_id, self.slot_wait_s, report, self._slot_blocked)
        except lease.Waited as err:
            return contracts.result(op, "environment_failure", "the macOS slot is busy",
                                    [contracts.finding("lease_wait_timeout", str(err))])
        try:
            with locked(self.state / "controller.lock"):
                refused = self._admit(op)
                if refused:
                    return refused
                allocation = dict(self.config["worker"])
                _, created, expires = self._open_run(run_id, patch)
                try:
                    slot = self._slot_vm(manifest, baseline_vm, allocation)
                except Exception as err:  # the slot could not be prepared; nothing was leased
                    return contracts.result(op, "environment_failure", "the macOS slot could not be prepared", [
                        contracts.finding(getattr(err, "code", "slot_unavailable"), _reason(err))])
                self.reg.hold(slot["vm"], run_id, expires)
            record = None
            with locked(self._lock(run_id)):
                try:
                    record = {"schema": contracts.WORKER_SCHEMA, "run_id": run_id, "worker": slot["vm"],
                              "vm_id": slot["vm_id"], "template": "macos", "candidate": manifest["candidate"],
                              "baseline_vm_id": manifest["vm_id"], "snapshot_id": manifest["snapshot_id"],
                              "reset_snapshot_id": slot["reset_snapshot_id"], "clone_strategy": "full",
                              "allocation": allocation, "source": source, "status": "provisioning",
                              "created_at": _stamp(created), "deadline": expires}
                    self._save(record)
                    self.event(run_id, "leased", worker=slot["vm"])
                    # The supervisor takes over the lease, and with it the run deadline.
                    self.supervise(run_id, lease=held)
                    # Whatever the last holder did, the guest starts from the slot's snapshot.
                    if self._running(slot["vm"]):
                        self.prl.stop(slot["vm"], kill=True)
                    self.prl.snapshot_switch(slot["vm"], slot["reset_snapshot_id"])
                    self.event(run_id, "reset", snapshot=slot["reset_snapshot_id"])
                    self._transfer(self._guest(record, boot=True), record)
                    record["status"] = "ready"
                    self._save(record)
                except Exception as err:  # the lease ends; the slot guest stays for the next holder
                    if record:
                        record["status"] = "failed"
                        self._save(record)
                    self.reg.hold(slot["vm"], None, None)
                    status = "timeout" if isinstance(err, template.DeadlineExceeded) else "environment_failure"
                    return contracts.result(op, status, f"worker {run_id} was not created", [
                        contracts.finding(getattr(err, "code", "worker_create_failed"), _reason(err))],
                        {"run_id": run_id})
        finally:
            held.close()  # the supervisor holds its own copy
        return contracts.result(op, "success", f"worker {run_id} is ready on the macOS slot", data={
            "run_id": run_id, "worker": slot["vm"], "deadline": expires, "source": source, "allocation": allocation})

    def _lose_lease(self, record, reason):
        record["status"] = "failed"
        self._save(record)
        self.event(record["run_id"], "lease_lost", reason=reason)

    def rehold(self, run_id):
        """A restarted supervisor's lease: the slot's lock while this run still
        holds the slot, else None, and a run that was working has lost it. A
        run whose creation still holds its operation lock is left alone: that
        creation holds the slot and starts the supervisor that keeps it."""
        if held(self._lock(run_id)):
            return None
        lock = self.slot.try_hold()
        if lock is not None and self.claimed(run_id):
            return lock
        if lock is not None:
            lock.close()
        record = _load(run_dir(self.state, run_id) / "worker.json")
        if record and record["status"] in ("ready", "provisioning"):
            self._lose_lease(record, "its supervisor ended and the macOS slot passed on")
        return None

    def _release(self, record):
        """End a macOS lease: the guest is stopped and kept for the next holder,
        whose grant restores it."""
        notes = []
        try:
            if self._running(record["worker"]):
                self.prl.stop(record["worker"], kill=True)
        except parallels.ParallelsError as err:
            notes.append(f"stopping the slot guest failed: {err}")
        record["status"] = "destroyed"
        self._save(record)
        if self.reg.get(record["worker"]) and self.reg.get(record["worker"]).get("holder") == record["run_id"]:
            self.reg.hold(record["worker"], None, None)
        self.event(record["run_id"], "released", notes=notes)
        return notes

    def _admit(self, op):
        """Under controller.lock: the refusal when another worker is active or
        the host lacks the allocation's storage, else None."""
        workers, notes = self._reconcile()
        active = [w["run_id"] for w in workers]
        if len(active) >= MAX_ACTIVE:
            return contracts.result(op, "environment_failure", "worker limit reached", [
                contracts.finding("worker_limit", f"{MAX_ACTIVE} worker(s) may be active; "
                                  f"destroy {', '.join(active)} first"), *notes])
        allotted, free = self.config["worker"]["storage_gib"], self.free_gib(self.state)
        if free < allotted:
            return contracts.result(op, "environment_failure", "not enough storage", [
                contracts.finding("storage_exhausted", f"a worker is allotted {allotted} GiB; {free} GiB is free")])
        return None

    def _open_run(self, run_id, patch):
        """The run directory with its patch; returns it, the creation time and the run deadline."""
        rdir = run_dir(self.state, run_id)
        rdir.mkdir(parents=True)
        if patch:
            evidence.durable(rdir / "source.patch", Path(patch).read_bytes())
        return rdir, _now(), self._start()

    def _source(self, revision, patch):
        found = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"{revision}^{{commit}}"], cwd=self.repo,
                               capture_output=True, text=True)
        if found.returncode != 0:
            raise Refused("source_invalid", f"{revision!r} is not a commit in this repository")
        if patch is not None and not Path(patch).is_file():
            raise Refused("source_invalid", "the patch is not a file")
        return {"revision": found.stdout.strip(),
                "patch_sha256": _sha256(Path(patch).read_bytes()) if patch is not None else None}

    # Commands: queued here, run and bounded by the supervisor

    def exec(self, run_id, argv, cwd, env, timeout, detach=False):
        op = "exec"
        if not argv:
            raise Refused("argv_empty", "exec needs a program to run")
        if not 1 <= timeout <= self.config["deadlines"]["scenario"]:
            raise Refused("timeout_invalid", f"timeout {timeout}s is outside 1..{self.config['deadlines']['scenario']}")
        bad = [k for k in env if not ENV_NAME.match(k)]
        if bad:
            raise Refused("env_invalid", f"not environment variable names: {', '.join(map(repr, bad))}")
        vm, record = self._owned(run_id)
        if record["status"] != "ready":
            raise Refused("worker_not_ready", f"worker {run_id} is {record['status']}")
        limit = int(min(timeout, self._run_window(record)))
        if limit < 1:
            return contracts.result(op, "timeout", f"worker {run_id}'s run deadline has passed", [
                contracts.finding("run_deadline_passed", f"the run ends at {record['deadline']}")])
        over = self.artifact_overrun(record)
        if over:
            raise Refused("artifact_budget_exceeded", over)
        edir = _next_dir(run_dir(self.state, run_id) / "exec")
        template._write_json(edir / "command.json", {
            "run_id": run_id, "exec": edir.name, "argv": argv, "cwd": cwd, "env": env, "timeout_s": limit,
            "stdout": self._rel(edir / "stdout"), "stderr": self._rel(edir / "stderr"), "queued_at": _stamp(_now())})
        self.supervise(run_id)
        if detach:
            return contracts.result(op, "success", f"exec {edir.name} is running; status, read and wait take it",
                                    data=self._handle(run_id, edir.name))
        return self.wait(run_id, edir.name)

    def _exec_dir(self, run_id, name):
        if not EXEC_NAME.match(str(name)):
            raise Refused("exec_invalid", f"{name!r} is not an exec number")
        edir = run_dir(self.state, run_id) / "exec" / name
        if not (edir / "command.json").is_file():
            raise Refused("exec_invalid", f"run {run_id} has no exec {name}")
        return edir

    def _handle(self, run_id, name):
        """Where one exec stands, without waiting for it. Quiet output is
        reported as such; it is never taken as a sign the command hangs."""
        edir = self._exec_dir(run_id, name)
        state, done = _load(edir / "state.json"), _load(edir / "result.json")
        sizes = {s: (edir / s).stat().st_size if (edir / s).is_file() else 0 for s in STREAMS}
        written = [(edir / s).stat().st_mtime for s in STREAMS if sizes[s]]
        if done:
            elapsed = done["data"].get("elapsed_s")
        else:
            elapsed = round(self.clock() - state["started"], 3) if state else None
        return {"run_id": run_id, "exec": name, "state": "finished" if done else "running" if state else "queued",
                "stdout": self._rel(edir / "stdout"), "stderr": self._rel(edir / "stderr"),
                "stdout_bytes": sizes["stdout"], "stderr_bytes": sizes["stderr"],
                "started_at": state["started_at"] if state else None, "elapsed_s": elapsed,
                "last_output_at": _stamp(datetime.fromtimestamp(max(written), timezone.utc)) if written else None,
                "deadline_at": _stamp(datetime.fromtimestamp(state["deadline_at"], timezone.utc)) if state else None,
                "result": done["status"] if done else None}

    def wait(self, run_id, name, timeout=None):
        """The exec's result once it has one. Without `timeout`, waits as long as
        the supervisor may take to end the command and record it."""
        self._owned(run_id)
        edir = self._exec_dir(run_id, name)
        if timeout is None:
            command = _load(edir / "command.json")
            timeout = command["timeout_s"] + HOST_MARGIN_S + self.config["deadlines"]["cleanup"]
        until = self.clock() + timeout
        while True:
            done = _load(edir / "result.json")
            if done:
                return done
            self.supervise(run_id)
            done = _load(edir / "result.json")
            if done:
                return done
            if self.clock() >= until:
                return contracts.result("wait", "timeout", f"exec {name} is still running", [
                    contracts.finding("still_running", f"exec {name} did not finish within {timeout}s; "
                                      "wait again or read its output")], self._handle(run_id, name))
            self.sleep(POLL_S)

    def read(self, run_id, name, stream, offset=0, limit=READ_LIMIT):
        """Bytes of one exec's stdout or stderr from `offset`. Successive reads
        from each `next_offset` see every byte once; `eof` is set only when the
        command has finished and nothing is left."""
        self._owned(run_id)
        if stream not in STREAMS:
            raise Refused("stream_invalid", f"{stream!r} is not one of {', '.join(STREAMS)}")
        if offset < 0 or not 1 <= limit <= READ_LIMIT:
            raise Refused("offset_invalid", f"offset must be >= 0 and limit within 1..{READ_LIMIT}")
        edir = self._exec_dir(run_id, name)
        # Checked before the size: once the result exists, the log is complete.
        finished = (edir / "result.json").exists()
        path = edir / stream
        size = path.stat().st_size if path.is_file() else 0
        if offset > size:
            raise Refused("offset_invalid", f"offset {offset} is past the {size} bytes {stream} holds")
        chunk = b""
        if size > offset:
            with open(path, "rb") as f:
                f.seek(offset)
                chunk = f.read(min(limit, size - offset))
        end = offset + len(chunk)
        return contracts.result("read", "success", f"{len(chunk)} byte(s) of {stream} from {offset}", data={
            "run_id": run_id, "exec": name, "stream": stream, "offset": offset, "next_offset": end, "size": size,
            "eof": finished and end == size, "content_b64": base64.b64encode(chunk).decode()})

    def _cancel(self, record, why):
        """End the run's unfinished commands before its guest is collected and changed."""
        run_id = record["run_id"]
        pending = self.unfinished(run_id)
        if not pending:
            return
        g = None
        try:
            if self.prl.info(record["worker"])["state"] == "running":
                g = self._guest(record)
        except (guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded) as err:
            self.event(run_id, "cancel_without_guest", reason=_reason(err))
        for name in pending:
            if g:
                try:
                    g.run(kill_command(guest_file(run_id, name, "pid")), self._bound("command"))
                except guest.GuestError as err:
                    self.event(run_id, "cancel_without_guest", exec=name, reason=_reason(err))
            edir = run_dir(self.state, run_id) / "exec" / name
            finalize(edir, contracts.result("exec", "cancelled", f"cancelled by worker {why}", [
                contracts.finding("exec_cancelled", f"worker {why} ended this command")],
                {**_load(edir / "command.json"), "finished_at": _stamp(_now()), "exit_code": None}))

    # Collection

    def _source_outputs(self, g, record):
        """What the guest's checkout holds beyond the recorded revision, as
        (artifact name, producer) pairs."""
        base = record["source"]["revision"]
        co = self.checkout(record)
        head = g.run(f"git -C {co} rev-parse HEAD", self._bound("command"))[1].strip()
        status = g.run(f"git -C {co} status --porcelain --untracked-files=all", self._bound("command"))[1]
        outputs = [("source.json", lambda: json.dumps({"base": base, "head": head, "status": status,
                                                       "patch_sha256": record["source"]["patch_sha256"]},
                                                      indent=2).encode())]
        if head != base:
            outputs.append(("commits.bundle", lambda: g.run(
                f"git -C {co} bundle create - HEAD ^{base}", self._bound("command"), raw=True)[1]))
        # Uncommitted and untracked changes, without touching the agent's index.
        outputs.append(("worktree.diff", lambda: g.run(
            f"cd {co} && i=$(mktemp) && cp .git/index $i && GIT_INDEX_FILE=$i git add -A && "
            f"GIT_INDEX_FILE=$i git diff --cached --binary HEAD; s=$?; rm -f $i; exit $s",
            self._bound("command"), raw=True)[1]))
        return outputs

    def _observe(self, g, record):
        processes = parse_processes(g.run(PS, self._bound("command"))[1])
        facts = self._facts(g, record["template"])
        return {"processes": processes, "daemons": {k: facts[k] for k in ("processes", "sockets", "paths")}}

    def _baseline(self, record):
        """The recorded facts of the baseline this worker was cloned from."""
        tdir = template.manifest_path(self.state, record["template"]).parent
        known = [_load(tdir / "manifest.json") or {}] + (_load(tdir / "retained.json") or [])
        found = next((m for m in known if m.get("candidate") == record["candidate"]), None)
        keys = ("name", "candidate", "arch", "provisioning_revision", "lock_hashes", "tools", "parallels_version",
                "validated_at")
        return {k: found[k] for k in keys} if found else None

    def _commands(self, rdir):
        commands = []
        for edir in sorted(p for p in (rdir / "exec").iterdir() if EXEC_NAME.match(p.name)) \
                if (rdir / "exec").is_dir() else []:
            command = _load(edir / "command.json")
            if command is None:
                continue
            done = _load(edir / "result.json")
            data = done["data"] if done else {}
            commands.append({**{k: command[k] for k in ("exec", "argv", "cwd", "env", "timeout_s", "stdout", "stderr")},
                             **{k: data.get(k) for k in ("started_at", "finished_at", "elapsed_s", "exit_code",
                                                          "provenance", "enforced_by")},
                             "status": done["status"] if done else "unfinished",
                             "findings": [f["code"] for f in done["findings"]] if done else []})
        return commands

    def _scenarios(self, rdir):
        records = evidence.scenario_records(rdir)
        results = [{"exec": r["exec"], "scenario": r["scenario"], "mode": r["mode"], "status": r["status"],
                    "failed": r["failed"], "path": self._rel(rdir / "scenarios" / r["exec"] / "result.json")}
                   for r in records]
        return {"results": results, "coverage": evidence.coverage(records)}

    def collect_run(self, record, reachable=True):
        """Export source changes and evidence; return the manifest, its path and
        what is missing. A frozen worker has not changed since its last
        attempt, so that attempt is completed: what it saved durably is
        acknowledged and only the rest is fetched."""
        rdir = run_dir(self.state, record["run_id"])
        try:
            latest = _latest_dir(rdir / "collect")
            frozen = record["status"] in contracts.FROZEN_STATES and latest is not None
            cdir = latest if frozen else _next_dir(rdir / "collect")
        except OSError as err:
            return None, None, [f"collection directory: {_reason(err)}"]
        try:
            previous = (_load(cdir / "collected.json") or {}) if frozen else {}
        except ValueError:  # torn by an interrupted write: everything is fetched again
            previous = {}
        saved = previous.get("artifacts", {})
        artifacts, acknowledged, missing = {}, [], []

        def durable(name):
            path = cdir / name
            return path.is_file() and saved.get(self._rel(path)) == _sha256(path.read_bytes())

        kept = [n for n in ("source.json", "commits.bundle", "worktree.diff") if durable(n)]
        source = json.loads((cdir / "source.json").read_text()) if "source.json" in kept else None
        complete = source is not None and "worktree.diff" in kept and \
            (source["head"] == source["base"] or "commits.bundle" in kept) and \
            not any(m.startswith("terminal ") for m in previous.get("missing", []))
        for name in kept:
            artifacts[self._rel(cdir / name)] = saved[self._rel(cdir / name)]
            acknowledged.append(self._rel(cdir / name))
        observations = previous.get("observations")
        outputs = []
        if not complete and not reachable:
            missing.append("source: the guest is not answering")
        elif not complete:
            if record["status"] in contracts.FROZEN_STATES:
                self.supervise(record["run_id"])  # halts it again if this process dies while it runs
            try:
                g = self._guest(record, boot=True)
                outputs = [(n, p) for n, p in self._source_outputs(g, record) if n not in kept]
                observations = self._observe(g, record)
                missing += self._fetch_terminals(g, rdir)
            except Exception as err:  # unreachable guest: every source artifact is missing
                missing.append(f"source: {_reason(err)}")
        for name, produce in outputs:
            try:
                data = produce()
                evidence.durable(cdir / name, data)
                artifacts[self._rel(cdir / name)] = _sha256(data)
            except Exception as err:  # recorded as missing, never skipped
                missing.append(f"{name}: {_reason(err)}")
        if self._rel(cdir / "source.json") in artifacts:
            source = json.loads((cdir / "source.json").read_text())
        # Logs, results, checkpoints, console screenshots and terminals are
        # already on the host; make them durable and digest them.
        on_host = sorted(rdir.glob("exec/*/*")) + sorted(rdir.glob("scenarios/*/*")) + sorted(rdir.glob("checkpoint/*")) \
            + sorted(rdir.glob("console/*")) + sorted(p for p in rdir.glob("terminal/**/*") if p.is_file())
        for path in on_host:
            if path.name not in EVIDENCE_FILES and path.relative_to(rdir).parts[0] not in ("console", "terminal"):
                continue
            try:
                with open(path, "rb") as f:
                    os.fsync(f.fileno())
                digest = _sha256(path.read_bytes())
            except OSError as err:
                missing.append(f"{self._rel(path)}: {_reason(err)}")
                continue
            artifacts[self._rel(path)] = digest
            if saved.get(self._rel(path)) == digest:
                acknowledged.append(self._rel(path))
        baseline = self._baseline(record)
        if baseline is None:
            missing.append(f"template: no record of baseline {record['candidate']}")
        manifest = {"schema": contracts.EVIDENCE_SCHEMA, "run_id": record["run_id"], "collected_at": _stamp(_now()),
                    "source": source or {"base": record["source"]["revision"], "head": None, "status": None,
                                         "patch_sha256": record["source"]["patch_sha256"]},
                    "template": baseline, "allocation": record["allocation"], "deadline": record["deadline"],
                    "commands": self._commands(rdir), "observations": observations,
                    "cleanup": self.events(record["run_id"]), "artifacts": artifacts,
                    "acknowledged": acknowledged, "missing": missing, "scenarios": self._scenarios(rdir),
                    "terminals": screen.handles(rdir)}
        path = cdir / "collected.json"
        try:
            evidence.durable(path.with_suffix(".tmp"), (json.dumps(manifest, indent=2) + "\n").encode())
            os.replace(path.with_suffix(".tmp"), path)
        except OSError as err:
            missing.append(f"collected.json: {_reason(err)}")
        return manifest, path, missing

    def _fetch_terminals(self, g, rdir):
        """Bring every `vmctl terminal` handle's recording and captures up to
        date from the guest; returns what could not be fetched. Scenario
        terminals were fetched when their scenario finished."""
        missing = []
        for meta_path in sorted(rdir.glob("terminal/*/handle.json")):
            meta = json.loads(meta_path.read_text())
            try:
                fetch_terminal(g, self._bound("command"), meta["guest_dir"], meta_path.parent)
                screen.render_handle(meta_path.parent)
            except (guest.GuestError, tarfile.TarError, OSError, screen.RecordingError, screen.RenderError) as err:
                missing.append(f"terminal {meta_path.parent.name}: {_reason(err)}")
        return missing

    def checkpoint(self, g, record):
        """Replace the run's source checkpoint with what the guest holds now, so
        a guest that dies later loses at most the work since. Returns False when
        an operation holds the worker: it collects the source itself."""
        with locked(self._lock(record["run_id"]), wait=False) as got:
            if got:
                self._checkpoint(g, record)
            return got

    def _checkpoint(self, g, record):
        rdir = run_dir(self.state, record["run_id"])
        fresh, current, old = rdir / "checkpoint.tmp", rdir / "checkpoint", rdir / "checkpoint.old"
        shutil.rmtree(fresh, ignore_errors=True)
        for name, produce in self._source_outputs(g, record):
            evidence.durable(fresh / name, produce())
        if current.exists():
            shutil.rmtree(old, ignore_errors=True)
            os.replace(current, old)
        os.replace(fresh, current)
        shutil.rmtree(old, ignore_errors=True)

    def retain(self, op, record, missing):
        self.halt(record)
        record["status"] = "retained"
        self._save(record)
        self.event(record["run_id"], "retained", missing=missing)
        return contracts.result(op, "incomplete_collection", f"worker {record['run_id']} kept, stopped", [
            contracts.finding("artifact_missing", m) for m in missing], {"run_id": record["run_id"],
                                                                         "missing": missing, "vm_state": "stopped"})

    def collect(self, run_id):
        vm, record = self._owned(run_id)
        with locked(self._lock(run_id)):
            self._window("cleanup")
            manifest, path, missing = self.collect_run(record)
            if record["status"] in contracts.FROZEN_STATES:
                self.halt(record)  # it was started only to finish collecting
        data = {"run_id": run_id, "missing": missing}
        findings = [contracts.finding("artifact_missing", m) for m in missing]
        if manifest:
            data.update(manifest=self._rel(path), artifacts=manifest["artifacts"], acknowledged=manifest["acknowledged"])
        over = self.artifact_overrun(record)
        if over:
            findings.append(contracts.finding("artifact_budget_exceeded", over, "warning"))
        if missing:
            return contracts.result("collect", "incomplete_collection", "some artifacts were not saved", findings, data)
        return contracts.result("collect", "success", f"{len(manifest['artifacts'])} artifact(s) saved", findings,
                                data)

    # Lifecycle

    def reset(self, run_id):
        op = "worker reset"
        vm, record = self._owned(run_id)
        if record["status"] == "failed":
            raise Refused("worker_not_ready", f"worker {run_id} failed creation; destroy it")
        if self._run_window(record) <= 0:
            raise Refused("run_deadline_passed", f"worker {run_id}'s run ended at {record['deadline']}; destroy it")
        with locked(self._lock(run_id)):
            self._window("cleanup")
            self._cancel(record, "reset")
            manifest = None
            if record["status"] != "provisioning":
                manifest, _, missing = self.collect_run(record)
                if missing:
                    return self.retain(op, record, missing)
            if self.prl.info(vm)["state"] != "stopped":
                self.prl.stop(vm, kill=True)
            self.prl.snapshot_switch(vm, record["reset_snapshot_id"])
            record["status"] = "provisioning"
            self._save(record)
            self.event(run_id, "reset", snapshot=record["reset_snapshot_id"])
            self.supervise(run_id)  # halts the worker if this process dies before the source is back
            self._window()
            try:
                self._transfer(self._guest(record, boot=True), record)
            except Exception as err:  # the restored worker has no source; say so, keep it destroyable
                return contracts.result(op, "environment_failure", f"worker {run_id} was restored without its source",
                                        [contracts.finding("reset_incomplete", _reason(err))],
                                        {"run_id": run_id, "collected": manifest})
            record["status"] = "ready"
            self._save(record)
        return contracts.result(op, "success", f"worker {run_id} restored to its recorded baseline", data={
            "run_id": run_id, "restored_snapshot_id": record["reset_snapshot_id"], "collected": manifest})

    def destroy(self, run_id):
        op = "worker destroy"
        try:
            vm, record = self._owned(run_id, need_record=False)
        except Refused:
            return self._destroyed_before(run_id)
        rdir = run_dir(self.state, run_id)
        rdir.mkdir(parents=True, exist_ok=True)
        with locked(self._lock(run_id)):
            self._window("cleanup")
            manifest, findings = None, []
            if self._gone(vm):
                # Deleted already: destroy deletes only after a complete collection.
                self.reg.release(vm)
                findings.append(contracts.finding("worker_vm_gone", f"worker {run_id}'s VM no longer existed; "
                                                  "its claim was released", "warning"))
            else:
                # Nothing reached the guest yet, or the source was never transferred back.
                if record is not None and record["status"] not in ("failed", "provisioning"):
                    self._cancel(record, "destroy")
                    manifest, _, missing = self.collect_run(record)
                    if missing:
                        return self.retain(op, record, missing)
                if record is not None and record["template"] == "macos":
                    notes = self._release(record)
                    return contracts.result(op, "success", f"worker {run_id} released the macOS slot", [
                        contracts.finding("cleanup_incomplete", n, "warning") for n in notes],
                        data={"run_id": run_id, "collected": manifest})
                notes = self._dispose(vm, rdir / "final-console.png")
                if notes:
                    return contracts.result(op, "environment_failure", f"worker {run_id} was not removed",
                                            [contracts.finding("cleanup_incomplete", n) for n in notes])
            if record is not None:
                record["status"] = "destroyed"
                self._save(record)
                self.event(run_id, "destroyed")
        return contracts.result(op, "success", f"worker {run_id} destroyed", findings, data={
            "run_id": run_id, "collected": manifest})

    def _destroyed_before(self, run_id):
        """A repeated destroy: the claim is gone, and so is the VM it named."""
        record = _load(run_dir(self.state, run_id) / "worker.json") if contracts.valid_run_id(run_id) else None
        # A macOS run's lease is gone (released, or lost with its supervisor); the slot guest stays.
        leased = record is not None and record["template"] == "macos"
        if record is None or (record["vm_id"] in self._host_vms() and not leased):
            self._owned(run_id, need_record=False)  # raises the refusal
        if record["status"] != "destroyed":
            record["status"] = "destroyed"
            self._save(record)
        return contracts.result("worker destroy", "success", f"worker {run_id} was already destroyed",
                                data={"run_id": run_id, "collected": None})

    def inspect(self, run_id):
        vm, record = self._owned(run_id)
        self._window()
        info = self.prl.info(vm)
        data = {"run_id": run_id, "status": record["status"], "vm_state": info["state"],
                "deadline": record["deadline"], "allocation": record["allocation"], "source": record["source"]}
        if info["state"] == "running":
            data.update(self._observe(self._guest(record), record))
        return contracts.result("inspect", "success", f"worker {run_id} is {info['state']}", data=data)

    def signal(self, run_id, name, pid):
        if name not in SIGNALS:
            raise Refused("signal_invalid", f"{name!r} is not one of {', '.join(SIGNALS)}")
        if not (pid.isdigit() and int(pid) > 1):
            raise Refused("pid_invalid", f"{pid!r} is not a single process id above 1")
        _, record = self._owned(run_id)
        self._window()
        status, _, err = self._guest(record).run(f"kill -s {name} {int(pid)}", self._bound("command"), check=False)
        if status != 0:
            return contracts.result("signal", "environment_failure", f"no signal delivered to {pid}",
                                    [contracts.finding("process_missing", err.strip() or f"kill exited {status}")])
        return contracts.result("signal", "success", f"sent {name} to {pid}", data={"run_id": run_id, "pid": int(pid)})

    def console_capture(self, run_id):
        vm, record = self._owned(run_id)
        self._window()
        state = self.prl.info(vm)["state"]
        if state != "running":
            return contracts.result("console capture", "environment_failure", "no display to capture", [
                contracts.finding("console_unavailable", f"worker {run_id} is {state}; Parallels captures only "
                                  "a running display")])
        path = console_path(self.state, run_id)
        self.prl.capture(vm, path)
        data = path.read_bytes() if path.is_file() else b""
        if not data.startswith(b"\x89PNG"):
            return contracts.result("console capture", "environment_failure", "capture produced no PNG",
                                    [contracts.finding("console_unavailable", "Parallels wrote no image")])
        return contracts.result("console capture", "success", f"{len(data)} bytes",
                                data={"run_id": run_id, "path": self._rel(path)})

    def export(self, run_id):
        """A sanitized copy of the run's evidence for publishing; works after destroy too."""
        if not contracts.valid_run_id(run_id):
            raise Refused("target_invalid", f"{run_id!r} is not a run id")
        record = _load(run_dir(self.state, run_id) / "worker.json")
        if record is None:
            raise Refused("target_not_owned", f"no recorded run {run_id}")
        out = evidence.export(self.repo, self.state, record)
        return contracts.result("export", "success", f"public evidence for run {run_id}",
                                [contracts.finding("artifact_missing", m, "warning") for m in out["missing"]], {
            "run_id": run_id, "path": self._rel(out["path"]), "files": len(out["files"]),
            "withheld": sorted(out["withheld"]), "missing": out["missing"], "redactions": out["redactions"]})
