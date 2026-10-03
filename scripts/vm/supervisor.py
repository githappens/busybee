"""The run supervisor: the watchdog outside the guest and outside the agent.

See docs/design/agent-lab.md §Bound execution and preserve failures. One
supervisor process per worker, started detached in its own session by the first
controller call that needs it and holding `supervisor.lock` in the run
directory for its lifetime. It runs every queued exec and enforces, whether or
not the process that queued the command is still alive:

- the command deadline: past `timeout -k` and a margin, the guest's own bound
  has failed, so the command's process group is killed;
- the artifact budget: a command whose logs outgrow it is killed;
- the run deadline: unfinished commands are killed, the worker is collected
  within the cleanup window and halted, and it becomes `expired`;
- guest control: when the guest stops answering, its console and the evidence
  already on the host are kept and the VM is stopped.

A supervisor that dies leaves its ssh processes writing to the run's files; the
next one adopts them and reads each command's exit status from the guest. No
host service is installed: the process is a plain child of the controller that
outlives it.
"""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

import contracts
import guest
import lease
import parallels
import template
import worker

KILL_WAIT_S = 5


def ensure(state, run_id, argv, wait_s=10, lease=None):
    """Start the run's supervisor unless one holds its lock. `argv` runs it.
    `lease`, the macOS slot's held lock file, is inherited by the supervisor,
    which holds the lease from then on."""
    rdir = worker.run_dir(state, run_id)
    lock = rdir / "supervisor.lock"
    until = time.monotonic() + wait_s
    # A supervisor started meanwhile by reconciliation, without the lease, gives
    # way at once (worker.rehold); the one holding the lease must still start.
    while lease and worker.held(lock) and time.monotonic() < until:
        time.sleep(0.1)
    if worker.held(lock):
        if lease:
            raise worker.Refused("supervisor_unavailable", f"run {run_id} has a supervisor that does not hold "
                                 "its lease")
        return
    fds = (lease.fileno(),) if lease else ()
    if lease:
        argv = [*argv, "--lease-fd", str(lease.fileno())]
    with open(rdir / "supervisor.log", "ab") as log:
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
                                 pass_fds=fds)
    until = time.monotonic() + wait_s
    while time.monotonic() < until:
        # Watching, or already done because nothing was left to watch.
        if worker.held(lock) or child.poll() == 0:
            return
        if child.poll() is not None:
            break
        time.sleep(0.1)
    raise worker.Refused("supervisor_unavailable", f"run {run_id}'s supervisor did not start; see its supervisor.log")


def serve(workers, run_id, lease_fd=None):
    """The supervisor process: tick until there is nothing left to watch. A
    macOS run's supervisor holds the slot's lease: inherited from `worker
    create` as `lease_fd`, or taken back on a restart while the run still
    holds it."""
    lock = worker.run_dir(workers.state, run_id) / "supervisor.lock"
    with worker.locked(lock, wait=False) as got:
        if not got:
            return  # another supervisor already watches this run
        record = worker._load(worker.run_dir(workers.state, run_id) / "worker.json")
        held = None
        if record and record["template"] == "macos" and lease_fd is None:
            held = workers.rehold(run_id)
            if held is None:
                return  # the lease is gone; rehold recorded why
        workers.event(run_id, "supervisor_started", pid=os.getpid())
        watch = Supervisor(workers, run_id)
        while True:
            try:
                if not watch.tick():
                    break
            except Exception as err:  # logged, and the deadlines stay enforced on the next tick
                workers.event(run_id, "supervisor_error", reason=worker._reason(err))
                watch.g = None
            time.sleep(worker.POLL_S)
        workers.event(run_id, "supervisor_finished", pid=os.getpid())


class Supervisor:
    def __init__(self, workers, run_id, alive=lease.alive):
        self.w, self.run_id = workers, run_id
        self.clock = workers.clock
        self.alive = alive
        self.rdir = worker.run_dir(workers.state, run_id)
        self.procs = {}  # exec name -> the ssh process this supervisor started
        self.g = None

    def _record(self):
        return worker._load(self.rdir / "worker.json")

    def tick(self):
        """One pass over the run. Returns whether there is anything left to watch."""
        record = self._record()
        if not self.w.claimed(self.run_id) or record is None \
                or record["status"] == "destroyed":
            return False
        if record["status"] in ("ready", "provisioning") and self.clock() >= worker.run_deadline(record):
            self._expire()
        for name in self.w.unfinished(self.run_id):
            self._step(name)
        record = self._record()
        if record["status"] == "ready":
            over = self.w.artifact_overrun(record)
            for name in self.w.unfinished(self.run_id) if over else ():
                self._enforce(name, "environment_failure", "artifact_budget_exceeded", over)
        record = self._record()
        busy = worker.held(self.w._lock(self.run_id))
        if record["status"] == "provisioning" and not busy:
            self._interrupted()
        if record["status"] in contracts.FROZEN_STATES:
            # Started again only to finish collecting: halt it once nothing holds it.
            running = self.w._running(record["worker"])
            if running and not busy:
                self._refreeze()
            return busy or running
        return self._record()["status"] in ("ready", "provisioning") or bool(self.w.unfinished(self.run_id))

    # One exec

    def _edir(self, name):
        return self.rdir / "exec" / name

    def _connect(self, kind="command"):
        """Command access, reused while it works; a failure raises GuestError."""
        self.w._window(kind)
        if self.g is None:
            self.g = self.w._guest(self._record())
        return self.g

    def _step(self, name):
        state = worker._load(self._edir(name) / "state.json")
        if state is None:
            if self._record()["status"] != "ready":
                self._finish_without(name, "environment_failure", "worker_not_ready",
                                     f"the worker was {self._record()['status']} when the command was due to start")
                return
            if not self._start(name):
                return
            state = worker._load(self._edir(name) / "state.json")
        proc = self.procs.get(name)
        done = proc.poll() is not None if proc else not (state["ssh_pid"] and self.alive(state["ssh_pid"]))
        if done:
            self._finish(name, state)
        elif self.clock() > state["deadline_at"] + worker.HOST_MARGIN_S:
            self._enforce(name, "timeout", "watchdog_deadline",
                          f"still running {worker.HOST_MARGIN_S}s past its {state['deadline_at'] - state['started']:.0f}s "
                          "deadline; the guest's own bound failed, so the supervisor killed it")

    def _start(self, name):
        edir = self._edir(name)
        command = worker._load(edir / "command.json")
        now = self.clock()
        state = {"started_at": worker._stamp(worker._now()), "started": now, "deadline_at": now + command["timeout_s"],
                 "ssh_pid": None, "supervisor_pid": os.getpid()}
        # Recorded before the spawn: a supervisor that dies in between leaves a
        # started exec for the next one to finish, never one it would start twice.
        template._write_json(edir / "state.json", state)
        files = (worker.guest_file(self.run_id, name, "status"), worker.guest_file(self.run_id, name, "pid"))
        try:
            g = self._connect()
            with open(edir / "stdout", "wb") as out, open(edir / "stderr", "wb") as err:
                proc = g.spawn(worker.exec_command(command["argv"], command["cwd"], command["env"],
                                                   command["timeout_s"], *files), out, err)
        except (guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded, worker.Refused) as err:
            self._contain(worker._reason(err))
            return False
        self.procs[name] = proc
        template._write_json(edir / "state.json", {**state, "ssh_pid": proc.pid})
        return True

    def _after(self, name):
        g = self._connect("cleanup")
        files = (worker.guest_file(self.run_id, name, "status"), worker.guest_file(self.run_id, name, "pid"))
        # The query ends in `true`: any other status means ssh never reached the guest.
        return worker.parse_after_exec(g.run(worker._after_exec(*files, self.w.checkout(self._record())),
                                             self.w._bound("command"))[1])

    def _outcome(self, name, state, after, status, summary, findings, enforced_by=None):
        command = worker._load(self._edir(name) / "command.json")
        record = self._record()
        data = {**command, "started_at": state["started_at"], "finished_at": worker._stamp(worker._now()),
                "elapsed_s": round(self.clock() - state["started"], 3), "exit_code": after["exit_code"],
                "enforced_by": enforced_by,
                "provenance": {"source": record["source"], "head": after["head"],
                               "dirty_files": after["dirty_files"], "binaries": after["binaries"]}}
        worker.finalize(self._edir(name), contracts.result("exec", status, summary, findings, data))
        self.procs.pop(name, None)

    def _finish(self, name, state):
        try:
            after = self._after(name)
            if after["exit_code"] is None:
                # Its connection ended without a status: whatever it left running goes too.
                self.g.run(worker.kill_command(worker.guest_file(self.run_id, name, "pid")),
                           self.w._bound("command"))
        except (guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded, worker.Refused) as err:
            self._contain(worker._reason(err))
            return
        # Checkpointed before the result is published, so whoever acts on the
        # result finds the source the command left.
        try:
            self.w._window("command")
            if not self.w.checkpoint(self.g, self._record()):
                self.w.event(self.run_id, "checkpoint_skipped", exec=name, reason="an operation holds the worker")
        except (guest.GuestError, template.DeadlineExceeded, OSError) as err:
            self.w.event(self.run_id, "checkpoint_failed", exec=name, reason=worker._reason(err))
        code = after["exit_code"]
        limit = state["deadline_at"] - state["started"]
        elapsed = self.clock() - state["started"]
        if code is None:
            self._outcome(name, state, after, "environment_failure", "the command's exit status was not recorded", [
                contracts.finding("exit_status_missing", "the connection ended and the guest recorded no status")])
        elif code in (124, 137) and elapsed >= limit:
            self._outcome(name, state, after, "timeout", f"exceeded {limit:.0f}s", [
                contracts.finding("command_timeout", f"stopped after its {limit:.0f}s deadline")])
        elif code != 0:
            self._outcome(name, state, after, "product_failure", f"exited {code}",
                          [contracts.finding("command_failed", f"exited {code}")])
        else:
            self._outcome(name, state, after, "success", "exited 0", [])

    def _finish_without(self, name, status, code, message):
        command = worker._load(self._edir(name) / "command.json")
        worker.finalize(self._edir(name), contracts.result("exec", status, message, [
            contracts.finding(code, message)], {**command, "finished_at": worker._stamp(worker._now()),
                                                "exit_code": None, "enforced_by": "supervisor"}))

    def _enforce(self, name, status, code, message):
        """Kill one command's process group in the guest and record why."""
        state = worker._load(self._edir(name) / "state.json")
        if state is None:
            self._finish_without(name, status, code, message)
            return
        try:
            self._connect("cleanup").run(worker.kill_command(worker.guest_file(self.run_id, name, "pid")),
                                         self.w._bound("command"))
        except (guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded, worker.Refused) as err:
            self._contain(f"{message}; killing it failed: {worker._reason(err)}")
            return
        self._reap(name, state)
        try:
            after = self._after(name)
        except (guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded, worker.Refused) as err:
            self._contain(worker._reason(err))
            return
        self.w.event(self.run_id, code, exec=name, reason=message)
        self._outcome(name, state, after, status, message, [contracts.finding(code, message)], "supervisor")

    def _reap(self, name, state):
        """Let the command's ssh end with it, then make sure it has."""
        proc = self.procs.get(name)
        until = time.monotonic() + KILL_WAIT_S
        pid = state["ssh_pid"]
        while (proc.poll() is None if proc else pid and self.alive(pid)) and time.monotonic() < until:
            time.sleep(0.1)
        if proc and proc.poll() is None:
            proc.kill()
            proc.wait()
        elif not proc and pid and self.alive(pid):
            os.kill(pid, signal.SIGKILL)

    # The worker

    def _contain(self, reason):
        """Guest control failed: keep its console and the evidence already on
        the host, then stop the VM. The worker stays registered."""
        self.w._window("cleanup")
        record = self._record()
        vm, notes = record["worker"], []
        try:
            self.w.prl.capture(vm, worker.console_path(self.w.state, self.run_id))
        except parallels.ParallelsError as err:
            notes.append(f"console capture failed: {err}")
        _, _, missing = self.w.collect_run(record, reachable=False)
        try:
            self.w.prl.stop(vm, kill=True)
            record["status"] = "stopped"
            self.w._save(record)
        except parallels.ParallelsError as err:
            notes.append(f"stopping {vm} failed: {err}")
        for proc in self.procs.values():
            proc.kill()
        self.procs.clear()
        self.g = None
        for name in self.w.unfinished(self.run_id):
            command = worker._load(self._edir(name) / "command.json")
            worker.finalize(self._edir(name), contracts.result(
                "exec", "timeout", "the guest stopped answering; the worker was stopped", [
                    contracts.finding("guest_unresponsive", reason),
                    *(contracts.finding("cleanup_incomplete", n) for n in notes)],
                {**command, "finished_at": worker._stamp(worker._now()), "exit_code": None,
                 "enforced_by": "supervisor"}))
        self.w.event(self.run_id, "guest_unresponsive", reason=reason, notes=notes, missing=missing)

    def _expire(self):
        """The run deadline passed: end its commands, collect within the
        cleanup window, and halt the VM."""
        with worker.locked(self.w._lock(self.run_id), wait=False) as got:
            if not got:
                return  # an operation on the worker is running; next tick
            self.w.event(self.run_id, "run_deadline", deadline=self._record()["deadline"])
            for name in self.w.unfinished(self.run_id):
                self._enforce(name, "timeout", "run_deadline", "the run's deadline passed")
            record = self._record()
            missing = []
            if record["status"] == "ready":
                self.w._window("cleanup")
                _, _, missing = self.w.collect_run(record)
            if record["status"] in ("ready", "provisioning"):
                try:
                    self.w.halt(record)
                except parallels.ParallelsError as err:
                    missing.append(f"halting the worker failed: {err}")
                record["status"] = "expired"
                self.w._save(record)
            self.w.event(self.run_id, "expired", missing=missing)

    def _refreeze(self):
        with worker.locked(self.w._lock(self.run_id), wait=False) as got:
            if got:
                self.w._window("cleanup")
                self.w.halt(self._record())
                self.w.event(self.run_id, "halted", reason="an operation that started the halted worker ended")

    def _interrupted(self):
        """A creation or reset that died before the source was in place."""
        with worker.locked(self.w._lock(self.run_id), wait=False) as got:
            if not got:
                return
            record = self._record()
            if record["status"] != "provisioning":
                return
            # Its source never arrived, so nothing in it is worth a clean shutdown.
            if self.w._running(record["worker"]):
                self.w.prl.stop(record["worker"], kill=True)
            record["status"] = "failed"
            self.w._save(record)
            self.w.event(self.run_id, "interrupted", reason="a creation or reset ended before its source was in place")


def argv(repo, config_path, run_id, root=None):
    return [sys.executable, str(Path(repo) / "scripts" / "vm" / "vmctl.py"), "--root", str(root or repo),
            "supervise", run_id, "--config", str(config_path)]
