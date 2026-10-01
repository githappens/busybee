import importlib.util
import json
from pathlib import Path
import sys
import re
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from reviews import GATE_MARKER

spec = importlib.util.spec_from_file_location("review_triage", Path(__file__).resolve().parents[1] / "review-triage.py")
triage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(triage)


class ReviewTriageTests(unittest.TestCase):
    def test_bot_feedback_is_rendered_without_human_review_comments(self):
        for name in ("LAB_WORKFLOW.md", "WORKFLOW.md"):
            self.check_bot_feedback(name)

    def check_bot_feedback(self, name):
        workflow = (Path(__file__).resolve().parents[1] / name).read_text()
        stack = []
        found = False
        for directive in re.findall(r"{{\s*(.*?)\s*}}", workflow):
            if directive == "if .bot_review_comments":
                found = True
                self.assertNotIn("if .review_comments", stack)
            if directive.startswith(("if ", "range ")):
                stack.append(directive)
            elif directive == "end":
                stack.pop()
        self.assertTrue(found)

    def review(self, verdict, id=1, head="a" * 40):
        return {"id": id, "commit_id": head, "user": {"login": "github-actions[bot]"},
                "body": GATE_MARKER + json.dumps({"head": head, "verdict": verdict, "input_id": "b" * 64}) + "\nReport"}

    def test_findings_dispatch_but_clean_and_waiting_reviews_do_not(self):
        for verdict, disposition in (("BLOCKED", "dispatch-agent"), ("READY", "handled"),
                                     ("WAITING", "handled"), ("UNSURE", "escalate")):
            self.assertEqual(triage.disposition([self.review(verdict)], "a" * 40), disposition)

    def test_stale_reviews_do_not_dispatch(self):
        self.assertEqual(triage.disposition([self.review("BLOCKED", head="c" * 40)], "a" * 40), "handled")

    def test_latest_result_supersedes_older_findings(self):
        self.assertEqual(triage.disposition([self.review("BLOCKED"), self.review("READY", id=2)], "a" * 40), "handled")

    def test_other_bots_and_author_receipts_are_not_gate_feedback(self):
        review = self.review("BLOCKED")
        review["user"]["login"] = "author"
        self.assertEqual(triage.disposition([review], "a" * 40), "handled")

    def test_malformed_latest_result_escalates_instead_of_using_old_green(self):
        broken = self.review("BLOCKED", id=2)
        broken["body"] = GATE_MARKER + "not json"
        self.assertEqual(triage.disposition([self.review("READY"), broken], "a" * 40), "escalate")


if __name__ == "__main__":
    unittest.main()
