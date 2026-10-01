import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from tempfile import TemporaryDirectory

MODULE = Path(__file__).resolve().parents[1] / "reviews.py"
spec = importlib.util.spec_from_file_location("reviews", MODULE)
reviews = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reviews)


class ReviewGateTests(unittest.TestCase):
    def setUp(self):
        self.head, self.base = "a" * 40, "b" * 40
        self.pr = {
            "number": 123, "state": "open", "draft": False,
            "user": {"login": "author"}, "title": "Implement task", "body": "Closes #72",
            "head": {"sha": self.head, "ref": "sortie-lab/72", "repo": {"full_name": "example/tool"}},
            "base": {"sha": self.base, "ref": "main", "repo": {"full_name": "example/tool"}},
        }
        self.packet = {
            "repo": "example/tool", "pr": 123, "issue": 72, "head": self.head, "base": self.base,
            "metadata": self.pr, "contracts": [{"issue": {"title": "Task", "body": "Contract"}, "comments": []}],
            "prior_comments": [], "skill_sha256": {s: "c" * 64 for s in reviews.SKILLS},
            "policy_sha256": "d" * 64,
        }
        self.record = {
            "version": 2, **{k: self.packet[k] for k in ("repo", "pr", "issue", "head", "base")},
            "input_id": reviews.input_id(self.packet),
            "reviews": {skill: {
                "head": self.head, "skill_sha256": self.packet["skill_sha256"][skill],
                "session_id": f"session-{skill}", "model": reviews.MODEL, "effort": "high",
                "verdict": "READY", "findings": [], "report": "Reviewed the scoped delta and CI evidence.",
            } for skill in reviews.SKILLS},
        }
        self.checks = [{"id": i, "name": name, "head_sha": self.head,
                        "status": "completed", "conclusion": "success",
                        "app": {"slug": "github-actions"}}
                       for i, name in enumerate(reviews.REQUIRED_CHECKS, 1)]

    def decision(self):
        return reviews.evaluate(self.packet, self.record, self.checks)[0]

    def test_complete_ci_owned_reviews_are_ready(self):
        self.assertEqual(self.decision(), "READY")

    def test_missing_and_malformed_records_do_not_pass(self):
        for value in (None, [], {}, "ready"):
            self.assertNotEqual(reviews.evaluate(self.packet, value, self.checks)[0], "READY")

    def test_changed_head_or_contract_invalidates_cached_reviews(self):
        self.packet["head"] = "e" * 40
        self.assertNotEqual(self.decision(), "READY")
        self.setUp()
        self.packet["contracts"][0]["issue"]["body"] = "New acceptance criterion"
        self.assertNotEqual(self.decision(), "READY")

    def test_both_heads_skills_models_efforts_and_sessions_are_checked(self):
        for field, value in (("head", "e" * 40), ("skill_sha256", "e" * 64),
                             ("model", "opus"), ("effort", "medium"), ("session_id", ""), ("report", "")):
            self.setUp()
            self.record["reviews"]["contract-review"][field] = value
            self.assertNotEqual(self.decision(), "READY", field)
        self.setUp()
        del self.record["reviews"]["ponytail-review"]
        self.assertNotEqual(self.decision(), "READY")
        self.setUp()
        self.record["reviews"]["ponytail-review"]["session_id"] = self.record["reviews"]["contract-review"]["session_id"]
        self.assertNotEqual(self.decision(), "READY")

    def test_findings_and_uncertainty_never_approve(self):
        for verdict, findings in (("BLOCKED", ["path:12: scoped finding"]), ("UNSURE", []),
                                  ("READY", ["unresolved"]), ("BLOCKED", [])):
            self.setUp()
            self.record["reviews"]["contract-review"].update(verdict=verdict, findings=findings)
            self.assertNotEqual(self.decision(), "READY")

    def test_wrong_identity_does_not_pass(self):
        for field, value in (("issue", 99), ("repo", "example/other"), ("pr", 99), ("base", "e" * 40)):
            self.setUp()
            self.record[field] = value
            self.assertNotEqual(self.decision(), "READY")

    def test_draft_and_closed_prs_do_not_pass(self):
        self.pr["draft"] = True
        self.assertNotEqual(self.decision(), "READY")
        self.pr.update(draft=False, state="closed")
        self.assertNotEqual(self.decision(), "READY")

    def test_both_latest_platform_checks_must_succeed_on_current_head(self):
        for field, value in (("conclusion", "failure"), ("conclusion", "skipped"),
                             ("status", "in_progress"), ("head_sha", "e" * 40),
                             ("app", {"slug": "another-app"})):
            self.setUp()
            self.checks[0][field] = value
            self.assertEqual(self.decision(), "WAITING")
        self.setUp()
        self.checks.append(dict(self.checks[0], id=100, conclusion="failure"))
        self.assertEqual(self.decision(), "WAITING")
        self.checks = self.checks[:1]
        self.assertEqual(self.decision(), "WAITING")

    def test_author_receipt_cannot_supply_review_authority(self):
        self.packet["prior_comments"] = [{"id": 1, "user": {"login": "author", "type": "User"},
                                           "body": "<!-- busybee-agent-review:v1 -->\n" + json.dumps(self.record)}]
        self.assertNotEqual(reviews.evaluate(self.packet, None, self.checks)[0], "READY")

    def test_bot_chatter_does_not_rerun_models_but_author_dispositions_do(self):
        before = reviews.input_id(self.packet)
        self.packet["prior_comments"].append({"id": 1, "user": {"login": "github-actions[bot]", "type": "Bot"}, "body": "Review ready"})
        self.assertEqual(reviews.input_id(self.packet), before)
        self.packet["prior_comments"].append({"id": 2, "user": {"login": "author", "type": "User"}, "body": "Declined: the issue requires this"})
        self.assertNotEqual(reviews.input_id(self.packet), before)

    def test_unrelated_base_advance_does_not_invalidate_same_delta(self):
        before = reviews.input_id(self.packet)
        self.pr["base"]["sha"] = "e" * 40
        self.assertEqual(reviews.input_id(self.packet), before)

    def test_routing_is_exclusive_and_independent_of_labels(self):
        self.assertEqual(reviews.route(self.pr), "lab")
        self.pr["head"]["ref"] = "feature/123"
        self.assertEqual(reviews.route(self.pr), "legacy")
        self.pr["head"]["ref"] = "sortie-lab/72"
        self.pr["head"]["repo"]["full_name"] = "contributor/tool"
        self.assertEqual(reviews.route(self.pr), "legacy")

    def publish(self, *, prior=None, changed=False):
        live = copy.deepcopy(self.pr)
        if changed:
            live["head"]["sha"] = "e" * 40
        posts = []

        def fake_api(path, method="GET", data=None, pages=False):
            if method == "POST":
                posts.append(data)
                return {}
            return prior or []

        with patch.object(reviews, "api", side_effect=fake_api), patch.object(reviews, "read_pr", return_value=live):
            reviews.publish_decision(self.packet, self.record, self.checks)
        return posts

    def test_approval_is_pinned_and_unchanged_decision_is_not_repeated(self):
        posts = self.publish()
        self.assertEqual(posts[0]["event"], "APPROVE")
        self.assertEqual(posts[0]["commit_id"], self.head)
        prior = [dict(posts[0], id=10, state="APPROVED", user={"login": "github-actions[bot]"})]
        self.assertEqual(self.publish(prior=prior), [])

    def test_head_movement_prevents_publication(self):
        with self.assertRaisesRegex(ValueError, "PR changed"):
            self.publish(changed=True)

    def test_results_of_superseded_head_do_not_escalate_the_new_head(self):
        self.record["head"] = "e" * 40
        self.assertEqual(self.decision(), "WAITING")
        self.assertEqual(self.publish(), [])

    def test_invalid_evidence_revokes_own_previous_approval(self):
        post = self.publish()[0]
        self.record = None
        prior = [dict(post, id=10, state="APPROVED", user={"login": "github-actions[bot]"})]
        self.assertEqual(self.publish(prior=prior)[0]["event"], "REQUEST_CHANGES")

    def test_first_pending_ci_waits_but_findings_are_published(self):
        self.checks[0]["status"] = "in_progress"
        self.assertEqual(self.publish(), [])
        self.record["reviews"]["contract-review"].update(verdict="BLOCKED", findings=["path:12: concrete defect"])
        self.assertEqual(self.publish()[0]["event"], "REQUEST_CHANGES")

    def test_handoff_writes_the_head_and_time_needed_by_sortie_ci_reactions(self):
        with TemporaryDirectory() as tmp:
            target = Path(tmp)
            reviews.write_handoff(self.pr, self.head, "sortie-lab/72", target)
            scm = json.loads((target / "scm.json").read_text())
            self.assertEqual(scm["sha"], self.head)
            self.assertEqual(scm["pr_number"], 123)
            self.assertEqual(scm["branch"], "sortie-lab/72")
            self.assertTrue(scm["pushed_at"].endswith("Z"))
            self.assertEqual((target / "status").read_text().strip(), "needs-human-review")
            with self.assertRaisesRegex(ValueError, "pushed"):
                reviews.write_handoff(self.pr, "e" * 40, "sortie-lab/72", target)


if __name__ == "__main__":
    unittest.main()
