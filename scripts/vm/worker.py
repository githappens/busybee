"""Workers: owned clones of the promoted baseline, and bounded commands in them.

See docs/design/agent-lab.md §Run an issue from reproduction to review,
§Controller interface and §Bound execution and preserve failures. A worker is
claimed in the registry, naming the baseline it depends on, before its clone
exists, and its record (contracts.worker_errors) is written before any guest
work. Reset restores the snapshot recorded at creation, never the newest one.
Reset and destroy collect first; a failed collection keeps the clone, stopped,
and reports what is missing. Deadlines hold while this controller runs:
supervision that outlives it is not provided here.
"""
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time

import contracts
import guest
import parallels
import template

# One active worker until concurrency becomes a budgeted controller setting.
MAX_ACTIVE = 1
CHECKOUT = template.GUEST_CHECKOUT
# Seconds timeout(1) waits after TERM before KILL, and the host's margin on top.
KILL_GRACE_S = 10
HOST_MARGIN_S = KILL_GRACE_S + guest.SSH_CONNECT_S + 20
SIGNALS = ("TERM", "KILL", "INT", "HUP", "QUIT", "USR1", "USR2", "STOP", "CONT")
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
BINARIES = ("busybee", "bzb", "bzbd")


class Refused(Exception):
    """A request the controller will not act on; nothing was changed."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def run_dir(state, run_id):
    return Path(state) / "runs" / run_id


def _now():
    return datetime.now(timezone.utc)


def _stamp(moment):
    return moment.strftime(contracts.TIMESTAMP)


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _durable(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def _reason(err):
    """An error for a result: an OSError's message would name a host path."""
    return err.strerror if isinstance(err, OSError) and err.strerror else str(err)


def _next_dir(parent):
    parent.mkdir(parents=True, exist_ok=True)
    taken = [int(p.name) for p in parent.iterdir() if p.name.isdigit()]
    path = parent / f"{max(taken, default=0) + 1:04d}"
    path.mkdir()
    return path


def exec_command(argv, cwd, env, timeout, status_path):
    """The guest shell command for one exec. Every word is quoted, so spaces and
    shell metacharacters reach the program literally. The exit status is also
    written to `status_path`, which tells a command's own 255 from ssh's."""
    # timeout(1) stops parsing options at the duration, so `--` goes before it.
    words = ["env", *(f"{k}={v}" for k, v in env.items()), "timeout", "-k", str(KILL_GRACE_S), "--", str(timeout),
             *argv]
    q = shlex.quote
    return (f"cd {q(cwd)} && {' '.join(map(q, words))}; s=$?; echo $s > {q(status_path)}; exit $s")


def _after_exec(status_path):
    """Read and remove the recorded status, then report source and binary provenance."""
    binaries = " ".join(f"build/*/{b}" for b in BINARIES)
    return (f'printf "status: %s\\n" "$(cat {status_path} 2>/dev/null)"; rm -f {status_path}; '
            f'cd {CHECKOUT} || exit 0; printf "head: %s\\n" "$(git rev-parse HEAD)"; '
            f'printf "dirty: %s\\n" "$(git status --porcelain | wc -l)"; '
            f'for f in {binaries}; do [ -f "$f" ] && sha256sum "$f" | sed "s/^/binary: /"; done; true')


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


def parse_processes(text):
    processes = []
    for line in text.splitlines():
        fields = line.split(None, 5)
        if len(fields) == 6 and fields[0].isdigit():
            pid, ppid, user, stat, elapsed, args = fields
            processes.append({"pid": int(pid), "ppid": int(ppid), "user": user, "stat": stat,
                              "elapsed_s": int(elapsed), "args": args})
    return processes


class Workers(template.Lab):
    """Worker operations against Parallels. `free_gib(path)` measures host
    storage; `connect` returns command access to a running worker, and tests
    substitute it."""

    def __init__(self, repo, config, prl, reg, free_gib, connect=None):
        super().__init__(repo, config, prl, reg)
        self.connect = connect or self._ssh
        self.free_gib = free_gib

    # Ownership and records

    def _owned(self, run_id, need_record=True):
        """The registry entry and record of a worker this controller created."""
        if not contracts.valid_run_id(run_id):
            raise Refused("target_invalid", f"{run_id!r} is not a run id")
        vm = contracts.worker_name(run_id)
        entry = self.reg.get(vm)
        if entry is None or entry["role"] != "worker" or entry["run_id"] != run_id:
            raise Refused("target_not_owned", f"no registered worker for run {run_id}")
        path = run_dir(self.state, run_id) / "worker.json"
        record = json.loads(path.read_text()) if path.is_file() else None
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
        deadline = datetime.strptime(record["deadline"], contracts.TIMESTAMP).replace(tzinfo=timezone.utc)
        left = (deadline - _now()).total_seconds()
        self.deadline = time.monotonic() + left
        return left

    # Guest access

    def _ssh(self, record, info, deadline):
        cdir = template.candidate_dir(self.state, record["template"], record["candidate"])
        ip = guest.wait_for_lease(info["mac"], deadline)
        known = run_dir(self.state, record["run_id"]) / "known_hosts"
        known.write_text(f"{ip} {' '.join((cdir / 'host.pub').read_text().split()[:2])}\n")
        g = guest.Guest(ip, cdir / "access", known)
        g.wait(deadline)
        return g

    def _guest(self, record, boot=False):
        vm = record["worker"]
        info = self.prl.info(vm)
        if info["state"] != "running":
            if not boot:
                raise Refused("worker_stopped", f"worker {record['run_id']} is {info['state']}")
            self.prl.start(vm)
        return self.connect(record, info, self._until(self.config["deadlines"]["command"]))

    def _transfer(self, g, record):
        """The recorded revision with its history and tags, as a guest-local checkout."""
        revision = record["source"]["revision"]
        bundle = subprocess.run(["git", "bundle", "create", "-", revision, "--tags"], cwd=self.repo,
                                capture_output=True, check=True).stdout
        # A bare revision is no ref in the bundle, so unbundle and recreate the tags.
        g.run(f"rm -rf {CHECKOUT} /var/tmp/source.bundle && cat > /var/tmp/source.bundle && "
              f"git init -q {CHECKOUT} && cd {CHECKOUT} && git bundle unbundle /var/tmp/source.bundle | "
              f"while read -r id ref; do git update-ref \"$ref\" \"$id\"; done && "
              f"git checkout -q --detach {revision} && rm /var/tmp/source.bundle",
              self._bound("command"), stdin=bundle)
        patch = run_dir(self.state, record["run_id"]) / "source.patch"
        if record["source"]["patch_sha256"]:
            g.run(f"cd {CHECKOUT} && git apply", self._bound("command"), stdin=patch.read_bytes())
        head = g.run(f"git -C {CHECKOUT} rev-parse HEAD", self._bound("command"))[1].strip()
        if head != revision:
            raise RuntimeError(f"guest checkout is at {head}, not {revision}")

    def _halt(self, vm):
        if self.prl.info(vm)["state"] == "stopped":
            return
        try:
            self._shutdown(vm)
        except (template.DeadlineExceeded, parallels.ParallelsError):
            self.prl.stop(vm, kill=True)

    # Operations

    def create(self, name, revision, patch=None):
        op = "worker create"
        if name != "linux":
            return contracts.result(op, "unsupported", f"{name} workers are not provided here", [
                contracts.finding("template_unsupported", "only linux workers are cloned by this controller")])
        path = template.manifest_path(self.state, name)
        if not path.is_file():
            return contracts.result(op, "environment_failure", "no baseline", [
                contracts.finding("baseline_missing", f"no promoted {name} baseline")])
        manifest = json.loads(path.read_text())
        errors = contracts.manifest_errors(manifest)
        strategy = self.config["clone_strategy"]
        if not errors and strategy not in manifest["clone_modes"]:
            errors.append(f"clone_strategy {strategy} was not validated for this baseline")
        baseline_vm = self.reg.name_for(manifest["vm_id"])
        if baseline_vm is None:
            errors.append("the baseline VM is not registered to this controller")
        if errors:
            return contracts.result(op, "environment_failure", "baseline is not usable",
                                    [contracts.finding("baseline_invalid", "; ".join(errors))])
        active = sorted(e["run_id"] for e in self.reg.entries().values() if e["role"] == "worker")
        if len(active) >= MAX_ACTIVE:
            return contracts.result(op, "environment_failure", "worker limit reached", [
                contracts.finding("worker_limit", f"{MAX_ACTIVE} worker(s) may be active; "
                                  f"destroy {', '.join(active)} first")])
        allocation = dict(self.config["worker"])
        # A clone can grow to the baseline's disk size, so that must fit the allocation.
        disk_mib = self.prl.info(baseline_vm)["disk_mib"]
        if disk_mib is None or disk_mib > allocation["storage_gib"] * 1024:
            return contracts.result(op, "environment_failure", "baseline disk exceeds the allocation", [
                contracts.finding("storage_exhausted", f"the baseline disk is {disk_mib} MiB; a worker is allotted "
                                  f"{allocation['storage_gib']} GiB")])
        free = self.free_gib(self.state)
        if free < allocation["storage_gib"]:
            return contracts.result(op, "environment_failure", "not enough storage", [
                contracts.finding("storage_exhausted", f"a worker is allotted {allocation['storage_gib']} GiB; "
                                  f"{free} GiB is free")])
        source = self._source(revision, patch)

        run_id = contracts.new_run_id()
        vm = contracts.worker_name(run_id)
        rdir = run_dir(self.state, run_id)
        rdir.mkdir(parents=True)
        if patch:
            _durable(rdir / "source.patch", Path(patch).read_bytes())
        created = _now()
        expires = self._start()
        record = None
        try:
            self.reg.claim(vm, "worker", name, run_id, expires, parent=manifest["vm_id"])
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

    def _source(self, revision, patch):
        found = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"{revision}^{{commit}}"], cwd=self.repo,
                               capture_output=True, text=True)
        if found.returncode != 0:
            raise Refused("source_invalid", f"{revision!r} is not a commit in this repository")
        if patch is not None and not Path(patch).is_file():
            raise Refused("source_invalid", "the patch is not a file")
        return {"revision": found.stdout.strip(),
                "patch_sha256": _sha256(Path(patch).read_bytes()) if patch is not None else None}

    def exec(self, run_id, argv, cwd, env, timeout, clock=time.monotonic):
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
        g = self._guest(record)
        edir = _next_dir(run_dir(self.state, run_id) / "exec")
        status_path = f"/var/tmp/busybee-lab-exec-{run_id}-{edir.name}"
        data = {"run_id": run_id, "exec": edir.name, "argv": argv, "cwd": cwd, "env": env, "timeout_s": limit,
                "stdout": self._rel(edir / "stdout"), "stderr": self._rel(edir / "stderr"),
                "started_at": _stamp(_now())}
        template._write_json(edir / "command.json", data)
        started = clock()
        try:
            with open(edir / "stdout", "wb") as out, open(edir / "stderr", "wb") as err:
                ssh_status = g.stream(exec_command(argv, cwd, env, limit, status_path), limit + HOST_MARGIN_S,
                                      out, err)
        except guest.GuestError as err:
            return self._contain(record, edir, data, str(err))
        elapsed = clock() - started
        self._window("cleanup")
        after = parse_after_exec(g.run(_after_exec(status_path), self._bound("command"), check=False)[1])
        data.update(finished_at=_stamp(_now()), elapsed_s=round(elapsed, 3), exit_code=after["exit_code"],
                    provenance={"source": record["source"], "head": after["head"],
                                "dirty_files": after["dirty_files"], "binaries": after["binaries"]})
        template._write_json(edir / "result.json", data)
        code = after["exit_code"]
        if code is None:
            return contracts.result(op, "environment_failure", "the command's exit status was not recorded", [
                contracts.finding("exit_status_missing", f"ssh exited {ssh_status} and the guest recorded "
                                  "no status")], data)
        if code in (124, 137) and elapsed >= limit:
            return contracts.result(op, "timeout", f"exceeded {limit}s", [
                contracts.finding("command_timeout", f"stopped after its {limit}s deadline")], data)
        if code != 0:
            return contracts.result(op, "product_failure", f"exited {code}",
                                    [contracts.finding("command_failed", f"exited {code}")], data)
        return contracts.result(op, "success", "exited 0", data=data)

    def _contain(self, record, edir, data, reason):
        """Guest control failed: keep what the console shows, then stop the VM.
        The worker stays registered for collection or destruction."""
        self._window("cleanup")
        notes = []
        vm = record["worker"]
        try:
            self.prl.capture(vm, edir / "console.png")
        except parallels.ParallelsError as err:
            notes.append(f"console capture failed: {err}")
        try:
            self.prl.stop(vm, kill=True)
            record["status"] = "stopped"
            self._save(record)
        except parallels.ParallelsError as err:
            notes.append(f"stopping {vm} failed: {err}")
        data.update(finished_at=_stamp(_now()), exit_code=None)
        template._write_json(edir / "result.json", data)
        return contracts.result("exec", "timeout", "the guest stopped answering; the worker was stopped", [
            contracts.finding("guest_unresponsive", reason),
            *(contracts.finding("cleanup_incomplete", n) for n in notes)], data)

    def _collect(self, record):
        """Export source changes and log digests; return the manifest and what is missing."""
        rdir = run_dir(self.state, record["run_id"])
        artifacts, missing = {}, []
        try:
            cdir = _next_dir(rdir / "collect")
        except OSError as err:
            return None, None, [f"collection directory: {_reason(err)}"]
        try:
            g = self._guest(record, boot=True)
            base = record["source"]["revision"]
            head = g.run(f"git -C {CHECKOUT} rev-parse HEAD", self._bound("command"))[1].strip()
            status = g.run(f"git -C {CHECKOUT} status --porcelain --untracked-files=all", self._bound("command"))[1]
            outputs = [("source.json", lambda: json.dumps({"base": base, "head": head, "status": status,
                                                           "patch_sha256": record["source"]["patch_sha256"]},
                                                          indent=2).encode())]
            if head != base:
                outputs.append(("commits.bundle", lambda: g.run(
                    f"git -C {CHECKOUT} bundle create - HEAD ^{base}", self._bound("command"), raw=True)[1]))
            # Uncommitted and untracked changes, without touching the agent's index.
            outputs.append(("worktree.diff", lambda: g.run(
                f"cd {CHECKOUT} && i=$(mktemp) && cp .git/index $i && GIT_INDEX_FILE=$i git add -A && "
                f"GIT_INDEX_FILE=$i git diff --cached --binary HEAD; s=$?; rm -f $i; exit $s",
                self._bound("command"), raw=True)[1]))
        except Exception as err:  # unreachable guest: every source artifact is missing
            outputs = []
            missing.append(f"source: {err}")
        for name, produce in outputs:
            try:
                data = produce()
                _durable(cdir / name, data)
                artifacts[self._rel(cdir / name)] = _sha256(data)
            except Exception as err:  # recorded as missing, never skipped
                missing.append(f"{name}: {_reason(err)}")
        for log in sorted((rdir / "exec").glob("*/std*")) if (rdir / "exec").is_dir() else []:
            try:
                with open(log, "rb") as f:
                    os.fsync(f.fileno())
                artifacts[self._rel(log)] = _sha256(log.read_bytes())
            except OSError as err:
                missing.append(f"{self._rel(log)}: {_reason(err)}")
        manifest = {"run_id": record["run_id"], "collected_at": _stamp(_now()), "artifacts": artifacts,
                    "missing": missing}
        try:
            _durable(cdir / "collected.json", json.dumps(manifest, indent=2).encode())
        except OSError as err:
            missing.append(f"collected.json: {_reason(err)}")
        return manifest, cdir / "collected.json", missing

    def _retain(self, op, record, missing):
        self._halt(record["worker"])
        record["status"] = "retained"
        self._save(record)
        return contracts.result(op, "incomplete_collection", f"worker {record['run_id']} kept, stopped", [
            contracts.finding("artifact_missing", m) for m in missing], {"run_id": record["run_id"],
                                                                         "missing": missing, "vm_state": "stopped"})

    def collect(self, run_id):
        _, record = self._owned(run_id)
        self._window()
        manifest, path, missing = self._collect(record)
        data = {"run_id": run_id, "missing": missing}
        if path:
            data.update(manifest=self._rel(path), artifacts=manifest["artifacts"])
        if missing:
            return contracts.result("collect", "incomplete_collection", "some artifacts were not saved",
                                    [contracts.finding("artifact_missing", m) for m in missing], data)
        return contracts.result("collect", "success", f"{len(manifest['artifacts'])} artifact(s) saved", data=data)

    def reset(self, run_id):
        op = "worker reset"
        vm, record = self._owned(run_id)
        if record["status"] == "failed":
            raise Refused("worker_not_ready", f"worker {run_id} failed creation; destroy it")
        self._window()
        manifest = None
        if record["status"] != "provisioning":
            manifest, _, missing = self._collect(record)
            if missing:
                return self._retain(op, record, missing)
        if self.prl.info(vm)["state"] != "stopped":
            self.prl.stop(vm, kill=True)
        self.prl.snapshot_switch(vm, record["reset_snapshot_id"])
        record["status"] = "provisioning"
        self._save(record)
        self._window()
        try:
            self._transfer(self._guest(record, boot=True), record)
        except Exception as err:  # the restored worker has no source; say so, keep it destroyable
            return contracts.result(op, "environment_failure", f"worker {run_id} was restored without its source", [
                contracts.finding("reset_incomplete", _reason(err))], {"run_id": run_id, "collected": manifest})
        record["status"] = "ready"
        self._save(record)
        return contracts.result(op, "success", f"worker {run_id} restored to its recorded baseline", data={
            "run_id": run_id, "restored_snapshot_id": record["reset_snapshot_id"], "collected": manifest})

    def destroy(self, run_id):
        op = "worker destroy"
        vm, record = self._owned(run_id, need_record=False)
        rdir = run_dir(self.state, run_id)
        self._window()
        manifest = None
        # Nothing reached the guest yet, or the source was never transferred back.
        if record is not None and record["status"] not in ("failed", "provisioning"):
            manifest, _, missing = self._collect(record)
            if missing:
                return self._retain(op, record, missing)
        notes = self._dispose(vm, rdir / "final-console.png")
        if notes:
            return contracts.result(op, "environment_failure", f"worker {run_id} was not removed",
                                    [contracts.finding("cleanup_incomplete", n) for n in notes])
        if record is not None:
            record["status"] = "destroyed"
            self._save(record)
        return contracts.result(op, "success", f"worker {run_id} destroyed", data={
            "run_id": run_id, "collected": manifest})

    def inspect(self, run_id):
        vm, record = self._owned(run_id)
        self._window()
        info = self.prl.info(vm)
        data = {"run_id": run_id, "status": record["status"], "vm_state": info["state"],
                "deadline": record["deadline"], "allocation": record["allocation"], "source": record["source"]}
        if info["state"] == "running":
            g = self._guest(record)
            data["processes"] = parse_processes(g.run("ps -eo pid=,ppid=,user=,stat=,etimes=,args=",
                                                      self._bound("command"))[1])
            facts = self._facts(g)
            data["daemons"] = {k: facts[k] for k in ("processes", "sockets", "paths")}
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
        path = run_dir(self.state, run_id) / "console" / f"{_now().strftime('%Y%m%dT%H%M%S%fZ')}.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        self.prl.capture(vm, path)
        data = path.read_bytes() if path.is_file() else b""
        if not data.startswith(b"\x89PNG"):
            return contracts.result("console capture", "environment_failure", "capture produced no PNG",
                                    [contracts.finding("console_unavailable", "Parallels wrote no image")])
        return contracts.result("console capture", "success", f"{len(data)} bytes",
                                data={"run_id": run_id, "path": self._rel(path)})
