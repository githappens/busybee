"""Linux worker templates: build a candidate, validate a clone of it, promote it.

See docs/design/agent-lab.md §Build and validate templates. A candidate is a
dedicated VM installed from the pinned installer in infra/vm/linux: its live
installer environment is the Linux builder, so nothing is built on the host.
Only a candidate whose clone passed every capability check can be promoted,
and promotion never deletes the baseline it replaces.
"""
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import secrets
import subprocess
import time
import urllib.request

import contracts
import guest
import parallels
import registry

# Devices a worker must not have: each one reaches into the host and some
# raise macOS privacy prompts when the VM starts.
HOST_DEVICES = ("sound0", "usb", "serial0")
CAPABILITIES = ("host_isolation", "boot", "command_access", "file_roundtrip", "terminal_transport", "console_capture",
                "task_state", "shutdown", "snapshot_reset")
# The guest disk, expanding. A built worker uses about 7.5 GiB, most of it the
# dev shell in the Nix store; this leaves room for release builds and tests.
DISK_MIB = 16 * 1024
INFRA = Path("infra/vm")
GUEST_CHECKOUT = "/root/busybee"
INSTALLER_BOOT_S = 300
TYPE_RETRIES = 3


class DeadlineExceeded(RuntimeError):
    pass


def candidate_dir(state, name, run_id):
    return Path(state) / "templates" / name / "candidates" / run_id


def manifest_path(state, name):
    return Path(state) / "templates" / name / "manifest.json"


def _write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    # Per process: the controller and a run's supervisor write the same records.
    tmp = path.with_suffix(f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    os.replace(tmp, path)


def _now():
    return datetime.now(timezone.utc)


def parse_facts(text):
    facts = {"processes": [], "sockets": [], "paths": [], "authorized_keys": []}
    kinds = {"process": "processes", "socket": "sockets", "path": "paths", "key": "authorized_keys"}
    for line in text.splitlines():
        kind, _, value = line.partition(": ")
        if kind in kinds:
            facts[kinds[kind]].append(value.strip())
    return facts


def task_state_problems(facts, bootstrap_key):
    """What a baseline must not carry: task processes, sockets, state or any key
    other than the designated bootstrap access."""
    problems = [f"process {p} is running" for p in facts["processes"]]
    problems += [f"socket {s} exists" for s in facts["sockets"]]
    problems += [f"task state {p} exists" for p in facts["paths"]]
    problems += [f"unexpected authorized key {k.split()[-1]}" for k in facts["authorized_keys"]
                 if k.strip() != bootstrap_key.strip()]
    return problems


def ineligibility(checks):
    reasons = []
    for name in CAPABILITIES:
        check = checks.get(name)
        if check is None:
            reasons.append(f"{name}: not checked")
        elif check["status"] != "pass":
            reasons.append(f"{name}: {check['status']}: {check.get('reason', 'no reason recorded')}")
    return reasons


def promote(state, name, run_id):
    """Make a validated candidate the baseline; keep the one it replaces."""
    path = candidate_dir(state, name, run_id) / "candidate.json"
    if not path.is_file():
        return contracts.result("template promote", "environment_failure", "no such candidate",
                                [contracts.finding("candidate_missing", f"no {name} candidate {run_id}")])
    record = json.loads(path.read_text())
    if record["status"] != "validated":
        return contracts.result("template promote", "environment_failure", "candidate is not eligible", [
            contracts.finding("candidate_not_validated",
                              f"candidate {run_id} is {record['status']}; only a validated candidate is promoted")])
    errors = contracts.manifest_errors(record["manifest"])
    if errors:
        return contracts.result("template promote", "environment_failure", "candidate manifest is invalid",
                                [contracts.finding("baseline_invalid", "; ".join(errors))])
    current = manifest_path(state, name)
    if current.is_file():
        # The replaced baseline's VM and snapshot stay: workers cloned from it
        # keep their parent until they are destroyed.
        retained_path = current.parent / "retained.json"
        retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
        _write_json(retained_path, retained + [json.loads(current.read_text())])
    _write_json(current, record["manifest"])
    return contracts.result("template promote", "success", f"{name} baseline is candidate {run_id}",
                            data={"manifest": record["manifest"]})


def prune(state, name, prl, reg):
    """Delete retained baselines no registered clone depends on; keep and report the rest."""
    retained_path = manifest_path(state, name).parent / "retained.json"
    retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
    current_path = manifest_path(state, name)
    current = json.loads(current_path.read_text())["vm_id"] if current_path.is_file() else None
    kept, pruned, findings = [], [], []
    for baseline in retained:
        vm_id = baseline["vm_id"]
        users = sorted(n for n, e in reg.entries().items() if e.get("parent") == vm_id)
        vm = reg.name_for(vm_id)
        if vm_id == current:
            findings.append(contracts.finding("retained_is_current", f"retained {baseline['candidate']} is the "
                                              f"current {name} baseline; it stays", "warning"))
        elif users:
            findings.append(contracts.finding("retained_in_use", f"retained {baseline['candidate']} stays: "
                                              f"{', '.join(users)} cloned from it", "warning"))
        elif vm is None:
            findings.append(contracts.finding("retained_not_owned", f"retained {baseline['candidate']} has no "
                                              "registered VM; it was not touched"))
        else:
            try:
                prl.delete(vm)
            except parallels.ParallelsError as err:
                findings.append(contracts.finding("cleanup_incomplete", f"{vm} stays registered: {err}"))
            else:
                reg.release(vm)
                pruned.append(baseline["candidate"])
                continue
        kept.append(baseline)
    _write_json(retained_path, kept)
    status = "environment_failure" if any(f["severity"] == "error" for f in findings) else "success"
    return contracts.result("template prune", status, f"pruned {len(pruned)}, kept {len(kept)} retained baseline(s)",
                            findings, {"pruned": pruned, "kept": [b["candidate"] for b in kept]})


def _sha256(path):
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


class Lab:
    """Template operations against real Parallels, bounded by the config's deadlines."""

    def __init__(self, repo, config, prl, reg):
        self.repo, self.config, self.prl, self.reg = Path(repo), config, prl, reg
        self.state = contracts.state_dir(config, self.repo)
        self.deadline = None

    # Deadlines: the whole operation gets `run`; one command gets `command`,
    # one long step (install, warm) gets `scenario`; none outlives the run.
    def _remaining(self):
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise DeadlineExceeded("the operation's run deadline passed")
        return left

    def _bound(self, kind):
        return min(self.config["deadlines"][kind], self._remaining())

    def _until(self, seconds):
        return time.monotonic() + min(seconds, self._remaining())

    def _start(self):
        self.deadline = time.monotonic() + self.config["deadlines"]["run"]
        return (_now() + timedelta(seconds=self.config["deadlines"]["run"])).strftime(contracts.TIMESTAMP)

    def _wait_state(self, vm, state, seconds):
        until = self._until(seconds)
        while time.monotonic() < until:
            if self.prl.info(vm)["state"] == state:
                return
            time.sleep(2)
        raise DeadlineExceeded(f"{vm} did not reach {state}")

    def _shutdown(self, vm):
        self.prl.stop(vm)
        self._wait_state(vm, "stopped", self.config["deadlines"]["command"])

    def _dispose(self, vm, evidence):
        """Owned cleanup: capture the console, stop, delete, release."""
        notes = []
        if self.reg.get(vm) is None:
            return notes
        if not self.reg.get(vm)["vm_id"]:
            # Claimed, but the run failed before recording the VM's identity:
            # it may or may not exist. Delete by the claimed name; only an
            # answered delete releases the claim.
            try:
                self.prl.delete(vm)
            except parallels.ParallelsError as err:
                notes.append(f"{vm} may exist without a recorded identity; its claim stays: {err}")
                return notes
        else:
            try:
                # Parallels can only capture a running display.
                running = self.prl.info(vm)["state"] != "stopped"
                if running:
                    self.prl.capture(vm, evidence)
            except parallels.ParallelsError as err:
                running = True
                notes.append(f"console capture failed: {err}")
            try:
                if running:
                    self.prl.stop(vm, kill=True)
                self.prl.delete(vm)
            except parallels.ParallelsError as err:
                # Still registered: the VM is owned and findable for cleanup.
                notes.append(f"cleanup of {vm} failed, it stays registered: {err}")
                return notes
        self.reg.release(vm)
        return notes

    def _installer(self, pin):
        cache = self.state / "cache"
        iso = cache / Path(pin["url"]).name
        if not iso.is_file():
            cache.mkdir(parents=True, exist_ok=True)
            partial = iso.with_suffix(".partial")
            with urllib.request.urlopen(pin["url"], timeout=60) as response, open(partial, "wb") as out:
                while block := response.read(1 << 20):
                    out.write(block)
            partial.rename(iso)
        actual = _sha256(iso)
        if actual != pin["sha256"]:
            raise RuntimeError(f"installer {iso.name} has sha256 {actual}, pinned {pin['sha256']}")
        return iso

    def _provenance(self, pin):
        revision = subprocess.run(["git", "-C", str(self.repo), "rev-parse", "HEAD"], capture_output=True,
                                  text=True, check=True).stdout.strip()
        dirty = subprocess.run(["git", "-C", str(self.repo), "status", "--porcelain", "--", str(INFRA)],
                               capture_output=True, text=True, check=True).stdout.strip()
        files = sorted(p for p in (self.repo / INFRA).rglob("*") if p.is_file() and p.name != "local.example.toml")
        hashes = {str(p.relative_to(self.repo)): f"sha256:{_sha256(p)}" for p in files}
        hashes["flake.lock"] = f"sha256:{_sha256(self.repo / 'flake.lock')}"
        hashes["installer"] = f"sha256:{pin['sha256']}"
        return revision + ("+dirty" if dirty else ""), hashes

    def build(self, name, arch):
        pin = json.loads((self.repo / INFRA / name / "installer.json").read_text())
        if arch != pin["arch"]:
            return contracts.result("template build", "unsupported", f"no pinned {name} installer for {arch}", [
                contracts.finding("arch_unsupported", f"{name} templates are pinned for {pin['arch']} only")])
        run_id = contracts.new_run_id()
        vm = f"{registry.PREFIX}tpl-{name}-{run_id}"
        cdir = candidate_dir(self.state, name, run_id)
        cdir.mkdir(parents=True)
        record = {"run_id": run_id, "vm": vm, "status": "building", "arch": arch}
        _write_json(cdir / "candidate.json", record)
        expires = self._start()
        try:
            iso = self._installer(pin)
            for key in ("access", "host"):
                subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", f"busybee-lab-{key}",
                                "-f", str(cdir / key)], check=True)
            self.reg.claim(vm, "candidate", name, run_id, expires)
            workers = self.state / "workers"
            workers.mkdir(parents=True, exist_ok=True)
            self.prl.create(vm, workers, DISK_MIB)
            info = self.prl.info(vm)
            self.reg.bind(vm, info["vm_id"])
            budget = self.config["budget"]
            self.prl.configure(vm, budget["cpus"], budget["memory_mib"], iso)
            self.prl.start(vm)

            ip = guest.wait_for_lease(info["mac"], self._until(INSTALLER_BOOT_S))
            live = guest.Guest(ip, cdir / "access", cdir / "installer_known_hosts", accept_new=True)
            self._authorize_installer(vm, live, (cdir / "access.pub").read_text().strip())
            self._install(live, cdir, pin)
            self._shutdown(vm)
            self.prl.boot_from_disk(vm)
            self.prl.start(vm)

            (cdir / "known_hosts").write_text(f"{ip} {' '.join((cdir / 'host.pub').read_text().split()[:2])}\n")
            installed = guest.Guest(ip, cdir / "access", cdir / "known_hosts")
            installed.wait(self._until(self.config["deadlines"]["command"]))
            tools = self._warm_and_clean(installed)
            problems = task_state_problems(self._facts(installed), (cdir / "access.pub").read_text())
            if problems:
                raise RuntimeError("baseline would carry task state: " + "; ".join(problems))
            guest_arch = installed.run("uname -m", self._bound("command"))[1].strip()
            self._shutdown(vm)
            snapshot = self.prl.snapshot(vm, f"baseline {run_id}")
        except Exception as err:  # every failure ends in owned cleanup and a recorded result
            notes = self._dispose(vm, cdir / "failure-console.png")
            record.update(status="failed", reason=str(err), cleanup=notes)
            _write_json(cdir / "candidate.json", record)
            status = "timeout" if isinstance(err, DeadlineExceeded) else "environment_failure"
            return contracts.result("template build", status, f"candidate {run_id} failed", [
                contracts.finding("template_build_failed", str(err)),
                *(contracts.finding("cleanup_incomplete", n) for n in notes)], {"candidate": run_id})

        revision, hashes = self._provenance(pin)
        parallels_version = self.prl.query(["--version"]).split()[2]
        record.update(status="built", manifest={
            "schema": contracts.TEMPLATE_SCHEMA, "name": name, "candidate": run_id, "os": "linux",
            "arch": guest_arch, "vm_id": info["vm_id"], "snapshot_id": snapshot,
            "provisioning_revision": revision, "lock_hashes": hashes, "tools": tools,
            "parallels_version": parallels_version, "clone_modes": [], "validated_at": None})
        _write_json(cdir / "candidate.json", record)
        return contracts.result("template build", "success", f"candidate {run_id} built; validate it next",
                                data={"candidate": run_id, "vm": vm, "snapshot_id": snapshot,
                                      "provenance": {"revision": revision, "lock_hashes": hashes, "tools": tools}})

    def _authorize_installer(self, vm, live, public_key):
        # The stock installer has no key and no guest tools: type one command
        # on its auto-login console to authorize the run's access key.
        command = f"\nsudo sh -c 'mkdir -p -m 700 /root/.ssh && echo \"{public_key}\" > /root/.ssh/authorized_keys'\n"
        # sshd opens shortly before the auto-login prompt; keys typed earlier
        # land in the boot log instead of the shell.
        guest.wait_for_port(live.ip, 22, self._until(INSTALLER_BOOT_S))
        last = None
        for _ in range(TYPE_RETRIES):
            time.sleep(min(15, self._remaining()))
            self.prl.type_text(vm, command)
            try:
                live.wait(self._until(30))
                return
            except guest.GuestError as err:
                last = err
        raise guest.GuestError(f"installer did not accept the typed access key: {last}")

    def _install(self, live, cdir, pin):
        bundle = subprocess.run(["tar", "-C", str(self.repo / INFRA), "-cf", "-", "flake.nix", "flake.lock",
                                 "linux"], capture_output=True, check=True).stdout
        live.run("rm -rf /tmp/infra && mkdir -p /tmp/infra && tar -C /tmp/infra -xf -", self._bound("command"),
                 stdin=bundle)
        live.run("cat > /tmp/authorized", self._bound("command"), stdin=(cdir / "access.pub").read_bytes())
        live.run("umask 077 && cat > /tmp/hostkey", self._bound("command"), stdin=(cdir / "host").read_bytes())
        _, out, err = live.run(f"bash /tmp/infra/linux/install.sh /tmp/infra {pin['configuration']} "
                               "/tmp/authorized /tmp/hostkey", self._bound("scenario"))
        (cdir / "install.log").write_text(out + err)

    def _warm_and_clean(self, installed):
        source = subprocess.run(["git", "-C", str(self.repo), "archive", "HEAD"], capture_output=True,
                                check=True).stdout
        installed.run(f"mkdir -p {GUEST_CHECKOUT} && tar -C {GUEST_CHECKOUT} -xf -", self._bound("command"),
                      stdin=source)
        # Fetch the dev shell and the crate sources; start neither busybee nor pueued.
        installed.run(f"cd {GUEST_CHECKOUT} && nix develop -c cargo fetch", self._bound("scenario"))
        tools = {}
        for tool, command in (("nixos", "nixos-version"), ("nix", "nix --version"), ("kernel", "uname -r"),
                              ("rustc", f"cd {GUEST_CHECKOUT} && nix develop -c rustc --version")):
            tools[tool] = installed.run(command, self._bound("command"))[1].strip().splitlines()[-1]
        installed.run("bash -s", self._bound("command"), stdin=(self.repo / INFRA / "linux" / "clean.sh").read_bytes())
        return tools

    def _facts(self, g):
        script = (self.repo / INFRA / "linux" / "facts.sh").read_bytes()
        return parse_facts(g.run("bash -s", self._bound("command"), stdin=script)[1])

    def validate(self, name, run_id):
        cdir = candidate_dir(self.state, name, run_id)
        record = json.loads((cdir / "candidate.json").read_text())
        if record["status"] not in ("built", "validated", "rejected"):
            return contracts.result("template validate", "environment_failure", "candidate was not built", [
                contracts.finding("candidate_not_built", f"candidate {run_id} is {record['status']}")])
        manifest = record["manifest"]
        strategy = self.config["clone_strategy"]
        val_id = contracts.new_run_id()
        vm = f"{registry.PREFIX}val-{name}-{val_id}"
        vdir = cdir / "validations" / val_id
        vdir.mkdir(parents=True)
        expires = self._start()
        checks = {}
        timed_out = False
        try:
            self.reg.claim(vm, "validation", name, val_id, expires)
            self.prl.clone(record["vm"], vm, manifest["snapshot_id"], self.state / "workers", strategy == "linked")
            info = self.prl.info(vm)
            self.reg.bind(vm, info["vm_id"])
            self._run_checks(vm, info, cdir, vdir, checks)
        except Exception as err:  # recorded against the check that was running
            timed_out = isinstance(err, DeadlineExceeded)
            pending = next(c for c in CAPABILITIES if checks.get(c, {}).get("status") != "pass")
            checks.setdefault(pending, {"status": "fail", "reason": str(err)})
        finally:
            notes = self._dispose(vm, vdir / "final-console.png")

        reasons = ineligibility(checks)
        if notes:
            reasons += [f"cleanup: {n}" for n in notes]
        report = {"validation": val_id, "clone_strategy": strategy, "checks": checks, "ineligible": reasons}
        _write_json(vdir / "validation.json", report)
        if reasons:
            record["status"] = "rejected"
        else:
            record["status"] = "validated"
            manifest.update(clone_modes=[strategy], validated_at=_now().strftime(contracts.TIMESTAMP))
        _write_json(cdir / "candidate.json", record)
        if reasons:
            status = "timeout" if timed_out else "environment_failure"
            return contracts.result("template validate", status, f"candidate {run_id} is not eligible",
                                    [contracts.finding("capability_failed", r) for r in reasons], report)
        return contracts.result("template validate", "success", f"candidate {run_id} is eligible", data=report)

    def _run_checks(self, vm, info, cdir, vdir, checks):
        def passed(name, **detail):
            checks[name] = {"status": "pass", **detail}

        present = sorted(set(info["devices"]) & set(HOST_DEVICES))
        if present:
            checks["host_isolation"] = {"status": "fail", "reason": f"clone has host devices: {', '.join(present)}"}
            raise RuntimeError("clone has host devices")
        passed("host_isolation")

        self.prl.start(vm)
        ip = guest.wait_for_lease(info["mac"], self._until(self.config["deadlines"]["command"]))
        (vdir / "known_hosts").write_text(f"{ip} {' '.join((cdir / 'host.pub').read_text().split()[:2])}\n")
        g = guest.Guest(ip, cdir / "access", vdir / "known_hosts")
        g.wait(self._until(self.config["deadlines"]["command"]))
        passed("boot", ip_assigned=True)

        passed("command_access", nixos=g.run("nixos-version", self._bound("command"))[1].strip())

        payload = secrets.token_bytes(1 << 20)
        g.run("cat > /root/roundtrip", self._bound("command"), stdin=payload)
        back = g.run("cat /root/roundtrip && rm /root/roundtrip", self._bound("command"), raw=True)[1]
        if back != payload:
            raise RuntimeError("file round-trip returned different bytes")
        passed("file_roundtrip", bytes=len(payload))

        _, out, _ = g.run("tty; stty size", self._bound("command"), tty=True)
        lines = out.split()
        if not lines or not lines[0].startswith("/dev/pts/"):
            raise RuntimeError(f"no pseudo-terminal over the transport: {out.strip()!r}")
        passed("terminal_transport", tty=lines[0], size=" ".join(lines[1:3]))

        shot = vdir / "console.png"
        self.prl.capture(vm, shot)
        data = shot.read_bytes() if shot.is_file() else b""
        if not data.startswith(b"\x89PNG") or len(data) < 1024:
            checks["console_capture"] = {"status": "unsupported", "reason": "capture produced no usable PNG"}
            raise RuntimeError("console capture produced no usable PNG")
        passed("console_capture", bytes=len(data))

        problems = task_state_problems(self._facts(g), (cdir / "access.pub").read_text())
        if problems:
            checks["task_state"] = {"status": "fail", "reason": "; ".join(problems)}
            raise RuntimeError("clone carries task state")
        passed("task_state")

        self._shutdown(vm)
        passed("shutdown")

        reset = self.prl.snapshot(vm, "validation reset point")
        self.prl.start(vm)
        g.wait(self._until(self.config["deadlines"]["command"]))
        g.run("touch /root/reset-marker", self._bound("command"))
        self._shutdown(vm)
        self.prl.snapshot_switch(vm, reset)
        self.prl.start(vm)
        g.wait(self._until(self.config["deadlines"]["command"]))
        if g.run("test -e /root/reset-marker", self._bound("command"), check=False)[0] == 0:
            raise RuntimeError("a file written after the snapshot survived the reset")
        passed("snapshot_reset", snapshot_id=reset)
