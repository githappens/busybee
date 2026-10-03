from pathlib import Path
import base64
import contextlib
import hashlib
import io
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import guest
import lease
import parallels
import registry
import supervisor
import template
import worker

REPO = Path(__file__).resolve().parents[3]
BASELINE_ID = "{11111111-2222-3333-4444-555555555555}"
BASELINE_SNAPSHOT = "{66666666-7777-8888-9999-000000000000}"
CANDIDATE = "r-20261001T000000Z-abcdef"
BASELINE_VM = f"busybee-lab-tpl-linux-{CANDIDATE}"
CONFIG = {"state_dir": "build/vm", "clone_strategy": "linked",
          "deadlines": {"command": 60, "scenario": 120, "run": 600, "cleanup": 30},
          "budget": {"cpus": 4, "memory_mib": 8192, "storage_gib": 64},
          "worker": {"cpus": 4, "memory_mib": 8192, "storage_gib": 32, "artifact_mib": 64}}


class Prlctl:
    """Answers prlctl argv the way the real tool does, for the VMs it knows."""

    def __init__(self):
        self.vms = {BASELINE_VM: {"id": BASELINE_ID, "state": "stopped", "snapshots": [BASELINE_SNAPSHOT]}}
        self.calls = []
        self.devices = ["cdrom0", "hdd0", "net0"]
        self.disk_mib = 32768

    def __call__(self, argv, timeout=None, stdin=None):
        command, name, rest = argv[1], argv[2], argv[3:]
        if argv[1:] == ["list", "--all", "--json"]:
            return json.dumps([{"uuid": vm["id"].strip("{}"), "status": vm["state"], "name": n}
                               for n, vm in self.vms.items()])
        self.calls.append([command, name, *rest])
        vm = self.vms.get(name)
        if command == "list" and vm is None:
            raise parallels.ParallelsError(f"prlctl list exited 255: {name} could not be found")
        if command == "clone":
            self.vms[rest[rest.index("--name") + 1]] = {"id": "{" + str(uuid.uuid4()) + "}", "state": "stopped",
                                                         "snapshots": []}
        elif command == "list":
            return json.dumps([{"ID": vm["id"].strip("{}"), "State": vm["state"],
                                "Hardware": {d: {"mac": "001C42000001", "size": f"{self.disk_mib}Mb"}
                                             for d in self.devices}}])
        elif command == "start":
            vm["state"] = "running"
        elif command == "stop":
            vm["state"] = "stopped"
        elif command == "delete":
            if vm is None:
                raise parallels.ParallelsError(f"prlctl delete exited 255: {name} could not be found")
            del self.vms[name]
        elif command == "snapshot":
            snap = "{" + str(uuid.uuid4()) + "}"
            vm["snapshots"].append(snap)
            return f"Creating the snapshot...\nThe snapshot with id {snap} has been successfully created.\n"
        elif command == "snapshot-switch":
            vm["switched_to"] = rest[rest.index("--id") + 1]
        elif command == "capture":
            Path(rest[rest.index("--file") + 1]).write_bytes(b"\x89PNG" + bytes(2048))
        return ""

    def mutations(self, name):
        return [c for c in self.calls if c[1] == name and c[0] != "list"]


class FakeProc:
    """The ssh process of one exec: `chunks` reach its stdout one poll at a time."""
    pids = iter(range(50000, 60000))

    def __init__(self, fake, stdout, stderr, chunks, exit_code):
        self.fake, self.out, self.err = fake, stdout, stderr
        self.chunks, self.exit_code = list(chunks), exit_code
        self.pid = next(self.pids)
        self.returncode = None
        fake.live.add(self.pid)

    def poll(self):
        if self.returncode is None and self.chunks:
            with open(self.out, "ab") as f:
                f.write(self.chunks.pop(0))
        if self.returncode is None and not self.chunks and not self.fake.hang:
            self.end(self.exit_code)
        return self.returncode

    def end(self, code):
        self.returncode = code
        self.fake.status = code
        self.fake.live.discard(self.pid)

    def kill(self):
        # ssh dies; what it ran in the guest is not told.
        if self.returncode is None:
            self.returncode = -9
            self.fake.live.discard(self.pid)

    def wait(self):
        return self.returncode


class FakeGuest:
    """A guest that answers the controller's commands from a script."""

    def __init__(self, exit_code=0, stdout=b"", stderr=b""):
        self.commands = []
        self.exit_code, self.stdout, self.stderr = exit_code, stdout, stderr
        self.head = None
        self.fail_on = None
        self.hang = False  # a command that never ends by itself
        self.chunks = None  # stdout delivered over several polls
        self.status = None  # what the guest's status file holds
        self.live = set()
        self.procs = []
        self.elapse = lambda: None

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        self.commands.append((command, stdin))
        if self.fail_on is not None and self.fail_on in command:
            # As Guest.run does: a failure raises only when checked, else ssh's 255 comes back.
            if check:
                raise guest.GuestError(f"`{self.fail_on}` failed")
            return 255, b"" if raw else "", "ssh: connect to host port 22: Operation timed out"
        if "rev-parse HEAD" in command and "printf" not in command:
            out = self.head.encode()
        elif command.startswith("printf \"status:"):
            status = "" if self.status is None else self.status
            self.status = None
            out = (f"status: {status}\nhead: {self.head}\ndirty: 0\n"
                   f"binary: {'a' * 64}  build/debug/busybee\n").encode()
        elif "git status --porcelain" in command:
            out = b" M crates/bzb/src/main.rs\n"
        elif "diff --cached --binary" in command:
            out = b"diff --git a/x b/x\n"
        elif command.startswith("ps "):
            out = b"    1     0 root     Ss       42 /run/current-system/systemd/lib/systemd/systemd\n"
        elif command.startswith("p=$(cat") and "kill -s KILL" in command:
            for proc in self.procs:
                if proc.returncode is None:
                    proc.end(137)
            out = b""
        else:
            out = b""
        if "checkout -q --detach" in command:
            self.head = command.split("--detach ")[1].split()[0]
        return 0, out if raw else out.decode(), ""

    def spawn(self, command, stdout, stderr):
        self.commands.append((command, None))
        stderr.write(self.stderr)
        chunks = self.chunks if self.chunks is not None else [self.stdout]
        self.elapse()
        proc = FakeProc(self, stdout.name, stderr.name, chunks, self.exit_code)
        self.procs.append(proc)
        return proc


class Lab:
    """A temporary repository with one tagged commit, a promoted baseline and
    a registry that owns it."""

    def __init__(self, test):
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        git = ["git", "-C", str(self.repo), "-c", "user.name=t", "-c", "user.email=t@example.com"]
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        (self.repo / "README").write_text("x\n")
        subprocess.run([*git, "add", "README"], check=True)
        subprocess.run([*git, "commit", "-qm", "one"], check=True)
        subprocess.run([*git, "tag", "-a", "0.1.0", "-m", "0.1.0"], check=True)
        facts = self.repo / template.INFRA / "linux" / "facts.sh"
        facts.parent.mkdir(parents=True)
        shutil.copy(REPO / template.INFRA / "linux" / "facts.sh", facts)
        self.revision = subprocess.run([*git, "rev-parse", "HEAD"], capture_output=True, text=True,
                                       check=True).stdout.strip()
        self.state = contracts.state_dir(CONFIG, self.repo)
        self.reg = registry.Registry(self.state)
        self.reg.claim(BASELINE_VM, "candidate", "linux", CANDIDATE, "2026-10-01T00:00:00Z")
        self.reg.bind(BASELINE_VM, BASELINE_ID)
        template._write_json(template.manifest_path(self.state, "linux"), {
            "schema": contracts.TEMPLATE_SCHEMA, "name": "linux", "candidate": CANDIDATE, "os": "linux",
            "arch": "aarch64", "vm_id": BASELINE_ID, "snapshot_id": BASELINE_SNAPSHOT,
            "provisioning_revision": "0" * 40, "lock_hashes": {}, "tools": {}, "parallels_version": "27.0.1",
            "clone_modes": ["linked"], "validated_at": "2026-10-01T00:00:00Z"})
        self.prlctl = Prlctl()
        self.prl = parallels.Parallels("prlctl", "prlsrvctl", self.prlctl, owned=self.reg)
        self.guest = FakeGuest()
        self.free_gib = 500
        self.now = time.time()  # the controller's clock, which tests move
        self.supervisors = {}
        self.supervising = True  # off: every supervisor process is dead
        self.config = CONFIG
        self.wait_s = 0  # how long a create waits in line for a Linux worker
        self.slept = []  # called with the fake clock's time on every sleep

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds
        for hook in self.slept:
            hook()

    def supervise(self, run_id):
        """In place of a detached supervisor process: one tick of this run's."""
        if self.supervising:
            self.supervisor(run_id).tick()

    def supervisor(self, run_id):
        if run_id not in self.supervisors:
            self.supervisors[run_id] = supervisor.Supervisor(self.workers(), run_id,
                                                             alive=lambda pid: pid in self.guest.live)
        return self.supervisors[run_id]

    def restart_supervisors(self):
        """The supervisor processes die; their ssh children live on."""
        self.supervisors = {}

    def workers(self):
        return worker.Workers(self.repo, self.config, self.prl, self.reg, lambda path: self.free_gib,
                              connect=lambda record, info, deadline: self.guest, supervise=self.supervise,
                              clock=self.clock, sleep=self.sleep, slot_wait_s=self.wait_s)

    def create(self):
        result = self.workers().create("linux", self.revision)
        assert result["status"] == "success", result
        return result["data"]["run_id"]


def codes(result):
    return {f["code"] for f in result["findings"]}


def linux_workers(n):
    """CONFIG with at most `n` active Linux workers."""
    return {**CONFIG, "concurrency": {"linux_workers": n}}


class CreateTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)

    def test_a_worker_is_claimed_and_recorded_before_guest_work(self):
        run_id = self.lab.create()
        vm = contracts.worker_name(run_id)
        entry = self.lab.reg.get(vm)
        self.assertEqual((entry["role"], entry["parent"]), ("worker", BASELINE_ID))
        record = json.loads((worker.run_dir(self.lab.state, run_id) / "worker.json").read_text())
        self.assertEqual(contracts.worker_errors(record), [])
        self.assertEqual(record["vm_id"], entry["vm_id"])
        self.assertEqual(record["snapshot_id"], BASELINE_SNAPSHOT)
        self.assertIn(record["reset_snapshot_id"], self.lab.prlctl.vms[vm]["snapshots"])
        clone = next(c for c in self.lab.prlctl.calls if c[0] == "clone")
        self.assertEqual(clone[1], BASELINE_VM)
        self.assertIn("--linked", clone)
        self.assertEqual(clone[clone.index("--id") + 1], BASELINE_SNAPSHOT)
        # Resources are allocated, and the clone is snapshotted, before it ever starts.
        order = [c[0] for c in self.lab.prlctl.mutations(vm)]
        self.assertLess(order.index("snapshot"), order.index("start"))
        allocate = next(c for c in self.lab.prlctl.mutations(vm) if "--cpus" in c)
        self.assertEqual((allocate[allocate.index("--cpus") + 1], allocate[allocate.index("--memsize") + 1]),
                         ("4", "8192"))

    def test_two_linux_workers_are_active_by_default(self):
        first, second = self.lab.create(), self.lab.create()
        self.assertNotEqual(first, second)
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("worker_limit", codes(result))
        message = result["findings"][0]["message"]
        self.assertIn("2 Linux worker(s)", message)
        self.assertIn(first, message)
        self.assertIn(second, message)
        # Each worker has its own allocation, which fits the per-worker budget.
        clones = [c for c in self.lab.prlctl.calls if c[0] == "set" and "--cpus" in c]
        self.assertEqual(len(clones), 2)

    def test_the_linux_cap_is_configured(self):
        self.lab.config = linux_workers(1)
        first = self.lab.create()
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("worker_limit", codes(result))
        self.assertIn(first, result["findings"][0]["message"])
        self.lab.config = linux_workers(3)
        self.lab.create()
        self.lab.create()

    def test_a_full_lab_makes_a_creation_wait_for_a_free_worker(self):
        first = self.lab.create()
        self.lab.create()
        self.lab.wait_s = 600
        waited = []

        def free_one():
            waited.append(self.lab.now)
            if len(waited) == 3:
                self.assertEqual(self.lab.workers().destroy(first)["status"], "success")
        self.lab.slept.append(free_one)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "success", result)
        self.assertEqual(len(waited), 3)
        self.assertIn("queued for a Linux worker", err.getvalue())
        self.assertEqual(len([e for e in self.lab.reg.entries().values() if e["role"] == "worker"]), 2)

    def test_a_wait_for_a_linux_worker_is_bounded(self):
        self.lab.create()
        self.lab.create()
        self.lab.wait_s = 30
        started = self.lab.now
        with contextlib.redirect_stderr(io.StringIO()):
            result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("worker_limit", codes(result))
        self.assertIn("within 30s", result["findings"][0]["message"])
        self.assertGreaterEqual(self.lab.now - started, 30)
        self.assertEqual(len([e for e in self.lab.reg.entries().values() if e["role"] == "worker"]), 2)

    def test_linux_waiters_are_served_in_arrival_order(self):
        # An earlier waiter (another live process) is ahead in line: a free
        # worker is not taken out from under it.
        line = lease.Queue(self.lab.state / "queues" / "linux")
        ticket = line.join("r-earlier")
        self.lab.wait_s = 10
        with contextlib.redirect_stderr(io.StringIO()) as err:
            result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("worker_limit", codes(result))
        self.assertIn("1 waiter(s) ahead", result["findings"][0]["message"])
        self.assertIn("1 ahead", err.getvalue())
        self.assertEqual([c for c in self.lab.prlctl.calls if c[0] == "clone"], [])
        line.leave(ticket)
        self.lab.create()

    def test_storage_is_refused_without_waiting(self):
        self.lab.wait_s = 600
        self.lab.free_gib = 10
        started = self.lab.now
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("storage_exhausted", codes(result))
        self.assertEqual(self.lab.now, started)

    def test_allocation_must_fit_the_host(self):
        self.lab.free_gib = 10
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("storage_exhausted", codes(result))
        self.assertEqual([c for c in self.lab.prlctl.calls if c[0] != "list"], [])

    def test_a_baseline_disk_larger_than_the_allocation_is_refused(self):
        self.lab.prlctl.disk_mib = 65536
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("storage_exhausted", codes(result))
        self.assertEqual([c for c in self.lab.prlctl.calls if c[0] != "list"], [])

    def test_a_clone_with_host_devices_is_disposed(self):
        self.lab.prlctl.devices.append("usb")
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("usb", result["findings"][0]["message"])
        self.assertEqual(list(self.lab.prlctl.vms), [BASELINE_VM])
        self.assertEqual([n for n, e in self.lab.reg.entries().items() if e["role"] == "worker"], [])

    def test_an_interrupted_creation_stays_owned_and_can_be_destroyed(self):
        # The controller dies after claiming, before the clone is recorded.
        run_id = contracts.new_run_id()
        vm = contracts.worker_name(run_id)
        self.lab.reg.claim(vm, "worker", "linux", run_id, "2026-10-01T00:00:00Z", parent=BASELINE_ID)
        self.lab.prlctl.vms[vm] = {"id": "{" + str(uuid.uuid4()) + "}", "state": "stopped", "snapshots": []}
        self.lab.config = linux_workers(1)
        blocked = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("worker_limit", codes(blocked))
        result = self.lab.workers().destroy(run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertNotIn(vm, self.lab.prlctl.vms)
        self.assertIsNone(self.lab.reg.get(vm))

    def test_a_creation_interrupted_during_transfer_can_be_destroyed(self):
        def interrupted(command, timeout, stdin=None, **kwargs):
            raise KeyboardInterrupt
        self.lab.guest.run = interrupted
        with self.assertRaises(KeyboardInterrupt):
            self.lab.workers().create("linux", self.lab.revision)
        run_id = next(e["run_id"] for e in self.lab.reg.entries().values() if e["role"] == "worker")
        record = json.loads((worker.run_dir(self.lab.state, run_id) / "worker.json").read_text())
        self.assertEqual(record["status"], "provisioning")
        # No source ever reached the guest, so there is nothing to collect.
        self.lab.guest = FakeGuest()
        result = self.lab.workers().destroy(run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertEqual(self.lab.guest.commands, [])
        self.assertEqual(list(self.lab.prlctl.vms), [BASELINE_VM])

    def test_an_unknown_revision_is_refused_before_anything_is_claimed(self):
        with self.assertRaises(worker.Refused) as caught:
            self.lab.workers().create("linux", "no-such-revision")
        self.assertEqual(caught.exception.code, "source_invalid")
        self.assertEqual(list(self.lab.reg.entries()), [BASELINE_VM])


class UntaggedSourceTests(unittest.TestCase):
    def test_a_source_without_tags_transfers_through_its_branch(self):
        lab = Lab(self)
        source = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, source, True)
        env = dict(os.environ, GIT_AUTHOR_NAME="F", GIT_AUTHOR_EMAIL="f@example.test", GIT_COMMITTER_NAME="F",
                   GIT_COMMITTER_EMAIL="f@example.test")
        for argv in (["init", "-q", "-b", "work"], ["commit", "-q", "--allow-empty", "-m", "c"]):
            subprocess.run(["git", *argv], cwd=source, env=env, check=True)
        sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
        lab.guest.head = sha
        w = lab.workers()
        w.source_repo, w.transfer_refs = source, ("refs/heads/work",)
        self.assertEqual(w.create("linux", sha)["status"], "success")
        bundles = [stdin for c, stdin in lab.guest.commands if "source.bundle" in c and stdin]
        self.assertTrue(bundles and bundles[0].startswith(b"# v2 git bundle"))


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)

    def test_worker_builds_recorded_revision(self):
        run_id = self.lab.create()
        transfer = next(stdin for command, stdin in self.lab.guest.commands if "source.bundle" in command)
        bundle = self.lab.repo / "check.bundle"
        bundle.write_bytes(transfer)
        heads = subprocess.run(["git", "-C", str(self.lab.repo), "bundle", "list-heads", str(bundle)],
                               capture_output=True, text=True, check=True).stdout
        # The tags travel with the revision, so `git describe` versions the build.
        self.assertIn("refs/tags/0.1.0", heads)
        record = json.loads((worker.run_dir(self.lab.state, run_id) / "worker.json").read_text())
        self.assertEqual(record["source"], {"revision": self.lab.revision, "patch_sha256": None})

        self.lab.guest.exit_code = 0
        result = self.lab.workers().exec(run_id, ["cargo", "build"], "/root/busybee", {}, 30)
        provenance = result["data"]["provenance"]
        self.assertEqual(provenance["head"], self.lab.revision)
        self.assertEqual(provenance["binaries"], {"build/debug/busybee": "a" * 64})

    def test_a_patch_is_applied_and_identified(self):
        patch = self.lab.repo / "fix.patch"
        patch.write_bytes(b"diff --git a/README b/README\n")
        result = self.lab.workers().create("linux", self.lab.revision, patch)
        self.assertEqual(result["status"], "success", result)
        self.assertEqual(len(result["data"]["source"]["patch_sha256"]), 64)
        applied = [stdin for command, stdin in self.lab.guest.commands if "git apply" in command]
        self.assertEqual(applied, [patch.read_bytes()])


class ExecTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()

    def test_worker_exec_round_trips_argv_and_exit(self):
        argv = ["printf", "%s\\n", "two words", "$HOME", "a;b|c&d", "*", "'quoted'", ""]
        command = worker.exec_command(argv, "/tmp/a dir", {"GREETING": "hi there; $USER"}, 30, "/var/tmp/s",
                                      "/var/tmp/p")
        # Split as a shell does: operators are their own tokens, quoted text is not.
        words = list(shlex.shlex(command, posix=True, punctuation_chars=True))
        self.assertEqual(words[:2], ["cd", "/tmp/a dir"])
        self.assertIn("GREETING=hi there; $USER", words)
        start = words.index("timeout")
        self.assertEqual(words[start:start + 5], ["timeout", "-k", str(worker.KILL_GRACE_S), "--", "30"])
        self.assertEqual(words[start + 5:start + 5 + len(argv)], argv)

        self.lab.guest.exit_code, self.lab.guest.stdout, self.lab.guest.stderr = 3, b"out\n", b"err\n"
        result = self.lab.workers().exec(self.run_id, argv, "/tmp", {"A": "1"}, 30)
        self.assertEqual(result["status"], "product_failure")
        data = result["data"]
        self.assertEqual(data["exit_code"], 3)
        self.assertEqual((self.lab.state / data["stdout"]).read_bytes(), b"out\n")
        self.assertEqual((self.lab.state / data["stderr"]).read_bytes(), b"err\n")
        for key in ("started_at", "finished_at", "elapsed_s"):
            self.assertIn(key, data)
        self.assertEqual(data["provenance"]["source"]["revision"], self.lab.revision)

    def test_a_timed_out_command_is_a_timeout(self):
        self.lab.guest.exit_code = 124

        def thirty_one_seconds():
            self.lab.now += 31
        self.lab.guest.elapse = thirty_one_seconds
        result = self.lab.workers().exec(self.run_id, ["sleep", "60"], "/", {}, 30)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("command_timeout", codes(result))

    def test_an_unresponsive_guest_is_stopped_and_kept(self):
        def hang(command, stdout, stderr):
            raise guest.GuestError("ssh exceeded its deadline")
        self.lab.guest.spawn = hang
        result = self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("guest_unresponsive", codes(result))
        vm = contracts.worker_name(self.run_id)
        self.assertEqual(self.lab.prlctl.vms[vm]["state"], "stopped")
        self.assertIsNotNone(self.lab.reg.get(vm))

    def test_a_missing_exit_status_is_an_environment_failure(self):
        self.lab.guest.exit_code = ""
        result = self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(result["status"], "environment_failure")

    def test_the_run_deadline_bounds_every_command(self):
        record_path = worker.run_dir(self.lab.state, self.run_id) / "worker.json"
        record = json.loads(record_path.read_text())
        record.update(created_at="2020-01-01T00:00:00Z", deadline="2020-01-01T01:00:00Z")
        record_path.write_text(json.dumps(record))
        result = self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("run_deadline_passed", codes(result))

    def test_environment_names_and_timeouts_are_checked(self):
        for env, timeout in (({"A B": "1"}, 5), ({}, 0), ({}, CONFIG["deadlines"]["scenario"] + 1)):
            with self.subTest(env=env, timeout=timeout), self.assertRaises(worker.Refused):
                self.lab.workers().exec(self.run_id, ["true"], "/", env, timeout)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()
        self.vm = contracts.worker_name(self.run_id)

    def test_reset_restores_named_baseline(self):
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        newer = self.lab.prl.snapshot(self.vm, "agent snapshot")
        result = self.lab.workers().reset(self.run_id)
        self.assertEqual(result["status"], "success", result)
        switched = self.lab.prlctl.vms[self.vm]["switched_to"]
        self.assertEqual(switched, record["reset_snapshot_id"])
        self.assertNotEqual(switched, newer)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "running")
        # Collected before restoring, and the source is back afterwards.
        self.assertTrue(result["data"]["collected"]["artifacts"])
        self.assertEqual(sum("source.bundle" in c for c, _ in self.lab.guest.commands), 2)

    def test_a_terminal_collected_before_a_reset_is_not_fetched_again(self):
        hdir = worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001"
        hdir.mkdir(parents=True)
        guest_dir = f"/var/tmp/busybee-terminal/{self.run_id}/0001"
        (hdir / "handle.json").write_text(json.dumps({"handle": "0001", "guest_dir": guest_dir}))
        w = self.lab.workers()
        w._fetch_terminals = lambda g, rdir: []  # the collection before the restore fetched it
        self.assertEqual(w.reset(self.run_id)["status"], "success")
        self.assertIsNotNone(json.loads((hdir / "handle.json").read_text())["final_at"])
        # The restored guest no longer has the terminal; collection keeps the host's copy.
        self.lab.guest.fail_on = guest_dir
        self.assertEqual(self.lab.workers()._fetch_terminals(self.lab.guest, hdir.parents[1]), [])

    def test_a_reset_whose_transfer_fails_is_explicit_and_destroyable(self):
        self.lab.guest.fail_on = "source.bundle"
        result = self.lab.workers().reset(self.run_id)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("reset_incomplete", codes(result))
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        self.assertEqual(record["status"], "provisioning")
        with self.assertRaises(worker.Refused):
            self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(self.lab.workers().destroy(self.run_id)["status"], "success")

    def test_lifecycle_rejects_unowned_targets(self):
        w = self.lab.workers()
        baseline_calls = self.lab.prlctl.mutations(BASELINE_VM)
        targets = ("../registry", "busybee-lab-" + self.run_id, CANDIDATE, contracts.new_run_id(), "", "*")
        for target in targets:
            for call in (lambda: w.reset(target), lambda: w.destroy(target), lambda: w.signal(target, "TERM", "42"),
                         lambda: w.exec(target, ["true"], "/", {}, 5), lambda: w.collect(target),
                         lambda: w.inspect(target), lambda: w.console_capture(target)):
                with self.subTest(target=target), self.assertRaises(worker.Refused):
                    call()
        for pid in ("1", "0", "-1", "1; reboot", "abc", ""):
            with self.subTest(pid=pid), self.assertRaises(worker.Refused):
                w.signal(self.run_id, "TERM", pid)
        with self.assertRaises(worker.Refused):
            w.signal(self.run_id, "TERM; reboot", "42")
        self.assertEqual(self.lab.prlctl.mutations(BASELINE_VM), baseline_calls)
        self.assertFalse(any("kill" in c for c, _ in self.lab.guest.commands))
        # The adapter itself refuses an unclaimed VM, whatever the caller.
        with self.assertRaises(parallels.ParallelsError):
            self.lab.prl.snapshot_switch("busybee-lab-" + contracts.new_run_id(), BASELINE_SNAPSHOT)

    def test_signal_reaches_a_process_in_the_worker(self):
        result = self.lab.workers().signal(self.run_id, "TERM", "4242")
        self.assertEqual(result["status"], "success", result)
        self.assertIn("kill -s TERM 4242", self.lab.guest.commands[-1][0])

    def test_inspect_reads_processes_and_vm_state(self):
        result = self.lab.workers().inspect(self.run_id)
        self.assertEqual(result["data"]["vm_state"], "running")
        self.assertEqual(result["data"]["processes"][0]["pid"], 1)

    def test_console_capture_needs_a_running_vm(self):
        result = self.lab.workers().console_capture(self.run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertTrue((self.lab.state / result["data"]["path"]).read_bytes().startswith(b"\x89PNG"))
        self.lab.prl.stop(self.vm, kill=True)
        stopped = self.lab.workers().console_capture(self.run_id)
        self.assertEqual(stopped["status"], "environment_failure")
        self.assertIn("console_unavailable", codes(stopped))

    def test_failed_export_retains_worker(self):
        self.lab.guest.fail_on = "diff --cached"
        result = self.lab.workers().destroy(self.run_id)
        self.assertEqual(result["status"], "incomplete_collection")
        self.assertIn("worktree.diff", " ".join(result["data"]["missing"]))
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        self.assertNotIn("delete", [c[0] for c in self.lab.prlctl.mutations(self.vm)])
        self.assertIsNotNone(self.lab.reg.get(self.vm))
        self.assertEqual(self.lab.workers().reset(self.run_id)["status"], "incomplete_collection")

        # Once collection works again, the same worker is collected and removed,
        # and a second full cycle works.
        self.lab.guest.fail_on = None
        result = self.lab.workers().destroy(self.run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertNotIn(self.vm, self.lab.prlctl.vms)
        self.assertIsNone(self.lab.reg.get(self.vm))
        second = self.lab.create()
        self.assertEqual(self.lab.workers().destroy(second)["status"], "success")
        self.assertEqual(list(self.lab.prlctl.vms), [BASELINE_VM])

    def test_collection_writes_a_durable_manifest(self):
        result = self.lab.workers().collect(self.run_id)
        self.assertEqual(result["status"], "success", result)
        manifest = json.loads((self.lab.state / result["data"]["manifest"]).read_text())
        self.assertEqual(manifest["missing"], [])
        names = {Path(p).name for p in manifest["artifacts"]}
        self.assertLessEqual({"source.json", "worktree.diff"}, names)
        for path, digest in manifest["artifacts"].items():
            self.assertEqual(hashlib.sha256((self.lab.state / path).read_bytes()).hexdigest(), digest)

    def test_a_failed_collection_names_no_host_path(self):
        collect = worker.run_dir(self.lab.state, self.run_id) / "collect"
        collect.mkdir(mode=0o555)
        self.addCleanup(collect.chmod, 0o755)
        result = self.lab.workers().destroy(self.run_id)
        self.assertEqual(result["status"], "incomplete_collection")
        self.assertNotIn(str(self.lab.repo), json.dumps(result))
        self.assertNotIn(str(self.lab.repo.resolve()), json.dumps(result))


class PruneTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        # Promote a second baseline over the first, which becomes retained.
        old = json.loads(template.manifest_path(self.lab.state, "linux").read_text())
        new_vm = "busybee-lab-tpl-linux-r-20261002T000000Z-abcdef"
        self.lab.reg.claim(new_vm, "candidate", "linux", "r-20261002T000000Z-abcdef", "2026-10-02T00:00:00Z")
        self.lab.reg.bind(new_vm, "{22222222-2222-3333-4444-555555555555}")
        self.retained = template.manifest_path(self.lab.state, "linux").parent / "retained.json"
        template._write_json(self.retained, [old])
        template._write_json(template.manifest_path(self.lab.state, "linux"),
                             {**old, "vm_id": "{22222222-2222-3333-4444-555555555555}"})

    def test_retained_baseline_pruned_only_when_unreferenced(self):
        run_id = contracts.new_run_id()
        vm = contracts.worker_name(run_id)
        self.lab.reg.claim(vm, "worker", "linux", run_id, "2026-10-01T00:00:00Z", parent=BASELINE_ID)
        kept = template.prune(self.lab.state, "linux", self.lab.prl, self.lab.reg)
        self.assertEqual(kept["status"], "success")
        self.assertIn("retained_in_use", codes(kept))
        self.assertIn(BASELINE_VM, self.lab.prlctl.vms)
        self.assertEqual(len(json.loads(self.retained.read_text())), 1)

        self.lab.reg.release(vm)
        pruned = template.prune(self.lab.state, "linux", self.lab.prl, self.lab.reg)
        self.assertEqual(pruned["status"], "success", pruned)
        self.assertNotIn(BASELINE_VM, self.lab.prlctl.vms)
        self.assertIsNone(self.lab.reg.get(BASELINE_VM))
        self.assertEqual(json.loads(self.retained.read_text()), [])

    def test_the_current_baseline_is_never_pruned(self):
        current = json.loads(template.manifest_path(self.lab.state, "linux").read_text())
        template._write_json(self.retained, [current])
        result = template.prune(self.lab.state, "linux", self.lab.prl, self.lab.reg)
        self.assertIn("retained_is_current", codes(result))
        self.assertEqual([c for c in self.lab.prlctl.calls if c[0] == "delete"], [])

    def test_an_unowned_retained_baseline_is_reported_not_deleted(self):
        self.lab.reg.release(BASELINE_VM)
        result = template.prune(self.lab.state, "linux", self.lab.prl, self.lab.reg)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("retained_not_owned", codes(result))
        self.assertIn(BASELINE_VM, self.lab.prlctl.vms)


def outcome(lab, run_id, name):
    return json.loads((worker.run_dir(lab.state, run_id) / "exec" / name / "result.json").read_text())


class WatchdogTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()
        self.vm = contracts.worker_name(self.run_id)

    def test_watchdog_survives_agent_and_cli_exit(self):
        # The guest's own bound has failed (its timeout(1) is stopped), so only
        # the controller's deadline can end the command.
        self.lab.guest.hang = True

        def killed(seconds):
            raise KeyboardInterrupt  # the submitting process dies while it waits
        w = self.lab.workers()
        w.sleep = killed
        with self.assertRaises(KeyboardInterrupt):
            w.exec(self.run_id, ["sh", "-c", "kill -STOP $PPID; sleep 600"], "/", {}, 30)
        name = "0001"
        self.assertEqual(self.lab.workers().status(self.run_id, name)["data"]["state"], "running")

        # Nobody waits for it; the supervisor alone keeps the deadline.
        self.lab.now += 30 + worker.HOST_MARGIN_S - 1
        self.lab.supervise(self.run_id)
        self.assertFalse((worker.run_dir(self.lab.state, self.run_id) / "exec" / name / "result.json").exists())
        self.lab.now += 2
        self.lab.supervise(self.run_id)
        result = outcome(self.lab, self.run_id, name)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("watchdog_deadline", codes(result))
        self.assertEqual(result["data"]["enforced_by"], "supervisor")
        self.assertTrue(any(c.startswith("p=$(cat") and f"{self.run_id}-{name}.pid" in c
                            for c, _ in self.lab.guest.commands))
        # The guest answered the kill, so the worker stays usable.
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "running")
        self.lab.guest.hang = False
        self.assertEqual(self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)["status"], "success")

    def test_watchdog_handles_unreachable_guest(self):
        first = self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(first["status"], "success")
        # The guest stops answering mid-command: ssh ends, and so does every later command.
        self.lab.guest.hang = True
        handle = self.lab.workers().exec(self.run_id, ["sleep", "600"], "/", {}, 30, detach=True)["data"]
        self.lab.guest.fail_on = ""  # every guest command fails
        self.lab.guest.procs[-1].end(255)
        self.lab.supervise(self.run_id)
        result = outcome(self.lab, self.run_id, handle["exec"])
        self.assertEqual(result["status"], "timeout")
        self.assertIn("guest_unresponsive", codes(result))
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        self.assertIsNotNone(self.lab.reg.get(self.vm))
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        self.assertEqual(record["status"], "stopped")
        # Bounded diagnostics: the console, and the evidence the host already had, without waiting on the guest.
        rdir = worker.run_dir(self.lab.state, self.run_id)
        self.assertTrue(any(c[0] == "capture" for c in self.lab.prlctl.mutations(self.vm)))
        manifest = json.loads(next(rdir.glob("collect/*/collected.json")).read_text())
        self.assertIn("exec/0001/stdout", " ".join(manifest["artifacts"]))
        self.assertIn("checkpoint", " ".join(manifest["artifacts"]))
        self.assertEqual(manifest["missing"], ["source: the guest is not answering"])
        self.assertIn("guest_unresponsive", [e["event"] for e in self.lab.workers().events(self.run_id)])
        # The supervisor has nothing left to watch.
        self.assertFalse(self.lab.supervisor(self.run_id).tick())

    def test_a_hung_guest_that_cannot_be_killed_is_stopped(self):
        self.lab.guest.hang = True
        handle = self.lab.workers().exec(self.run_id, ["sleep", "600"], "/", {}, 30, detach=True)["data"]
        self.lab.guest.fail_on = ""
        self.lab.now += 30 + worker.HOST_MARGIN_S + 1
        self.lab.supervise(self.run_id)
        result = outcome(self.lab, self.run_id, handle["exec"])
        self.assertIn("guest_unresponsive", codes(result))
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")

    def test_the_run_deadline_is_enforced_by_the_supervisor(self):
        self.lab.guest.hang = True
        handle = self.lab.workers().exec(self.run_id, ["sleep", "600"], "/", {}, 60, detach=True)["data"]
        self.lab.now += CONFIG["deadlines"]["run"] + 1
        self.lab.supervise(self.run_id)
        result = outcome(self.lab, self.run_id, handle["exec"])
        self.assertEqual(result["status"], "timeout")
        self.assertIn("run_deadline", codes(result))
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        self.assertEqual(record["status"], "expired")
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        events = [e["event"] for e in self.lab.workers().events(self.run_id)]
        self.assertEqual(events[-1], "expired")
        self.assertFalse(self.lab.supervisor(self.run_id).tick())
        with self.assertRaises(worker.Refused):
            self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        # Collected at expiry: destroy acknowledges that, and need not start the VM again.
        starts = len([c for c in self.lab.prlctl.mutations(self.vm) if c[0] == "start"])
        result = self.lab.workers().destroy(self.run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertEqual(len([c for c in self.lab.prlctl.calls if c[1] == self.vm and c[0] == "start"]), starts)

    def test_an_exec_that_outgrows_its_artifact_budget_is_stopped(self):
        self.lab.guest.hang = True
        self.lab.guest.chunks = [b"x" * (CONFIG["worker"]["artifact_mib"] * 1024 * 1024 + 1)]
        handle = self.lab.workers().exec(self.run_id, ["yes"], "/", {}, 60, detach=True)["data"]
        self.lab.supervise(self.run_id)
        result = outcome(self.lab, self.run_id, handle["exec"])
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("artifact_budget_exceeded", codes(result))
        with self.assertRaises(worker.Refused) as caught:
            self.lab.workers().exec(self.run_id, ["true"], "/", {}, 5)
        self.assertEqual(caught.exception.code, "artifact_budget_exceeded")
        self.assertIn("artifact_budget_exceeded", codes(self.lab.workers().collect(self.run_id)))


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        # A lab VM this controller did not create: it must never be touched.
        self.sentinel = "busybee-lab-sentinel"
        self.lab.prlctl.vms[self.sentinel] = {"id": "{" + str(uuid.uuid4()) + "}", "state": "running",
                                              "snapshots": []}

    def untouched(self):
        self.assertEqual(self.lab.prlctl.mutations(self.sentinel), [])
        # The baseline is only ever cloned from.
        self.assertEqual({c[0] for c in self.lab.prlctl.mutations(BASELINE_VM)} - {"clone"}, set())
        self.assertEqual(self.lab.prlctl.vms[self.sentinel]["state"], "running")

    def test_restart_reconciles_owned_workers(self):
        w = self.lab.workers
        # 1. Killed between the claim and the record.
        run_id = contracts.new_run_id()
        vm = contracts.worker_name(run_id)
        (worker.run_dir(self.lab.state, run_id)).mkdir(parents=True)
        self.lab.reg.claim(vm, "worker", "linux", run_id, "2026-10-01T00:00:00Z", parent=BASELINE_ID)
        status = w().status()
        self.assertEqual([(x["run_id"], x["status"], x["vm_state"]) for x in status["data"]["workers"]],
                         [(run_id, "claimed", "missing")])
        self.assertIn("interrupted_create", codes(status))
        self.lab.config = linux_workers(1)
        blocked = w().create("linux", self.lab.revision)
        self.assertIn("worker_limit", codes(blocked))
        self.assertIn("interrupted_create", codes(blocked))
        self.assertEqual(w().destroy(run_id)["status"], "success")

        # 2. Killed during the transfer: the supervisor halts the half-made worker.
        def interrupted(command, timeout, stdin=None, **kwargs):
            raise KeyboardInterrupt
        run = self.lab.guest.run
        self.lab.guest.run = interrupted
        with self.assertRaises(KeyboardInterrupt):
            w().create("linux", self.lab.revision)
        self.lab.guest.run = run
        run_id = next(e["run_id"] for e in self.lab.reg.entries().values() if e["role"] == "worker")
        self.lab.restart_supervisors()
        status = w().status()
        self.assertEqual(status["data"]["workers"][0]["status"], "failed")
        self.assertEqual(status["data"]["workers"][0]["vm_state"], "stopped")
        self.assertEqual(status["data"]["resources"]["allocated"]["cpus"], CONFIG["worker"]["cpus"])
        self.assertEqual(w().destroy(run_id)["status"], "success")

        # 3. The supervisor dies mid-command; its successor adopts the command.
        run_id = self.lab.create()
        self.lab.guest.hang = True
        handle = w().exec(run_id, ["make"], "/", {}, 60, detach=True)["data"]
        self.lab.restart_supervisors()
        self.lab.supervise(run_id)
        self.assertEqual(w().status(run_id, handle["exec"])["data"]["state"], "running")
        self.lab.guest.procs[-1].end(0)  # the adopted ssh ends, and the guest recorded the status
        self.lab.restart_supervisors()
        result = w().wait(run_id, handle["exec"])
        self.assertEqual((result["status"], result["data"]["exit_code"]), ("success", 0))
        self.lab.guest.hang = False

        # 4. Killed while destroy was collecting: still owned and accounted, and a retry finishes.
        self.lab.guest.fail_on = "diff --cached"
        real = self.lab.guest.run

        def dies(command, timeout, stdin=None, **kwargs):
            if "diff --cached" in command:
                raise KeyboardInterrupt
            return real(command, timeout, stdin, **kwargs)
        self.lab.guest.run = dies
        with self.assertRaises(KeyboardInterrupt):
            w().destroy(run_id)
        self.lab.guest.run, self.lab.guest.fail_on = real, None
        status = w().status()
        self.assertEqual([(x["run_id"], x["status"]) for x in status["data"]["workers"]], [(run_id, "ready")])
        self.assertEqual(w().destroy(run_id)["status"], "success")
        # And a destroy repeated after it finished is not an error.
        self.assertEqual(w().destroy(run_id)["status"], "success")

        self.assertEqual(w().status()["data"]["workers"], [])
        self.assertEqual(sorted(self.lab.reg.entries()), [BASELINE_VM])
        self.assertEqual(sorted(self.lab.prlctl.vms), sorted([BASELINE_VM, self.sentinel]))
        self.untouched()

    def test_a_destroy_interrupted_after_the_delete_is_finished_by_its_retry(self):
        run_id = self.lab.create()
        vm = contracts.worker_name(run_id)
        del self.lab.prlctl.vms[vm]  # deleted, but the process died before releasing the claim
        result = self.lab.workers().destroy(run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertIsNone(self.lab.reg.get(vm))
        self.untouched()


class CollectionTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()
        self.vm = contracts.worker_name(self.run_id)
        self.rdir = worker.run_dir(self.lab.state, self.run_id)

    def test_collect_preserves_work_on_failure(self):
        self.lab.guest.stdout = b"built\n"
        self.assertEqual(self.lab.workers().exec(self.run_id, ["cargo", "build"], "/", {}, 30)["status"], "success")
        # Each finished command checkpoints the source outside the guest.
        self.assertEqual(sorted(p.name for p in (self.rdir / "checkpoint").iterdir()),
                         ["source.json", "worktree.diff"])

        self.lab.guest.fail_on = "diff --cached"
        failed = self.lab.workers().destroy(self.run_id)
        self.assertEqual(failed["status"], "incomplete_collection")
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        self.assertIsNotNone(self.lab.reg.get(self.vm))
        first = json.loads(next(self.rdir.glob("collect/*/collected.json")).read_text())
        self.assertEqual(contracts.evidence_errors(first), [])
        saved = set(first["artifacts"])
        self.assertTrue({f"runs/{self.run_id}/exec/0001/stdout", f"runs/{self.run_id}/checkpoint/worktree.diff"}
                        <= saved, saved)
        self.assertTrue(any("worktree.diff" in m for m in first["missing"]))

        # The retry completes that collection: what was durable is acknowledged, only the rest is fetched.
        self.lab.guest.fail_on = None
        self.lab.guest.commands.clear()
        result = self.lab.workers().collect(self.run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertEqual(len(list(self.rdir.glob("collect/*"))), 1)
        self.assertLessEqual(saved, set(result["data"]["acknowledged"]))
        self.assertEqual(sum("diff --cached" in c for c, _ in self.lab.guest.commands), 1)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        # Once complete, a further retry fetches nothing and changes nothing.
        self.lab.guest.commands.clear()
        again = self.lab.workers().collect(self.run_id)
        self.assertEqual(again["data"]["artifacts"], result["data"]["artifacts"])
        self.assertEqual(self.lab.guest.commands, [])
        self.assertEqual(self.lab.workers().destroy(self.run_id)["status"], "success")

    def test_a_tampered_artifact_is_fetched_again(self):
        self.lab.guest.fail_on = "diff --cached"
        self.lab.workers().destroy(self.run_id)
        source = next(self.rdir.glob("collect/*/source.json"))
        source.write_text("{}")
        self.lab.guest.fail_on = None
        result = self.lab.workers().collect(self.run_id)
        self.assertEqual(result["status"], "success", result)
        self.assertNotIn(self.lab._rel(source) if hasattr(self.lab, "_rel") else str(source.relative_to(self.lab.state)),
                         result["data"]["acknowledged"])
        self.assertEqual(json.loads(source.read_text())["base"], self.lab.revision)

    def test_the_evidence_manifest_is_versioned_and_complete(self):
        self.lab.workers().exec(self.run_id, ["cargo", "build"], "/", {"RUST_LOG": "debug"}, 30)
        result = self.lab.workers().collect(self.run_id)
        manifest = json.loads((self.lab.state / result["data"]["manifest"]).read_text())
        self.assertEqual(contracts.evidence_errors(manifest), [])
        self.assertEqual(manifest["schema"], contracts.EVIDENCE_SCHEMA)
        self.assertEqual(manifest["source"]["base"], self.lab.revision)
        self.assertEqual(manifest["template"]["candidate"], CANDIDATE)
        command = manifest["commands"][0]
        self.assertEqual((command["argv"], command["exit_code"], command["status"]), (["cargo", "build"], 0, "success"))
        self.assertEqual(command["provenance"]["binaries"], {"build/debug/busybee": "a" * 64})
        self.assertEqual(manifest["observations"]["processes"][0]["pid"], 1)
        self.assertEqual(manifest["allocation"], CONFIG["worker"])
        self.assertTrue(contracts.evidence_errors({**manifest, "schema": "busybee.vm.evidence/v0"}))


class LogTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()

    def read_all(self, name, stream, limit, offset=0):
        """Read to the end of what is there the way an agent would: from each
        next_offset, one call at a time. Returns the bytes and whether that was eof."""
        data = b""
        while True:
            result = self.lab.workers().read(self.run_id, name, stream, offset, limit)
            self.assertEqual(result["status"], "success", result)
            chunk = base64.b64decode(result["data"]["content_b64"])
            self.assertEqual(result["data"]["offset"], offset)
            data += chunk
            offset = result["data"]["next_offset"]
            if result["data"]["eof"] or not chunk:
                return data, result["data"]["eof"]

    def test_incremental_logs_are_lossless(self):
        chunks = [b"line one\r\n", b"\x00\xff binary \xfe", b"", "unicode é\n".encode(), b"tail without newline"]
        self.lab.guest.chunks, self.lab.guest.exit_code = list(chunks), 3
        self.lab.guest.hang = True
        handle = self.lab.workers().exec(self.run_id, ["make"], "/", {}, 60, detach=True)["data"]
        name = handle["exec"]
        seen = b""
        for i in range(len(chunks)):
            if i:
                self.lab.guest.procs[-1].poll()  # the guest sends more, whoever watches
            self.lab.restart_supervisors()  # every read is a fresh client and a fresh supervisor
            self.lab.supervise(self.run_id)
            part, eof = self.read_all(name, "stdout", limit=3, offset=len(seen))
            seen += part
            self.assertFalse(eof)  # still running: the end of what is there is not the end
            status = self.lab.workers().status(self.run_id, name)["data"]
            self.assertEqual((status["state"], status["stdout_bytes"]), ("running", len(seen)))
            # Read again from where the agent stopped: nothing is repeated.
            again = self.lab.workers().read(self.run_id, name, "stdout", len(seen))["data"]
            self.assertEqual(again["content_b64"], "")
        self.lab.guest.procs[-1].end(3)
        self.lab.supervise(self.run_id)
        rest, eof = self.read_all(name, "stdout", limit=3, offset=len(seen))
        seen += rest
        self.assertTrue(eof)
        self.assertEqual(seen, b"".join(chunks))
        self.assertEqual(outcome(self.lab, self.run_id, name)["status"], "product_failure")
        # The final collection digests exactly those bytes.
        collected = self.lab.workers().collect(self.run_id)["data"]["artifacts"]
        self.assertEqual(collected[f"runs/{self.run_id}/exec/{name}/stdout"], hashlib.sha256(seen).hexdigest())
        with self.assertRaises(worker.Refused):
            self.lab.workers().read(self.run_id, name, "stdout", len(seen) + 1)
        for bad in ("stdin", "../worker.json"):
            with self.assertRaises(worker.Refused):
                self.lab.workers().read(self.run_id, name, bad, 0)
        for bad in ("../../worker", "1", "9999"):
            with self.assertRaises(worker.Refused):
                self.lab.workers().read(self.run_id, bad, "stdout", 0)

    def test_exec_handles_report_progress(self):
        self.lab.guest.hang = True
        self.lab.guest.chunks = [b"compiling\n"]
        handle = self.lab.workers().exec(self.run_id, ["cargo", "build"], "/", {}, 60, detach=True)
        self.assertEqual(handle["status"], "success")
        self.lab.now += 12
        status = self.lab.workers().status(self.run_id, handle["data"]["exec"])["data"]
        self.assertEqual((status["state"], status["stdout_bytes"]), ("running", len(b"compiling\n")))
        self.assertEqual(status["elapsed_s"], 12)
        self.assertIsNotNone(status["last_output_at"])
        # Quiet output alone is never treated as a hang.
        self.lab.now += 50
        self.lab.supervise(self.run_id)
        self.assertEqual(self.lab.workers().status(self.run_id, handle["data"]["exec"])["data"]["state"], "running")
        waited = self.lab.workers().wait(self.run_id, handle["data"]["exec"], timeout=3)
        self.assertEqual(waited["status"], "timeout")
        self.assertIn("still_running", codes(waited))
        run = self.lab.workers().status(self.run_id)["data"]
        self.assertEqual(run["execs"], {handle["data"]["exec"]: "running"})


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()

    def test_public_export_removes_private_values(self):
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        token = "ghp_" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"
        secret = "s3cr3t-session-value"
        home = str(Path.home())
        private = [secret, token, home, str(self.lab.repo), str(self.lab.state), "192.0.2.17", "00:1C:42:AB:CD:EF",
                   "001C42ABCDEF", record["vm_id"], record["vm_id"].strip("{}"), record["reset_snapshot_id"],
                   record["worker"], self.run_id, "-----BEGIN OPENSSH PRIVATE KEY-----"]
        self.lab.guest.stdout = (
            f"token {token} for {secret}\nhome {home}/.config\nrepo {self.lab.repo}/build\nguest 192.0.2.17 "
            f"mac 00:1C:42:AB:CD:EF / 001C42ABCDEF\nvm {record['vm_id']} {record['worker']}\n"
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n"
            "test result: FAILED. 3 passed; 1 failed\n").encode()
        self.lab.guest.exit_code = 101
        self.lab.workers().exec(self.run_id, ["cargo", "test", f"--token={secret}"], "/root/busybee",
                                {"API_TOKEN": secret, "RUST_LOG": "debug"}, 30)
        self.lab.workers().console_capture(self.run_id)
        self.lab.workers().collect(self.run_id)
        result = self.lab.workers().export(self.run_id)
        self.assertEqual(result["status"], "success", result)
        out = self.lab.state / result["data"]["path"]
        blob = b"".join(p.read_bytes() for p in sorted(out.rglob("*")) if p.is_file())
        for value in private:
            with self.subTest(value=value):
                self.assertNotIn(value.encode(), blob)
        # The diagnosis survives: what ran, how it ended, what it printed.
        command = json.loads((out / "exec/0001/command.json").read_text())
        self.assertEqual(command["env"], {"API_TOKEN": "<redacted:API_TOKEN>", "RUST_LOG": "debug"})
        self.assertEqual(command["argv"][:2], ["cargo", "test"])
        self.assertEqual(json.loads((out / "exec/0001/result.json").read_text())["data"]["exit_code"], 101)
        stdout = (out / "exec/0001/stdout").read_bytes()
        self.assertIn(b"test result: FAILED. 3 passed; 1 failed", stdout)
        for label in (b"<token>", b"<redacted:API_TOKEN>", b"<home>", b"<ip>", b"<mac>", b"<vm-id>", b"<vm-name>",
                      b"<private-key>"):
            self.assertIn(label, stdout)
        manifest = json.loads((out / "manifest.json").read_text())
        self.assertTrue(manifest["withheld"])
        self.assertTrue(all(name.endswith((".png", ".bundle")) for name in manifest["withheld"]))
        self.assertNotIn("worker.json", manifest["files"])
        self.assertIn("collect/0001/collected.json", manifest["files"])
        # The raw evidence is untouched.
        raw = worker.run_dir(self.lab.state, self.run_id) / "exec/0001/stdout"
        self.assertIn(secret.encode(), raw.read_bytes())


class FrozenSupervisionTests(unittest.TestCase):
    """A halted worker started again by the controller stays bounded if that controller dies."""

    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.create()
        self.vm = contracts.worker_name(self.run_id)
        self.rdir = worker.run_dir(self.lab.state, self.run_id)
        # Collection fails: the worker is retained, stopped, and its supervisor has nothing to watch.
        self.lab.guest.fail_on = "diff --cached"
        self.assertEqual(self.lab.workers().destroy(self.run_id)["status"], "incomplete_collection")
        self.assertFalse(self.lab.supervisor(self.run_id).tick())
        self.lab.restart_supervisors()

    def dies_during(self, needle):
        real = self.lab.guest.run

        def dies(command, timeout, stdin=None, **kwargs):
            if needle in command:
                raise KeyboardInterrupt
            return real(command, timeout, stdin, **kwargs)
        self.lab.guest.run = dies
        return real

    def test_a_reset_of_a_halted_worker_is_supervised(self):
        self.lab.guest.fail_on = None
        self.dies_during("source.bundle")
        with self.assertRaises(KeyboardInterrupt):
            self.lab.workers().reset(self.run_id)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "running")
        # The supervisor the reset started halts the half-reset worker on its own.
        self.assertIn(self.run_id, self.lab.supervisors)
        self.lab.supervise(self.run_id)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        record = json.loads((self.rdir / "worker.json").read_text())
        self.assertEqual(record["status"], "failed")

    def test_a_collection_that_boots_a_halted_worker_is_supervised(self):
        self.lab.guest.fail_on = None
        self.dies_during("diff --cached")
        with self.assertRaises(KeyboardInterrupt):
            self.lab.workers().collect(self.run_id)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "running")
        self.assertIn(self.run_id, self.lab.supervisors)
        self.lab.supervise(self.run_id)
        self.assertEqual(self.lab.prlctl.vms[self.vm]["state"], "stopped")
        self.assertEqual(json.loads((self.rdir / "worker.json").read_text())["status"], "retained")

    def test_reconciliation_halts_a_running_halted_worker(self):
        # Booted by a controller that died before it could start a supervisor.
        self.lab.prl.start(self.vm)
        status = self.lab.workers().status()
        self.assertEqual(status["data"]["workers"][0]["vm_state"], "stopped")


class EnsureTests(unittest.TestCase):
    """The detached supervisor process, started for real (no Parallels involved)."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.state = Path(tmp.name)
        self.run_id = contracts.new_run_id()
        worker.run_dir(self.state, self.run_id).mkdir(parents=True)
        self.lock = worker.run_dir(self.state, self.run_id) / "supervisor.lock"

    def child(self, body):
        return [sys.executable, "-c", f"import fcntl, os, sys, time\n{body}"]

    def test_a_supervisor_outlives_its_starter_in_its_own_session(self):
        hold = f"f = open({str(self.lock)!r}, 'a'); fcntl.flock(f, fcntl.LOCK_EX)\n" \
               "print(os.getsid(0), flush=True); time.sleep(30)"
        supervisor.ensure(self.state, self.run_id, self.child(hold))
        self.assertTrue(worker.held(self.lock))
        log = worker.run_dir(self.state, self.run_id) / "supervisor.log"
        for _ in range(100):
            if log.read_text().strip():
                break
            time.sleep(0.05)
        self.assertNotEqual(int(log.read_text().split()[0]), os.getsid(0))
        # A second call finds it alive and starts nothing.
        supervisor.ensure(self.state, self.run_id, self.child("sys.exit(7)"))
        subprocess.run(["pkill", "-f", str(self.lock)])

    def test_a_supervisor_with_nothing_to_watch_is_not_a_failure(self):
        supervisor.ensure(self.state, self.run_id, self.child("sys.exit(0)"))

    def test_a_supervisor_that_cannot_start_is_loud(self):
        with self.assertRaises(worker.Refused) as caught:
            supervisor.ensure(self.state, self.run_id, self.child("sys.exit(1)"))
        self.assertEqual(caught.exception.code, "supervisor_unavailable")


if __name__ == "__main__":
    unittest.main()
