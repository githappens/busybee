"""Verify one source identity on every required platform: the platform matrix.

See docs/design/agent-lab.md §Run an issue from reproduction to review, step 5.
For each platform in turn, a fresh worker
of the promoted baseline gets the revision (and patch), builds it, and runs
the repository's required checks (fmt, clippy, workspace tests) and every
applicable regression scenario in its required fixture modes. The matrix
records, per platform, the checkout's head, `busybee --version`, the built
binaries' digests and every outcome, and is written to
`verifications/<id>/matrix.json` (VERIFICATION_SCHEMA).

The verdict is `verified` only when every platform ran every check and
scenario on the same source and they all passed. A platform whose baseline
is unusable, a check that could not run (a tool missing, a timeout, a
worker that did not answer), a head or binary that does not match the
source, or evidence that was not collected makes it `incomplete`; nothing
substitutes for the missing platform. A product failure, such as a known
regression's red scenario, keeps it `failed`. Scenarios that cover no issue
check the harness itself and are not part of the matrix.

A matrix is bound to what produced it, so the evidence gate (gate.py) can
tell current evidence from stale: its role (`candidate`, or `base` for the
red run), the source revision with the candidate's patch or, for a base, the
test overlay recorded apart from the product revision, the controller and
scenario revision, and per platform the template baseline the worker came
from. A scenario that drives a terminal is marked `ui`, with the terminals
each run captured.
"""
import getpass
import json
from pathlib import Path
import socket

import contracts
import evidence
import guest
import parallels
import scenario
import screen
import template
import worker

SCHEMA = "busybee.vm.verification/v1"
ROLES = ("candidate", "base")
# The repository's required checks (CI's), in the checkout's development shell.
CHECKS = (("build", ["cargo", "build", "--workspace", "--bins"]),
          ("fmt", ["cargo", "fmt", "--all", "--check"]),
          ("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
          ("test", ["cargo", "test", "--workspace"]))
BUSYBEE = "build/debug/busybee"
# A shell's "command not found": the check never ran.
NOT_FOUND = 127
# A worker that stops serving mid-run: expired, unreachable, out of time.
LOST = (worker.Refused, guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded)


def applicable(repo, platform):
    """The regression scenarios for `platform`, and those declared elsewhere."""
    runs, elsewhere = {}, []
    for path in sorted((Path(repo) / scenario.SCENARIOS).glob("*.toml")):
        meta = scenario.load(repo, path.stem)
        if meta.get("issue") is None:
            continue
        if platform in meta["platforms"]:
            runs[meta["id"]] = meta
        else:
            elsewhere.append(meta["id"])
    return runs, elsewhere


def _platform(ops, platform, revision, patch, findings):
    """Run one platform's checks and scenarios; returns its matrix entry."""
    created = ops.create(platform, revision, patch)
    if created["status"] != "success":
        findings.append(contracts.finding("platform_unavailable", f"{platform}: no worker: "
                                          + "; ".join(f["message"] for f in created["findings"])))
        return {"status": "unavailable", "reason": created["findings"]}
    run_id = created["data"]["run_id"]
    entry = {"status": "ran", "run_id": run_id, "baseline": ops.baseline_of(run_id), "head": None, "version": None, "binaries": {}, "checks": {},
             "scenarios": {}, "not_applicable": [], "collected": None, "evidence": None}
    try:
        _checks(ops, platform, run_id, entry, findings)
    except LOST as err:  # what ran so far is kept; the platform is not verified
        entry["status"] = "interrupted"
        findings.append(contracts.finding("platform_interrupted", f"{platform}: worker {run_id} stopped serving: "
                                          f"{worker._reason(err)}"))
    _dispose(ops, platform, run_id, entry, findings)
    return entry


def _checks(ops, platform, run_id, entry, findings):
    cwd = ops.checkout(run_id)
    for name, argv in CHECKS:
        done = ops.exec(run_id, ["nix", "develop", "-c", *argv], cwd)
        data = done["data"]
        status = done["status"]
        if data.get("exit_code") == NOT_FOUND:
            status = "environment_failure"
            findings.append(contracts.finding("tool_missing", f"{platform} {name}: a command was not found"))
        entry["checks"][name] = {"status": status, "exec": data.get("exec"), "exit_code": data.get("exit_code"),
                                 "elapsed_s": data.get("elapsed_s")}
        provenance = data.get("provenance") or {}
        if name == "build":
            entry["head"] = provenance.get("head")
    version = ops.exec(run_id, [BUSYBEE, "--version"], cwd)
    if version["status"] == "success":
        entry["version"] = version["stdout"].strip()
    # `cargo test` can relink the binaries with test-time features; the
    # scenarios run what is on disk now, which this exec's provenance records.
    entry["binaries"] = (version["data"].get("provenance") or {}).get("binaries", {})
    runs, entry["not_applicable"] = applicable(ops.repo, platform)
    for scenario_id, meta in runs.items():
        modes = {}
        for mode in meta["required_modes"]:
            done = ops.scenario(run_id, scenario_id, mode)
            modes[mode] = {"status": done["status"], "exec": done["exec"], "failed": done["failed"],
                           "binaries_match": done["busybee_sha256"] == entry["binaries"].get(BUSYBEE),
                           "terminals": done["terminals"]}
        entry["scenarios"][scenario_id] = {"issue": meta["issue"], "ui": ui(meta), "modes": modes}


def ui(meta):
    """Whether a scenario observes the interface through a real terminal."""
    return "zellij" in meta.get("tools", {})


def _dispose(ops, platform, run_id, entry, findings):
    """Collect and destroy the worker, then export its public evidence."""
    try:
        destroyed = ops.destroy(run_id)
    except LOST as err:
        destroyed = {"status": "environment_failure", "summary": worker._reason(err)}
    entry["collected"] = destroyed["status"]
    if destroyed["status"] == "success":
        try:
            entry["evidence"] = ops.export(run_id)
        except (OSError, ValueError, screen.RecordingError, screen.RenderError) as err:
            entry["collected"] = "export_failed"
            findings.append(contracts.finding("evidence_incomplete", f"{platform}: the public evidence of {run_id} "
                                              f"could not be exported: {worker._reason(err)}"))
    else:
        findings.append(contracts.finding("evidence_incomplete", f"{platform}: worker {run_id} was not collected "
                                          f"and destroyed: {destroyed['status']}"))


def verdict(matrix, findings):
    """`verified`, `failed` or `incomplete`, adding a finding for each reason."""
    source = matrix["source"]
    statuses, versions = [], set()
    for platform, entry in matrix["platforms"].items():
        if entry["status"] == "unavailable":
            statuses.append("environment_failure")
            continue
        if entry["status"] != "ran":
            statuses.append("environment_failure")
        if entry["head"] != source["revision"]:
            findings.append(contracts.finding("revision_mismatch", f"{platform} built {entry['head']}, not "
                                              f"{source['revision']} (patch {source['patch_sha256']})"))
            statuses.append("environment_failure")
        versions.add(entry["version"])
        statuses += [c["status"] for c in entry["checks"].values()]
        for scenario_id, run in entry["scenarios"].items():
            for mode, outcome in run["modes"].items():
                statuses.append(outcome["status"])
                if not outcome["binaries_match"]:
                    findings.append(contracts.finding("binary_mismatch", f"{platform} {scenario_id} ({mode}) ran "
                                                      "a busybee that is not the one this source built"))
                    statuses.append("environment_failure")
        if entry["collected"] != "success":
            statuses.append("environment_failure")
    if len(versions) > 1 or None in versions:
        findings.append(contracts.finding("version_mismatch", f"busybee --version differs or is missing: "
                                          f"{sorted(map(str, versions))}"))
        statuses.append("environment_failure")
    if any(s not in ("success", "product_failure") for s in statuses):
        return "incomplete"
    return "failed" if "product_failure" in statuses else "verified"


def run(ops, platforms, revision, patch, out_dir, overlay=None, role="candidate"):
    """Verify `revision` (and `patch`) on `platforms`; `ops` reaches the workers.
    A `base` run takes a test `overlay` instead of a patch, recorded apart."""
    if role not in ROLES or (overlay and (patch or role != "base")):
        raise ValueError("an overlay belongs to a base run without a patch")
    vid = contracts.new_run_id()
    path = Path(out_dir) / "verifications" / vid / "matrix.json"
    findings = []
    digest = lambda f: worker._sha256(Path(f).read_bytes()) if f else None  # noqa: E731
    source = {"revision": revision, "patch_sha256": digest(patch), "overlay_sha256": digest(overlay)}
    matrix = {"schema": SCHEMA, "id": vid, "role": role, "source": source, "controller": ops.controller(),
              "required_platforms": platforms, "platforms": {}, "verdict": None}
    unusable = {p: ops.baseline(p) for p in platforms}
    for platform, problems in unusable.items():
        if problems:
            matrix["platforms"][platform] = {"status": "unavailable", "reason": problems}
            findings.append(contracts.finding("platform_missing", f"{platform} is required but unavailable: "
                                              + "; ".join(p["message"] for p in problems)))
    if not findings:
        for platform in platforms:
            matrix["platforms"][platform] = _platform(ops, platform, revision, patch or overlay, findings)
    matrix["verdict"] = verdict(matrix, findings)
    matrix["findings"] = findings
    evidence.durable(path, (json.dumps(matrix, indent=2) + "\n").encode())
    # For publishing: run identities and machine values become placeholders.
    runs = {e["run_id"]: "run-id" for e in matrix["platforms"].values() if e.get("run_id")}
    runs.update({e["baseline"]: "baseline" for e in matrix["platforms"].values() if e.get("baseline")})
    runs.update({getpass.getuser(): "user", socket.gethostname().split(".")[0]: "host"})
    public = path.with_name("matrix.public.json")
    evidence.durable(public, evidence.Redactor(runs).json(path.read_bytes()))
    status = {"verified": "success", "failed": "product_failure", "incomplete": "environment_failure"}[
        matrix["verdict"]]
    summary = f"{revision[:12]} on {', '.join(platforms)}: {matrix['verdict']}"
    return contracts.result("verify", status, summary, findings, {
        "id": vid, "matrix": str(path.relative_to(out_dir)), "public": str(public.relative_to(out_dir)),
        "verdict": matrix["verdict"]})


class WorkerOps:
    """The matrix's view of real workers: worker.Workers and scenario.run."""

    def __init__(self, workers):
        self.w, self.repo = workers, workers.repo

    def baseline(self, platform):
        if platform not in self.w.config.get("templates", {}):
            return [contracts.finding("baseline_missing", f"no [templates.{platform}] in the local config")]
        return self.w.usable_baseline(platform)[2]

    def create(self, platform, revision, patch):
        return self.w.create(platform, revision, patch)

    def checkout(self, run_id):
        return self.w.checkout(self.w._owned(run_id)[1])

    def baseline_of(self, run_id):
        return self.w._owned(run_id)[1]["candidate"]

    def controller(self):
        return scenario._controller(self.repo)

    def exec(self, run_id, argv, cwd):
        # As long as a scenario may take: a workspace build or test run is not one command's worth.
        done = self.w.exec(run_id, argv, cwd, {}, self.w.config["deadlines"]["scenario"])
        out = self.w.state / done["data"]["stdout"] if "stdout" in done["data"] else None
        return {**done, "stdout": out.read_text(errors="replace") if out and out.is_file() else ""}

    def scenario(self, run_id, scenario_id, mode):
        done = scenario.run(self.w, run_id, scenario_id, mode)
        record = json.loads((self.w.state / done["data"]["path"]).read_text()) if done["data"]["path"] else {}
        tools = ((record.get("result") or {}).get("preflight") or {}).get("tools", {})
        return {"status": done["status"], "exec": done["data"]["exec"],
                "failed": [a["name"] for a in done["data"]["assertions"] if a["status"] == "failed"],
                "busybee_sha256": tools.get("busybee", {}).get("sha256"),
                "terminals": sorted((record.get("terminals") or {}).keys())}

    def destroy(self, run_id):
        return self.w.destroy(run_id)

    def export(self, run_id):
        return self.w.export(run_id)["data"]["path"]
