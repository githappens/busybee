#!/usr/bin/env python3
"""Route current CI findings to Sortie; ignore approval/waiting bot chatter."""
import json
import os
from pathlib import Path
import subprocess
import sys

from reviews import GATE_MARKER, api, read_pr


def disposition(reviews, head):
    matching = [r for r in reviews if r.get("commit_id") == head
                and r.get("user", {}).get("login") == "github-actions[bot]"
                and (r.get("body") or "").startswith(GATE_MARKER)]
    if not matching:
        return "handled"
    latest = max(matching, key=lambda r: r["id"])
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
