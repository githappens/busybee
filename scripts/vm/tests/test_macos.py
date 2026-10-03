"""macOS baselines and the leased macOS guest, against fake Parallels and guests.

See docs/design/agent-lab.md §Build and validate templates and §macOS workers.
"""
from pathlib import Path
import json
import os
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
import macos
import parallels
import registry
import scenario
import supervisor
import template
import terminal_ops
import worker
from test_worker import FakeGuest

REPO = Path(__file__).resolve().parents[3]
SOURCE = "operators-prepared-mac"
SOURCE_ID = "{99999999-2222-3333-4444-555555555555}"
CONFIG = {"state_dir": "build/vm", "clone_strategy": "linked",
          "deadlines": {"command": 60, "scenario": 120, "run": 600, "cleanup": 30},
          "budget": {"cpus": 4, "memory_mib": 8192, "storage_gib": 64},
          "worker": {"cpus": 4, "memory_mib": 8192, "storage_gib": 32, "artifact_mib": 64},
          "templates": {"macos": {"manifest": "templates/macos/manifest.json", "clone_strategy": "full",
                                  "source": SOURCE, "user": "lab", "bootstrap_key": "keys/bootstrap"}}}
LIMIT = ("10-03 09:39:16.944 F /macvm:1/ Failed to start: The number of virtual machines exceeds the limit. "
         "The maximum supported number of active virtual machines has been reached.\n")


class Prlctl:
    """prlctl for macOS VMs: full clones copy the source's current state, and
    `start` can be made to fail the way Parallels does at the macOS guest limit."""

    def __init__(self, homes):
        self.homes = homes
        self.vms = {SOURCE: {"id": SOURCE_ID, "state": "stopped", "snapshots": [], "devices": ["hdd0", "net0",
                                                                                                "sound0", "usb"]}}
        self.calls = []
        self.refuse_start = False

    def home(self, name):
        path = self.homes / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    def __call__(self, argv, timeout=None, stdin=None):
        if argv[1:] == ["list", "--all", "--json"]:
            return json.dumps([{"uuid": vm["id"].strip("{}"), "status": vm["state"], "name": n}
                               for n, vm in self.vms.items()])
        if argv[1:2] == ["--version"]:
            return "prlctl version 27.0.1 (58670)\n"
        command, name, rest = argv[1], argv[2], argv[3:]
        self.calls.append([command, name, *rest])
        vm = self.vms.get(name)
        if command == "clone":
            self.vms[rest[rest.index("--name") + 1]] = {"id": "{" + str(uuid.uuid4()) + "}", "state": "stopped",
                                                         "snapshots": [], "devices": list(vm["devices"])}
        elif command == "list":
            return json.dumps([{"ID": vm["id"].strip("{}"), "State": vm["state"], "Home": f"{self.home(name)}/",
                                "Hardware": {d: {"mac": "001C42000009", "size": "131072Mb"} for d in vm["devices"]}}])
        elif command == "set" and "--device-del" in rest:
            vm["devices"].remove(rest[rest.index("--device-del") + 1])
        elif command == "start":
            if self.refuse_start:
                with open(self.home(name) / "parallels.log", "a") as log:
                    log.write(LIMIT)
                raise parallels.ParallelsError("prlctl start exited 255: An unexpected error occurred.")
            vm["state"] = "running"
        elif command == "stop":
            vm["state"] = "stopped"
        elif command == "delete":
            del self.vms[name]
        elif command == "snapshot":
            snap = "{" + str(uuid.uuid4()) + "}"
            vm["snapshots"].append(snap)
            return f"The snapshot with id {snap} has been successfully created.\n"
        elif command == "snapshot-switch":
            vm["switched_to"] = rest[rest.index("--id") + 1]
        elif command == "capture":
            Path(rest[rest.index("--file") + 1]).write_bytes(b"\x89PNG" + bytes(2048))
        return ""

    def of(self, name):
        return [c for c in self.calls if c[1] == name and c[0] != "list"]


class MacGuest:
    """A macOS guest answering the controller's commands. `missing` names the
    prerequisites it lacks; `marker_survives` breaks snapshot restoration."""

    def __init__(self, prlctl, access_key, missing=()):
        self.prlctl, self.access_key, self.missing = prlctl, access_key, set(missing)
        self.commands, self.users, self.files = [], [], {}
        self.marker_survives = False
        self.vm = None  # the VM this guest answers for, so a shutdown stops it
        self.nix_after = 0  # how many readiness probes see no /nix yet, as right after boot

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        self.commands.append((command, stdin))
        status, out = 0, ""
        if "mount" in command and "/nix" in command:
            self.nix_after -= 1
            status = 1 if "nix" in self.missing or self.nix_after >= 0 else 0
        elif command.startswith("xcode-select -p"):
            status, out = (2, "") if "clt" in self.missing else (0, "/Library/Developer/CommandLineTools\n")
        elif command.startswith("timeout --version"):
            status, out = (127, "") if "timeout" in self.missing else (0, "timeout (GNU coreutils) 9.7\n")
        elif command.startswith("tty"):
            out = "/dev/ttys001\n40 120\n"
        elif command.startswith("cat > roundtrip"):
            self.files["roundtrip"] = stdin
        elif command.startswith("cat roundtrip"):
            out = self.files.pop("roundtrip")
        elif command.startswith("test -e reset-marker"):
            status = 0 if self.marker_survives else 1
        elif command.startswith("bash -s") and stdin and b"pgrep" in stdin:
            out = f"key: {self.access()}\n"
        elif command.startswith("sudo -n shutdown"):
            self.prlctl.vms[self.vm]["state"] = "stopped"
        elif command.startswith("sw_vers"):
            out = "26.6.2\n"
        elif command.startswith("uname -m"):
            out = "arm64\n"
        elif command.startswith("uname -r"):
            out = "25.6.0\n"
        elif "command -v timeout" in command:
            out = "/nix/store/0000-coreutils-9.7/bin/timeout\n"
        elif "--version" in command:
            out = "tool 1.0\n"
        if check and status != 0:
            raise guest.GuestError(f"`{command}` exited {status}")
        if raw:
            return status, out if isinstance(out, bytes) else out.encode(), ""
        return status, out.decode() if isinstance(out, bytes) else out, ""

    def access(self):
        return Path(self.access_key()).read_text().strip()

    def wait(self, deadline):
        pass


class Lab:
    """A temporary repository with the macOS infra, a registry and a fake host."""

    def __init__(self, test, config=CONFIG):
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        git = ["git", "-C", str(self.repo), "-c", "user.name=t", "-c", "user.email=t@example.com"]
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        shutil.copytree(REPO / template.INFRA / "macos", self.repo / template.INFRA / "macos")
        shutil.copy(REPO / "flake.lock", self.repo / "flake.lock")
        shutil.copytree(REPO / "tests" / "scenarios", self.repo / "tests" / "scenarios",
                        ignore=shutil.ignore_patterns("__pycache__", "tests"))
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run([*git, "commit", "-qm", "one"], check=True)
        subprocess.run([*git, "tag", "-a", "0.1.0", "-m", "0.1.0"], check=True)
        self.config = config
        self.state = contracts.state_dir(config, self.repo)
        (self.state / "keys").mkdir(parents=True)
        (self.state / "keys" / "bootstrap").write_text("not a real key\n")
        self.reg = registry.Registry(self.state)
        self.prlctl = Prlctl(self.repo / "homes")
        self.prl = parallels.Parallels("prlctl", "prlsrvctl", self.prlctl, owned=self.reg)
        self.guests = []
        self.connected = []
        self.missing = ()

    def connect(self, ip, key, known_hosts, accept_new=False, user="root", posix=False):
        g = MacGuest(self.prlctl, lambda: self.candidate_key(), self.missing)
        g.vm = next(n for n, vm in self.prlctl.vms.items() if vm["state"] == "running")
        self.connected.append(g)
        self.guests.append({"key": Path(key).name, "user": user, "posix": posix, "accept_new": accept_new})
        return g

    def candidate_key(self):
        return next(self.state.glob("templates/macos/candidates/*/access.pub"))

    def mac(self):
        lab = macos.MacLab(self.repo, self.config, self.prl, self.reg, connect=self.connect,
                           lease=lambda mac, deadline: "192.0.2.30")
        lab.sleep = lambda seconds: None
        return lab

    def built(self):
        result = self.mac().build("macos", "arm64")
        assert result["status"] == "success", result
        return result["data"]["candidate"]


def codes(result):
    return {f["code"] for f in result["findings"]}


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)

    def test_a_candidate_is_a_full_clone_of_the_source_with_its_own_access(self):
        run_id = self.lab.built()
        vm = f"busybee-lab-tpl-macos-{run_id}"
        clone = next(c for c in self.lab.prlctl.calls if c[0] == "clone")
        self.assertEqual(clone[1], SOURCE)
        self.assertNotIn("--linked", clone)
        # The source is only read: nothing else ever touches it.
        self.assertEqual([c[0] for c in self.lab.prlctl.of(SOURCE)], ["clone"])
        self.assertEqual(self.lab.reg.get(vm)["role"], "candidate")
        # Host devices go before the first boot, and the guest is shut down from inside.
        order = [c[0] if c[0] != "set" else " ".join(c[2:4]) for c in self.lab.prlctl.of(vm)]
        self.assertLess(order.index("--device-del sound0"), order.index("start"))
        self.assertNotIn("stop", order)
        # Provisioning used the operator's bootstrap key once; everything after it the run's own key.
        self.assertEqual([(g["key"], g["accept_new"]) for g in self.lab.guests], [("bootstrap", True), ("access", False)])
        self.assertTrue(all(g["user"] == "lab" and g["posix"] for g in self.lab.guests))
        record = json.loads((template.candidate_dir(self.lab.state, "macos", run_id) / "candidate.json").read_text())
        self.assertEqual((record["status"], record["guest_user"]), ("built", "lab"))
        manifest = record["manifest"]
        self.assertEqual((manifest["os"], manifest["arch"], manifest["clone_modes"]), ("macos", "arm64", []))
        self.assertIn(manifest["snapshot_id"], self.lab.prlctl.vms[vm]["snapshots"])
        self.assertIn("command_line_tools", manifest["tools"])
        self.assertIn("infra/vm/macos/provision.sh", manifest["lock_hashes"])

    def test_the_dependency_cache_is_warmed_and_timeout_installed(self):
        self.lab.built()
        ran = "\n".join(c for g in self.lab.connected for c, _ in g.commands)
        self.assertIn("nix develop -c cargo fetch", ran)
        # GNU timeout bounds every exec; it comes from the dev shell's own coreutils.
        self.assertIn("nix profile add /nix/store/0000-coreutils-9.7", ran)
        provisioned = self.lab.connected[0].commands
        self.assertTrue(any(b"softwareupdate" in (stdin or b"") for _, stdin in provisioned))

    def test_build_needs_its_source_and_access_configured(self):
        broken = {**CONFIG, "templates": {"macos": {"manifest": "templates/macos/manifest.json",
                                                    "clone_strategy": "full"}}}
        lab = Lab(self, broken)
        result = lab.mac().build("macos", "arm64")
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("config_invalid", codes(result))
        self.assertEqual(lab.reg.entries(), {})

    def test_a_running_source_is_not_cloned(self):
        self.lab.prlctl.vms[SOURCE]["state"] = "running"
        result = self.lab.mac().build("macos", "arm64")
        self.assertIn("source_running", codes(result))
        self.assertEqual(self.lab.prlctl.calls, [])
        self.assertEqual(self.lab.reg.entries(), {})

    def test_the_macos_guest_limit_is_a_named_finding(self):
        self.lab.prlctl.refuse_start = True
        result = self.lab.mac().build("macos", "arm64")
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("macos_guest_limit", codes(result))
        # The failed candidate is cleaned up through ownership like any other.
        self.assertEqual(list(self.lab.prlctl.vms), [SOURCE])
        self.assertEqual(self.lab.reg.entries(), {})


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        self.run_id = self.lab.built()
        self.vm = f"busybee-lab-tpl-macos-{self.run_id}"
        self.lab.guests.clear()
        self.lab.prlctl.calls.clear()

    def validate(self):
        return self.lab.mac().validate("macos", self.run_id)

    def record(self):
        path = template.candidate_dir(self.lab.state, "macos", self.run_id) / "candidate.json"
        return json.loads(path.read_text())

    def test_macos_template_validates_unattended_capabilities(self):
        result = self.validate()
        self.assertEqual(result["status"], "success", result["findings"])
        checks = result["data"]["checks"]
        self.assertEqual(list(checks), list(macos.CAPABILITIES))
        self.assertEqual({c["status"] for c in checks.values()}, {"pass"})
        for needle in ("nix_store", "dev_tools", "terminal_transport", "console_capture", "snapshot_reset"):
            self.assertIn(needle, checks)
        self.assertEqual(checks["terminal_transport"]["tty"], "/dev/ttys001")
        # A full clone of the recorded snapshot, never a linked one, and never from whatever the VM holds now.
        manifest = self.record()["manifest"]
        switch = next(i for i, c in enumerate(self.lab.prlctl.calls) if c[:2] == ["snapshot-switch", self.vm])
        clone = next(i for i, c in enumerate(self.lab.prlctl.calls) if c[0] == "clone")
        self.assertLess(switch, clone)
        self.assertEqual(self.lab.prlctl.calls[switch][3], manifest["snapshot_id"])
        self.assertNotIn("--linked", self.lab.prlctl.calls[clone])
        self.assertEqual(manifest["clone_modes"], ["full"])
        # Access is unattended: the run's key, pinned host key, no bootstrap key.
        self.assertEqual({(g["key"], g["accept_new"]) for g in self.lab.guests}, {("access", False)})
        self.assertEqual([n for n in self.lab.prlctl.vms if n.startswith("busybee-lab-val-")], [])

    def test_the_nix_store_is_awaited_after_boot(self):
        # determinate-nixd mounts /nix some seconds after SSH answers.
        original = self.lab.connect

        def slow(*args, **kwargs):
            g = original(*args, **kwargs)
            g.nix_after = 3
            return g
        self.lab.connect = slow
        lab = self.lab.mac()
        lab.sleep = lambda seconds: None
        result = lab.validate("macos", self.run_id)
        self.assertEqual(result["status"], "success", result["findings"])

    def test_a_missing_prerequisite_is_named_before_eligibility(self):
        for missing, needle in ((("clt",), "Command Line Tools"), (("nix",), "/nix"), (("timeout",), "timeout")):
            with self.subTest(missing):
                self.lab.missing = missing
                # A store that never mounts is waited for until the command deadline.
                self.lab.config = {**CONFIG, "deadlines": {**CONFIG["deadlines"], "command": 1}}
                result = self.validate()
                self.assertEqual(result["status"], "environment_failure")
                text = "\n".join(f["message"] for f in result["findings"])
                self.assertIn(needle, text)
                self.assertEqual(self.record()["status"], "rejected")
                self.assertEqual(self.record()["manifest"]["clone_modes"], [])

    def test_a_reset_that_keeps_a_marker_is_rejected(self):
        original = self.lab.connect

        def leaky(*args, **kwargs):
            g = original(*args, **kwargs)
            g.marker_survives = True
            return g
        self.lab.connect = leaky
        result = self.validate()
        self.assertIn("snapshot_reset", "\n".join(f["message"] for f in result["findings"]))


CANDIDATE = "r-20261003T000000Z-abcdef"
BASELINE_VM = f"busybee-lab-tpl-macos-{CANDIDATE}"
BASELINE_ID = "{11111111-2222-3333-4444-555555555555}"
BASELINE_SNAPSHOT = "{66666666-7777-8888-9999-000000000000}"


class Leases:
    """A promoted macOS baseline, and in-process supervisors that hold a run's
    lease the way the real one does: through a duplicate of the creator's
    descriptor, closed when the supervisor dies."""

    def __init__(self, test):
        self.lab = Lab(test)
        self.lab.prlctl.vms[BASELINE_VM] = {"id": BASELINE_ID, "state": "stopped", "snapshots": [BASELINE_SNAPSHOT],
                                            "devices": ["hdd0", "net0"]}
        self.reg, self.state = self.lab.reg, self.lab.state
        self.reg.claim(BASELINE_VM, "candidate", "macos", CANDIDATE, "2026-10-03T00:00:00Z")
        self.reg.bind(BASELINE_VM, BASELINE_ID)
        self.promote(CANDIDATE, BASELINE_ID, BASELINE_SNAPSHOT)
        self.revision = subprocess.run(["git", "-C", str(self.lab.repo), "rev-parse", "HEAD"], capture_output=True,
                                       text=True, check=True).stdout.strip()
        self.guest = FakeGuest()
        self.now = 1_000_000.0
        self.held = {}  # run id -> the descriptor its supervisor holds
        self.supervisors = {}
        test.addCleanup(lambda: [os.close(fd) for fd in self.held.values()])

    def promote(self, candidate, vm_id, snapshot):
        cdir = template.candidate_dir(self.state, "macos", candidate)
        cdir.mkdir(parents=True, exist_ok=True)
        (cdir / "candidate.json").write_text(json.dumps({"run_id": candidate, "guest_user": "operator7"}))
        template._write_json(template.manifest_path(self.state, "macos"), {
            "schema": contracts.TEMPLATE_SCHEMA, "name": "macos", "candidate": candidate, "os": "macos",
            "arch": "arm64", "vm_id": vm_id, "snapshot_id": snapshot, "provisioning_revision": "0" * 40,
            "lock_hashes": {}, "tools": {}, "parallels_version": "27.0.1", "clone_modes": ["full"],
            "validated_at": "2026-10-03T00:00:00Z"})

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds

    def supervise(self, run_id, lease=None):
        if lease is not None:
            self.held[run_id] = os.dup(lease.fileno())
        if run_id not in self.supervisors:
            self.supervisors[run_id] = supervisor.Supervisor(self.workers(), run_id,
                                                             alive=lambda pid: pid in self.guest.live)
        if run_id in self.held and not self.supervisors[run_id].tick():
            self.kill_supervisor(run_id)  # nothing left to watch: the process ends, and its lease with it

    def settle(self):
        """Time passes: every live supervisor ticks once."""
        for run_id in list(self.held):
            self.supervise(run_id)

    def kill_supervisor(self, run_id):
        os.close(self.held.pop(run_id))

    def workers(self):
        return worker.Workers(self.lab.repo, CONFIG, self.lab.prl, self.reg, lambda path: 500,
                              connect=lambda record, info, deadline: self.guest, supervise=self.supervise,
                              clock=self.clock, sleep=self.sleep, slot_wait_s=5)

    def create(self):
        self.settle()
        result = self.workers().create("macos", self.revision)
        assert result["status"] == "success", result
        return result["data"]["run_id"]

    def slot(self):
        return [n for n, e in self.reg.entries().items() if e["role"] == "slot"]


class LeaseTests(unittest.TestCase):
    def setUp(self):
        self.l = Leases(self)
        self.prlctl = self.l.lab.prlctl

    def record(self, run_id):
        return json.loads((worker.run_dir(self.l.state, run_id) / "worker.json").read_text())

    def test_a_macos_worker_leases_the_slot_instead_of_cloning(self):
        first = self.l.create()
        [slot] = self.l.slot()
        self.assertTrue(slot.startswith(contracts.SLOT_PREFIX))
        entry = self.l.reg.get(slot)
        self.assertEqual((entry["holder"], entry["parent"]), (first, BASELINE_ID))
        record = self.record(first)
        self.assertEqual(contracts.worker_errors(record), [])
        self.assertEqual((record["worker"], record["clone_strategy"]), (slot, "full"))
        self.assertIn(record["reset_snapshot_id"], self.prlctl.vms[slot]["snapshots"])
        self.assertEqual(self.l.workers().destroy(first)["status"], "success")
        # Released: the guest is kept and stopped, the lease is gone.
        self.assertIn(slot, self.prlctl.vms)
        self.assertEqual(self.prlctl.vms[slot]["state"], "stopped")
        self.assertIsNone(self.l.reg.get(slot)["holder"])
        self.assertEqual(self.record(first)["status"], "destroyed")
        second = self.l.create()
        self.assertEqual(self.l.slot(), [slot])
        self.assertEqual(self.record(second)["worker"], slot)
        clones = [c for c in self.prlctl.calls if c[0] == "clone"]
        self.assertEqual(len(clones), 1)
        self.assertNotIn("--linked", clones[0])

    def test_every_grant_restores_the_slot_before_the_guest_is_used(self):
        for _ in range(2):
            before = len(self.prlctl.calls)
            run_id = self.l.create()
            record = self.record(run_id)
            calls = [c[0] for c in self.prlctl.calls[before:] if c[1] == record["worker"] and c[0] != "list"]
            # Restored to the recorded snapshot, then started on demand.
            self.assertEqual(calls[-2:], ["snapshot-switch", "start"])
            switch = next(c for c in reversed(self.prlctl.calls) if c[0] == "snapshot-switch")
            self.assertEqual(switch[3], record["reset_snapshot_id"])
            self.assertEqual(self.l.workers().destroy(run_id)["status"], "success")

    def test_the_slot_is_not_counted_against_the_linux_workers(self):
        # Two Linux workers (claimed, as by creations in flight) fill the Linux
        # cap; the macOS slot has its own lease and is still granted.
        for _ in range(2):
            run_id = contracts.new_run_id()
            worker.run_dir(self.l.state, run_id).mkdir(parents=True)
            self.l.reg.claim(contracts.worker_name(run_id), "worker", "linux", run_id, "2026-10-01T00:00:00Z",
                             parent=BASELINE_ID)
        run_id = self.l.create()
        self.assertEqual(self.l.reg.get(self.l.slot()[0])["holder"], run_id)

    def test_a_second_lease_waits_in_line(self):
        first = self.l.create()
        result = self.l.workers().create("macos", self.l.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("lease_wait_timeout", codes(result))
        self.assertEqual(self.l.reg.get(self.l.slot()[0])["holder"], first)

    def test_a_holder_that_died_frees_the_slot_and_the_next_grant_resets(self):
        dead = self.l.create()
        self.l.kill_supervisor(dead)  # SIGKILL: its descriptor closes, its record still says ready
        before = len(self.prlctl.calls)
        after = self.l.create()
        self.assertEqual(self.record(dead)["status"], "failed")
        self.assertIn("lease_lost", [e["event"] for e in self.l.workers().events(dead)])
        self.assertEqual(self.l.reg.get(self.l.slot()[0])["holder"], after)
        self.assertIn("snapshot-switch", [c[0] for c in self.prlctl.calls[before:]])
        # The dead holder can no longer reach the guest, and destroying it changes nothing of the new lease.
        with self.assertRaises(worker.Refused):
            self.l.workers().exec(dead, ["true"], "/", {}, 10)
        self.assertEqual(self.l.workers().destroy(dead)["status"], "success")
        self.assertEqual(self.l.reg.get(self.l.slot()[0])["holder"], after)

    def test_a_restarted_supervisor_takes_its_lease_back_only_while_it_holds_it(self):
        run_id = self.l.create()
        self.l.kill_supervisor(run_id)
        held = self.l.workers().rehold(run_id)
        self.assertIsNotNone(held)
        self.assertEqual(self.record(run_id)["status"], "ready")
        held.close()
        # Someone else holds the slot's lock now: the run has lost its lease.
        other = self.l.workers().slot.try_hold()
        self.addCleanup(other.close)
        self.assertIsNone(self.l.workers().rehold(run_id))
        self.assertEqual(self.record(run_id)["status"], "failed")
        self.assertIn("lease_lost", [e["event"] for e in self.l.workers().events(run_id)])

    def test_a_supervisor_started_while_the_lease_is_being_created_leaves_it_alone(self):
        # `vmctl status` reconciles while `worker create macos` still holds the
        # run's operation lock and the slot: that is no lost lease.
        run_id = contracts.new_run_id()
        rdir = worker.run_dir(self.l.state, run_id)
        rdir.mkdir(parents=True)
        record = {"run_id": run_id, "template": "macos", "status": "provisioning"}
        template._write_json(rdir / "worker.json", record)
        creating = self.l.workers().slot.try_hold()
        self.addCleanup(creating.close)
        with worker.locked(self.l.workers()._lock(run_id)):
            self.assertIsNone(self.l.workers().rehold(run_id))
        self.assertEqual(self.record(run_id)["status"], "provisioning")
        self.assertNotIn("lease_lost", [e["event"] for e in self.l.workers().events(run_id)])

    def test_the_supervisor_inherits_the_lease_descriptor(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_id = "r-20261003T000000Z-000000"
            rdir = worker.run_dir(tmp, run_id)
            rdir.mkdir(parents=True)
            slot = lease.Slot(tmp)
            held = slot.try_hold()
            # Stands in for `vmctl supervise`: it takes the run's supervisor lock
            # and checks that the descriptor it was given is the held lease.
            script = ("import fcntl, sys, time; s = open(sys.argv[1], 'a'); fcntl.flock(s, fcntl.LOCK_EX); "
                      "fd = int(sys.argv[sys.argv.index('--lease-fd') + 1]); fcntl.flock(fd, fcntl.LOCK_EX | "
                      "fcntl.LOCK_NB); time.sleep(30)")
            child = []
            real = supervisor.subprocess.Popen

            def popen(*args, **kwargs):
                child.append(real(*args, **kwargs))
                return child[-1]
            supervisor.subprocess.Popen = popen
            try:
                supervisor.ensure(tmp, run_id, [sys.executable, "-c", script, str(rdir / "supervisor.lock")],
                                  lease=held)
            finally:
                supervisor.subprocess.Popen = real
            self.addCleanup(lambda: (child[0].kill(), child[0].wait()))
            held.close()
            # The child holds it alone now: nobody else can take it until it ends.
            self.assertIsNone(slot.try_hold())
            self.assertIsNone(child[0].poll())
            child[0].kill()
            child[0].wait()
            again = slot.try_hold()
            self.assertIsNotNone(again)
            again.close()

    def test_a_retained_holder_keeps_the_slot_until_destroyed(self):
        kept = self.l.create()
        record = self.record(kept)
        record["status"] = "retained"
        template._write_json(worker.run_dir(self.l.state, kept) / "worker.json", record)
        self.l.kill_supervisor(kept)  # a frozen worker's supervisor has nothing left to watch
        self.l.settle()
        blocked = self.l.workers().create("macos", self.l.revision)
        self.assertIn("lease_wait_timeout", codes(blocked))
        self.assertIn(kept, "\n".join(f["message"] for f in blocked["findings"]))

    def test_a_slot_guest_that_cannot_be_prepared_is_removed(self):
        self.l.lab.prlctl.vms[BASELINE_VM]["devices"].append("usb")
        result = self.l.workers().create("macos", self.l.revision)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("usb", result["findings"][0]["message"])
        self.assertEqual(self.l.slot(), [])
        self.assertEqual([n for n in self.prlctl.vms if n.startswith(contracts.SLOT_PREFIX)], [])

    def test_a_new_baseline_replaces_an_idle_slot(self):
        old = self.l.create()
        [slot] = self.l.slot()
        self.l.workers().destroy(old)
        new_vm, new_id, new_snap = f"busybee-lab-tpl-macos-{contracts.new_run_id()}", \
            "{33333333-2222-3333-4444-555555555555}", "{44444444-7777-8888-9999-000000000000}"
        self.prlctl.vms[new_vm] = {"id": new_id, "state": "stopped", "snapshots": [new_snap], "devices": ["hdd0", "net0"]}
        self.l.reg.claim(new_vm, "candidate", "macos", new_vm[-25:], "2026-10-03T00:00:00Z")
        self.l.reg.bind(new_vm, new_id)
        self.l.promote(new_vm[-25:], new_id, new_snap)
        run_id = self.l.create()
        [replacement] = self.l.slot()
        self.assertNotEqual(replacement, slot)
        self.assertNotIn(slot, self.prlctl.vms)
        self.assertEqual(self.l.reg.get(replacement)["parent"], new_id)
        self.assertEqual(self.record(run_id)["candidate"], new_vm[-25:])


class MacWorkerTests(unittest.TestCase):
    def setUp(self):
        self.l = Leases(self)
        self.run_id = self.l.create()

    def test_scenarios_run_as_root_from_the_lab_accounts_checkout(self):
        scenario.run(self.l.workers(), self.run_id, "wedged-task", "prepared")
        command = json.loads(next(worker.run_dir(self.l.state, self.run_id).glob("exec/*/command.json")).read_text())
        self.assertEqual(command["cwd"], "/Users/operator7/busybee")
        # The exec user is the lab account; the runner must start as root to drop to nobody.
        self.assertEqual(command["argv"][:4], ["nix", "develop", "-c", "sudo"])
        self.assertIn("python3", command["argv"])

    def test_the_terminal_driver_is_linux_only(self):
        with self.assertRaises(worker.Refused) as caught:
            terminal_ops.open_terminal(self.l.workers(), self.run_id, ["true"], 80, 24, None, {}, 60)
        self.assertEqual(caught.exception.code, "platform_unsupported")

    def test_the_public_export_hides_the_guest_account(self):
        self.l.guest.stdout = b"  501 operator7 S 00:42 /usr/sbin/sshd: operator7@ttys000\n"
        self.l.workers().exec(self.run_id, ["ps", "-ax"], "/Users/operator7/busybee", {}, 30)
        self.l.workers().collect(self.run_id)
        out = self.l.state / self.l.workers().export(self.run_id)["data"]["path"]
        blob = b"".join(p.read_bytes() for p in sorted(out.rglob("*")) if p.is_file())
        self.assertNotIn(b"operator7", blob)
        self.assertIn(b"<guest-user>", blob)

    def test_observations_parse_bsd_elapsed_times(self):
        self.assertEqual(worker._seconds("2-03:04:05"), 2 * 86400 + 3 * 3600 + 4 * 60 + 5)
        self.assertEqual(worker._seconds("04:05"), 245)
        self.assertEqual(worker._seconds("42"), 42)


class QueueTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dead = set()
        self.slot = lease.Slot(tmp.name, alive=lambda pid: pid not in self.dead)

    def test_waiters_are_served_in_arrival_order(self):
        tickets = [self.slot.join(f"run-{n}") for n in range(3)]
        self.assertEqual([self.slot.ahead(t) for t in tickets], [0, 1, 2])
        self.slot.leave(tickets[0])
        self.assertEqual([self.slot.ahead(t) for t in tickets[1:]], [0, 1])

    def test_a_waiter_whose_process_is_gone_is_skipped(self):
        first = self.slot.join("run-a")
        second = self.slot.join("run-b")
        queue = json.loads((self.slot.dir / "queue.json").read_text())
        queue["waiting"][0]["pid"] = 999_999_999
        (self.slot.dir / "queue.json").write_text(json.dumps(queue))
        self.dead.add(999_999_999)
        self.assertEqual(self.slot.ahead(second), 0)

    def test_the_lock_is_exclusive_and_freed_with_its_holder(self):
        held = self.slot.try_hold()
        self.assertIsNotNone(held)
        self.assertIsNone(self.slot.try_hold())
        held.close()
        self.assertIsNotNone(self.slot.try_hold())

    def test_a_waiter_reports_its_position(self):
        held = self.slot.try_hold()
        self.addCleanup(held.close)
        now = [0.0]
        slot = lease.Slot(self.slot.dir.parent.parent, clock=lambda: now[0],
                          sleep=lambda s: now.__setitem__(0, now[0] + s))
        heard = []
        with self.assertRaises(lease.Waited):
            slot.acquire("run-b", 3, lambda ahead, why: heard.append((ahead, why)), lambda: None)
        self.assertEqual(heard, [(1, "a lease holds the slot")])
        self.assertEqual(json.loads((slot.dir / "queue.json").read_text())["waiting"], [])


if __name__ == "__main__":
    unittest.main()
