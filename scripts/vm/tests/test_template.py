from pathlib import Path
import json
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import guest
import parallels
import registry
import template


class State:
    def __init__(self, test):
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)


class RecordingRunner:
    def __init__(self):
        self.calls = []

    def __call__(self, argv, timeout=None, stdin=None):
        self.calls.append((list(argv), stdin))
        return ""


class OwnershipTests(unittest.TestCase):
    def setUp(self):
        self.state = State(self).root
        self.runner = RecordingRunner()
        self.prl = parallels.Parallels("prlctl", "prlsrvctl", self.runner, owned=registry.Registry(self.state))

    def test_mutations_refuse_a_vm_the_controller_does_not_own(self):
        for call in (lambda: self.prl.start("unrelated-vm"), lambda: self.prl.delete("busybee-lab-x"),
                     lambda: self.prl.stop("someone-else", kill=True),
                     lambda: self.prl.create("busybee-lab-unclaimed", self.state, template.DISK_MIB)):
            with self.assertRaises(parallels.ParallelsError):
                call()
        self.assertEqual(self.runner.calls, [])

    def test_a_claim_must_carry_the_lab_prefix(self):
        reg = registry.Registry(self.state)
        with self.assertRaises(ValueError):
            reg.claim("unrelated-vm", "candidate", "linux", contracts.new_run_id(), "2026-10-01T00:00:00Z")

    def test_claimed_vms_can_be_operated(self):
        reg = registry.Registry(self.state)
        reg.claim("busybee-lab-tpl-linux-x", "candidate", "linux", contracts.new_run_id(), "2026-10-01T00:00:00Z")
        self.prl.create("busybee-lab-tpl-linux-x", self.state, template.DISK_MIB)
        self.prl.start("busybee-lab-tpl-linux-x")
        self.assertEqual([c[0][1] for c in self.runner.calls], ["create", "set", "start"])

    def test_an_installed_candidate_loses_its_host_devices(self):
        reg = registry.Registry(self.state)
        reg.claim("busybee-lab-tpl-linux-x", "candidate", "linux", contracts.new_run_id(), "2026-10-01T00:00:00Z")
        self.prl.configure("busybee-lab-tpl-linux-x", 2, 4096, "/iso")
        self.prl.boot_from_disk("busybee-lab-tpl-linux-x")
        removed = [argv[argv.index("--device-del") + 1] for argv, _ in self.runner.calls if "--device-del" in argv]
        self.assertEqual(sorted(removed), sorted(template.HOST_DEVICES))

    def test_a_candidate_gets_an_explicitly_sized_disk(self):
        reg = registry.Registry(self.state)
        reg.claim("busybee-lab-tpl-linux-x", "candidate", "linux", contracts.new_run_id(), "2026-10-01T00:00:00Z")
        self.prl.create("busybee-lab-tpl-linux-x", self.state, template.DISK_MIB)
        create, add = (argv for argv, _ in self.runner.calls)
        self.assertIn("--no-hdd", create)
        self.assertEqual(add[add.index("--device-add") + 1:], ["hdd", "--type", "expand", "--size", "16384"])

    def test_the_registry_survives_a_new_controller(self):
        run_id = contracts.new_run_id()
        registry.Registry(self.state).claim("busybee-lab-a", "candidate", "linux", run_id, "2026-10-01T00:00:00Z")
        registry.Registry(self.state).bind("busybee-lab-a", "{11111111-2222-3333-4444-555555555555}")
        again = registry.Registry(self.state)
        self.assertEqual(again.get("busybee-lab-a")["vm_id"], "{11111111-2222-3333-4444-555555555555}")
        with self.assertRaises(ValueError):
            again.claim("busybee-lab-a", "candidate", "linux", run_id, "2026-10-01T00:00:00Z")
        again.release("busybee-lab-a")
        self.assertIsNone(registry.Registry(self.state).get("busybee-lab-a"))


class FailingPrl:
    """Records VM-changing calls; `delete` fails when told to."""

    def __init__(self, delete_fails=False):
        self.calls, self.delete_fails = [], delete_fails

    def delete(self, name):
        self.calls.append(("delete", name))
        if self.delete_fails:
            raise parallels.ParallelsError("prlctl delete exited 255")

    def info(self, name):
        return {"state": "stopped"}


class CleanupTests(unittest.TestCase):
    """A run can fail after Parallels created a VM but before its identity was
    recorded; the claim must not be dropped while that VM may still exist."""

    def setUp(self):
        self.state = State(self).root
        self.reg = registry.Registry(self.state)
        self.reg.claim("busybee-lab-tpl-linux-x", "candidate", "linux", contracts.new_run_id(),
                       "2026-10-01T00:00:00Z")

    def lab(self, prl):
        config = {"state_dir": "build/vm", "deadlines": {"command": 60, "scenario": 60, "run": 60, "cleanup": 60}}
        return template.Lab(self.state, config, prl, self.reg)

    def test_an_unbound_vm_is_deleted_before_its_claim_is_released(self):
        prl = FailingPrl()
        self.assertEqual(self.lab(prl)._dispose("busybee-lab-tpl-linux-x", self.state / "console.png"), [])
        self.assertEqual(prl.calls, [("delete", "busybee-lab-tpl-linux-x")])
        self.assertIsNone(self.reg.get("busybee-lab-tpl-linux-x"))

    def test_an_unanswered_delete_keeps_the_claim(self):
        notes = self.lab(FailingPrl(delete_fails=True))._dispose("busybee-lab-tpl-linux-x",
                                                                 self.state / "console.png")
        self.assertTrue(notes)
        self.assertIsNotNone(self.reg.get("busybee-lab-tpl-linux-x"))


class ConsoleTests(unittest.TestCase):
    def test_text_becomes_set1_scancodes_with_shift(self):
        events = parallels.key_events("aB!\n")
        presses = [(e["scancode"], e["event"]) for e in events]
        self.assertEqual(presses, [(30, "press"), (30, "release"),
                                   (42, "press"), (48, "press"), (48, "release"), (42, "release"),
                                   (42, "press"), (2, "press"), (2, "release"), (42, "release"),
                                   (28, "press"), (28, "release")])

    def test_an_untypable_character_is_refused(self):
        with self.assertRaises(ValueError):
            parallels.key_events("é")


class LeaseTests(unittest.TestCase):
    def test_the_lease_for_a_mac_is_found(self):
        text = ('10.211.55.7="1000000000,1800,001c42000001,01001c42000001"\n'
                '10.211.55.19="1000000500,1800,001c42000002,01001c42000002"\n'
                '10.211.55.20="1000000100,1800,001c42000002,01001c42000002"\n')
        # The newest lease for the MAC wins, whatever its line order.
        self.assertEqual(guest.lease_ip(text, "00:1C:42:00:00:02"), "10.211.55.19")
        self.assertIsNone(guest.lease_ip(text, "001c42000003"))


CLEAN = {"processes": [], "sockets": [], "paths": [], "authorized_keys": ["ssh-ed25519 AAAA bootstrap"]}


class TaskStateTests(unittest.TestCase):
    def test_baseline_has_no_task_state(self):
        bootstrap = "ssh-ed25519 AAAA bootstrap"
        self.assertEqual(template.task_state_problems(CLEAN, bootstrap), [])
        dirty = {"processes": ["pueued", "bzbd"], "sockets": ["/run/user/0/bzbd.sock"],
                 "paths": ["/root/.local/share/pueue", "/root/.local/state/busybee/leases.json"],
                 "authorized_keys": [bootstrap, "ssh-ed25519 BBBB run-credential"]}
        problems = template.task_state_problems(dirty, bootstrap)
        text = "\n".join(problems)
        for needle in ("pueued", "bzbd", "bzbd.sock", "pueue", "leases.json", "run-credential"):
            self.assertIn(needle, text)


def candidate(state, run_id, status="built"):
    record = {"run_id": run_id, "status": status, "vm": f"busybee-lab-tpl-linux-{run_id}",
              "manifest": {"schema": contracts.TEMPLATE_SCHEMA, "name": "linux", "candidate": run_id, "os": "linux",
                           "arch": "aarch64", "vm_id": "{11111111-2222-3333-4444-555555555555}",
                           "snapshot_id": "{66666666-7777-8888-9999-000000000000}",
                           "provisioning_revision": "0" * 40, "lock_hashes": {}, "tools": {},
                           "parallels_version": "27.0.1", "clone_modes": ["linked"],
                           "validated_at": "2026-10-01T00:00:00Z"}}
    path = template.candidate_dir(state, "linux", run_id) / "candidate.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(record))
    return record


class PromotionTests(unittest.TestCase):
    def setUp(self):
        self.state = State(self).root

    def test_failed_template_is_not_promoted(self):
        good, bad = contracts.new_run_id(), contracts.new_run_id()
        candidate(self.state, good, "validated")
        self.assertEqual(template.promote(self.state, "linux", good)["status"], "success")
        before = template.manifest_path(self.state, "linux").read_text()

        candidate(self.state, bad, "rejected")
        result = template.promote(self.state, "linux", bad)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("candidate_not_validated", {f["code"] for f in result["findings"]})
        self.assertEqual(template.manifest_path(self.state, "linux").read_text(), before)

    def test_promotion_retains_the_previous_baseline(self):
        first, second = contracts.new_run_id(), contracts.new_run_id()
        candidate(self.state, first, "validated")
        candidate(self.state, second, "validated")
        template.promote(self.state, "linux", first)
        template.promote(self.state, "linux", second)
        manifest = json.loads(template.manifest_path(self.state, "linux").read_text())
        self.assertEqual(manifest["candidate"], second)
        retained = json.loads((self.state / "templates" / "linux" / "retained.json").read_text())
        self.assertEqual([m["candidate"] for m in retained], [first])
        self.assertEqual(contracts.manifest_errors(manifest), [])


class EligibilityTests(unittest.TestCase):
    def test_a_failed_or_unsupported_capability_prevents_eligibility(self):
        checks = {name: {"status": "pass"} for name in template.CAPABILITIES}
        self.assertEqual(template.ineligibility(checks), [])
        checks["console_capture"] = {"status": "unsupported", "reason": "capture returned no image"}
        checks["snapshot_reset"] = {"status": "fail", "reason": "marker survived the reset"}
        del checks["file_roundtrip"]
        reasons = "\n".join(template.ineligibility(checks))
        for needle in ("console_capture", "capture returned no image", "snapshot_reset", "file_roundtrip"):
            self.assertIn(needle, reasons)


if __name__ == "__main__":
    unittest.main()
