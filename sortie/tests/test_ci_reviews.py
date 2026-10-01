import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from types import SimpleNamespace
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import reviews

spec = importlib.util.spec_from_file_location("ci_reviews", Path(__file__).resolve().parents[1] / "ci_reviews.py")
ci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci)


class CIReviewTests(unittest.TestCase):
    def test_selection_is_repository_wide_but_excludes_forks_drafts_and_closed_prs(self):
        candidates = [{"number": i, "state": "open", "draft": False,
                       "head": {"ref": branch, "repo": {"full_name": "example/tool"}},
                       "base": {"repo": {"full_name": "example/tool"}}}
                      for i, branch in enumerate(("sortie-lab/72", "sortie/12", "feature/fix", "draft", "closed", "fork"), 1)]
        candidates[3]["draft"] = True
        candidates[4]["state"] = "closed"
        candidates[5]["head"]["repo"]["full_name"] = "contributor/tool"
        with patch.object(ci, "trigger_allowed", return_value=True), \
                patch.object(ci, "api", return_value=candidates), patch.object(ci, "output") as output:
            ci.select(SimpleNamespace(repo="example/tool", pr=None))
            output.assert_called_once_with("prs", "[1, 2, 3]")

    def test_rejected_event_actors_cannot_create_cached_failures(self):
        for kind, permission, allowed in (("Bot", "write", False), ("User", "read", False),
                                           ("User", "none", False), ("User", "write", True),
                                           ("User", "admin", True), ("User", "maintain", False)):
            with patch.dict(ci.os.environ, {"GITHUB_ACTOR": "event-actor"}), \
                    patch.object(ci, "api", side_effect=[{"type": kind}, {"permission": permission}]):
                self.assertEqual(ci.trigger_allowed("example/tool"), allowed)
        with patch.object(ci, "trigger_allowed", return_value=False), \
                patch.object(ci, "output") as output, patch.object(ci, "read_pr") as read:
            from types import SimpleNamespace
            ci.select(SimpleNamespace(repo="example/tool", pr=123))
            output.assert_called_once_with("prs", "[]")
            read.assert_not_called()

    def test_only_default_branch_runs_of_the_trusted_workflow_supply_cache(self):
        run = {"path": ci.WORKFLOW, "head_branch": "main", "event": "schedule"}
        self.assertTrue(ci.trusted_run(run, "main"))
        for field, value in (("path", ".github/workflows/ci.yml"), ("head_branch", "feature"),
                             ("event", "pull_request"), ("event", "pull_request_target")):
            self.assertFalse(ci.trusted_run(dict(run, **{field: value}), "main"))

    def test_artifact_is_read_without_extracting_paths(self):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as archive:
            archive.writestr("record.json", '{"version":2}')
            archive.writestr("../../escape", "do not extract")
        self.assertEqual(ci.read_record(buf.getvalue()), {"version": 2})

    def test_malformed_newest_trusted_artifact_does_not_fall_back(self):
        artifacts = [{"id": i, "expired": False, "workflow_run": {"id": i}} for i in (2, 1)]
        with patch.object(ci, "paged_objects", return_value=artifacts), \
                patch.object(ci, "api", return_value={"path": ci.WORKFLOW, "head_branch": "main", "event": "schedule"}), \
                patch.object(ci, "download", side_effect=[b"invalid zip", b"old green"] ) as download:
            with self.assertRaises(zipfile.BadZipFile):
                ci.cached_record("example/tool", "name", "main")
            self.assertEqual(download.call_count, 1)

    def test_candidate_workflow_artifact_is_not_review_authority(self):
        artifacts = [{"id": 1, "expired": False, "workflow_run": {"id": 1}}]
        with patch.object(ci, "paged_objects", return_value=artifacts), \
                patch.object(ci, "api", return_value={"path": ci.WORKFLOW, "head_branch": "feature", "event": "workflow_dispatch"}), \
                patch.object(ci, "download") as download:
            self.assertIsNone(ci.cached_record("example/tool", "name", "main"))
            download.assert_not_called()

    def test_action_failure_is_explicit_uncertainty_and_never_ready(self):
        result = ci.normalize("failure", "", "", "a" * 40, "hash")
        self.assertEqual(result["verdict"], "UNSURE")
        self.assertIn("failure", result["report"])
        self.assertEqual(result["session_id"], "")

    def test_action_supplies_identity_model_cannot_forge_it(self):
        model = {"head": "a" * 40, "verdict": "READY", "findings": [], "report": "Scoped delta verified.",
                 "session_id": "invented", "model": "wrong"}
        result = ci.normalize("success", json.dumps(model), "actual-action-session", "a" * 40, "trusted-hash")
        self.assertEqual(result["session_id"], "actual-action-session")
        self.assertEqual(result["skill_sha256"], "trusted-hash")
        self.assertEqual(result["model"], "claude-opus-5-5")
        self.assertEqual(result["effort"], "high")

    def test_stale_or_missing_structured_result_cannot_pass(self):
        for output in ('{}', 'not-json', json.dumps({"head": "b" * 40, "verdict": "READY", "findings": [], "report": "Ready"})):
            result = ci.normalize("success", output, "session", "a" * 40, "hash")
            self.assertEqual(result["verdict"], "UNSURE")


if __name__ == "__main__":
    unittest.main()
