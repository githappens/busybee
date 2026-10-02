"""Scenario acceptance against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and its promoted Linux baseline. Every
worker checks out the umask-startup scenario's recorded affected revision, so
the red #69 run stays red after the fix lands on main. The scenarios come from
this checkout. Every worker a test creates is destroyed through the controller.
CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import tomllib
import unittest

REPO = Path(__file__).resolve().parents[3]
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
STATE = REPO / "build" / "vm"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
BUILD_S = "1800"
AFFECTED = tomllib.loads((REPO / "tests" / "scenarios" / "umask-startup.toml").read_text())["affected_revision"]
# The scenario user: nothing else in a worker runs as it.
NOBODY = "nobody"


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", *args], capture_output=True, text=True)
    return json.loads(out.stdout)


def codes(result):
    return {f["code"] for f in result["findings"]}


def registry():
    return json.loads((STATE / "registry.json").read_text())["vms"]


def workers():
    return {e["run_id"] for e in registry().values() if e["role"] == "worker"}


def host_vms():
    listed = subprocess.run(["prlctl", "list", "--all", "--json"], capture_output=True, text=True, check=True)
    return {"{" + vm["uuid"].strip("{}") + "}": vm["status"] for vm in json.loads(listed.stdout)}


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealScenarioTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(workers(), set(), "a worker is already registered; destroy it first")
        before = host_vms()
        baselines = {e["vm_id"] for e in registry().values() if e["role"] != "worker"}

        def untouched():
            after = host_vms()
            self.assertLessEqual(set(before), set(after))
            self.assertEqual({k: after[k] for k in baselines}, {k: before[k] for k in baselines})
        self.addCleanup(untouched)

    def create(self, revision=AFFECTED):
        result = vmctl("worker", "create", "linux", "--revision", revision)
        self.assertEqual(result["status"], "success", result["findings"])
        run_id = result["data"]["run_id"]
        self.addCleanup(self.destroy_if_owned, run_id)
        self.build(run_id)
        return run_id

    def destroy_if_owned(self, run_id):
        if f"busybee-lab-{run_id}" in registry():
            result = vmctl("worker", "destroy", run_id)
            self.assertEqual(result["status"], "success", result["findings"])

    def exec(self, run_id, *argv, timeout="60"):
        return vmctl("exec", run_id, "--cwd", "/root/busybee", "--timeout", timeout, "--", *argv)

    def build(self, run_id):
        result = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(result["status"], "success", (STATE / result["data"]["stderr"]).read_bytes()[-2000:])
        return result

    def scenario(self, run_id, scenario_id, mode, *extra):
        result = vmctl("scenario", run_id, scenario_id, "--mode", mode, *extra)
        self.assertIn("path", result["data"], result)
        return result, json.loads((STATE / result["data"]["path"]).read_text())

    def assert_no_scenario_processes(self, run_id):
        left = self.exec(run_id, "pgrep", "-u", NOBODY, "-l")
        self.assertEqual(left["data"]["exit_code"], 1, (STATE / left["data"]["stdout"]).read_text())

    def test_cold_fixture_starts_no_daemons(self):
        run_id = self.create()
        result, record = self.scenario(run_id, "umask-startup", "cold")
        fixture = record["result"]["fixture"]
        self.assertEqual(fixture["mode"], "cold")
        self.assertEqual([(c["name"], c["status"]) for c in fixture["checks"]],
                         [("cold_fixture_starts_no_daemons", "passed")])
        # Setup wrote the binaries and routing config, nothing else ...
        self.assertEqual(fixture["written"], ["bin/busybee", "bin/bzbd", "config/busybee.toml", "config/pueue.yml"])
        # ... and the real client startup path created the runtime state.
        modes = record["result"]["diagnostics"]["modes"]
        self.assertEqual(modes["state/bzbd.sock"]["type"], "socket")
        self.assertEqual(modes["pueue"]["type"], "dir")
        self.assertEqual(record["result"]["cleanup"]["remaining"], [])
        self.assert_no_scenario_processes(run_id)

    def test_prepared_fixture_is_explicit(self):
        run_id = self.create()
        result, record = self.scenario(run_id, "umask-startup", "prepared")
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual((record["mode"], record["result"]["fixture"]["mode"]), ("prepared", "prepared"))
        self.assertFalse(record["result"]["satisfies_required_mode"])
        daemons = {d["name"]: d for d in record["result"]["fixture"]["daemons"]}
        self.assertEqual(daemons["pueued"]["umask"], "0022")
        # A prepared pass does not verify the scenario, whose required mode is cold.
        self.assertFalse(result["data"]["coverage"]["verified"])
        self.assertEqual(result["data"]["coverage"]["missing"], ["cold"])
        self.assertIn("required_mode_missing", codes(result))

        cold, _ = self.scenario(run_id, "umask-startup", "cold")
        self.assertEqual(cold["status"], "product_failure", cold["findings"])
        self.assertEqual(cold["data"]["coverage"]["modes"], {"prepared": "success", "cold": "product_failure"})
        self.assertFalse(cold["data"]["coverage"]["verified"])

    def test_missing_tool_is_environment_failure(self):
        run_id = self.create()
        staged = self.exec(run_id, "sh", "-c", "mkdir -p /var/tmp/empty /var/tmp/old && cp build/debug/bzbd "
                           "/var/tmp/old/ && printf '#!/bin/sh\\necho bzb 0.0.1\\n' > /var/tmp/old/busybee && "
                           "chmod +x /var/tmp/old/busybee")
        self.assertEqual(staged["status"], "success", staged["findings"])
        for bin_dir, expected in (("/var/tmp/empty", "tool_missing"), ("/var/tmp/old", "tool_version_mismatch")):
            with self.subTest(bin_dir=bin_dir):
                result, record = self.scenario(run_id, "umask-startup", "cold", "--bin-dir", bin_dir)
                self.assertEqual(result["status"], "environment_failure", result["findings"])
                self.assertIn(expected, codes(result))
                self.assertTrue(all(a["status"] == "not_reached" for a in record["assertions"]))
                self.assertIsNone(record["result"]["fixture"].get("root"))

    def test_umask_repro_repeats_after_reset(self):
        run_id = self.create()
        failures = []
        for attempt in range(2):
            if attempt:
                reset = vmctl("worker", "reset", run_id)
                self.assertEqual(reset["status"], "success", reset["findings"])
                self.build(run_id)
            result, record = self.scenario(run_id, "umask-startup", "cold")
            self.assertEqual(result["status"], "product_failure", result["findings"])
            self.assertEqual(record["source"]["revision"], AFFECTED)
            found = record["result"]
            # Masks and modes: the broker's restrictive umask, and the Pueue directory it produced.
            stopped = {p["comm"]: p for p in found["cleanup"]["stopped"]}
            self.assertEqual(stopped["bzbd"]["umask"], "0177")
            self.assertEqual(found["diagnostics"]["modes"]["pueue"]["mode"], "0600")
            self.assertEqual(found["fixture"]["umask"], "0022")
            self.assertIn("state/bzbd.log", found["diagnostics"]["files"])
            # The binaries that ran are the ones the worker built.
            built = record["provenance"]["binaries"]
            for name in ("busybee", "bzbd"):
                self.assertEqual(found["preflight"]["tools"][name]["sha256"], built[f"build/debug/{name}"])
            self.assertEqual(found["cleanup"]["remaining"], [])
            self.assert_no_scenario_processes(run_id)
            failures.append(record["failed"])
        self.assertIn("task_runs", failures[0])
        self.assertEqual(failures[0], failures[1])

        collected = vmctl("collect", run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        manifest = json.loads((STATE / collected["data"]["manifest"]).read_text())
        self.assertEqual([r["status"] for r in manifest["scenarios"]["results"]], ["product_failure"] * 2)
        exported = vmctl("export", run_id)
        self.assertEqual(exported["status"], "success", exported["findings"])
        public = STATE / exported["data"]["path"]
        records = sorted(public.glob("scenarios/*/result.json"))
        self.assertEqual(len(records), 2)
        text = records[0].read_text()
        self.assertNotIn(run_id, text)
        self.assertIn('"0177"', text)

    def test_scenario_timeout_is_not_success(self):
        run_id = self.create()
        result, record = self.scenario(run_id, "wedged-task", "prepared")
        self.assertEqual(result["status"], "timeout", result["findings"])
        self.assertIn("scenario_timeout", codes(result))
        found = record["result"]
        self.assertLess(found["elapsed_s"], found["deadline_s"] + 30)
        self.assertIn("logs/task.stderr", found["diagnostics"]["files"])
        self.assertIn("state/bzbd.log", found["diagnostics"]["files"])
        self.assertEqual(found["cleanup"]["remaining"], [])
        self.assert_no_scenario_processes(run_id)
        collected = vmctl("collect", run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        self.assertIn(result["data"]["path"], collected["data"]["artifacts"])


if __name__ == "__main__":
    unittest.main()
