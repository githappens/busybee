#!/usr/bin/env python3
"""Actions controller: prepare read-only Claude sessions and retain their evidence."""
import argparse
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import zipfile

from reviews import (EFFORT, MODEL, SKILLS, api, checks_for, ci_errors,
                     collect_packet, input_id, lab_errors, paged_objects, publish_decision,
                     read_pr, result_errors, reviewable)

WORKFLOW = ".github/workflows/agent-review-gate.yml"
SCHEMA = {
    "type": "object", "additionalProperties": False,
    "required": ["head", "verdict", "findings", "report"],
    "properties": {
        "head": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
        "verdict": {"type": "string", "enum": ["READY", "BLOCKED", "UNSURE"]},
        "findings": {"type": "array", "maxItems": 20, "items": {"type": "string", "maxLength": 2000}},
        "report": {"type": "string", "minLength": 1, "maxLength": 12000},
    },
}


def output(name, value):
    with open(os.environ["GITHUB_OUTPUT"], "a") as target:
        target.write(f"{name}={value}\n")


def trusted_run(run, default_branch):
    # These events load policy from the default branch. A candidate-ref dispatch,
    # pull_request workflow, or artifact from another workflow is not authority.
    return (run.get("path") == WORKFLOW and run.get("head_branch") == default_branch
            and run.get("event") in ("schedule", "workflow_run", "issue_comment", "workflow_dispatch"))


def download(repo, artifact_id):
    result = subprocess.run(["gh", "api", f"repos/{repo}/actions/artifacts/{artifact_id}/zip"],
                            capture_output=True, timeout=60)
    if result.returncode:
        raise RuntimeError(result.stderr.decode().strip())
    return result.stdout


def read_record(data):
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        info = archive.getinfo("record.json")
        if info.file_size > 128000:
            raise ValueError("Oversized CI review artifact")
        return json.loads(archive.read(info))


def cached_record(repo, name, default_branch):
    artifacts = paged_objects(f"repos/{repo}/actions/artifacts?name={name}&per_page=100", "artifacts")
    for artifact in sorted(artifacts, key=lambda a: a["id"], reverse=True):
        run = api(f"repos/{repo}/actions/runs/{artifact['workflow_run']['id']}")
        if not trusted_run(run, default_branch):
            continue
        if artifact["expired"]:
            print("::notice::Prior review artifact expired; a fresh review is required")
            return None
        # Do not fall back to an older green artifact when the newest is corrupt.
        return read_record(download(repo, artifact["id"]))
    return None


def normalize(outcome, raw, session, head, digest):
    try:
        result = json.loads(raw)
        errors = result_errors(result, head)
    except ValueError:
        result, errors = {}, ["missing or malformed structured output"]
    if outcome != "success" or not session or errors:
        detail = "; ".join(errors) or "missing Action session ID"
        result = {"head": head, "verdict": "UNSURE", "findings": [],
                  "report": f"Claude Action outcome={outcome}: {detail}. Inspect the review run logs. "
                            "Resolve the environment/authentication or output error before retrying."}
    return {**{k: result[k] for k in ("head", "verdict", "findings", "report")},
            "session_id": session, "skill_sha256": digest, "model": MODEL, "effort": EFFORT}


def bundle(packet, results):
    return {"version": 2, **{k: packet[k] for k in ("repo", "pr", "issue", "head", "base")},
            "input_id": input_id(packet), "reviews": results,
            "run_url": f"https://github.com/{packet['repo']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"}


def trigger_allowed(repo):
    # Untrusted events must not spend allowance; the next scheduled run picks the PR up.
    actor = os.environ["GITHUB_ACTOR"]
    if api(f"users/{actor}")["type"] != "User":
        return False
    return api(f"repos/{repo}/collaborators/{actor}/permission")["permission"] in ("admin", "write")


def select(args):
    if not trigger_allowed(args.repo):
        print("::notice::Actor is not eligible for Claude review; leaving work for a trusted trigger")
        output("prs", "[]")
        return
    if args.pr:
        candidates = [read_pr(args.repo, args.pr)]
    else:
        candidates = api(f"repos/{args.repo}/pulls?state=open&per_page=100", pages=True)
    output("prs", json.dumps([pr["number"] for pr in candidates if reviewable(pr)]))


def review_needed(packet, record, checks):
    """Whether to start the model sessions: only for inputs with no settled
    record, current-head CI that passed, and, for a lab PR, accepted lab
    evidence. Unchanged inputs keep their judgement."""
    settled = isinstance(record, dict) and record.get("input_id") == input_id(packet)
    return (not settled and not ci_errors(packet["head"], checks) and not lab_errors(packet)
            and reviewable(packet["metadata"]))


def prepare(args):
    pr = read_pr(args.repo, args.pr)
    if not reviewable(pr):
        raise ValueError("PR is no longer eligible for review")
    packet = collect_packet(args.repo, args.pr)
    checks = checks_for(args.repo, packet["head"])
    packet["checks"] = checks
    name = f"busybee-reviews-{args.pr}-{input_id(packet)}"
    default = api(f"repos/{args.repo}")["default_branch"]
    record = cached_record(args.repo, name, default)
    args.directory.mkdir(parents=True, exist_ok=True)
    (args.directory / "packet.json").write_text(json.dumps(packet, indent=2) + "\n")
    (args.directory / "record.json").write_text(json.dumps(record, indent=2) + "\n")
    needed = record is None and review_needed(packet, record, checks)
    flags = ["--model", MODEL, "--effort", EFFORT, "--max-turns", "100", "--restricted",
             "--tools", "Read,Glob,Grep", "--allowedTools", "Read,Glob,Grep", "--permission-mode", "dontAsk",
             "--setting-sources", "user", "--disable-slash-commands", "--strict-mcp-config",
             "--json-schema", json.dumps(SCHEMA, separators=(",", ":"))]
    for key, value in (("needed", str(needed).lower()), ("head", packet["head"]),
                       ("artifact", name), ("claude_args", shlex.join(flags))):
        output(key, value)


def collect(args):
    packet = json.loads((args.directory / "packet.json").read_text())
    results = {}
    for skill, prefix in zip(SKILLS, ("CONTRACT", "PONYTAIL")):
        results[skill] = normalize(os.environ[f"{prefix}_OUTCOME"], os.environ.get(f"{prefix}_RESULT", ""),
                                  os.environ.get(f"{prefix}_SESSION", ""), packet["head"], packet["skill_sha256"][skill])
    record = bundle(packet, results)
    (args.directory / "record.json").write_text(json.dumps(record, indent=2) + "\n")
    for skill, result in results.items():
        if result["verdict"] == "UNSURE":
            print(f"::warning::{skill}: {result['report']}")


def publish(args):
    pr = read_pr(args.repo, args.pr)
    if not reviewable(pr):
        return
    packet = collect_packet(args.repo, args.pr)
    path = args.directory / "record.json"
    if path.exists():
        record = json.loads(path.read_text())
    else:
        # A failed prepare/action/upload job must reach Sortie's escalation path.
        print("::error::Review job did not produce its artifact; publishing UNSURE")
        record = bundle(packet, {s: normalize("missing-artifact", "", "", packet["head"], packet["skill_sha256"][s]) for s in SKILLS})
    publish_decision(packet, record, checks_for(args.repo, packet["head"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("select", "prepare", "collect", "publish"))
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--pr", type=int)
    parser.add_argument("--directory", type=Path, default=Path("build/ci-review"))
    args = parser.parse_args()
    if not args.repo or (args.operation != "select" and not args.pr):
        parser.error("repo and positive PR number required")
    globals()[args.operation](args)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError, zipfile.BadZipFile, subprocess.TimeoutExpired) as error:
        print(f"ci-reviews: {error}", file=sys.stderr)
        sys.exit(1)
