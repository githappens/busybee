"""The evidence gate: when a candidate counts as a verified fix.

See docs/design/agent-lab.md §Evidence required for a verified fix. The
matrices are built here (or replayed from a recorded run); the scenarios are
the repository's own.
"""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import gate
import verify

REPO = Path(__file__).resolve().parents[3]
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "gate"
ISSUE = 69
REV, BASE = "a" * 40, "b" * 40
CONTROLLER = {"head": "c" * 40, "scenarios_dirty": False}
BASELINES = {"linux": "baseline-linux", "macos": "baseline-macos"}
METAS = gate.scenario_metas(REPO)


def entry(platform, revision, red=()):
    """A platform that ran every check and scenario; `red` scenarios failed."""
    scenarios, elsewhere = {}, []
    for sid, meta in METAS.items():
        if meta.get("issue") is None:
            continue
        if platform not in meta["platforms"]:
            elsewhere.append(sid)
            continue
        modes = {m: {"status": "product_failure" if sid in red else "success", "exec": "0006",
                     "failed": ["task_runs"] if sid in red else [], "binaries_match": True,
                     "terminals": ["0006-monitor"] if verify.ui(meta) else []} for m in meta["required_modes"]}
        scenarios[sid] = {"issue": meta["issue"], "ui": verify.ui(meta), "modes": modes}
    run_id = f"r-20261003T00000{len(revision) % 10}Z-{platform[:3]}{revision[:3]}"
    return {"status": "ran", "run_id": run_id, "baseline": BASELINES[platform], "head": revision,
            "version": "bzb 0.1.98", "binaries": {verify.BUSYBEE: "1" * 64},
            "checks": {n: {"status": "success", "exec": f"000{i}", "exit_code": 0, "elapsed_s": 1.0}
                       for i, (n, _) in enumerate(verify.CHECKS, 1)},
            "scenarios": scenarios, "not_applicable": sorted(elsewhere), "collected": "success",
            "evidence": f"runs/{run_id}/public"}


def matrix(role, revision, red=(), overlay=None):
    return {"schema": verify.SCHEMA, "id": f"r-{role}", "role": role,
            "source": {"revision": revision, "patch_sha256": None, "overlay_sha256": overlay},
            "controller": dict(CONTROLLER), "required_platforms": ["linux", "macos"],
            "platforms": {p: entry(p, revision, red) for p in ("linux", "macos")}, "verdict": "verified"}


def expect(**changes):
    return {"revision": REV, "base": BASE, "overlay_sha256": None, "controller": dict(CONTROLLER),
            "baselines": dict(BASELINES), "platforms": ["linux", "macos"], **changes}


class AssessTests(unittest.TestCase):
    def setUp(self):
        self.candidate = matrix("candidate", REV)
        self.base = matrix("base", BASE, red=("umask-startup",))
        self.missing = set()  # evidence paths that are not on disk
        self.workers = {}  # run id -> status other than destroyed

    def assess(self, issue=ISSUE, **changes):
        return gate.assess(issue, expect(**changes), self.candidate, self.base, METAS,
                           lambda rel: rel not in self.missing, lambda run: self.workers.get(run, "destroyed"))

    def codes(self, result):
        return {f["code"] for f in result["findings"] if f["severity"] == "error"}

    def test_red_base_and_green_candidate_on_both_platforms_is_verified(self):
        result = self.assess()
        self.assertEqual(result["verdict"], "verified", result["findings"])
        self.assertEqual(result["regressions"], [
            {"scenario": "umask-startup", "platform": p, "mode": "cold", "base": "product_failure",
             "candidate": "success"} for p in ("linux", "macos")])
        self.assertEqual(result["preexisting"], [])

    def test_verification_rejects_stale_or_incomplete_evidence(self):
        cases = {
            # A new commit or rebase: the evidence is for another revision.
            "stale revision": ("stale", "evidence_stale", lambda: self.candidate["source"].update(revision="d" * 40)),
            "stale built head": ("stale", "evidence_stale",
                                 lambda: self.candidate["platforms"]["macos"].update(head="d" * 40)),
            "uncommitted patch": ("stale", "evidence_stale",
                                  lambda: self.candidate["source"].update(patch_sha256="e" * 64)),
            "base of an older main": ("stale", "evidence_stale", lambda: self.base["source"].update(revision="d" * 40)),
            "other scenario revision": ("stale", "evidence_stale",
                                        lambda: self.candidate["controller"].update(head="d" * 40)),
            "other template": ("stale", "evidence_stale",
                               lambda: self.candidate["platforms"]["linux"].update(baseline="older")),
            "missing macOS": ("incomplete", "platform_missing", lambda: (
                self.candidate.update(required_platforms=["linux"]), self.candidate["platforms"].pop("macos"))),
            "macOS unavailable": ("incomplete", "platform_missing", lambda: self.candidate["platforms"].update(
                macos={"status": "unavailable", "reason": [{"code": "baseline_missing"}]})),
            "missing UI check": ("incomplete", "ui_evidence_missing", lambda: self.candidate["platforms"]["linux"][
                "scenarios"]["live-monitor"]["modes"]["prepared"].update(terminals=[])),
            "skipped check": ("incomplete", "skipped",
                              lambda: self.candidate["platforms"]["macos"]["checks"].pop("clippy")),
            "skipped scenario": ("incomplete", "skipped",
                                 lambda: self.candidate["platforms"]["linux"]["scenarios"].pop("live-monitor")),
            "undeclared not-applicable": ("incomplete", "skipped", lambda: (
                self.candidate["platforms"]["macos"]["not_applicable"].clear())),
            "timeout": ("incomplete", "timeout",
                        lambda: self.candidate["platforms"]["macos"]["checks"]["test"].update(status="timeout")),
            "interrupted worker": ("incomplete", "platform_missing",
                                   lambda: self.candidate["platforms"]["linux"].update(status="interrupted")),
            "missing artifact": ("incomplete", "artifact_missing",
                                 lambda: self.missing.add(self.candidate["platforms"]["macos"]["evidence"])),
            "export failed": ("incomplete", "artifact_missing",
                              lambda: self.candidate["platforms"]["macos"].update(evidence=None,
                                                                                  collected="export_failed")),
            "failed cleanup": ("incomplete", "cleanup_failed",
                               lambda: self.candidate["platforms"]["linux"].update(collected="incomplete_collection")),
            "worker left behind": ("incomplete", "cleanup_failed",
                                   lambda: self.workers.update({self.base["platforms"]["macos"]["run_id"]: "stopped"})),
            "other binary": ("incomplete", "binary_mismatch", lambda: self.candidate["platforms"]["linux"]["scenarios"][
                "umask-startup"]["modes"]["cold"].update(binaries_match=False)),
            "base without the red run": ("incomplete", "regression_not_red_on_base", lambda: self.base["platforms"][
                "macos"]["scenarios"]["umask-startup"]["modes"]["cold"].update(status="success", failed=[])),
            "no evidence": ("incomplete", "evidence_missing", lambda: setattr(self, "candidate", None)),
            "regression still red": ("failed", "regression_still_failing", lambda: self.candidate["platforms"][
                "linux"]["scenarios"]["umask-startup"]["modes"]["cold"].update(status="product_failure")),
            "new failure": ("failed", "new_failure",
                            lambda: self.candidate["platforms"]["macos"]["checks"]["fmt"].update(
                                status="product_failure", exit_code=1)),
        }
        for name, (verdict, code, damage) in cases.items():
            with self.subTest(name):
                self.setUp()
                damage()
                kept = copy.deepcopy((self.candidate, self.base))
                result = self.assess()
                self.assertEqual(result["verdict"], verdict, result["findings"])
                self.assertNotIn(result["verdict"], gate.PASSING)
                self.assertIn(code, self.codes(result), result["findings"])
                # Diagnostic state is kept: the gate reads the evidence and changes none of it.
                self.assertEqual((self.candidate, self.base), kept)

    def test_failures_the_base_shares_are_listed_never_hidden(self):
        # e.g. a known product failure in `cargo test` that the base fails too.
        for m in (self.candidate, self.base):
            m["platforms"]["linux"]["checks"]["test"].update(status="product_failure", exit_code=101)
        result = self.assess()
        self.assertEqual(result["verdict"], "preexisting_failures")
        self.assertIn(result["verdict"], gate.PASSING)
        self.assertEqual(result["preexisting"], [{"platform": "linux", "kind": "check", "name": "test",
                                                  "status": "product_failure"}])
        self.assertIn("preexisting_failure", {f["code"] for f in result["findings"]})

    def test_a_test_overlay_is_the_regression_and_is_never_excused(self):
        # A new regression test on old code: the base runs with the candidate's
        # test overlay, recorded apart from the base revision.
        self.base = matrix("base", BASE, red=("umask-startup",), overlay="f" * 64)
        self.base["platforms"]["linux"]["checks"]["test"].update(status="product_failure", exit_code=101)
        self.base["platforms"]["macos"]["checks"]["test"].update(status="product_failure", exit_code=101)
        result = self.assess(overlay_sha256="f" * 64)
        self.assertEqual(result["verdict"], "verified", result["findings"])
        self.assertIn({"overlay": "f" * 64, "platform": "linux", "base": "product_failure", "candidate": "success"},
                      result["regressions"])
        self.candidate["platforms"]["linux"]["checks"]["test"].update(status="product_failure", exit_code=101)
        result = self.assess(overlay_sha256="f" * 64)
        self.assertEqual(result["verdict"], "failed")
        self.assertIn("regression_still_failing", self.codes(result))
        # The overlay must be the one the base ran with.
        self.assertEqual(self.assess(overlay_sha256="0" * 64)["verdict"], "stale")

    def test_an_issue_without_a_declared_regression_says_so(self):
        result = self.assess(issue=80)
        self.assertEqual(result["verdict"], "verified", result["findings"])
        self.assertEqual(result["regressions"], [])
        self.assertIn("regression_undeclared", {f["code"] for f in result["findings"] if f["severity"] == "warning"})


class FakeOps:
    """Verification that writes its matrices like verify.run, from templates."""

    def __init__(self, state):
        self.state = state
        self.candidate = lambda rev: matrix("candidate", rev)
        self.base = lambda rev, overlay: matrix("base", rev, red=("umask-startup",), overlay=overlay)
        self.runs = []

    def verify(self, revision, overlay, platforms, role):
        self.runs.append((role, revision))
        digest = verify.worker._sha256(Path(overlay).read_bytes()) if overlay else None
        m = self.candidate(revision) if role == "candidate" else self.base(revision, digest)
        m["id"] = f"r-20261003T0000{len(self.runs):02d}Z-abcdef"
        path = self.state / "verifications" / m["id"] / "matrix.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps(m))
        for e in m["platforms"].values():
            if e.get("evidence"):
                (self.state / e["evidence"]).mkdir(parents=True, exist_ok=True)
        return {"status": "success", "findings": [], "data": {"matrix": str(path.relative_to(self.state))}}

    def controller(self):
        return dict(CONTROLLER)

    def baselines(self, platforms):
        return {p: BASELINES[p] for p in platforms}

    def worker_status(self, run_id):
        return "destroyed"

    def metas(self):
        return METAS


class EvaluateTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.state = Path(tmp.name)
        self.ops = FakeOps(self.state)

    def test_the_gate_record_names_its_evidence_and_publishes_a_redacted_copy(self):
        result = gate.evaluate(self.ops, ISSUE, REV, BASE)
        self.assertEqual(result["status"], "success", result["findings"])
        record = json.loads((self.state / result["data"]["gate"]).read_text())
        self.assertEqual(record["schema"], gate.SCHEMA)
        self.assertEqual((record["issue"], record["verdict"]), (ISSUE, "verified"))
        self.assertEqual(record["candidate"]["revision"], REV)
        self.assertEqual(record["base"]["revision"], BASE)
        self.assertTrue((self.state / record["candidate"]["matrix"]).is_file())
        public = (self.state / result["data"]["public"]).read_text()
        for m in (record["candidate"], record["base"]):
            for run_id in m["runs"]:
                self.assertNotIn(run_id, public)
        self.assertNotIn("baseline-linux", public)
        self.assertIn(REV, public)

    def test_unchanged_evidence_is_reused_not_verified_again(self):
        first = gate.evaluate(self.ops, ISSUE, REV, BASE)
        self.assertEqual(len(self.ops.runs), 2)
        again = gate.evaluate(self.ops, ISSUE, REV, BASE)
        self.assertEqual(len(self.ops.runs), 2)
        self.assertEqual(again["data"]["evidence_id"], first["data"]["evidence_id"])
        # A new head is verified; the base it shares is not.
        gate.evaluate(self.ops, ISSUE, "d" * 40, BASE)
        self.assertEqual(self.ops.runs[2:], [("candidate", "d" * 40)])
        # A refused candidate is verified again when asked again (a flaky check
        # must not stick to an unchanged head); its base, red on main, is reused.
        self.ops.candidate = lambda rev: dict(matrix("candidate", rev), verdict="failed")
        gate.evaluate(self.ops, ISSUE, "f" * 40, BASE)
        gate.evaluate(self.ops, ISSUE, "f" * 40, BASE)
        self.assertEqual(self.ops.runs[3:], [("candidate", "f" * 40)] * 2)
        # Evidence that could not complete is never reused.
        self.ops.candidate = lambda rev: dict(matrix("candidate", rev), verdict="incomplete")
        gate.evaluate(self.ops, ISSUE, "e" * 40, BASE)
        gate.evaluate(self.ops, ISSUE, "e" * 40, BASE)
        self.assertEqual(self.ops.runs[5:], [("candidate", "e" * 40)] * 2)

    def test_pilot_replays_red_base_and_green_candidate(self):
        # #69's red run, recorded by the lab on both platforms (the sanitized
        # public matrix). Its fix lives in #69's own PR; until it lands, the
        # unfixed candidate is refused, and a candidate that turns the
        # regression green is accepted with the base's other failures listed.
        recorded = json.loads((FIXTURES / "69-base.matrix.public.json").read_text())
        revision = recorded["source"]["revision"]
        baselines = {p: e["baseline"] for p, e in recorded["platforms"].items()}
        self.ops.controller = lambda: dict(recorded["controller"])
        self.ops.baselines = lambda platforms: {p: baselines[p] for p in platforms}
        self.ops.base = lambda rev, overlay: copy.deepcopy(recorded)
        unfixed = dict(copy.deepcopy(recorded), role="candidate")
        self.ops.candidate = lambda rev: copy.deepcopy(unfixed)
        refused = gate.evaluate(self.ops, ISSUE, revision, revision)
        record = json.loads((self.state / refused["data"]["gate"]).read_text())
        self.assertEqual(record["verdict"], "failed", record["findings"])
        self.assertEqual({r["candidate"] for r in record["regressions"]}, {"product_failure"})
        self.assertEqual({r["platform"] for r in record["regressions"]}, {"linux", "macos"})
        self.assertIn("regression_still_failing", {f["code"] for f in record["findings"]})

        fixed = copy.deepcopy(unfixed)
        fixed["source"]["revision"] = "f" * 40
        for e in fixed["platforms"].values():
            e["head"] = "f" * 40
            e["scenarios"]["umask-startup"]["modes"]["cold"].update(status="success", failed=[])
        self.ops.candidate = lambda rev: copy.deepcopy(fixed)
        accepted = gate.evaluate(self.ops, ISSUE, "f" * 40, revision)
        record = json.loads((self.state / accepted["data"]["gate"]).read_text())
        self.assertIn(record["verdict"], gate.PASSING, record["findings"])
        self.assertEqual({(r["platform"], r["base"], r["candidate"]) for r in record["regressions"]},
                         {("linux", "product_failure", "success"), ("macos", "product_failure", "success")})
        # Whatever else the base fails is carried into the result, not dropped.
        base_failures = {(p, "check", n) for p, e in recorded["platforms"].items()
                         for n, c in e["checks"].items() if c["status"] == "product_failure"}
        self.assertEqual({(x["platform"], x["kind"], x["name"]) for x in record["preexisting"]}, base_failures)
        body = gate.comment(json.loads((self.state / accepted["data"]["public"]).read_text()))
        self.assertTrue(body.startswith(f"<!-- {gate.MARKER} "))
        self.assertLess(len(body.encode()), 60000)


if __name__ == "__main__":
    unittest.main()
