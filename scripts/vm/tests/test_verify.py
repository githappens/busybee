"""The platform matrix: one source identity verified on every required platform.

See docs/design/agent-lab.md §Run an issue from reproduction to review, step 5.
The workers are substituted; the scenarios are the repository's own.
"""
from pathlib import Path
import json
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import verify

REPO = Path(__file__).resolve().parents[3]
REVISION = "a" * 40
DIGESTS = {"linux": "1" * 64, "macos": "2" * 64}


class FakeOps:
    """Workers on each platform that run every command and scenario as told."""

    def __init__(self):
        self.repo = REPO
        self.unusable = {}  # platform -> why its baseline cannot be used
        self.heads = {}  # platform -> the revision its checkout reports
        self.versions = {"linux": "bzb 0.1.93", "macos": "bzb 0.1.93"}
        self.exit_codes = {}  # (platform, check) -> exit code
        self.scenario_status = {}  # (platform, scenario, mode) -> status
        self.scenario_digest = {}  # platform -> the busybee a scenario ran
        self.calls = []
        self.runs = {}
        self.export_fails = set()
        self.raise_on = {}  # (platform, check) -> exception exec raises
        self.relinked = set()  # platforms whose `cargo test` rebuilds busybee, as test features can

    def on_disk(self, platform, check):
        """The busybee in build/debug after `check` ran."""
        done = [c[2] for c in self.calls if c[:2] == ("exec", platform)]
        return "7" * 64 if platform in self.relinked and "test" in done else DIGESTS[platform]

    def baseline(self, platform):
        return [self.unusable[platform]] if platform in self.unusable else []

    def create(self, platform, revision, patch):
        self.calls.append(("create", platform))
        run_id = f"r-20261003T00000{len(self.runs)}Z-abcdef"
        self.runs[run_id] = platform
        return {"status": "success", "findings": [], "data": {"run_id": run_id}}

    def checkout(self, run_id):
        return "/checkout"

    def exec(self, run_id, argv, cwd):
        platform = self.runs[run_id]
        name = "version" if argv[-1] == "--version" else next(c for c, a in verify.CHECKS if a == argv[3:])
        self.calls.append(("exec", platform, name))
        if (platform, name) in self.raise_on:
            raise self.raise_on[(platform, name)]
        code = self.exit_codes.get((platform, name), 0)
        status = "success" if code == 0 else "product_failure"
        return {"status": status, "findings": [], "data": {
            "exec": f"{len(self.calls):04d}", "exit_code": code, "elapsed_s": 1.0,
            "provenance": {"source": {"revision": REVISION, "patch_sha256": None},
                           "head": self.heads.get(platform, REVISION), "dirty_files": 0,
                           "binaries": {"build/debug/busybee": self.on_disk(platform, name)}}},
            "stdout": self.versions[platform] + "\n" if name == "version" else ""}

    def scenario(self, run_id, scenario_id, mode):
        platform = self.runs[run_id]
        self.calls.append(("scenario", platform, scenario_id, mode))
        status = self.scenario_status.get((platform, scenario_id, mode), "success")
        return {"status": status, "exec": f"{len(self.calls):04d}",
                "failed": ["task_runs"] if status == "product_failure" else [],
                "busybee_sha256": self.scenario_digest.get(platform, self.on_disk(platform, "scenario"))}

    def destroy(self, run_id):
        self.calls.append(("destroy", self.runs[run_id]))
        return {"status": "success", "findings": []}

    def export(self, run_id):
        if self.runs[run_id] in self.export_fails:
            raise verify.screen.RecordingError("output came before the terminal had a size")
        return f"runs/{run_id}/public"


class VerifyTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.out = Path(tmp.name)
        self.ops = FakeOps()

    def verify(self, platforms=("linux", "macos")):
        return verify.run(self.ops, list(platforms), REVISION, None, self.out)

    def matrix(self, result):
        return json.loads((self.out / result["data"]["matrix"]).read_text())

    def codes(self, result):
        return {f["code"] for f in result["findings"]}

    def test_verification_uses_same_revision_on_both_platforms(self):
        result = self.verify()
        self.assertEqual(result["status"], "success", result["findings"])
        matrix = self.matrix(result)
        self.assertEqual((matrix["schema"], matrix["verdict"]), (verify.SCHEMA, "verified"))
        self.assertEqual(matrix["source"], {"revision": REVISION, "patch_sha256": None})
        for platform in ("linux", "macos"):
            entry = matrix["platforms"][platform]
            self.assertEqual(entry["head"], REVISION)
            self.assertEqual(entry["version"], "bzb 0.1.93")
            self.assertEqual(list(entry["checks"]), [name for name, _ in verify.CHECKS])
            self.assertEqual({c["status"] for c in entry["checks"].values()}, {"success"})
            for scenario in entry["scenarios"].values():
                for run in scenario["modes"].values():
                    self.assertTrue(run["binaries_match"])
        # Platform-specific scenarios declare where they apply; the rest run on both.
        linux, mac = matrix["platforms"]["linux"]["scenarios"], matrix["platforms"]["macos"]["scenarios"]
        self.assertIn("live-monitor", linux)
        self.assertNotIn("live-monitor", mac)
        self.assertEqual(matrix["platforms"]["macos"]["not_applicable"], ["live-monitor"])
        self.assertIn("umask-startup", mac)
        self.assertEqual(list(linux["umask-startup"]["modes"]), ["cold"])
        # Scenarios that cover no issue check the harness itself, not the revision.
        self.assertNotIn("wedged-task", linux)
        # One worker at a time: each platform's is destroyed before the next is created.
        lifecycle = [c[:2] for c in self.ops.calls if c[0] in ("create", "destroy")]
        self.assertEqual(lifecycle, [("create", "linux"), ("destroy", "linux"), ("create", "macos"),
                                     ("destroy", "macos")])

    def test_a_different_head_or_binary_is_not_the_same_revision(self):
        self.ops.heads["macos"] = "b" * 40
        result = self.verify()
        self.assertEqual(self.matrix(result)["verdict"], "incomplete")
        self.assertIn("revision_mismatch", self.codes(result))

        self.ops.heads.clear()
        self.ops.scenario_digest["macos"] = "9" * 64
        result = self.verify()
        self.assertEqual(self.matrix(result)["verdict"], "incomplete")
        self.assertIn("binary_mismatch", self.codes(result))

        self.ops.scenario_digest.clear()
        self.ops.versions["macos"] = "bzb 0.1.92"
        result = self.verify()
        self.assertIn("version_mismatch", self.codes(result))

    def test_scenarios_are_matched_to_the_binaries_they_ran_after_a_rebuild(self):
        # `cargo test` relinks build/debug/busybee; the scenarios run that one.
        self.ops.relinked.update(("linux", "macos"))
        result = self.verify()
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual(self.matrix(result)["platforms"]["linux"]["binaries"]["build/debug/busybee"], "7" * 64)

    def test_missing_platform_is_incomplete(self):
        self.ops.unusable["macos"] = {"code": "baseline_missing", "severity": "error",
                                      "message": "no promoted macos baseline"}
        result = self.verify()
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("platform_missing", self.codes(result))
        matrix = self.matrix(result)
        self.assertEqual(matrix["verdict"], "incomplete")
        self.assertEqual(matrix["platforms"]["macos"]["status"], "unavailable")
        # Nothing is spent on a verification that cannot complete.
        self.assertEqual(self.ops.calls, [])

    def test_a_missing_tool_cannot_complete_verification(self):
        self.ops.exit_codes[("macos", "clippy")] = 127
        result = self.verify()
        matrix = self.matrix(result)
        self.assertEqual(matrix["verdict"], "incomplete")
        self.assertEqual(matrix["platforms"]["macos"]["checks"]["clippy"]["status"], "environment_failure")
        self.assertIn("tool_missing", self.codes(result))

    def test_evidence_that_cannot_be_exported_is_incomplete(self):
        self.ops.export_fails.add("linux")
        result = self.verify()
        matrix = self.matrix(result)
        self.assertEqual(matrix["verdict"], "incomplete")
        self.assertIn("evidence_incomplete", self.codes(result))
        self.assertIn("had a size", "\n".join(f["message"] for f in result["findings"]))
        # The other platform still runs: the matrix shows everything that could be checked.
        self.assertEqual(matrix["platforms"]["macos"]["status"], "ran")

    def test_a_worker_lost_mid_run_still_records_the_matrix_and_is_destroyed(self):
        # e.g. the run deadline expired: the worker refuses further commands.
        self.ops.raise_on[("macos", "test")] = verify.worker.Refused("worker_not_ready", "worker r-x is expired")
        result = self.verify()
        matrix = self.matrix(result)
        self.assertEqual(matrix["verdict"], "incomplete")
        self.assertEqual(matrix["platforms"]["linux"]["status"], "ran")
        self.assertEqual(matrix["platforms"]["macos"]["status"], "interrupted")
        self.assertIn("platform_interrupted", self.codes(result))
        self.assertIn(("destroy", "macos"), self.ops.calls)

    def test_the_public_matrix_carries_no_run_identities_or_addresses(self):
        self.ops.raise_on[("macos", "test")] = verify.guest.GuestError("ssh lab@192.0.2.30: timed out")
        result = self.verify()
        public = (self.out / result["data"]["public"]).read_text()
        self.assertNotIn("192.0.2.30", public)
        for run_id in self.ops.runs:
            self.assertNotIn(run_id, public)
        self.assertIn(REVISION, public)

    def test_known_product_failures_stay_failures(self):
        self.ops.scenario_status[("macos", "umask-startup", "cold")] = "product_failure"
        result = self.verify()
        self.assertEqual(result["status"], "product_failure")
        matrix = self.matrix(result)
        self.assertEqual(matrix["verdict"], "failed")
        run = matrix["platforms"]["macos"]["scenarios"]["umask-startup"]
        self.assertEqual((run["issue"], run["modes"]["cold"]["failed"]), (69, ["task_runs"]))

    def test_a_scenario_that_could_not_run_is_incomplete_not_failed(self):
        self.ops.scenario_status[("linux", "live-monitor", "prepared")] = "environment_failure"
        self.ops.scenario_status[("macos", "umask-startup", "cold")] = "product_failure"
        self.assertEqual(self.matrix(self.verify())["verdict"], "incomplete")


if __name__ == "__main__":
    unittest.main()
