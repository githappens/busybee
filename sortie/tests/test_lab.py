import io
import json
from pathlib import Path
import contextlib
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import lab


class DependencyTests(unittest.TestCase):
    def setUp(self):
        self.issue = {"number": 72, "state": "open", "milestone": {"title": "agent lab: autonomous VM development"},
                      "labels": [{"name": "sortie:ready"}]}

    def test_only_explicitly_ready_lab_tasks_are_eligible(self):
        self.assertEqual(lab.task_error(self.issue), "")
        for labels in ([], [{"name": "epic"}, {"name": "sortie:ready"}],
                       [{"name": "needs-human"}, {"name": "sortie:ready"}]):
            self.issue["labels"] = labels
            self.assertTrue(lab.task_error(self.issue))

    def test_any_milestone_or_none_is_eligible(self):
        # Dispatch is repository-wide: ordering comes from blocked-by relations only.
        for milestone in ({"title": "another milestone"}, None):
            self.issue["milestone"] = milestone
            self.assertEqual(lab.task_error(self.issue), "")

    def test_closed_tasks_and_pull_requests_are_rejected(self):
        self.issue["state"] = "closed"
        self.assertTrue(lab.task_error(self.issue))
        self.setUp()
        self.issue["pull_request"] = {"url": "https://example.test/pr/1"}
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


def lab_issue(number, labels=("sortie:ready",), body="", milestone="agent lab: autonomous VM development"):
    return {"number": number, "state": "open", "body": body,
            "milestone": {"title": milestone} if milestone else None,
            "labels": [{"name": name} for name in labels]}


class Tracker:
    """The GitHub API as lab.py reads it, recording every write."""

    def __init__(self, issues, blocked_by, merged):
        self.issues = {issue["number"]: issue for issue in issues}
        self.blocked_by, self.merged = blocked_by, merged
        self.writes = []

    def __call__(self, path, method="GET", data=None, pages=False):
        if method != "GET" and path != "graphql":
            self.writes.append((method, path, data))
            return {}
        if path == "graphql":
            number = data["variables"]["number"]
            nodes = [{"merged": True, "baseRefName": "main"}] if number in self.merged else []
            state = "CLOSED" if number in self.merged else "OPEN"
            return {"data": {"repository": {"issue": {
                "state": state, "stateReason": "COMPLETED" if number in self.merged else None,
                "closedByPullRequestsReferences": {"nodes": nodes,
                                                   "pageInfo": {"hasNextPage": False, "endCursor": None}}}}}}
        if "/dependencies/blocked_by" in path:
            number = int(path.split("/issues/")[1].split("/")[0])
            return [{"number": n} for n in self.blocked_by.get(number, [])]
        if path.startswith(f"repos/{lab.REPO}/issues?"):
            return [i for i in self.issues.values()
                    if "sortie:ready" in {label["name"] for label in i["labels"]}]
        return self.issues[int(path.rsplit("/", 1)[1])]


class DispatchTests(unittest.TestCase):
    def setUp(self):
        self.capabilities = {"worker:linux": "", "worker:macos": "", "controller:session": "",
                             "controller:gate": "", "controller:verify": "",
                             "controller:terminal": "no terminal support in this controller"}
        self.tracker = Tracker(
            [lab_issue(10), lab_issue(11), lab_issue(12, body="**Lab requires:** controller:terminal"),
             lab_issue(13, body="Lab requires: worker:windows"),
             lab_issue(14, milestone="another milestone"), lab_issue(15, labels=("sortie:ready", "epic")),
             lab_issue(17, milestone=None, body="Lab requires: controller:verify"),
             lab_issue(18, milestone="another milestone"),
             lab_issue(16, labels=("sortie:ready", "sortie"), body="Lab requires: controller:terminal, controller:verify")],
            blocked_by={11: [9], 10: [8], 18: [11]}, merged={8})
        self.original = lab.api, lab.available_capabilities
        lab.api = self.tracker
        lab.available_capabilities = lambda: self.capabilities

    def tearDown(self):
        lab.api, lab.available_capabilities = self.original

    def release(self, dry_run):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            lab.release(dry_run=dry_run)
        return out.getvalue().splitlines()

    def test_dispatch_respects_blockers_and_capabilities(self):
        self.assertEqual(self.release(dry_run=True), [
            "#10: eligible",
            "#11: waiting for merged prerequisites [9]",
            "#12: capability controller:terminal is unavailable: no terminal support in this controller",
            "#13: unsupported capability worker:windows",
            "#14: eligible",
            "#16: capability controller:terminal is unavailable: no terminal support in this controller",
            "#17: eligible",
            "#18: waiting for merged prerequisites [11]",
        ])
        # A dry run changes nothing, and unrelated issues are never touched.
        self.assertEqual(self.tracker.writes, [])
        self.release(dry_run=False)
        self.assertEqual(sorted((m, p) for m, p, _ in self.tracker.writes), [
            ("DELETE", f"repos/{lab.REPO}/issues/16/labels/sortie"),
            ("POST", f"repos/{lab.REPO}/issues/10/labels"),
            ("POST", f"repos/{lab.REPO}/issues/14/labels"),
            ("POST", f"repos/{lab.REPO}/issues/17/labels"),
        ])
        for issue in (15, 18):
            self.assertFalse(any(f"/issues/{issue}/" in p for _, p, _ in self.tracker.writes))

    def test_a_closed_blocker_is_not_enough_when_its_capability_is_missing(self):
        self.capabilities["worker:linux"] = "the linux baseline VM or snapshot no longer exists"
        with self.assertRaises(ValueError) as raised:
            lab.check(10)
        self.assertEqual(str(raised.exception),
                         "#10: capability worker:linux is unavailable: "
                         "the linux baseline VM or snapshot no longer exists")

    def test_every_dispatch_needs_a_linux_worker_and_sessions(self):
        # A session works in a Linux worker; its handoff is verified on Linux and macOS by the gate.
        defaults = ["controller:gate", "controller:session", "worker:linux", "worker:macos"]
        self.assertEqual(lab.required_capabilities(lab_issue(1)), defaults)
        self.assertEqual(lab.required_capabilities(lab_issue(1, body="**Lab requires:** worker:macos,terminal")),
                         sorted(defaults + ["terminal"]))

    def test_no_dispatch_when_the_handoff_could_not_be_verified(self):
        self.capabilities["worker:macos"] = "no promoted macos baseline"
        with self.assertRaises(ValueError) as raised:
            lab.check(10)
        self.assertEqual(str(raised.exception),
                         "#10: capability worker:macos is unavailable: no promoted macos baseline")

    def test_capabilities_come_from_the_controller_doctor(self):
        doctor = {"data": {"templates": {"linux": {"state": "ready"}, "macos": {"state": "missing"}},
                           "controller": {"capabilities": ["session", "verify"]}},
                  "findings": [{"code": "baseline_missing", "message": "no promoted macos baseline",
                                "severity": "error"}]}
        found = lab.capabilities_from(doctor)
        self.assertEqual(found["worker:linux"], "")
        self.assertIn("macos baseline is missing", found["worker:macos"])
        self.assertEqual(found["controller:session"], "")
        self.assertNotIn("controller:terminal", found)


class ProfileTests(unittest.TestCase):
    def test_infrastructure_needs_an_authorized_label(self):
        policy = lab.guard_policy()
        self.assertEqual(lab.profile(lab_issue(1, labels=("sortie:ready", "area:harness")), policy),
                         "infrastructure")
        self.assertEqual(lab.profile(lab_issue(1), policy), "product")

    def test_product_sessions_forbid_dispatch_and_review_policy(self):
        forbidden = lab.guard_policy()["profiles"]["product"]["forbidden_paths"]
        for path in ("sortie/", ".github/workflows/"):
            self.assertIn(path, forbidden)
        self.assertEqual(lab.guard_policy()["profiles"]["infrastructure"]["forbidden_paths"], [])


class ResetTests(unittest.TestCase):
    """`lab.py reset` clears what Sortie recorded for an issue whose retries a
    harness failure burned, and hands the issue back as ready."""

    def setUp(self):
        import sqlite3
        import tempfile
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.state = Path(tmp.name)
        self.db = self.state / "sortie.db"
        con = sqlite3.connect(self.db)
        con.executescript("""
            CREATE TABLE run_history (id INTEGER PRIMARY KEY, issue_id TEXT, identifier TEXT, attempt INTEGER);
            CREATE TABLE retry_entries (issue_id TEXT PRIMARY KEY, identifier TEXT, attempt INTEGER);
            CREATE TABLE session_metadata (issue_id TEXT PRIMARY KEY, session_id TEXT);
            CREATE TABLE parked_issues (issue_id TEXT PRIMARY KEY, identifier TEXT);
            CREATE TABLE budget_hold_notices (issue_id TEXT PRIMARY KEY);
            CREATE TABLE handoff_absence_resets (issue_id TEXT PRIMARY KEY);
            CREATE TABLE reaction_fingerprints (issue_id TEXT, kind TEXT, PRIMARY KEY (issue_id, kind));
            CREATE TABLE aggregate_metrics (key TEXT PRIMARY KEY);
        """)
        for issue in ("7", "8"):
            con.execute("INSERT INTO run_history (issue_id, identifier, attempt) VALUES (?, ?, 1)", (issue, issue))
            con.execute("INSERT INTO run_history (issue_id, identifier, attempt) VALUES (?, ?, 2)", (issue, issue))
            con.execute("INSERT INTO retry_entries VALUES (?, ?, 3)", (issue, issue))
            for table in ("session_metadata", "budget_hold_notices", "handoff_absence_resets"):
                con.execute(f"INSERT INTO {table} (issue_id) VALUES (?)", (issue,))
            con.execute("INSERT INTO parked_issues VALUES (?, ?)", (issue, issue))
            con.execute("INSERT INTO reaction_fingerprints VALUES (?, 'ci')", (issue,))
        con.execute("INSERT INTO aggregate_metrics VALUES ('totals')")
        con.commit()
        con.close()
        self.tracker = Tracker([lab_issue(7, labels=("sortie:working", "sortie")),
                                lab_issue(8, labels=("sortie:working",))], {}, set())
        self.original = lab.api
        lab.api = self.tracker
        self.addCleanup(setattr, lab, "api", self.original)

    def rows(self, issue):
        import sqlite3
        con = sqlite3.connect(self.db)
        tables = [t for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table'")
                  if t != "aggregate_metrics"]
        found = {t: con.execute(f"SELECT count(*) FROM {t} WHERE issue_id = ?", (issue,)).fetchone()[0]
                 for t in tables}
        con.close()
        return found

    def test_reset_clears_one_issue_and_makes_it_ready(self):
        with contextlib.redirect_stdout(io.StringIO()):
            lab.reset([7], self.state)
        self.assertEqual(set(self.rows("7").values()), {0})
        self.assertEqual(set(self.rows("8").values()), {1, 2})
        self.assertEqual(self.tracker.writes, [
            ("DELETE", f"repos/{lab.REPO}/issues/7/labels/sortie:working", None),
            ("POST", f"repos/{lab.REPO}/issues/7/labels", {"labels": ["sortie:ready"]})])

    def test_reset_refuses_while_a_launcher_holds_the_lock(self):
        (self.state / "launcher.lock").mkdir()
        with self.assertRaisesRegex(RuntimeError, "launcher.lock"):
            lab.reset([7], self.state)
        self.assertEqual(self.rows("7")["run_history"], 2)
        self.assertEqual(self.tracker.writes, [])

    def test_reset_needs_sortie_state(self):
        with self.assertRaisesRegex(RuntimeError, "sortie.db"):
            lab.reset([7], self.state / "missing")


if __name__ == "__main__":
    unittest.main()
