#!/usr/bin/env python3
"""Check lab task eligibility and release only merged prerequisites whose
worker capabilities are available. See docs/design/agent-lab.md §Agent sessions."""
import argparse
import functools
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from reviews import api

REPO = "githappens/busybee"
MILESTONE = "agent lab: autonomous VM development"
# The trusted snapshot this file runs from: its controller and guard policy.
ROOT = Path(__file__).resolve().parents[1]
# Every dispatched agent runs in a Linux worker through a controller session.
DEFAULT_CAPABILITIES = ("controller:session", "worker:linux")
REQUIRES = re.compile(r"^[\s*_]*lab requires:[\s*_]*(.+)$", re.IGNORECASE | re.MULTILINE)


def task_error(issue):
    labels = {label["name"] for label in issue.get("labels", [])}
    if (issue.get("pull_request") or issue.get("state") != "open"
            or (issue.get("milestone") or {}).get("title") != MILESTONE):
        return "not an open lab issue"
    if "epic" in labels or "needs-human" in labels:
        return "tracking or parked issue"
    if not labels.intersection({"sortie:ready", "sortie:working"}):
        return "issue has not been enabled for dispatch"
    return ""


def completed_by_merge(issue):
    return (issue.get("state") == "CLOSED" and issue.get("stateReason") == "COMPLETED"
            and any(pr.get("merged") and pr.get("baseRefName") == "main"
                    for pr in issue["closedByPullRequestsReferences"]["nodes"]))


def blocker_complete(number):
    query = """query($number: Int!, $cursor: String) {
      repository(owner: "githappens", name: "busybee") {
        issue(number: $number) { state stateReason
          closedByPullRequestsReferences(first: 100, after: $cursor) {
            nodes { merged baseRefName }
            pageInfo { hasNextPage endCursor }
          }
        }
      }
    }"""
    cursor = None
    while True:
        result = api("graphql", "POST", {"query": query, "variables": {"number": number, "cursor": cursor}})
        if result.get("errors"):
            raise RuntimeError(f"Cannot verify merged prerequisite #{number}: {result['errors']}")
        issue = result["data"]["repository"]["issue"]
        if issue is None:
            raise RuntimeError(f"Missing prerequisite #{number}")
        if completed_by_merge(issue):
            return True
        page = issue["closedByPullRequestsReferences"]["pageInfo"]
        if issue["state"] != "CLOSED" or not page["hasNextPage"]:
            return False
        cursor = page["endCursor"]


def blockers(number):
    linked = api(f"repos/{REPO}/issues/{number}/dependencies/blocked_by?per_page=100", pages=True)
    return [item["number"] for item in linked if not blocker_complete(item["number"])]


def required_capabilities(issue):
    """The defaults plus what the issue body declares on a `Lab requires:` line."""
    wanted = set(DEFAULT_CAPABILITIES)
    for line in REQUIRES.findall(issue.get("body") or ""):
        wanted.update(item.strip(" *_`") for item in line.split(",") if item.strip(" *_`"))
    return sorted(wanted)


def capabilities_from(doctor):
    """Capability -> "" when available, else why not, from `vmctl doctor --json`."""
    data = doctor.get("data", {})
    errors = "; ".join(f["message"] for f in doctor.get("findings", []) if f["severity"] == "error")
    found = {}
    for name in ("linux", "macos"):
        template = data.get("templates", {}).get(name)
        if template is None:
            found[f"worker:{name}"] = errors or f"no {name} template is configured"
        else:
            found[f"worker:{name}"] = "" if template["state"] == "ready" else \
                f"the {name} baseline is {template['state']}" + (f" ({errors})" if errors else "")
    for capability in data.get("controller", {}).get("capabilities", []):
        found[f"controller:{capability}"] = ""
    return found


@functools.cache
def available_capabilities():
    """Read-only: the trusted controller's doctor against the operator's lab state."""
    root = os.environ.get("BUSYBEE_LAB_ROOT")
    if not root:
        raise RuntimeError("BUSYBEE_LAB_ROOT must name the checkout that holds the lab state")
    done = subprocess.run([sys.executable, str(ROOT / "scripts/vm/vmctl.py"), "--root", root, "--json", "doctor"],
                          capture_output=True, text=True, timeout=120)
    try:
        return capabilities_from(json.loads(done.stdout))
    except ValueError as err:
        raise RuntimeError(f"the lab doctor gave no result: {done.stderr.strip()[-300:]}") from err


def capability_error(issue):
    available = available_capabilities()
    for capability in required_capabilities(issue):
        if capability not in available:
            return f"unsupported capability {capability}"
        if available[capability]:
            return f"capability {capability} is unavailable: {available[capability]}"
    return ""


def guard_policy():
    return json.loads((ROOT / "sortie/guard-policy.json").read_text())


def profile(issue, policy):
    """infrastructure only for an explicitly authorized issue; product otherwise."""
    labels = {label["name"] for label in issue.get("labels", [])}
    return "infrastructure" if labels.intersection(policy["infrastructure_labels"]) else "product"


def dispatch_error(issue):
    waiting = blockers(issue["number"])
    if waiting:
        return f"waiting for merged prerequisites {waiting}"
    return capability_error(issue)


def check(number):
    issue = api(f"repos/{REPO}/issues/{number}")
    reason = task_error(issue) or dispatch_error(issue)
    if reason:
        raise ValueError(f"#{number}: {reason}")
    print(f"#{number}: eligible; required prerequisites are merged and capabilities are available")


def release(dry_run=False):
    """Mark eligible ready lab issues for dispatch and unmark waiting ones. A dry
    run prints the same decisions and changes nothing."""
    candidates = api(f"repos/{REPO}/issues?state=open&labels=sortie:ready&per_page=100", pages=True)
    for issue in sorted(candidates, key=lambda i: i["number"]):
        if task_error(issue):
            continue
        number = issue["number"]
        reason = dispatch_error(issue)
        labelled = "sortie" in {label["name"] for label in issue["labels"]}
        if dry_run:
            print(f"#{number}: {reason or 'eligible'}")
        elif reason:
            if labelled:
                api(f"repos/{REPO}/issues/{number}/labels/sortie", "DELETE")
            print(f"#{number}: {reason}")
        elif not labelled:
            api(f"repos/{REPO}/issues/{number}/labels", "POST", {"labels": ["sortie"]})
            print(f"#{number}: released")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    task = sub.add_parser("check")
    task.add_argument("--issue", type=int, required=True)
    sub.add_parser("release").add_argument("--dry-run", action="store_true",
                                           help="print each ready issue's decision; change nothing")
    sub.add_parser("profile", help="print the issue's execution profile").add_argument("--issue", type=int,
                                                                                        required=True)
    args = parser.parse_args()
    if getattr(args, "issue", 1) < 1:
        parser.error("issue must be positive")
    if args.operation == "check":
        check(args.issue)
    elif args.operation == "profile":
        print(profile(api(f"repos/{REPO}/issues/{args.issue}"), guard_policy()))
    else:
        release(args.dry_run)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"lab: {error}", file=sys.stderr)
        sys.exit(1)
