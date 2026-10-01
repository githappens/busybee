#!/usr/bin/env python3
"""Read PR packets and gate lab handoff on explicit, current-head skill reports.

The gate verifies provenance and completeness, not the truth of an agent's
reasoning. Review reports are assertions by the trusted implementing agent.
Only the base-controlled Actions job uses the gate's write operation.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SKILLS = ("contract-review", "ponytail-review")
MILESTONE = "agent lab: autonomous VM development"
MARKER = "<!-- busybee-agent-review:v1 -->"
GATE_MARKER = "busybee-agent-review-gate:"
REQUIRED_CHECKS = ("ubuntu-latest", "macos-latest")


def command(argv):
    result = subprocess.run(argv, text=True, capture_output=True, timeout=60)
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
        result = subprocess.run(args + ["--input", "-"], input=json.dumps(data),
                                text=True, capture_output=True, timeout=60)
        if result.returncode:
            raise RuntimeError(result.stderr.strip())
        raw = result.stdout
    result = json.loads(raw) if raw.strip() else None
    return [entry for page in result for entry in page] if pages else result


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


def latest_receipt(pr, comments):
    eligible = [c for c in comments
                if c.get("user", {}).get("login") == pr["user"]["login"]
                and c.get("author_association") in ("OWNER", "MEMBER", "COLLABORATOR")
                and (c.get("body") or "").startswith(MARKER)]
    if not eligible:
        return None
    # A malformed newest report is not permission to use an older green one.
    body = max(eligible, key=lambda c: c["id"])["body"][len(MARKER):].strip()
    try:
        return json.loads(body)
    except ValueError:
        return None


def receipt_errors(pr, base, receipt, hashes):
    if not isinstance(receipt, dict):
        return ["Missing or malformed author review record"]
    if route(pr) != "lab":
        return ["PR does not belong to the lab workflow"]
    expected = {"version": 1, "repo": pr["base"]["repo"]["full_name"],
                "pr": pr["number"], "issue": int(pr["head"]["ref"].split("/")[1]),
                "head": pr["head"]["sha"], "base": base}
    errors = [f"Review record has stale or incorrect {key}" for key, value in expected.items()
              if type(receipt.get(key)) is not type(value) or receipt.get(key) != value]
    records = receipt.get("reviews")
    if not isinstance(records, dict) or set(records) != set(SKILLS):
        return errors + ["Both named skill reviews are required"]
    reviewers = []
    for name in SKILLS:
        review = records[name]
        if not isinstance(review, dict):
            errors.append(f"{name}: malformed review")
            continue
        if review.get("head") != expected["head"]:
            errors.append(f"{name}: stale reviewed head")
        if review.get("skill_sha256") != hashes[name]:
            errors.append(f"{name}: review did not use the trusted skill version")
        if review.get("verdict") != "READY" or review.get("findings") != []:
            errors.append(f"{name}: findings or uncertainty remain")
        for field in ("reviewer", "report"):
            if not isinstance(review.get(field), str) or not review[field].strip():
                errors.append(f"{name}: missing {field}")
        reviewers.append(review.get("reviewer"))
    if len(reviewers) == 2 and reviewers[0] == reviewers[1]:
        errors.append("Use a distinct fresh reviewer context for each skill")
    verification = receipt.get("verification")
    if not isinstance(verification, list) or not verification:
        errors.append("Missing verification evidence")
    else:
        for entry in verification:
            if (not isinstance(entry, dict) or entry.get("result") != "passed"
                    or not isinstance(entry.get("command"), str) or not entry["command"].strip()
                    or not isinstance(entry.get("evidence"), str) or not entry["evidence"].strip()):
                errors.append("Verification is incomplete or failed")
    return errors


def evaluate(pr, base, receipt, hashes, checks):
    errors = receipt_errors(pr, base, receipt, hashes)
    if pr.get("state") != "open" or pr.get("draft"):
        errors.append("PR must be open and ready for review")
    for name in REQUIRED_CHECKS:
        matching = [check for check in checks if check.get("name") == name
                    and check.get("head_sha") == pr["head"]["sha"]
                    and check.get("app", {}).get("slug") == "github-actions"]
        check = max(matching, key=lambda c: c["id"]) if matching else {}
        if check.get("status") != "completed" or check.get("conclusion") != "success":
            errors.append(f"CI {name} is missing, pending, skipped, or failed")
    return ("BLOCKED", errors) if errors else ("READY", [])


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


def packet(args):
    pr = read_pr(args.repo, args.pr)
    metadata = json.loads(command(["gh", "pr", "view", str(args.pr), "--repo", args.repo,
                                   "--json", "closingIssuesReferences"]))
    refs = metadata["closingIssuesReferences"]
    if args.issue:
        refs = [{"number": args.issue, "url": f"https://github.com/{args.repo}/issues/{args.issue}"}]
    issues = []
    for ref in refs:
        match = re.fullmatch(r"https://github.com/([^/]+/[^/]+)/issues/([0-9]+)", ref["url"])
        if not match:
            raise ValueError("Unsupported issue reference in PR contract")
        path = f"repos/{match[1]}/issues/{match[2]}"
        issue = api(path)
        if "pull_request" in issue:
            raise ValueError("Contract reference names another PR, not an issue")
        issues.append({"issue": issue, "comments": api(path + "/comments?per_page=100", pages=True)})
    base = merge_base(args.repo, pr)
    diff = command(["gh", "pr", "diff", str(args.pr), "--repo", args.repo])
    comments = api(f"repos/{args.repo}/issues/{args.pr}/comments?per_page=100", pages=True)
    live = read_pr(args.repo, args.pr)
    if (live["head"]["sha"], live["base"]["sha"]) != (pr["head"]["sha"], pr["base"]["sha"]):
        raise ValueError("PR changed while collecting the packet; collect it again")
    result = {"repo": args.repo, "pr": args.pr, "head": pr["head"]["sha"], "base": base,
              "base_ref": pr["base"]["ref"], "metadata": pr, "contracts": issues,
              "contract_source": "issues" if issues else "PR description only",
              "diff": diff, "prior_comments": comments, "skill_sha256": skill_hashes()}
    target = Path(args.output)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Saved packet for PR #{args.pr} at {result['head']} to {target}")


def publish(args):
    pr = read_pr(args.repo, args.pr)
    check_issue(args.repo, pr)
    if api("user")["login"] != pr["user"]["login"]:
        raise ValueError("The implementing PR author must publish the review record")
    receipt = json.loads(Path(args.file).read_text())
    errors = receipt_errors(pr, merge_base(args.repo, pr), receipt, skill_hashes())
    if errors:
        raise ValueError("; ".join(errors))
    path = f"repos/{args.repo}/issues/{args.pr}/comments"
    comments = api(path + "?per_page=100", pages=True)
    if read_pr(args.repo, args.pr)["head"]["sha"] != receipt["head"]:
        raise ValueError("PR head changed before publishing")
    if latest_receipt(pr, comments) != receipt:
        body = MARKER + "\n" + json.dumps(receipt, indent=2)
        if len(body.encode()) > 60000:
            raise ValueError("Review record is too large; keep a concise report and link detailed evidence")
        posted = api(path, "POST", {"body": body})
        print(posted["html_url"])
    else:
        print("Identical current-head review record already published")
    if args.ready:
        live = read_pr(args.repo, args.pr)
        if live["head"]["sha"] != receipt["head"]:
            raise ValueError("PR head changed; leave it draft and repeat the reviews")
        if live.get("draft"):
            print(command(["gh", "pr", "ready", str(args.pr), "--repo", args.repo]).strip())


def gate(args):
    pr = read_pr(args.repo, args.pr)
    if route(pr) != "lab":
        print("legacy: handled by the existing product gate")
        return
    check_issue(args.repo, pr)
    comments = api(f"repos/{args.repo}/issues/{args.pr}/comments?per_page=100", pages=True)
    # check-runs pagination wraps each page in an object, unlike issue lists.
    pages = json.loads(command(["gh", "api", f"repos/{args.repo}/commits/{pr['head']['sha']}/check-runs?per_page=100",
                               "--paginate", "--slurp"]))
    checks = [check for page in pages for check in page["check_runs"]]
    verdict, reasons = evaluate(pr, merge_base(args.repo, pr), latest_receipt(pr, comments), skill_hashes(), checks)
    print(json.dumps({"pr": args.pr, "head": pr["head"]["sha"], "verdict": verdict, "reasons": reasons}))
    if not args.apply or pr.get("draft") or pr["state"] != "open":
        return
    reviews = api(f"repos/{args.repo}/pulls/{args.pr}/reviews?per_page=100", pages=True)
    ours = [review for review in reviews if review.get("user", {}).get("login") == "github-actions[bot]"
            and review.get("commit_id") == pr["head"]["sha"]
            and (review.get("body") or "").startswith(GATE_MARKER)]
    latest = max(ours, key=lambda r: r["id"]) if ours else {}
    desired = "APPROVED" if verdict == "READY" else "CHANGES_REQUESTED"
    # Do not post repeat verdicts for unchanged evidence, or negative verdicts
    # while waiting for a first complete report/CI. Revoke our own prior green.
    if latest.get("state") == desired or (verdict != "READY" and not latest):
        return
    live = read_pr(args.repo, args.pr)
    if live["head"]["sha"] != pr["head"]["sha"] or live.get("draft") or live["state"] != "open":
        raise ValueError("PR changed before gate publication; no verdict posted")
    event = "APPROVE" if verdict == "READY" else "REQUEST_CHANGES"
    reason = "Both skill reviews and required CI passed." if not reasons else "; ".join(reasons)
    api(f"repos/{args.repo}/pulls/{args.pr}/reviews", "POST", {
        "event": event, "commit_id": pr["head"]["sha"],
        "body": f"{GATE_MARKER} {event} on {pr['head']['sha']}. {reason}",
    })


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    for name in ("packet", "publish", "gate", "route"):
        command_parser = sub.add_parser(name)
        command_parser.add_argument("--repo", required=True)
        command_parser.add_argument("--pr", required=True, type=int)
        if name == "packet":
            command_parser.add_argument("--issue", type=int)
            command_parser.add_argument("--output", required=True)
        elif name == "publish":
            command_parser.add_argument("--file", required=True)
            command_parser.add_argument("--ready", action="store_true")
        elif name == "gate":
            command_parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.pr < 1:
        parser.error("Expected OWNER/REPO and a positive PR number")
    if args.operation == "route":
        print(route(read_pr(args.repo, args.pr)))
    else:
        globals()[args.operation](args)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"reviews: {error}", file=sys.stderr)
        sys.exit(1)
