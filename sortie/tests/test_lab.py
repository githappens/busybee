import importlib.util
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
spec = importlib.util.spec_from_file_location("lab", Path(__file__).resolve().parents[1] / "lab.py")
lab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lab)


class DependencyTests(unittest.TestCase):
    def setUp(self):
        self.issue = {"number": 72, "state": "open", "milestone": {"title": lab.MILESTONE},
                      "labels": [{"name": "sortie:ready"}]}

    def test_only_explicitly_ready_lab_tasks_are_eligible(self):
        self.assertEqual(lab.task_error(self.issue), "")
        for labels in ([], [{"name": "epic"}, {"name": "sortie:ready"}],
                       [{"name": "needs-human"}, {"name": "sortie:ready"}]):
            self.issue["labels"] = labels
            self.assertTrue(lab.task_error(self.issue))

    def test_wrong_milestone_and_closed_tasks_are_rejected(self):
        self.issue["milestone"] = {"title": "another milestone"}
        self.assertTrue(lab.task_error(self.issue))
        self.setUp()
        self.issue["state"] = "closed"
        self.assertTrue(lab.task_error(self.issue))

    def test_closed_not_planned_does_not_release_a_dependent(self):
        blocker = {"state": "CLOSED", "stateReason": "NOT_PLANNED",
                   "closedByPullRequestsReferences": {"nodes": [], "pageInfo": {"hasNextPage": False}}}
        self.assertFalse(lab.completed_by_merge(blocker))

    def test_manual_closure_without_a_merged_pr_is_not_implementation(self):
        blocker = {"state": "CLOSED", "stateReason": "COMPLETED",
                   "closedByPullRequestsReferences": {"nodes": [], "pageInfo": {"hasNextPage": False}}}
        self.assertFalse(lab.completed_by_merge(blocker))

    def test_merged_closing_pr_on_main_releases_dependency(self):
        blocker = {"state": "CLOSED", "stateReason": "COMPLETED",
                   "closedByPullRequestsReferences": {"nodes": [{"merged": True, "baseRefName": "main"}],
                                                       "pageInfo": {"hasNextPage": False}}}
        self.assertTrue(lab.completed_by_merge(blocker))
        blocker["closedByPullRequestsReferences"]["nodes"][0]["baseRefName"] = "feature"
        self.assertFalse(lab.completed_by_merge(blocker))


if __name__ == "__main__":
    unittest.main()
