"""
Regression overlay for issue #107: reserve storage for admitted Linux workers.

These tests are RED on the base (no storage reservation in _admit) and GREEN
on the fix.  They duplicate nothing from test_worker.py; the acceptance-criteria
tests in test_worker.CreateTests are authoritative, these provide the overlay.
"""
import sys
import uuid
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_worker import BASELINE_ID, Lab, codes  # noqa: E402


class Issue107StorageReservationTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)

    def test_second_admission_counts_the_first_workers_allocation(self):
        # Free storage covers exactly one worker's allocation.  After the first
        # worker is admitted, the second sees that space as reserved and is
        # refused storage_exhausted at once, without waiting for a free slot.
        self.lab.free_gib = self.lab.config["worker"]["storage_gib"]
        self.lab.create()
        self.lab.wait_s = 600
        started = self.lab.now
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("storage_exhausted", codes(result))
        self.assertEqual(self.lab.now, started)

    def test_claimed_unrecorded_worker_reserves_storage(self):
        # A worker claimed under the lock but not yet recorded (allocation None)
        # must still reserve its full storage_gib.  The race: a second admission
        # arrives while the first is still cloning, before worker.json is saved.
        allotted = self.lab.config["worker"]["storage_gib"]
        self.lab.free_gib = allotted  # room for exactly one worker
        run_id = contracts.new_run_id()
        vm = contracts.worker_name(run_id)
        # Simulate a worker that was claimed but whose create() never recorded it.
        self.lab.reg.claim(vm, "worker", "linux", run_id, "2026-10-01T00:00:00Z", parent=BASELINE_ID)
        self.lab.prlctl.vms[vm] = {"id": "{" + str(uuid.uuid4()) + "}", "state": "stopped", "snapshots": []}
        self.lab.wait_s = 600
        started = self.lab.now
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertIn("storage_exhausted", codes(result))
        self.assertEqual(self.lab.now, started)

    def test_admission_with_room_for_two_succeeds(self):
        # Free storage covers two workers' allocations.  With one active worker
        # the reserved space still leaves room for a second admission.
        self.lab.free_gib = 2 * self.lab.config["worker"]["storage_gib"]
        self.lab.create()
        result = self.lab.workers().create("linux", self.lab.revision)
        self.assertEqual(result["status"], "success", result)
