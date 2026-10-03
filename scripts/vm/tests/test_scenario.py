"""The scenario adapter against a simulated worker: staging, bounds, result
interpretation, coverage of required fixture modes, and the evidence trail."""
from pathlib import Path
import io
import json
import shutil
import sys
import tarfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import evidence
import scenario
import worker
from test_worker import CONFIG, REPO, Lab, codes

ASSERTIONS = ["task_runs", "task_directory_traversable", "task_executable_output_runs", "lease_released"]


def runner_result(status, mode="cold", scenario_id="umask-startup", failed=(), exit_code=None, **extra):
    assertions = [{"name": n, "status": "failed" if n in failed else "passed", "detail": "d"} for n in ASSERTIONS]
    if status in ("environment_failure", "timeout"):
        assertions = [{"name": n, "status": "not_reached", "detail": "d"} for n in ASSERTIONS]
    result = {"schema": "busybee.scenario.result/v1", "scenario": scenario_id, "issue": 69, "mode": mode,
              "required_modes": ["cold"], "satisfies_required_mode": mode == "cold", "status": status,
              "summary": "s", "findings": [{"code": "assertion_failed", "message": "m"}] if failed else [],
              "assertions": assertions, "preflight": {"tools": {"busybee": {"sha256": "b" * 64}}},
              "fixture": {"mode": mode, "checks": [], "daemons": []},
              "observations": {"task": {"umask": "0177"}},
              "diagnostics": {"files": {"state/bzbd.log": "ERROR cannot submit"}, "modes": {"pueue": {"mode": "0600"}}},
              "cleanup": {"stopped": [], "remaining": [], "removed": True}, **extra}
    return (json.dumps(result) + "\n").encode()


class ScenarioTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        # The controller's scenarios, as its checkout holds them.
        shutil.copytree(REPO / "tests" / "scenarios", self.lab.repo / "tests" / "scenarios",
                        ignore=shutil.ignore_patterns("tests", "__pycache__"))
        meta = (self.lab.repo / "tests" / "scenarios" / "umask-startup.toml").read_text()
        # Inside the simulated config's 120s scenario window.
        (self.lab.repo / "tests" / "scenarios" / "umask-startup.toml").write_text(
            meta.replace("deadline_s = 90", "deadline_s = 30"))
        self.run_id = self.lab.create()

    def scenario(self, stdout=b"", exit_code=0, mode="cold", scenario_id="umask-startup"):
        self.lab.guest.stdout, self.lab.guest.exit_code = stdout, exit_code
        return scenario.run(self.lab.workers(), self.run_id, scenario_id, mode)

    def record(self, result):
        return json.loads((self.lab.state / result["data"]["path"]).read_text())

    def test_runner_is_staged_from_the_controller_and_run_bounded(self):
        self.scenario(runner_result("success", failed=()), exit_code=0)
        staged = [(c, s) for c, s in self.lab.guest.commands if s and "tar -xf -" in c]
        self.assertEqual(len(staged), 1)
        names = tarfile.open(fileobj=io.BytesIO(staged[0][1])).getnames()
        self.assertIn("runner.py", names)
        self.assertIn("umask-startup.toml", names)
        self.assertNotIn("tests", {n.split("/")[0] for n in names})
        command = json.loads((worker.run_dir(self.lab.state, self.run_id) / "exec" / "0001" / "command.json")
                             .read_text())
        digest = scenario.archive(self.lab.repo)[1]
        self.assertEqual(command["argv"][:5], ["nix", "develop", "-c", "python3",
                                               f"{scenario.STAGE}/{digest[:16]}/runner.py"])
        self.assertIn("--cleanup-s", command["argv"])
        self.assertEqual(command["timeout_s"], 30 + scenario.CLEANUP_S + scenario.START_S)
        self.assertEqual(command["cwd"], worker.CHECKOUT)

    def test_product_failure_is_kept_with_its_evidence(self):
        result = self.scenario(runner_result("product_failure", failed=("task_runs",)), exit_code=1)
        self.assertEqual(result["status"], "product_failure", result["findings"])
        record = self.record(result)
        self.assertEqual(record["schema"], scenario.RECORD_SCHEMA)
        self.assertEqual((record["scenario"], record["mode"], record["exec"]), ("umask-startup", "cold", "0001"))
        self.assertEqual(record["failed"], ["task_runs"])
        self.assertEqual(record["source"]["revision"], self.lab.revision)
        self.assertRegex(record["runner"]["sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(record["result"]["observations"]["task"]["umask"], "0177")

    def test_missing_tool_is_environment_failure(self):
        result = self.scenario(runner_result("environment_failure", findings=[
            {"code": "tool_missing", "message": "pueued (>=4.0) is not an executable on PATH"}]), exit_code=2)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("tool_missing", codes(result))

    def test_output_that_is_no_result_is_never_a_product_failure(self):
        for stdout, exit_code in ((b"", 1), (b"error: flake has no devShell\n", 1), (b"{}", 0),
                                  (runner_result("success", scenario_id="wedged-task"), 0)):
            with self.subTest(stdout=stdout[:30]):
                result = self.scenario(stdout, exit_code)
                self.assertEqual(result["status"], "environment_failure")
                self.assertIn("scenario_result_invalid", codes(result))

    def test_a_success_claim_must_hold(self):
        claimed = json.loads(runner_result("success"))
        claimed["assertions"][1]["status"] = "not_reached"
        result = self.scenario((json.dumps(claimed) + "\n").encode(), 0)
        self.assertEqual(result["status"], "environment_failure")
        # A status its exit code contradicts is not believed either.
        result = self.scenario(runner_result("product_failure", failed=("task_runs",)), 0)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("scenario_result_invalid", codes(result))

    def test_scenario_timeout_is_not_success(self):
        # The runner's own bound failed: the controller's exec deadline ends it.
        self.lab.guest.hang = True
        self.lab.guest.chunks = [b'{"schema": "busybee.scenario.result/v1", "partial']
        result = self.scenario()
        self.assertEqual(result["status"], "timeout", result["findings"])
        record = self.record(result)
        self.assertEqual(record["exec_status"], "timeout")
        self.assertIsNone(record["result"])
        self.assertTrue(all(a["status"] == "not_reached" for a in record["assertions"]))
        # The partial output is kept as evidence and collected.
        collected = self.lab.workers().collect(self.run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        artifacts = collected["data"]["artifacts"]
        self.assertIn(result["data"]["path"], artifacts)
        self.assertIn(f"runs/{self.run_id}/exec/0001/stdout", artifacts)

        # The runner's own deadline: a timeout result from it is a timeout too.
        self.lab.guest.hang, self.lab.guest.chunks = False, None
        result = self.scenario(runner_result("timeout", findings=[{"code": "scenario_timeout", "message": "m"}]), 3)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("scenario_timeout", codes(result))

    def test_a_refused_exec_writes_no_record(self):
        # The run deadline has passed: exec refuses before it queues anything.
        self.lab.now += CONFIG["deadlines"]["run"] + 1
        first = self.scenario(runner_result("success"), 0)
        second = self.scenario(runner_result("success"), 0)
        for result in (first, second):
            self.assertEqual(result["status"], "timeout", result["findings"])
            self.assertIn("run_deadline_passed", codes(result))
            self.assertIsNone(result["data"]["exec"])
            self.assertIsNone(result["data"]["path"])
        self.assertFalse((worker.run_dir(self.lab.state, self.run_id) / "scenarios").exists())

    def test_prepared_fixture_is_explicit(self):
        result = self.scenario(runner_result("success", mode="prepared"), 0, mode="prepared")
        self.assertEqual(result["status"], "success")
        self.assertEqual(result["data"]["mode"], "prepared")
        coverage = result["data"]["coverage"]
        self.assertEqual(coverage["modes"], {"prepared": "success"})
        self.assertFalse(coverage["verified"])
        self.assertEqual(coverage["missing"], ["cold"])
        self.assertIn("required_mode_missing", codes(result))

        failed = self.scenario(runner_result("product_failure", failed=("task_runs",)), 1)
        coverage = failed["data"]["coverage"]
        self.assertEqual(coverage["modes"], {"prepared": "success", "cold": "product_failure"})
        self.assertFalse(coverage["verified"])
        self.assertEqual(coverage["failing"], ["cold"])

        passed = self.scenario(runner_result("success"), 0)
        self.assertTrue(passed["data"]["coverage"]["verified"])

        manifest = json.loads((self.lab.state / self.lab.workers().collect(self.run_id)["data"]["manifest"])
                              .read_text())
        self.assertEqual(contracts.evidence_errors(manifest), [])
        self.assertEqual([(r["mode"], r["status"]) for r in manifest["scenarios"]["results"]],
                         [("prepared", "success"), ("cold", "product_failure"), ("cold", "success")])
        self.assertTrue(manifest["scenarios"]["coverage"]["umask-startup"]["verified"])

    def test_refusals(self):
        w = self.lab.workers()
        for args, code in ((("nope", "cold"), "scenario_invalid"), (("../x", "cold"), "scenario_invalid"),
                           (("wedged-task", "cold"), "mode_invalid")):
            with self.subTest(args=args):
                with self.assertRaises(worker.Refused) as raised:
                    scenario.run(w, self.run_id, *args)
                self.assertEqual(raised.exception.code, code)
        meta = (self.lab.repo / "tests" / "scenarios" / "wedged-task.toml")
        meta.write_text(meta.read_text().replace("deadline_s = 20", "deadline_s = 3600"))
        with self.assertRaises(worker.Refused) as raised:
            scenario.run(w, self.run_id, "wedged-task", "prepared")
        self.assertEqual(raised.exception.code, "timeout_invalid")
        self.assertFalse((worker.run_dir(self.lab.state, self.run_id) / "exec").exists())

    def test_public_export_keeps_scenario_results(self):
        self.scenario(runner_result("product_failure", failed=("task_runs",)), 1)
        self.lab.workers().collect(self.run_id)
        out = self.lab.workers().export(self.run_id)
        public = self.lab.state / out["data"]["path"]
        record = json.loads((public / "scenarios" / "0001" / "result.json").read_text())
        self.assertEqual(record["failed"], ["task_runs"])
        self.assertNotIn(self.run_id, (public / "scenarios" / "0001" / "result.json").read_text())


class ScenarioTerminalTests(unittest.TestCase):
    """A scenario that drove terminals: their recordings come back to the host."""

    def setUp(self):
        from test_terminal_ops import TerminalGuest
        self.lab = Lab(self)
        shutil.copytree(REPO / "tests" / "scenarios", self.lab.repo / "tests" / "scenarios",
                        ignore=shutil.ignore_patterns("tests", "__pycache__"))
        meta = self.lab.repo / "tests" / "scenarios" / "umask-startup.toml"
        meta.write_text(meta.read_text().replace("deadline_s = 90", "deadline_s = 30"))
        self.lab.guest = TerminalGuest()
        self.lab.guest.hang = False
        self.run_id = self.lab.create()

    def scenario(self, status, exit_code, terminals):
        self.lab.guest.stdout = runner_result(status, failed=("task_runs",) if status == "product_failure" else (),
                                              terminals=terminals)
        self.lab.guest.exit_code = exit_code
        return scenario.run(self.lab.workers(), self.run_id, "umask-startup", "cold")

    def test_terminals_are_fetched_rendered_and_removed_from_the_guest(self):
        result = self.scenario("success", 0, {"dir": "/var/tmp/bzt-abc123", "names": ["monitor"], "closed": {}})
        self.assertEqual(result["status"], "success", result["findings"])
        record = json.loads((self.lab.state / result["data"]["path"]).read_text())
        shown = record["terminals"]["0001-monitor"]
        self.assertEqual([c["agrees"] for c in shown["captures"]], [True, True, True, None])
        hdir = worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001-monitor"
        self.assertTrue((hdir / "captures" / "0003.png").read_bytes().startswith(b"\x89PNG"))
        commands = [c for c, _ in self.lab.guest.commands]
        self.assertIn("cd /var/tmp/bzt-abc123/monitor && tar -cf - state.json recording captures", commands)
        self.assertIn("rm -rf /var/tmp/bzt-abc123", commands)

    def test_terminal_evidence_that_cannot_be_fetched_keeps_a_pass_from_passing(self):
        for terminals in ({"dir": "/etc", "names": ["monitor"]}, {"dir": "/var/tmp/bzt-abc123", "names": ["../x"]}):
            with self.subTest(terminals=terminals):
                result = self.scenario("success", 0, terminals)
                self.assertEqual(result["status"], "environment_failure")
                self.assertIn("terminal_evidence_missing", codes(result))
        # A product failure stays one; the missing evidence is said alongside it.
        self.lab.guest.files = self.lab.state  # a directory without a recording
        result = self.scenario("product_failure", 1, {"dir": "/var/tmp/bzt-abc123", "names": ["monitor"]})
        self.assertEqual(result["status"], "product_failure")
        self.assertIn("terminal_evidence_missing", codes(result))
        # The guest keeps the only copy of what could not be fetched.
        self.assertNotIn("rm -rf /var/tmp/bzt-abc123", [c for c, _ in self.lab.guest.commands])


class CoverageTests(unittest.TestCase):
    def test_latest_result_per_mode_decides(self):
        records = [{"scenario": "s", "mode": "cold", "status": "success", "required_modes": ["cold"]},
                   {"scenario": "s", "mode": "cold", "status": "product_failure", "required_modes": ["cold"]}]
        self.assertFalse(evidence.coverage(records)["s"]["verified"])
        self.assertTrue(evidence.coverage(records[1:] + records[:1])["s"]["verified"])
        self.assertEqual(evidence.coverage([]), {})

    def test_coverage_counts_only_runs_of_the_current_head(self):
        def run(mode, status, head):
            return {"scenario": "s", "mode": mode, "status": status, "required_modes": ["cold", "prepared"],
                    "provenance": {"head": head}}
        # A cold pass on an old head cannot combine with a prepared pass on a new one.
        records = [run("cold", "success", "old"), run("prepared", "success", "new")]
        covered = evidence.coverage(records, head="new")["s"]
        self.assertEqual((covered["head"], covered["modes"], covered["missing"]), ("new", {"prepared": "success"},
                                                                                   ["cold"]))
        self.assertFalse(covered["verified"])
        self.assertEqual(covered["other_head_runs"], 1)
        # Without an explicit head, the latest run's head is the current one.
        self.assertEqual(evidence.coverage(records)["s"]["head"], "new")
        self.assertTrue(evidence.coverage(records + [run("cold", "success", "new")])["s"]["verified"])
        # A scenario that ran only on another head is reported, never verified.
        stale = evidence.coverage([run("cold", "success", "old")], head="new")["s"]
        self.assertEqual((stale["modes"], stale["verified"], stale["other_head_runs"]), ({}, False, 1))


if __name__ == "__main__":
    unittest.main()
