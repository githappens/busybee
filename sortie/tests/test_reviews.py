import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from types import SimpleNamespace
from contextlib import redirect_stdout
from io import StringIO

MODULE = Path(__file__).resolve().parents[1] / "reviews.py"
spec = importlib.util.spec_from_file_location("reviews", MODULE)
reviews = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reviews)


class ReviewGateTests(unittest.TestCase):
    def setUp(self):
        self.head, self.base = "a" * 40, "b" * 40
        self.pr = {
            "number": 123, "state": "open", "draft": False,
            "user": {"login": "author"},
            "head": {"sha": self.head, "ref": "sortie-lab/72", "repo": {"full_name": "example/tool"}},
            "base": {"sha": self.base, "ref": "main", "repo": {"full_name": "example/tool"}},
        }
        self.hashes = {skill: "c" * 64 for skill in reviews.SKILLS}
        self.receipt = {
            "version": 1, "repo": "example/tool", "pr": 123, "issue": 72,
            "head": self.head, "base": self.base,
            "reviews": {skill: {
                "head": self.head, "skill_sha256": self.hashes[skill],
                "reviewer": f"agent:{skill}:session", "verdict": "READY",
                "findings": [], "report": "Reviewed the full scoped delta; no findings remain.",
            } for skill in reviews.SKILLS},
            "verification": [{"command": "test command", "result": "passed", "evidence": "All required assertions passed."}],
        }
        self.checks = [{"id": i, "name": name, "head_sha": self.head,
                        "status": "completed", "conclusion": "success",
                        "app": {"slug": "github-actions"}}
                       for i, name in enumerate(reviews.REQUIRED_CHECKS, 1)]

    def decision(self, receipt=None):
        return reviews.evaluate(self.pr, self.base, self.receipt if receipt is None else receipt,
                                self.hashes, self.checks)

    def test_complete_current_head_reviews_and_ci_are_ready(self):
        self.assertEqual(self.decision()[0], "READY")

    def test_missing_and_malformed_records_do_not_pass(self):
        for value in (None, [], {}, "ready"):
            with self.subTest(value=value):
                self.assertNotEqual(reviews.evaluate(self.pr, self.base, value, self.hashes, self.checks)[0], "READY")

    def test_new_head_invalidates_both_reviews(self):
        self.pr["head"]["sha"] = "d" * 40
        self.assertNotEqual(self.decision()[0], "READY")

    def test_each_review_must_name_the_current_head_and_trusted_skill(self):
        for field, value in (("head", "d" * 40), ("skill_sha256", "d" * 64)):
            record = copy.deepcopy(self.receipt)
            record["reviews"]["contract-review"][field] = value
            self.assertNotEqual(self.decision(record)[0], "READY")

    def test_both_distinct_review_contexts_are_required(self):
        del self.receipt["reviews"]["ponytail-review"]
        self.assertNotEqual(self.decision()[0], "READY")
        self.setUp()
        self.receipt["reviews"]["ponytail-review"]["reviewer"] = self.receipt["reviews"]["contract-review"]["reviewer"]
        self.assertNotEqual(self.decision()[0], "READY")

    def test_unresolved_findings_and_unsure_verdict_do_not_pass(self):
        for skill in reviews.SKILLS:
            for verdict, findings in (("BLOCKED", ["a scoped finding"]), ("UNSURE", []), ("READY", ["unresolved"])):
                record = copy.deepcopy(self.receipt)
                record["reviews"][skill].update(verdict=verdict, findings=findings)
                self.assertNotEqual(self.decision(record)[0], "READY")

    def test_empty_report_or_verification_is_not_evidence(self):
        self.receipt["verification"] = []
        self.assertNotEqual(self.decision()[0], "READY")
        self.setUp()
        self.receipt["reviews"]["ponytail-review"]["report"] = ""
        self.assertNotEqual(self.decision()[0], "READY")

    def test_wrong_issue_repo_pr_or_base_do_not_pass(self):
        for field, value in (("issue", 99), ("repo", "example/other"), ("pr", 99), ("base", "d" * 40)):
            record = copy.deepcopy(self.receipt)
            record[field] = value
            self.assertNotEqual(self.decision(record)[0], "READY")

    def test_draft_and_closed_prs_never_pass(self):
        self.pr["draft"] = True
        self.assertNotEqual(self.decision()[0], "READY")
        self.pr.update(draft=False, state="closed")
        self.assertNotEqual(self.decision()[0], "READY")

    def test_ci_must_cover_both_platforms_on_this_head(self):
        for field, value in (("conclusion", "failure"), ("conclusion", "skipped"),
                             ("status", "in_progress"), ("head_sha", "d" * 40),
                             ("app", {"slug": "another-app"})):
            self.setUp()
            self.checks[0][field] = value
            self.assertNotEqual(self.decision()[0], "READY")
        self.checks = self.checks[:1]
        self.assertNotEqual(self.decision()[0], "READY")

    def test_newer_failed_ci_attempt_overrides_old_green(self):
        check = dict(self.checks[0], id=100, conclusion="failure")
        self.checks.append(check)
        self.assertNotEqual(self.decision()[0], "READY")

    def test_routing_is_exclusive_and_does_not_depend_on_a_label(self):
        self.assertEqual(reviews.route(self.pr), "lab")
        self.pr["head"]["ref"] = "feature/123"
        self.assertEqual(reviews.route(self.pr), "legacy")
        self.pr["head"]["ref"] = "sortie-lab/72"
        self.pr["head"]["repo"]["full_name"] = "contributor/tool"
        self.assertEqual(reviews.route(self.pr), "legacy")

    def comment(self, receipt, id=1, author="author", association="OWNER"):
        return {"id": id, "user": {"login": author}, "author_association": association,
                "body": reviews.MARKER + "\n" + json.dumps(receipt)}

    def test_only_author_reports_from_repo_collaborators_are_consumed(self):
        comments = [self.comment(self.receipt, author="stranger"),
                    self.comment(self.receipt, id=2, association="CONTRIBUTOR")]
        self.assertIsNone(reviews.latest_receipt(self.pr, comments))

    def test_malformed_new_report_does_not_fall_back_to_an_old_ready_report(self):
        comments = [self.comment(self.receipt), self.comment(self.receipt, id=2)]
        comments[-1]["body"] = reviews.MARKER + "\nnot-json"
        self.assertIsNone(reviews.latest_receipt(self.pr, comments))

    def test_unchanged_receipt_is_a_stable_decision(self):
        comments = [self.comment(self.receipt)]
        first = reviews.latest_receipt(self.pr, comments)
        self.assertEqual(first, reviews.latest_receipt(self.pr, comments))
        self.assertEqual(self.decision(first), self.decision(first))

    def run_gate(self, *, changed_head=False, prior_state=None, report=True):
        posts = []
        prior = [] if prior_state is None else [{
            "id": 10, "user": {"login": "github-actions[bot]"},
            "commit_id": self.head, "state": prior_state, "body": reviews.GATE_MARKER + " prior result",
        }]

        def fake_api(path, method="GET", data=None, pages=False):
            if method == "POST":
                posts.append((path, data))
                return {}
            if path.endswith("/issues/72"):
                return {"milestone": {"title": reviews.MILESTONE}, "labels": []}
            if "/issues/123/comments" in path:
                return [self.comment(self.receipt)] if report else []
            if "/compare/" in path:
                return {"merge_base_commit": {"sha": self.base}}
            if "/pulls/123/reviews" in path:
                return prior
            raise AssertionError(path)

        live = copy.deepcopy(self.pr)
        if changed_head:
            live["head"]["sha"] = "d" * 40
        args = SimpleNamespace(repo="example/tool", pr=123, apply=True)
        with patch.object(reviews, "api", side_effect=fake_api), \
                patch.object(reviews, "read_pr", side_effect=[self.pr, live]), \
                patch.object(reviews, "skill_hashes", return_value=self.hashes), \
                patch.object(reviews, "command", return_value=json.dumps([{"check_runs": self.checks}])), \
                redirect_stdout(StringIO()):
            if changed_head:
                with self.assertRaisesRegex(ValueError, "PR changed"):
                    reviews.gate(args)
            else:
                reviews.gate(args)
        return posts

    def test_approval_is_pinned_to_verified_head(self):
        posts = self.run_gate()
        self.assertEqual(len(posts), 1)
        self.assertEqual(posts[0][1]["event"], "APPROVE")
        self.assertEqual(posts[0][1]["commit_id"], self.head)

    def test_head_movement_before_posting_discards_approval(self):
        self.assertEqual(self.run_gate(changed_head=True), [])

    def test_unchanged_green_does_not_post_another_review(self):
        self.assertEqual(self.run_gate(prior_state="APPROVED"), [])

    def test_deleted_report_revokes_previous_gate_approval(self):
        posts = self.run_gate(prior_state="APPROVED", report=False)
        self.assertEqual(posts[0][1]["event"], "REQUEST_CHANGES")

    def test_first_pending_ci_waits_without_repeated_negative_reviews(self):
        self.checks[0]["status"] = "in_progress"
        self.assertEqual(self.run_gate(), [])


if __name__ == "__main__":
    unittest.main()
