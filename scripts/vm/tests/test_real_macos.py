"""macOS verification against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml with its promoted Linux and macOS
baselines (see infra/vm/local.example.toml). Each named test of #78 runs here
at this checkout's HEAD. Every lease and worker a test takes is released or
destroyed through the controller; the macOS slot guest itself is kept, as in
routine use. CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile
import tomllib
import unittest

REPO = Path(__file__).resolve().parents[3]
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
STATE = REPO / "build" / "vm"
CONFIG = STATE / "local.toml"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
BUILD_S = "1800"
# What a scenario leaves behind, planted where each kind of state lives.
MARKERS = ("/tmp/bzlab-marker", "~/bzlab-marker", "/usr/local/bzlab-marker")


def vmctl(*args, config=None):
    extra = ["--config", str(config)] if config else []
    out = subprocess.run([sys.executable, str(VMCTL), "--json", *args, *extra], capture_output=True, text=True)
    return json.loads(out.stdout)


def codes(result):
    return {f["code"] for f in result["findings"]}


def head():
    return subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True,
                          check=True).stdout.strip()


def registry():
    return json.loads((STATE / "registry.json").read_text())["vms"]


def active():
    """Runs holding a worker or the macOS slot."""
    return {e["run_id"] if e["role"] == "worker" else e["holder"] for e in registry().values()
            if e["role"] == "worker" or e["role"] == "slot" and e.get("holder")}


def checkout():
    user = tomllib.loads(CONFIG.read_text())["templates"]["macos"]["user"]
    return f"/Users/{user}/busybee"


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealMacTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(active(), set(), "a worker or lease is already active; destroy it first")

    def lease(self):
        result = vmctl("worker", "create", "macos", "--revision", head())
        self.assertEqual(result["status"], "success", result["findings"])
        run_id = result["data"]["run_id"]
        self.addCleanup(self.release, run_id)
        return run_id

    def release(self, run_id):
        if run_id in active():
            result = vmctl("worker", "destroy", run_id)
            self.assertEqual(result["status"], "success", result["findings"])

    def exec(self, run_id, *argv, timeout=None):
        args = ["exec", run_id, "--cwd", checkout()] + (["--timeout", timeout] if timeout else [])
        result = vmctl(*args, "--", *argv)
        result["stdout"] = (STATE / result["data"]["stdout"]).read_text() if "stdout" in result["data"] else ""
        return result

    def test_macos_template_validates_unattended_capabilities(self):
        manifest = json.loads((STATE / "templates" / "macos" / "manifest.json").read_text())
        self.assertNotIn("linked", manifest["clone_modes"])
        result = vmctl("template", "validate", "macos", "--candidate", manifest["candidate"])
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual({c["status"] for c in result["data"]["checks"].values()}, {"pass"})
        # The routine cycle, twice, with nobody at the guest: lease (restore, boot,
        # transfer), command access, a PTY, the console, a reset and a release.
        for _ in range(2):
            run_id = self.lease()
            done = self.exec(run_id, "git", "rev-parse", "HEAD")
            self.assertEqual((done["status"], done["stdout"].strip()), ("success", head()))
            pty = self.exec(run_id, "script", "-q", "/dev/null", "tty")
            self.assertEqual(pty["status"], "success", pty["findings"])
            self.assertRegex(pty["stdout"], r"/dev/ttys\d+")
            shot = vmctl("console", "capture", run_id)
            self.assertEqual(shot["status"], "success", shot["findings"])
            self.assertTrue((STATE / shot["data"]["path"]).read_bytes().startswith(b"\x89PNG"))
            reset = vmctl("worker", "reset", run_id)
            self.assertEqual(reset["status"], "success", reset["findings"])
            self.release(run_id)

    def test_macos_reset_removes_scenario_state(self):
        run_id = self.lease()
        build = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(build["status"], "success", build["findings"])
        first = vmctl("scenario", run_id, "umask-startup", "--mode", "prepared")
        self.assertIn(first["status"], ("success", "product_failure"), first["findings"])
        plant = self.exec(run_id, "sh", "-c", "touch /tmp/bzlab-marker ~/bzlab-marker && "
                          "sudo -n touch /usr/local/bzlab-marker && echo leaked >> README.md && "
                          "(nohup sleep 86400 >/dev/null 2>&1 &) && sleep 1 && pgrep -x sleep")
        self.assertEqual(plant["status"], "success", plant["findings"])

        def clean(run):
            state = self.exec(run, "sh", "-c", f"for m in {' '.join(MARKERS)}; do [ -e $m ] && echo $m; done; "
                              "pgrep -x 'sleep|pueued|bzbd'; git status --porcelain; true")
            self.assertEqual(state["status"], "success", state["findings"])
            self.assertEqual(state["stdout"], "", "state survived the reset")

        reset = vmctl("worker", "reset", run_id)
        self.assertEqual(reset["status"], "success", reset["findings"])
        clean(run_id)
        build = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(build["status"], "success", build["findings"])
        again = vmctl("scenario", run_id, "umask-startup", "--mode", "prepared")
        self.assertIn(again["status"], ("success", "product_failure"), again["findings"])
        self.release(run_id)
        # The next holder's grant restores the guest too.
        clean(self.lease())

    def test_verification_uses_same_revision_on_both_platforms(self):
        result = vmctl("verify", "--revision", head())
        matrix = json.loads((STATE / result["data"]["matrix"]).read_text())
        (STATE / "evidence").mkdir(exist_ok=True)
        (STATE / "evidence" / f"matrix-{head()[:12]}.json").write_text(json.dumps(matrix, indent=2) + "\n")
        # Known product failures may keep it `failed`; it must never be incomplete.
        self.assertIn(matrix["verdict"], ("verified", "failed"), result["findings"])
        linux, mac = matrix["platforms"]["linux"], matrix["platforms"]["macos"]
        self.assertEqual((linux["head"], mac["head"]), (head(), head()))
        self.assertEqual(linux["version"], mac["version"])
        self.assertIn("live-monitor", linux["scenarios"])
        self.assertIn("live-monitor", mac["not_applicable"])
        for entry in (linux, mac):
            self.assertEqual(entry["collected"], "success")
            for run in entry["scenarios"].values():
                self.assertTrue(all(m["binaries_match"] for m in run["modes"].values()))

    def test_missing_platform_is_incomplete(self):
        before = registry()
        config = tomllib.loads(CONFIG.read_text())
        text = CONFIG.read_text()
        cut = text[:text.index("[templates.macos]")]
        self.assertNotIn("[templates.macos]", cut)
        self.assertIn("macos", config["templates"])
        with tempfile.NamedTemporaryFile("w", suffix=".toml", dir=STATE) as partial:
            partial.write(cut)
            partial.flush()
            result = vmctl("verify", "--revision", head(), config=partial.name)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("platform_missing", codes(result))
        self.assertEqual(result["data"]["verdict"], "incomplete")
        self.assertEqual(registry(), before)


if __name__ == "__main__":
    unittest.main()
