"""The evidence gate: whether a candidate revision is a verified fix.

See docs/design/agent-lab.md §Evidence required for a verified fix. An agent's
report of success is not evidence. A candidate is accepted only on platform
matrices (verify.py) that the controller produced for exactly this candidate
and its base:

- **Bound.** The candidate matrix is for the committed candidate head with no
  patch; the base matrix is for the candidate's merge base, with the test
  overlay (if any) recorded apart from the base revision. Both come from this
  controller and scenario revision and, per platform, from the promoted
  template baseline. Anything else is `stale`.
- **Complete.** Every required platform ran; every required check and every
  applicable regression scenario in its required modes has an outcome on each,
  and every scenario that does not apply was declared not applicable by the
  matrix. A skip, a timeout or an environment failure, a scenario that ran
  another binary, a terminal scenario without its captured terminals, public
  evidence that is not on disk, or a worker that was not collected and
  destroyed makes it `incomplete`.
- **A fix.** Each regression (a scenario naming the issue, and the test
  overlay's `cargo test`) fails on the base and passes on the candidate. A
  candidate failure the base does not share is `failed`. A failure the base
  shares is listed in `preexisting`, never dropped; the verdict is then
  `preexisting_failures` rather than `verified`.

`evaluate` runs or reuses the two matrices and writes the result to
`gates/<id>/gate.json`, with a redacted `gate.public.json` beside it; `comment`
renders that public copy for the pull request. Unchanged evidence is reused
rather than verified again: a verified candidate, and a complete base whether
it passed or failed. A refused candidate is verified again when asked again,
so a flaky check does not stick to its head; incomplete evidence never is
reused.
"""
import getpass
import hashlib
import json
from pathlib import Path
import socket

import contracts
import evidence
import scenario
import verify
import worker

SCHEMA = "busybee.vm.gate/v1"
# Verdicts that let a candidate go to review.
PASSING = ("verified", "preexisting_failures")
# The first line of the pull-request comment that carries the evidence.
MARKER = "busybee-lab-evidence:v1"
STALE = ("evidence_stale",)
FAILED = ("regression_still_failing", "new_failure")
PASS, FAIL = "success", "product_failure"


def scenario_metas(repo):
    """Every scenario's metadata, by id, from the controller's checkout."""
    return {p.stem: scenario.load(repo, p.stem) for p in sorted((Path(repo) / scenario.SCENARIOS).glob("*.toml"))}


def _outcomes(entry):
    """(kind, name, mode) -> status for every check and scenario mode."""
    found = {("check", n, None): c["status"] for n, c in entry.get("checks", {}).items()}
    for sid, run in entry.get("scenarios", {}).items():
        found.update({("scenario", sid, m): o["status"] for m, o in run["modes"].items()})
    return found


def _bound(role, m, expect, findings):
    """Stale findings when matrix `m` is not evidence for what `expect` names."""
    want = {"candidate": (expect["revision"], None, None),
            "base": (expect["base"], None, expect["overlay_sha256"])}[role]
    source = m.get("source") or {}
    have = (source.get("revision"), source.get("patch_sha256"), source.get("overlay_sha256"))
    if m.get("role") != role or have != want:
        findings.append(contracts.finding("evidence_stale", f"the {role} evidence is for {m.get('role')} "
                                          f"{have[0]} (patch {have[1]}, overlay {have[2]}), not {want[0]} "
                                          f"(overlay {want[2]})"))
    controller = m.get("controller") or {}
    if controller.get("head") != expect["controller"]["head"] or controller.get("scenarios_dirty"):
        findings.append(contracts.finding("evidence_stale", f"the {role} evidence ran scenarios of "
                                          f"{controller.get('head')}, not {expect['controller']['head']}"))
    for platform, entry in m.get("platforms", {}).items():
        if entry.get("status") != "ran":
            continue
        if entry.get("head") != want[0]:
            findings.append(contracts.finding("evidence_stale", f"{role} {platform} built {entry.get('head')}, "
                                              f"not {want[0]}"))
        if entry.get("baseline") != expect["baselines"].get(platform):
            findings.append(contracts.finding("evidence_stale", f"{role} {platform} ran on template "
                                              f"{entry.get('baseline')}, not the promoted "
                                              f"{expect['baselines'].get(platform)}"))


def _complete(role, m, expect, metas, exists, worker_status, findings):
    """Incomplete findings for a matrix that does not cover what is required."""
    for platform in expect["platforms"]:
        entry = m.get("platforms", {}).get(platform)
        if platform not in m.get("required_platforms", ()) or entry is None or entry.get("status") != "ran":
            why = (entry or {}).get("status", "not in the matrix")
            findings.append(contracts.finding("platform_missing", f"{role}: required platform {platform} did "
                                              f"not complete ({why})"))
            continue
        for name, _ in verify.CHECKS:
            status = entry["checks"].get(name, {}).get("status", "skipped")
            if status not in (PASS, FAIL):
                code = "timeout" if status == "timeout" else "skipped" if status == "skipped" \
                    else "outcome_incomplete"
                findings.append(contracts.finding(code, f"{role} {platform} check {name}: {status}"))
        for sid, meta in metas.items():
            if meta.get("issue") is None:
                continue
            if platform not in meta["platforms"]:
                if sid not in entry.get("not_applicable", ()):
                    findings.append(contracts.finding("skipped", f"{role} {platform}: {sid} does not apply here "
                                                      "but the matrix does not declare it not applicable"))
                continue
            run = entry.get("scenarios", {}).get(sid)
            for mode in meta["required_modes"]:
                outcome = (run or {}).get("modes", {}).get(mode)
                if outcome is None:
                    findings.append(contracts.finding("skipped", f"{role} {platform}: {sid} ({mode}) did not run"))
                    continue
                if outcome["status"] not in (PASS, FAIL):
                    code = "timeout" if outcome["status"] == "timeout" else "outcome_incomplete"
                    findings.append(contracts.finding(code, f"{role} {platform}: {sid} ({mode}) "
                                                      f"{outcome['status']}"))
                if not outcome.get("binaries_match"):
                    findings.append(contracts.finding("binary_mismatch", f"{role} {platform}: {sid} ({mode}) ran "
                                                      "a busybee this source did not build"))
                if role == "candidate" and verify.ui(meta) and not outcome.get("terminals"):
                    findings.append(contracts.finding("ui_evidence_missing", f"candidate {platform}: {sid} "
                                                      f"({mode}) drives a terminal but captured none"))
        if not entry.get("evidence") or not exists(entry["evidence"]):
            findings.append(contracts.finding("artifact_missing", f"{role} {platform}: public evidence "
                                              f"{entry.get('evidence')} is not on disk ({entry.get('collected')})"))
        status = worker_status(entry["run_id"])
        if entry.get("collected") not in ("success", "export_failed") or status != "destroyed":
            findings.append(contracts.finding("cleanup_failed", f"{role} {platform}: worker {entry['run_id']} "
                                              f"was not collected and destroyed ({entry.get('collected')}, "
                                              f"{status})"))


def _compare(issue, candidate, base, expect, metas, findings):
    """The regressions and pre-existing failures, adding their findings."""
    regressions, preexisting = [], []
    overlay = expect["overlay_sha256"]
    for platform in expect["platforms"]:
        c = candidate["platforms"].get(platform) or {}
        b = base["platforms"].get(platform) or {}
        mine, theirs = _outcomes(c), _outcomes(b)
        owned = set()
        for sid, meta in metas.items():
            if meta.get("issue") != issue or platform not in meta["platforms"]:
                continue
            for mode in meta["required_modes"]:
                key = ("scenario", sid, mode)
                owned.add(key)
                regressions.append({"scenario": sid, "platform": platform, "mode": mode,
                                    "base": theirs.get(key), "candidate": mine.get(key)})
        if overlay:
            key = ("check", "test", None)
            owned.add(key)
            regressions.append({"overlay": overlay, "platform": platform, "base": theirs.get(key),
                                "candidate": mine.get(key)})
        for kind, name, mode in sorted(set(mine) - owned, key=str):
            if mine[(kind, name, mode)] != FAIL:
                continue
            label = f"{platform} {name}" + (f" ({mode})" if mode else "")
            if theirs.get((kind, name, mode)) == FAIL:
                preexisting.append({"platform": platform, "kind": kind, "name": name, **({"mode": mode} if mode else {}),
                                    "status": FAIL})
                findings.append(contracts.finding("preexisting_failure", f"{label} fails on the base too", "warning"))
            else:
                findings.append(contracts.finding("new_failure", f"{label} fails on the candidate and not on "
                                                  "the base"))
    for r in regressions:
        what = f"{r.get('scenario') or 'overlay test'} on {r['platform']}"
        if r["base"] == PASS:
            findings.append(contracts.finding("regression_not_red_on_base", f"{what} passes on the base, so it "
                                              "does not show the failure this fix addresses"))
        if r["candidate"] == FAIL:
            findings.append(contracts.finding("regression_still_failing", f"{what} still fails on the candidate"))
    if not regressions:
        findings.append(contracts.finding("regression_undeclared", f"no scenario names #{issue} and no test "
                                          "overlay was given; there is no red base run to show", "warning"))
    return regressions, preexisting


def assess(issue, expect, candidate, base, metas, exists, worker_status):
    """The gate's verdict on two matrices. `expect` names the candidate, its
    base and overlay, controller, template baselines and required platforms;
    `exists(path)` and `worker_status(run_id)` look at the lab's state. Pure:
    the matrices are read, never changed."""
    findings = []
    for role, m in (("candidate", candidate), ("base", base)):
        if m is None:
            findings.append(contracts.finding("evidence_missing", f"there is no {role} matrix"))
            continue
        _bound(role, m, expect, findings)
        _complete(role, m, expect, metas, exists, worker_status, findings)
    regressions, preexisting = [], []
    if candidate is not None and base is not None:
        regressions, preexisting = _compare(issue, candidate, base, expect, metas, findings)
    errors = {f["code"] for f in findings if f["severity"] == "error"}
    if errors & set(STALE):
        verdict = "stale"
    elif errors - set(FAILED):
        verdict = "incomplete"
    elif errors:
        verdict = "failed"
    else:
        verdict = "preexisting_failures" if preexisting else "verified"
    return {"verdict": verdict, "findings": findings, "regressions": regressions, "preexisting": preexisting}


def _reusable(ops, role, expect, platforms):
    """The newest complete matrix already bound to exactly this evidence,
    whose public evidence is still on disk and whose workers are gone."""
    found = []
    exists = lambda rel: (ops.state / rel).exists()  # noqa: E731
    for path in (ops.state / "verifications").glob("*/matrix.json"):
        try:
            m = json.loads(path.read_text())
        except ValueError:
            continue
        problems = []
        _bound(role, m, expect, problems)
        if not problems:
            _complete(role, m, expect, ops.metas(), exists, ops.worker_status, problems)
        # A refused candidate is verified again on request, so a flaky check does
        # not stick to its head; a base, red on main by design, is reused.
        settled = ("verified",) if role == "candidate" else ("verified", "failed")
        if not [f for f in problems if f["severity"] == "error"] and m.get("verdict") in settled \
                and set(platforms) <= set(m.get("required_platforms", ())):
            found.append((m["id"], path))
    return max(found)[1] if found else None


def _matrix(ops, role, revision, overlay, expect, platforms):
    reused = _reusable(ops, role, expect, platforms)
    if reused:
        return reused, True
    done = ops.verify(revision, overlay, platforms, role)
    rel = done["data"].get("matrix")
    return (ops.state / rel if rel else None), False


def evaluate(ops, issue, revision, base, overlay=None, platforms=contracts.GUEST_OS):
    """Verify (or reuse) `revision` and its `base` (with `overlay`, a test
    patch) on `platforms`, then assess them; returns a contracts result."""
    platforms = list(platforms)
    expect = {"revision": revision, "base": base, "controller": ops.controller(),
              "overlay_sha256": hashlib.sha256(Path(overlay).read_bytes()).hexdigest() if overlay else None,
              "baselines": ops.baselines(platforms), "platforms": platforms}
    paths = {"candidate": _matrix(ops, "candidate", revision, None, expect, platforms),
             "base": _matrix(ops, "base", base, overlay, expect, platforms)}
    matrices = {role: json.loads(p.read_text()) if p and p.is_file() else None for role, (p, _) in paths.items()}
    result = assess(issue, expect, matrices["candidate"], matrices["base"], ops.metas(),
                    lambda rel: (ops.state / rel).exists(), ops.worker_status)
    gid = contracts.new_run_id()
    gdir = ops.state / "gates" / gid
    sides = {}
    for role, (path, reused) in paths.items():
        m = matrices[role]
        sides[role] = {"revision": expect["revision"] if role == "candidate" else base,
                       "overlay_sha256": expect["overlay_sha256"] if role == "base" else None,
                       "matrix": str(path.relative_to(ops.state)) if path else None, "reused": reused,
                       "matrix_verdict": m.get("verdict") if m else None,
                       "runs": [e["run_id"] for e in (m or {}).get("platforms", {}).values() if e.get("run_id")],
                       "outcomes": {p: {"checks": {n: c["status"] for n, c in e.get("checks", {}).items()},
                                        "scenarios": {s: {mode: o["status"] for mode, o in r["modes"].items()}
                                                      for s, r in e.get("scenarios", {}).items()},
                                        "not_applicable": e.get("not_applicable", []),
                                        "evidence": e.get("evidence")} if e.get("status") == "ran"
                                    else {"status": e.get("status")}
                                    for p, e in (m or {}).get("platforms", {}).items()}}
    # What the verdict rests on; the same evidence gives the same id.
    evidence_id = hashlib.sha256(json.dumps(
        {"issue": issue, "expect": expect, "matrices": {r: s["matrix"] for r, s in sides.items()},
         "verdict": result["verdict"]}, sort_keys=True).encode()).hexdigest()
    record = {"schema": SCHEMA, "id": gid, "issue": issue, "verdict": result["verdict"],
              "evidence_id": evidence_id, "controller": expect["controller"], "platforms": platforms,
              **sides, "regressions": result["regressions"], "preexisting": result["preexisting"],
              "findings": result["findings"]}
    evidence.durable(gdir / "gate.json", (json.dumps(record, indent=2) + "\n").encode())
    literals = {run: "run-id" for s in sides.values() for run in s["runs"]}
    literals.update({b: "baseline" for b in expect["baselines"].values() if b})
    literals.update({getpass.getuser(): "user", socket.gethostname().split(".")[0]: "host"})
    public = {k: v for k, v in record.items() if k not in ("id",)}
    for side in ("candidate", "base"):
        public[side] = {k: v for k, v in public[side].items() if k != "runs"}
    evidence.durable(gdir / "gate.public.json", evidence.Redactor(literals).json(
        (json.dumps(public, indent=2) + "\n").encode()))
    status = {"verified": "success", "preexisting_failures": "success", "failed": "product_failure"}.get(
        result["verdict"], "environment_failure")
    summary = f"#{issue} at {revision[:12]} against {base[:12]}: {result['verdict']}"
    return contracts.result("gate", status, summary, result["findings"], {
        "id": gid, "verdict": result["verdict"], "evidence_id": evidence_id,
        "gate": str((gdir / "gate.json").relative_to(ops.state)),
        "public": str((gdir / "gate.public.json").relative_to(ops.state))})


def comment(public):
    """The pull-request comment for a gate's public record: a marker line the
    CI review gate reads, a short table, and the record itself."""
    header = {"head": public["candidate"]["revision"], "base": public["base"]["revision"],
              "issue": public["issue"], "verdict": public["verdict"], "evidence_id": public["evidence_id"]}
    lines = [f"<!-- {MARKER} {json.dumps(header, sort_keys=True)} -->",
             f"**Lab evidence** for #{public['issue']} at `{header['head'][:12]}` against base "
             f"`{header['base'][:12]}`: **{public['verdict']}**", "",
             "| platform | candidate checks | candidate scenarios | base scenarios |", "|---|---|---|---|"]
    for platform in public["platforms"]:
        cells = []
        for side, what in (("candidate", "checks"), ("candidate", "scenarios"), ("base", "scenarios")):
            got = public[side]["outcomes"].get(platform, {})
            if what not in got:
                cells.append(got.get("status", "missing"))
            elif what == "checks":
                cells.append(", ".join(f"{n} {s}" for n, s in got["checks"].items()))
            else:
                cells.append(", ".join(f"{s} {m} {st}" for s, modes in got["scenarios"].items()
                                       for m, st in modes.items()) or "none")
        lines.append(f"| {platform} | " + " | ".join(cells) + " |")
    if public["preexisting"]:
        lines += ["", "Failing on the base too: " + ", ".join(
            f"{p['platform']} {p['name']}" + (f" ({p['mode']})" if p.get("mode") else "") for p in public["preexisting"])]
    errors = [f for f in public["findings"] if f["severity"] == "error"]
    if errors:
        lines += ["", *[f"- `{f['code']}`: {f['message']}" for f in errors]]
    lines += ["", "<details><summary>gate.public.json</summary>", "", "```json",
              json.dumps(public, indent=2), "```", "</details>"]
    return "\n".join(lines) + "\n"


class WorkerOps:
    """The gate's view of the lab: verify.run on real workers, the promoted
    baselines, the worker records and the controller's scenarios."""

    def __init__(self, workers):
        self.w, self.state = workers, workers.state

    def verify(self, revision, overlay, platforms, role):
        return verify.run(verify.WorkerOps(self.w), platforms, revision, None, self.state, overlay=overlay, role=role)

    def controller(self):
        return scenario._controller(self.w.repo)

    def baselines(self, platforms):
        return {p: (self.w.usable_baseline(p)[0] or {}).get("candidate") for p in platforms}

    def worker_status(self, run_id):
        record = worker._load(worker.run_dir(self.state, run_id) / "worker.json")
        return record["status"] if record else None

    def metas(self):
        return scenario_metas(self.w.repo)
