"""Acceptance against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and leaves the built candidates in
place (registered, unpromoted) for inspection. CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import unittest

REPO = Path(__file__).resolve().parents[3]
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", *args], capture_output=True, text=True)
    return json.loads(out.stdout)


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealTemplateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.first = vmctl("template", "build", "linux", "--arch", "aarch64")
        cls.second = vmctl("template", "build", "linux", "--arch", "aarch64")

    def test_linux_template_bootstraps_unattended(self):
        self.assertEqual(self.first["status"], "success", self.first["findings"])
        self.assertTrue(self.first["data"]["snapshot_id"].startswith("{"))

    def test_template_validation_checks_real_capabilities(self):
        result = vmctl("template", "validate", "linux", "--candidate", self.first["data"]["candidate"])
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual({c["status"] for c in result["data"]["checks"].values()}, {"pass"})

    def test_repeat_provisioning_has_the_same_provenance(self):
        self.assertEqual(self.second["status"], "success", self.second["findings"])
        a, b = self.first["data"]["provenance"], self.second["data"]["provenance"]
        self.assertEqual(a, b)


if __name__ == "__main__":
    unittest.main()
