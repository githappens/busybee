from pathlib import Path
import hashlib
import json
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import guest
import parallels
import registry
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
          "worker": {"cpus": 4, "memory_mib": 8192, "storage_gib": 32}}


class Prlctl:
    """Answers prlctl argv the way the real tool does, for the VMs it knows."""

    def __init__(self):
        self.vms = {BASELINE_VM: {"id": BASELINE_ID, "state": "stopped", "snapshots": [BASELINE_SNAPSHOT]}}
        self.calls = []
        self.devices = ["cdrom0", "hdd0", "net0"]
        self.disk_mib = 32768

    def __call__(self, argv, timeout=None, stdin=None):
        command, name, rest = argv[1], argv[2], argv[3:]
        self.calls.append([command, name, *rest])
        vm = self.vms.get(name)
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


class FakeGuest:
    """A guest that answers the controller's commands from a script."""

    def __init__(self, exit_code=0, stdout=b"", stderr=b""):
        self.commands = []
        self.exit_code, self.stdout, self.stderr = exit_code, stdout, stderr
        self.head = None
        self.fail_on = None

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        self.commands.append((command, stdin))
        if self.fail_on and self.fail_on in command:
            raise guest.GuestError(f"`{self.fail_on}` failed")
        if "rev-parse HEAD" in command and "printf" not in command:
            out = self.head.encode()
        elif command.startswith("printf \"status:"):
            out = (f"status: {self.exit_code}\nhead: {self.head}\ndirty: 0\n"
                   f"binary: {'a' * 64}  build/debug/busybee\n").encode()
        elif "git status --porcelain" in command:
            out = b" M crates/bzb/src/main.rs\n"
        elif "diff --cached --binary" in command:
            out = b"diff --git a/x b/x\n"
        elif command.startswith("ps "):
            out = b"    1     0 root     Ss       42 /run/current-system/systemd/lib/systemd/systemd\n"
        else:
            out = b""
        if "checkout -q --detach" in command:
            self.head = command.split("--detach ")[1].split()[0]
        return 0, out if raw else out.decode(), ""

    def stream(self, command, timeout, stdout, stderr):
        self.commands.append((command, None))
        stdout.write(self.stdout)
        stderr.write(self.stderr)
        return self.exit_code


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

    def workers(self):
        return worker.Workers(self.repo, CONFIG, self.prl, self.reg, lambda path: self.free_gib,
                              connect=lambda record, info, deadline: self.guest)

    def create(self):
        result = self.workers().create("linux", self.revision)
        assert result["status"] == "success", result
        return result["data"]["run_id"]


def codes(result):
    return {f["code"] for f in result["findings"]}


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

    def test_only_one_worker_is_active(self):
        first = self.lab.create()
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("worker_limit", codes(result))
        self.assertIn(first, result["findings"][0]["message"])

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
        command = worker.exec_command(argv, "/tmp/a dir", {"GREETING": "hi there; $USER"}, 30, "/var/tmp/s")
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
        clock = iter([0.0, 31.0])
        result = self.lab.workers().exec(self.run_id, ["sleep", "60"], "/", {}, 30, clock=lambda: next(clock))
        self.assertEqual(result["status"], "timeout")
        self.assertIn("command_timeout", codes(result))

    def test_an_unresponsive_guest_is_stopped_and_kept(self):
        def hang(command, timeout, stdout, stderr):
            raise guest.GuestError("ssh exceeded its deadline")
        self.lab.guest.stream = hang
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


if __name__ == "__main__":
    unittest.main()
