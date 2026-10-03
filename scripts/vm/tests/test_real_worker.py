"""Worker acceptance against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and its promoted Linux baseline. Every
worker a test creates is destroyed through the controller. The retained-baseline
test promotes a freshly built candidate, so it leaves a new baseline in place of
the old one. CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import re
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
BUILD_S = "3600"


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", "--root", str(ROOT), *args], capture_output=True, text=True)
    return json.loads(out.stdout)


def codes(result):
    return {f["code"] for f in result["findings"]}


def head():
    return subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True,
                          check=True).stdout.strip()


def registry():
    return json.loads((STATE / "registry.json").read_text())["vms"]


def host_vms():
    listed = subprocess.run(["prlctl", "list", "--all", "--json"], capture_output=True, text=True, check=True)
    return {"{" + vm["uuid"].strip("{}") + "}": vm["status"] for vm in json.loads(listed.stdout)}


def manifest():
    return json.loads((STATE / "templates" / "linux" / "manifest.json").read_text())


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealWorkerTests(unittest.TestCase):
    def create(self):
        result = vmctl("worker", "create", "linux", "--revision", head())
        self.assertEqual(result["status"], "success", result["findings"])
        run_id = result["data"]["run_id"]
        self.addCleanup(self.destroy_if_owned, run_id)
        return run_id

    def destroy_if_owned(self, run_id):
        if f"busybee-lab-{run_id}" in registry():
            result = vmctl("worker", "destroy", run_id)
            self.assertEqual(result["status"], "success", result["findings"])

    def exec(self, run_id, *argv, cwd="/root/busybee", env=(), timeout=None):
        args = ["exec", run_id, "--cwd", cwd, *(f"--env={e}" for e in env)]
        args += ["--timeout", str(timeout)] if timeout else []
        return vmctl(*args, "--", *argv)

    def output(self, result, stream="stdout"):
        return (STATE / result["data"][stream]).read_bytes()

    def build(self, run_id):
        result = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(result["status"], "success", self.output(result, "stderr")[-2000:])
        return result

    def test_worker_exec_round_trips_argv_and_exit(self):
        run_id = self.create()
        words = ["two words", "$HOME", "a;b|c&d", "*", "'quoted'", "back\\slash", ""]
        result = self.exec(run_id, "sh", "-c", 'printf "[%s]" "$@"; echo; echo "$GREETING"; pwd; echo oops >&2; '
                           "exit 3", "sh", *words, cwd="/tmp", env=["GREETING=hi there; $USER"])
        self.assertEqual(result["status"], "product_failure", result["findings"])
        self.assertEqual(result["data"]["exit_code"], 3)
        expected = "".join(f"[{w}]" for w in words) + "\nhi there; $USER\n/tmp\n"
        self.assertEqual(self.output(result).decode(), expected)
        self.assertEqual(self.output(result, "stderr"), b"oops\n")
        self.assertEqual(result["data"]["provenance"]["head"], head())

        # A command's own 255 is its exit status, not a lost connection.
        result = self.exec(run_id, "sh", "-c", "exit 255")
        self.assertEqual((result["status"], result["data"]["exit_code"]), ("product_failure", 255))

        result = self.exec(run_id, "sleep", "60", timeout=3)
        self.assertEqual(result["status"], "timeout", result["findings"])
        self.assertLess(result["data"]["elapsed_s"], 30)
        # The timed-out process is gone.
        left = self.exec(run_id, "pgrep", "-x", "sleep")
        self.assertEqual(left["data"]["exit_code"], 1, self.output(left))

        shot = vmctl("console", "capture", run_id)
        self.assertEqual(shot["status"], "success", shot["findings"])
        self.assertTrue((STATE / shot["data"]["path"]).read_bytes().startswith(b"\x89PNG"))

    def test_worker_builds_recorded_revision(self):
        run_id = self.create()
        record = json.loads((STATE / "runs" / run_id / "worker.json").read_text())
        self.assertEqual(record["source"]["revision"], head())
        built = self.build(run_id)
        binaries = built["data"]["provenance"]["binaries"]
        for name in ("busybee", "bzbd"):
            self.assertRegex(binaries.get(f"build/debug/{name}", ""), r"^[0-9a-f]{64}$")
        self.assertEqual(built["data"]["provenance"]["head"], head())
        self.assertEqual(built["data"]["provenance"]["dirty_files"], 0)

        # The binary is versioned from the transferred history and tags.
        describe = subprocess.run(["git", "-C", str(REPO), "describe", "--tags", "--long", "--match",
                                   "[0-9]*.[0-9]*.[0-9]*", head()], capture_output=True, text=True,
                                  check=True).stdout.strip()
        major, minor, patch, ahead = re.match(r"^(\d+)\.(\d+)\.(\d+)-(\d+)-g", describe).groups()
        version = self.exec(run_id, "build/debug/busybee", "--version")
        self.assertEqual(version["status"], "success", version["findings"])
        self.assertIn(f"{major}.{minor}.{int(patch) + int(ahead)}", self.output(version).decode())

    def test_reset_restores_named_baseline(self):
        run_id = self.create()
        vm = f"busybee-lab-{run_id}"
        record = json.loads((STATE / "runs" / run_id / "worker.json").read_text())
        changed = self.exec(run_id, "sh", "-c", "touch /root/state-marker && echo x >> README.md")
        self.assertEqual(changed["status"], "success", changed["findings"])
        sys.path.insert(0, str(REPO / "scripts" / "vm"))
        import parallels
        import registry as owned
        extra = parallels.Parallels("prlctl", "prlsrvctl", owned=owned.Registry(STATE)).snapshot(vm, "agent snapshot")

        result = vmctl("worker", "reset", run_id)
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual(result["data"]["restored_snapshot_id"], record["reset_snapshot_id"])
        self.assertNotEqual(result["data"]["restored_snapshot_id"], extra)
        after = self.exec(run_id, "sh", "-c", "test ! -e /root/state-marker && git diff --quiet && git rev-parse HEAD")
        self.assertEqual(after["status"], "success", self.output(after, "stderr"))
        self.assertEqual(self.output(after).decode().strip(), head())

    def test_lifecycle_rejects_unowned_targets(self):
        run_id = self.create()
        started = self.exec(run_id, "sh", "-c", "sleep 600 >/dev/null 2>&1 & echo $!")
        pid = self.output(started).decode().strip()
        self.assertEqual(vmctl("signal", run_id, "TERM", pid)["status"], "success")
        gone = vmctl("inspect", run_id)
        self.assertNotIn(int(pid), [p["pid"] for p in gone["data"]["processes"]])

        owned = {e["vm_id"] for e in registry().values()}
        before_registry = registry()
        before_vms = {k: v for k, v in host_vms().items() if k in owned}
        baseline = manifest()
        targets = [baseline["candidate"], "../registry", f"busybee-lab-{run_id}", "r-20200101T000000Z-000000"]
        for target in targets:
            for argv in (["worker", "reset", target], ["worker", "destroy", target], ["signal", target, "TERM", "42"],
                         ["inspect", target], ["collect", target], ["console", "capture", target]):
                with self.subTest(argv=argv):
                    result = vmctl(*argv)
                    self.assertEqual(result["status"], "environment_failure")
                    self.assertTrue(codes(result) & {"target_invalid", "target_not_owned"}, result)
        for pid in ("1", "0", "-1", "1;reboot"):
            with self.subTest(pid=pid):
                self.assertIn("pid_invalid", codes(vmctl("signal", run_id, "KILL", pid)))
        self.assertEqual(registry(), before_registry)
        self.assertEqual({k: v for k, v in host_vms().items() if k in owned}, before_vms)

    def test_failed_export_retains_worker(self):
        run_id = self.create()
        vm = f"busybee-lab-{run_id}"
        self.build(run_id)
        collect = STATE / "runs" / run_id / "collect"
        collect.mkdir()
        collect.chmod(0o555)
        try:
            result = vmctl("worker", "destroy", run_id)
        finally:
            collect.chmod(0o755)
        self.assertEqual(result["status"], "incomplete_collection", result["findings"])
        self.assertTrue(result["data"]["missing"])
        self.assertIn(vm, registry())
        record = json.loads((STATE / "runs" / run_id / "worker.json").read_text())
        self.assertEqual(host_vms()[record["vm_id"]], "stopped")

        # Collection works again: the retained worker is collected and removed.
        result = vmctl("worker", "destroy", run_id)
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertNotIn(vm, registry())
        self.assertNotIn(record["vm_id"], host_vms())

        # A second full create/build/export/dispose cycle.
        second = self.create()
        self.build(second)
        exported = vmctl("collect", second)
        self.assertEqual(exported["status"], "success", exported["findings"])
        self.assertEqual(vmctl("worker", "destroy", second)["status"], "success")

    def test_retained_baseline_pruned_only_when_unreferenced(self):
        old = manifest()
        run_id = self.create()
        built = vmctl("template", "build", "linux", "--arch", old["arch"])
        self.assertEqual(built["status"], "success", built["findings"])
        candidate = built["data"]["candidate"]
        validated = vmctl("template", "validate", "linux", "--candidate", candidate)
        self.assertEqual(validated["status"], "success", validated["findings"])
        self.assertEqual(vmctl("template", "promote", "linux", "--candidate", candidate)["status"], "success")

        kept = vmctl("template", "prune", "linux")
        self.assertEqual(kept["status"], "success", kept["findings"])
        self.assertIn("retained_in_use", codes(kept))
        self.assertIn(old["candidate"], kept["data"]["kept"])
        self.assertIn(old["vm_id"], host_vms())

        self.assertEqual(vmctl("worker", "destroy", run_id)["status"], "success")
        pruned = vmctl("template", "prune", "linux")
        self.assertEqual(pruned["status"], "success", pruned["findings"])
        self.assertIn(old["candidate"], pruned["data"]["pruned"])
        self.assertNotIn(old["vm_id"], host_vms())
        retained = json.loads((STATE / "templates" / "linux" / "retained.json").read_text())
        self.assertNotIn(old["vm_id"], [b["vm_id"] for b in retained])
        self.assertEqual(manifest()["candidate"], candidate)


if __name__ == "__main__":
    unittest.main()
