#!/usr/bin/env python3
"""Route current CI findings to Sortie; ignore approval/waiting bot chatter."""
import json
import os
from pathlib import Path
import subprocess
import sys

from reviews import GATE_MARKER, api, latest_gate_review, read_pr


def disposition(reviews, head):
    latest = latest_gate_review(reviews, head)
    if latest is None:
        return "handled"
    try:
        header = json.loads(latest["body"].splitlines()[0][len(GATE_MARKER):])
        if header["head"] != head:
            return "escalate"
        return {"BLOCKED": "dispatch-agent", "READY": "handled", "WAITING": "handled",
                "UNSURE": "escalate"}[header["verdict"]]
    except (ValueError, KeyError, TypeError):
        return "escalate"


def main():
    # Re-read live reviews so an old reaction cannot reopen a settled finding.
    try:
        scm = json.loads(Path(".sortie/scm.json").read_text())
        repo, number = f"{scm['owner']}/{scm['repo']}", scm["pr_number"]
        pr = read_pr(repo, number)
        reviews = api(f"repos/{repo}/pulls/{number}/reviews?per_page=100", pages=True)
        result = disposition(reviews, pr["head"]["sha"])
    except (ValueError, KeyError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"review-triage: cannot verify current CI review: {error}", file=sys.stderr)
        result = "escalate"
    print(f"CI review triage: {result}")
    Path(os.environ["SORTIE_REACTION_RESULT"]).write_text(json.dumps({"disposition": result}) + "\n")


if __name__ == "__main__":
    main()
