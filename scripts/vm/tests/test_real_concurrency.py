"""Concurrent Linux workers against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and its promoted Linux baseline, with
`[concurrency] linux_workers` at its default of 2. Two workers are active at
once, each running its own command; a creation and a verification that find
both busy wait in line until one is destroyed. Every worker a test creates is
destroyed through the controller. CI has no Parallels and skips.
"""
import json
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from test_real_supervision import REAL, ROOT, STATE, VMCTL, codes, head, host_vms, record, registry, until, \
    vmctl, workers

QUEUED = "queued for a Linux worker"


def start(*args, err):
    """A controller process whose stderr (its queue reports) goes to `err`."""
    return subprocess.Popen([sys.executable, str(VMCTL), "--json", "--root", str(ROOT), *args],
                            stdout=subprocess.PIPE, stderr=err, text=True)


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealConcurrencyTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(workers(), set(), "a worker is already registered; destroy it first")
        cap = vmctl("doctor")["data"]["config"]["concurrency"]["linux_workers"]
        self.assertEqual(cap, 2, "these tests fill the default of two Linux workers")
        before = host_vms()
        baselines = {e["vm_id"] for e in registry().values() if e["role"] != "worker"}

        def untouched():
            after = host_vms()
            self.assertLessEqual(set(before), set(after))
            self.assertEqual({k: after[k] for k in baselines}, {k: before[k] for k in baselines})
            self.assertEqual(workers(), set(), "a worker outlived its test")
        self.addCleanup(untouched)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)

    def owned(self, result):
        self.assertEqual(result["status"], "success", result["findings"])
        run_id = result["data"]["run_id"]
        self.addCleanup(self.destroy_if_owned, run_id)
        return run_id

    def create(self):
        return self.owned(vmctl("worker", "create", "linux", "--revision", head()))

    def destroy_if_owned(self, run_id):
        if f"busybee-lab-{run_id}" in registry():
            result = vmctl("worker", "destroy", run_id)
            self.assertEqual(result["status"], "success", result["findings"])

    def destroy(self, run_id):
        vm_id = record(run_id)["vm_id"]
        result = vmctl("worker", "destroy", run_id)
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertNotIn(vm_id, host_vms())

    def test_two_linux_workers_run_side_by_side(self):
        # Both creations race; admission and the claim are serialised by the controller lock.
        procs = [start("worker", "create", "linux", "--revision", head(), err=subprocess.DEVNULL) for _ in range(2)]
        a, b = (self.owned(json.loads(p.communicate(timeout=1800)[0])) for p in procs)
        self.assertNotEqual(record(a)["vm_id"], record(b)["vm_id"])
        status = vmctl("status")["data"]
        self.assertEqual({(w["run_id"], w["status"], w["vm_state"]) for w in status["workers"]},
                         {(a, "ready", "running"), (b, "ready", "running")})
        self.assertEqual(status["resources"]["linux_workers"], 2)

        # Each runs its own command at the same time, and sees only its own guest.
        # A linked clone keeps its baseline's machine id; its network card's address is its own.
        script = ('printf %s "$MARK" > /tmp/mark; sleep 20; cat /tmp/mark; echo; '
                  'grep -vh 00:00:00:00:00:00 /sys/class/net/*/address | sort | head -1')
        handles = {run: vmctl("exec", run, "--cwd", "/tmp", f"--env=MARK=mark-{run}", "--detach", "--",
                              "sh", "-c", script)["data"]["exec"] for run in (a, b)}
        until(lambda: all(vmctl("status", run, handles[run])["data"]["state"] == "running" for run in (a, b)),
              60, "both commands were not running at once")
        out = {}
        for run in (a, b):
            done = vmctl("wait", run, handles[run])
            self.assertEqual(done["status"], "success", done["findings"])
            out[run] = (STATE / done["data"]["stdout"]).read_text().split()
            self.assertEqual(out[run][0], f"mark-{run}")
            self.assertTrue((STATE / "runs" / run / "exec" / handles[run] / "stdout").is_file())
        self.assertNotEqual(out[a][1], out[b][1], "the two workers are one machine")

        for run in (a, b):
            self.destroy(run)
        self.assertEqual(workers(), set())

    def test_a_creation_waits_for_a_free_linux_worker(self):
        a, b = self.create(), self.create()
        started = time.monotonic()
        bounded = vmctl("worker", "create", "linux", "--revision", head(), "--wait", "10")
        self.assertGreaterEqual(time.monotonic() - started, 10)
        self.assertIn("worker_limit", codes(bounded))
        self.assertIn(a, bounded["findings"][0]["message"])
        self.assertIn(b, bounded["findings"][0]["message"])
        self.assertEqual(workers(), {a, b})

        with open(self.tmp / "waiter.err", "w") as err:
            waiter = start("worker", "create", "linux", "--revision", head(), "--wait", "1800", err=err)
        until(lambda: QUEUED in (self.tmp / "waiter.err").read_text(), 120, "the creation never queued")
        time.sleep(15)
        self.assertIsNone(waiter.poll(), "the creation did not wait")
        self.assertEqual(workers(), {a, b})
        self.destroy(a)
        c = self.owned(json.loads(waiter.communicate(timeout=1800)[0]))
        self.assertEqual(workers(), {b, c})
        for run in (b, c):
            self.destroy(run)

    def test_a_verification_waits_for_a_linux_worker(self):
        # A verification that finds both Linux workers busy (two sessions, or a
        # session and a retained worker) waits instead of ending incomplete.
        a, b = self.create(), self.create()
        with open(self.tmp / "verify.err", "w") as err:
            verifying = start("verify", "--revision", head(), "--platform", "linux", err=err)
        until(lambda: QUEUED in (self.tmp / "verify.err").read_text(), 120, "the verification never queued")
        time.sleep(15)
        self.assertIsNone(verifying.poll(), "the verification did not wait")
        self.assertEqual(workers(), {a, b})
        self.destroy(a)
        result = json.loads(verifying.communicate(timeout=3600)[0])
        matrix = json.loads((STATE / result["data"]["matrix"]).read_text())
        linux = matrix["platforms"]["linux"]
        self.assertEqual(linux["status"], "ran", linux)
        self.assertNotIn("platform_missing", codes(result))
        # The verification's own worker is gone again; the other stays until destroyed.
        self.assertEqual(workers(), {b})
        self.destroy(b)


if __name__ == "__main__":
    unittest.main()
