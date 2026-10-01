#!/usr/bin/env python3
"""Check lab task eligibility and release only merged prerequisites."""
import argparse
import subprocess
import sys

from reviews import api

REPO = "githappens/busybee"
MILESTONE = "agent lab: autonomous VM development"


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


def check(number):
    issue = api(f"repos/{REPO}/issues/{number}")
    reason = task_error(issue)
    if reason:
        raise ValueError(f"#{number}: {reason}")
    waiting = blockers(number)
    if waiting:
        raise ValueError(f"#{number}: prerequisites lack merged completion: {waiting}")
    print(f"#{number}: eligible; required prerequisites are merged")


def release():
    candidates = api(f"repos/{REPO}/issues?state=open&labels=sortie:ready&per_page=100", pages=True)
    for issue in candidates:
        if task_error(issue):
            continue
        number = issue["number"]
        waiting = blockers(number)
        labelled = "sortie" in {label["name"] for label in issue["labels"]}
        if waiting:
            if labelled:
                api(f"repos/{REPO}/issues/{number}/labels/sortie", "DELETE")
            print(f"#{number}: waiting for merged prerequisites {waiting}")
        elif not labelled:
            api(f"repos/{REPO}/issues/{number}/labels", "POST", {"labels": ["sortie"]})
            print(f"#{number}: released")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    task = sub.add_parser("check")
    task.add_argument("--issue", type=int, required=True)
    sub.add_parser("release")
    args = parser.parse_args()
    if args.operation == "check":
        if args.issue < 1:
            parser.error("issue must be positive")
        check(args.issue)
    else:
        release()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"lab: {error}", file=sys.stderr)
        sys.exit(1)
