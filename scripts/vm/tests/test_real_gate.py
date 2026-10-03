"""The evidence gate on real workers: opt in with BUSYBEE_VM_LAB=1.

The pilot is #69 (cold-start umask), whose regression scenario
`umask-startup` names it. Its fix belongs to #69's own PR and has not landed,
so the replay judges origin/main as both the base and an unfixed candidate:
the red run must show the failure on Linux and macOS, and the gate must
refuse the candidate because the regression still fails, keeping every
matrix and run behind its verdict. Every worker it uses is destroyed and the
macOS slot released. CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import unittest

REPO = Path(__file__).resolve().parents[3]
# The checkout holding the lab state (build/vm); another worktree of the same
# repository can run these tests against it.
ROOT = Path(os.environ.get("BUSYBEE_VM_LAB_ROOT") or REPO).resolve()
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
STATE = ROOT / "build" / "vm"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
PILOT = 69


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", "--root", str(ROOT), *args], capture_output=True,
                         text=True)
    return json.loads(out.stdout)


def active():
    """Runs holding a worker or the macOS slot."""
    vms = json.loads((STATE / "registry.json").read_text())["vms"].values()
    return {e["run_id"] if e["role"] == "worker" else e["holder"] for e in vms
            if e["role"] == "worker" or e["role"] == "slot" and e.get("holder")}


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealGateTests(unittest.TestCase):
    def test_pilot_replays_red_base_and_unfixed_candidate(self):
        self.assertEqual(active(), set(), "a worker or the macOS slot is held; release it first")
        main = subprocess.run(["git", "-C", str(REPO), "rev-parse", "origin/main"], capture_output=True, text=True,
                              check=True).stdout.strip()
        result = vmctl("gate", "--issue", str(PILOT), "--revision", main, "--base", main)
        self.assertEqual(result["operation"], "gate", result)
        record = json.loads((STATE / result["data"]["gate"]).read_text())
        # The unfixed candidate is refused for the regression, not for the environment.
        self.assertEqual(record["verdict"], "failed", record["findings"])
        self.assertEqual(result["status"], "product_failure")
        self.assertIn("regression_still_failing", {f["code"] for f in record["findings"]})
        self.assertFalse({"evidence_stale", "platform_missing", "skipped", "timeout", "artifact_missing",
                          "cleanup_failed"} & {f["code"] for f in record["findings"]}, record["findings"])
        self.assertEqual({(r["platform"], r["mode"], r["base"], r["candidate"]) for r in record["regressions"]},
                         {("linux", "cold", "product_failure", "product_failure"),
                          ("macos", "cold", "product_failure", "product_failure")})
        # The evidence behind the verdict is kept, bound to this controller, and public.
        for side in ("candidate", "base"):
            matrix = json.loads((STATE / record[side]["matrix"]).read_text())
            self.assertEqual((matrix["role"], matrix["source"]["revision"]), (side, main))
            self.assertEqual(matrix["controller"]["head"], record["controller"]["head"])
            for platform, entry in matrix["platforms"].items():
                self.assertTrue((STATE / entry["evidence"]).is_dir(), (side, platform))
                self.assertEqual(entry["collected"], "success")
        public = (STATE / result["data"]["public"]).read_text()
        for side in ("candidate", "base"):
            for run_id in record[side]["runs"]:
                self.assertNotIn(run_id, public)
        self.assertNotIn(str(Path.home()), public)
        # Every owned worker is accounted for: none is left, the slot is free.
        self.assertEqual(active(), set())
        # The same evidence is judged again without verifying it again.
        again = vmctl("gate", "--issue", str(PILOT), "--revision", main, "--base", main)
        self.assertEqual(again["data"]["evidence_id"], result["data"]["evidence_id"])
        reused = json.loads((STATE / again["data"]["gate"]).read_text())
        self.assertEqual((reused["candidate"]["reused"], reused["base"]["reused"]), (True, True))


if __name__ == "__main__":
    unittest.main()
