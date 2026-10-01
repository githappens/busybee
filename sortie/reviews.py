#!/usr/bin/env python3
"""Collect PR contracts and publish deterministic decisions from CI-owned reviews."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SKILLS = ("contract-review", "ponytail-review")
MILESTONE = "agent lab: autonomous VM development"
MODEL = "claude-opus-5-5"
EFFORT = "high"
GATE_MARKER = "busybee-agent-review-gate:v2 "
REQUIRED_CHECKS = ("ubuntu-latest", "macos-latest")


def command(argv, stdin=None):
    result = subprocess.run(argv, input=stdin, text=True, capture_output=True, timeout=60)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"Command failed: {argv[0]}")
    return result.stdout


def api(path, method="GET", data=None, pages=False):
    args = ["gh", "api", "--method", method, path]
    if pages:
        args += ["--paginate", "--slurp"]
    if data is None:
        raw = command(args)
    else:
        raw = command(args + ["--input", "-"], json.dumps(data))
    result = json.loads(raw) if raw.strip() else None
    return [entry for page in result for entry in page] if pages else result


def paged_objects(path, key):
    pages = json.loads(command(["gh", "api", path, "--paginate", "--slurp"]))
    return [entry for page in pages for entry in page[key]]


def route(pr):
    head, base = pr.get("head", {}), pr.get("base", {})
    if (re.fullmatch(r"sortie-lab/[1-9][0-9]*", head.get("ref", ""))
            and head.get("repo") and base.get("repo")
            and head["repo"]["full_name"] == base["repo"]["full_name"]):
        return "lab"
    return "legacy"


def skill_hashes():
    return {name: hashlib.sha256((ROOT / "skills" / name / "SKILL.md").read_bytes()).hexdigest()
            for name in SKILLS}


def policy_hash():
    files = ("sortie/reviews.py", "sortie/ci_reviews.py", ".github/workflows/agent-review-gate.yml",
             "AGENTS.md", "CLAUDE.md", "docs/development/agent-review.md")
    return hashlib.sha256(b"\0".join((ROOT / name).read_bytes() for name in files)).hexdigest()


def human_comments(comments, author=None):
    return [{"id": c["id"], "author": c["user"]["login"], "body": c.get("body")}
            for c in comments if c.get("user", {}).get("type") != "Bot"
            and (c["user"]["login"] == author if author else
                 c.get("author_association") in ("OWNER", "MEMBER", "COLLABORATOR"))]


def input_id(packet):
    pr = packet["metadata"]
    data = {k: packet[k] for k in ("repo", "pr", "issue", "head", "base", "skill_sha256", "policy_sha256")}
    data.update(title=pr["title"], body=pr.get("body"),
                dispositions=human_comments(packet["prior_comments"], pr["user"]["login"]),
                contracts=[{"issue": {k: c["issue"].get(k) for k in ("number", "title", "body")},
                            "comments": human_comments(c["comments"])} for c in packet["contracts"]])
    return hashlib.sha256(json.dumps(data, sort_keys=True).encode()).hexdigest()


def result_errors(result, head):
    if not isinstance(result, dict):
        return ["missing structured review output"]
    errors = []
    if result.get("head") != head:
        errors.append("stale reviewed head")
    verdict, findings = result.get("verdict"), result.get("findings")
    if verdict not in ("READY", "BLOCKED", "UNSURE"):
        errors.append("invalid verdict")
    if (not isinstance(findings, list) or len(findings) > 20
            or any(not isinstance(f, str) or not f.strip() or len(f) > 2000 for f in findings)):
        errors.append("invalid findings")
    elif (verdict == "READY" and findings) or (verdict == "BLOCKED" and not findings):
        errors.append("verdict contradicts findings")
    if not isinstance(result.get("report"), str) or not 0 < len(result["report"].strip()) <= 12000:
        errors.append("missing or oversized report")
    return errors


def record_errors(packet, record):
    if not isinstance(record, dict):
        return ["Missing or malformed CI review artifact"]
    expected = {k: packet[k] for k in ("repo", "pr", "issue", "head", "base")}
    expected.update(version=2, input_id=input_id(packet))
    errors = [f"Review artifact has stale or incorrect {k}" for k, v in expected.items()
              if type(record.get(k)) is not type(v) or record.get(k) != v]
    records = record.get("reviews")
    if not isinstance(records, dict) or set(records) != set(SKILLS):
        return errors + ["Both named skill reviews are required"]
    sessions = []
    for skill in SKILLS:
        result = records[skill]
        errors += [f"{skill}: {e}" for e in result_errors(result, packet["head"])]
        if not isinstance(result, dict):
            continue
        for k, v in (("skill_sha256", packet["skill_sha256"][skill]), ("model", MODEL), ("effort", EFFORT)):
            if result.get(k) != v:
                errors.append(f"{skill}: incorrect {k}")
        session = result.get("session_id")
        if not isinstance(session, str) or not session.strip():
            errors.append(f"{skill}: missing Action session ID")
        sessions.append(session)
    if len(sessions) == 2 and sessions[0] == sessions[1]:
        errors.append("Each skill requires a separate Claude session")
    return errors


def ci_errors(head, checks):
    errors = []
    for name in REQUIRED_CHECKS:
        matching = [c for c in checks if c.get("name") == name and c.get("head_sha") == head
                    and c.get("app", {}).get("slug") == "github-actions"]
        check = max(matching, key=lambda c: c["id"]) if matching else {}
        if check.get("status") != "completed" or check.get("conclusion") != "success":
            errors.append(f"CI {name} is missing, pending, skipped, or failed")
    return errors


def evaluate(packet, record, checks):
    pr = packet["metadata"]
    if isinstance(record, dict) and any(record.get(k) != packet[k] for k in ("head", "base")):
        return "WAITING", ["PR changed while review ran; waiting for the new head's evidence"]
    if isinstance(record, dict) and record.get("input_id") != input_id(packet):
        return "WAITING", ["Review inputs changed; waiting for current evidence"]
    errors = record_errors(packet, record)
    if errors:
        return ("WAITING" if record is None else "UNSURE"), errors
    for verdict in ("UNSURE", "BLOCKED"):
        if any(r["verdict"] == verdict for r in record["reviews"].values()):
            return verdict, [f"{s}: {r['report']}" for s, r in record["reviews"].items() if r["verdict"] == verdict]
    errors = ci_errors(packet["head"], checks)
    if route(pr) != "lab" or pr.get("state") != "open" or pr.get("draft"):
        errors.append("PR must be an open, ready lab PR")
    return ("WAITING", errors) if errors else ("READY", [])


def read_pr(repo, number):
    return api(f"repos/{repo}/pulls/{number}")


def merge_base(repo, pr):
    comparison = api(f"repos/{repo}/compare/{pr['base']['sha']}...{pr['head']['sha']}")
    return comparison["merge_base_commit"]["sha"]


def check_issue(repo, pr):
    if route(pr) != "lab":
        raise ValueError("Lab PRs use a same-repository sortie-lab/ISSUE branch")
    number = int(pr["head"]["ref"].split("/")[1])
    issue = api(f"repos/{repo}/issues/{number}")
    if (issue.get("pull_request") or (issue.get("milestone") or {}).get("title") != MILESTONE
            or "epic" in [label["name"] for label in issue.get("labels", [])]):
        raise ValueError("Branch issue is not an implementation task in the lab milestone")
    return issue


def collect_packet(repo, number, issue=None):
    pr = read_pr(repo, number)
    if issue:
        refs = [{"url": f"https://github.com/{repo}/issues/{issue}"}]
    else:
        refs = json.loads(command(["gh", "pr", "view", str(number), "--repo", repo,
                                   "--json", "closingIssuesReferences"]))["closingIssuesReferences"]
    issues = []
    for ref in refs:
        match = re.fullmatch(r"https://github.com/([^/]+/[^/]+)/issues/([0-9]+)", ref["url"])
        if not match:
            raise ValueError("Unsupported issue reference in PR contract")
        path = f"repos/{match[1]}/issues/{match[2]}"
        contract = api(path)
        if "pull_request" in contract:
            raise ValueError("Contract reference names another PR, not an issue")
        issues.append({"issue": contract, "comments": api(path + "/comments?per_page=100", pages=True)})
    base = merge_base(repo, pr)
    diff = command(["gh", "pr", "diff", str(number), "--repo", repo])
    comments = api(f"repos/{repo}/issues/{number}/comments?per_page=100", pages=True)
    prior = api(f"repos/{repo}/pulls/{number}/reviews?per_page=100", pages=True)
    live = read_pr(repo, number)
    if (live["head"]["sha"], live["base"]["sha"]) != (pr["head"]["sha"], pr["base"]["sha"]):
        raise ValueError("PR changed while collecting the packet; collect it again")
    return {"repo": repo, "pr": number, "issue": issue, "head": pr["head"]["sha"], "base": base,
            "metadata": pr, "contracts": issues, "contract_source": "issues" if issues else "PR description only",
            "diff": diff, "prior_comments": comments, "prior_reviews": prior,
            "skill_sha256": skill_hashes(), "policy_sha256": policy_hash()}


def checks_for(repo, head):
    return paged_objects(f"repos/{repo}/commits/{head}/check-runs?per_page=100", "check_runs")


def publish_decision(packet, record, checks):
    repo, number, head = packet["repo"], packet["pr"], packet["head"]
    verdict, reasons = evaluate(packet, record, checks)
    pr = packet["metadata"]
    if pr.get("draft") or pr["state"] != "open":
        return
    existing = api(f"repos/{repo}/pulls/{number}/reviews?per_page=100", pages=True)
    ours = [r for r in existing if r.get("user", {}).get("login") == "github-actions[bot]"
            and r.get("commit_id") == head and (r.get("body") or "").startswith(GATE_MARKER)]
    latest = max(ours, key=lambda r: r["id"]) if ours else {}
    # Waiting is not a code defect. Only publish it to revoke an earlier approval.
    if verdict == "WAITING" and latest.get("state") != "APPROVED":
        return
    event = "APPROVE" if verdict == "READY" else "REQUEST_CHANGES"
    header = {"head": head, "input_id": input_id(packet), "verdict": verdict}
    evidence = {"reviews": record.get("reviews") if isinstance(record, dict) else None, "reasons": reasons}
    header["evidence_id"] = hashlib.sha256(json.dumps(evidence, sort_keys=True).encode()).hexdigest()
    lines = [GATE_MARKER + json.dumps(header, sort_keys=True), "", f"**{verdict}** — Opus 5.5, high effort."]
    if isinstance(record, dict) and isinstance(record.get("reviews"), dict):
        for skill, result in record["reviews"].items():
            if isinstance(result, dict):
                lines += ["", f"### {skill}: {result.get('verdict', 'UNSURE')}", str(result.get("report", ""))]
                if isinstance(result.get("findings"), list):
                    lines += [f"- {f}" for f in result["findings"]]
        if record.get("run_url"):
            lines += ["", f"[CI review run]({record['run_url']})"]
    if reasons and (verdict == "WAITING" or record_errors(packet, record)):
        lines += ["", "Gate: " + "; ".join(reasons)]
    body = "\n".join(lines)
    if len(body.encode()) > 60000:
        raise ValueError("Review report is too large for GitHub; no verdict posted")
    desired = "APPROVED" if event == "APPROVE" else "CHANGES_REQUESTED"
    if latest.get("state") == desired and latest.get("body", "").splitlines()[0] == lines[0]:
        return
    live = read_pr(repo, number)
    if live["head"]["sha"] != head or live.get("draft") or live["state"] != "open":
        raise ValueError("PR changed before gate publication; no verdict posted")
    api(f"repos/{repo}/pulls/{number}/reviews", "POST", {"event": event, "commit_id": head, "body": body})


def write_handoff(pr, sha, branch, target):
    if (route(pr) != "lab" or pr["state"] != "open" or pr.get("draft")
            or pr["head"]["sha"] != sha or pr["head"]["ref"] != branch):
        raise ValueError("Handoff requires the local branch/head to match an open, ready, pushed lab PR")
    owner, repo = pr["base"]["repo"]["full_name"].split("/")
    scm = {"branch": branch, "sha": sha, "pr_number": pr["number"], "owner": owner, "repo": repo,
           "pushed_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")}
    target.mkdir(parents=True, exist_ok=True)
    (target / "scm.json").write_text(json.dumps(scm, indent=2) + "\n")
    (target / "status").write_text("needs-human-review\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    for name in ("packet", "route", "handoff"):
        p = sub.add_parser(name)
        p.add_argument("--repo", required=True)
        p.add_argument("--pr", required=True, type=int)
        if name == "packet":
            p.add_argument("--issue", type=int)
            p.add_argument("--output", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.pr < 1:
        parser.error("Expected OWNER/REPO and a positive PR number")
    if args.operation == "route":
        print(route(read_pr(args.repo, args.pr)))
        return
    if args.operation == "handoff":
        pr = read_pr(args.repo, args.pr)
        check_issue(args.repo, pr)
        write_handoff(pr, command(["git", "rev-parse", "HEAD"]).strip(),
                      command(["git", "branch", "--show-current"]).strip(), Path(".sortie"))
        print(f"Handed PR #{args.pr} at {pr['head']['sha']} to Sortie and CI review")
        return
    if args.operation == "packet":
        result = collect_packet(args.repo, args.pr, args.issue)
        target = Path(args.output)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(result, indent=2) + "\n")
        print(f"Saved PR #{args.pr} at {result['head']} to {target}")
        return


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"reviews: {error}", file=sys.stderr)
        sys.exit(1)
