# Autonomous PR review and handoff

Sortie owns issue dispatch, continuation, CI feedback, and merge completion.
The implementing agent owns implementation and the review/fix loop. These are
separate from the [VM lab](../design/agent-lab.md), whose worker controller is
still being implemented.

The lab uses `sortie/LAB_WORKFLOW.md`. Existing product work retains
`sortie/WORKFLOW.md` until migrated. Same-repository branches named
`sortie-lab/<issue-number>` belong exclusively to the lab review gate; the old
Codex gate skips them. The issue must belong to the lab milestone. This routing
does not depend on the implementing agent being Claude, Codex, or Cursor.

## Shared skills

The canonical skills are:

- [contract-review](../../skills/contract-review/SKILL.md): does this delta
  satisfy its contract without introducing a concrete P0/P1 failure?
- [ponytail-review](../../skills/ponytail-review/SKILL.md): what unnecessary
  complexity can be removed while preserving that contract?

Discovery aliases expose the same files under `.agents/skills/` for Codex and
`.claude/skills/` for Claude and Cursor. Every runner can also read the canonical
files directly. Explicit paths avoid a personal skill with the same name
overriding the repository's version. No plugin, personal path, or external
project configuration is required.

## One issue through review

1. Read the issue, applicable specification, `AGENTS.md`, and `CLAUDE.md`.
   Implement only the assigned scope and its named regressions. Infrastructure
   issues may change the runner/controller files their scope explicitly names;
   ordinary product issues do not acquire that authority.
2. Commit and push the implementation on `sortie-lab/<issue>`, then open or
   reuse a **draft PR**. Include `Closes #<issue>` and a concrete description of
   the failure, new behavior, and verification. The PR's actual base is the
   comparison target, even when it is not the default branch.
3. Collect the PR packet with the trusted `sortie/reviews.py packet` helper.
   Give each skill a distinct fresh reviewer context: the skill, packet,
   repository rules, relevant specification/code, and verification evidence.
   Do not include the author's proposed verdict. Reviewers can be subagents or
   separate CLI sessions; a second pass in the author's existing context does
   not meet this workflow's review requirement.
4. Both reviewers return reports without modifying the PR. The implementing
   agent fixes genuine in-scope findings, runs the relevant tests, and pushes.
   Record a concrete reason for declining a finding; a bare acknowledgement or
   promise is not a resolution. The appropriate reviewer must settle the
   disposition. Unrelated improvements remain outside this PR.
5. After a push, both final records must name the new head. Follow-up reviews
   verify fixes and new changes, preserving already settled decisions. Do not
   restart discovery over unchanged code. Work within the issue's assigned
   deadline and turn budget; if review cannot converge, retain reports and
   work, identify the exact blocker, and leave the PR draft. Never manufacture
   a clean verdict to exhaust a loop.
6. Assemble the completion record below, publish it with the trusted helper,
   and mark the PR ready. Required CI must pass on the current head before the
   gate approves. Sortie then uses its existing automerge implementation and
   repository protections; the implementing agent never merges.
7. A CI failure, review finding, or merge conflict resumes the same issue and
   PR. New commits invalidate old records. With no new evidence, no additional
   review model call, reply, commit, or push is required.

Only run checks needed by the change and its issue. Retain the repository's
required checks; do not weaken or skip them. Product tests own their daemon
state. In allocated VMs, builds use controller limits rather than gating
through the binary under test. Source and review reports must survive worker
replacement.

## Completion record

Write a JSON file under ignored `build/review/`. Copy identity and skill hashes
from the packet; `base` is the merge-base SHA, so an unrelated advance of the
base branch does not invalidate an unchanged PR delta. `report` contains the
reviewer's actual concise reasoning, resolved findings and dispositions, and
references to durable sanitized evidence where needed.

```json
{
  "version": 1,
  "repo": "OWNER/REPO",
  "pr": 123,
  "issue": 72,
  "head": "FULL_HEAD_SHA",
  "base": "FULL_MERGE_BASE_SHA",
  "reviews": {
    "contract-review": {
      "head": "FULL_HEAD_SHA",
      "skill_sha256": "HASH_FROM_PACKET",
      "reviewer": "RUNNER:REVIEW_SESSION_ID",
      "verdict": "READY",
      "findings": [],
      "report": "Contract source, required evidence, and review outcome."
    },
    "ponytail-review": {
      "head": "FULL_HEAD_SHA",
      "skill_sha256": "HASH_FROM_PACKET",
      "reviewer": "RUNNER:DIFFERENT_REVIEW_SESSION_ID",
      "verdict": "READY",
      "findings": [],
      "report": "Scoped simplifications and their resolutions, or why no cuts remain."
    }
  },
  "verification": [
    {
      "command": "the actual verification command",
      "result": "passed",
      "evidence": "Observed outcome and durable evidence reference."
    }
  ]
}
```

```sh
python3 sortie/reviews.py packet --repo OWNER/REPO --pr 123 --issue 72 \
  --output build/review/packet.json
python3 sortie/reviews.py publish --repo OWNER/REPO --pr 123 \
  --file build/review/completion.json --ready
```

In a Sortie task, invoke the copy under `$BUSYBEE_SORTIE_TRUSTED`, which is
outside the candidate workspace. Review the trusted skills too: an issue that
changes a skill cannot pass by weakening the skill reviewing itself. New skill
versions become operational only after their change is merged and a new
trusted launcher snapshot is selected.

Publishing is the implementing agent's action; the review skills remain
read-only. The helper adds an author comment with the `busybee-agent-review:v1`
marker. It does not publish raw transcripts, secrets, or local machine paths.
The latest such comment from the PR author must come from a repository owner,
member, or collaborator. Missing, malformed, stale, uncertain, or incomplete
records cannot earn approval. The gate checks both actual CI platform jobs and
the trusted skill digests, and rechecks the head before publishing its verdict.

The record is an assertion by a trusted implementation agent, not cryptographic
proof that a reviewer reasoned correctly. The independent contexts and reports
make the reasoning inspectable; CI verifies execution. VM artifact and complete
scenario-matrix enforcement are delivered by lab issue #80, not implied by this
initial review gate. No additional model judges the wording of a clean record.

## Runner support

Select one native adapter for each Sortie process. A continuation can move to a
different runner after the previous session has stopped, using the same branch,
PR, and durable reports; do not assume native conversation IDs transfer between
providers.

| Runner | Sortie adapter and command | Readiness |
|---|---|---|
| Claude | `claude-code`, `claude` | Sortie 1.24 requires `bypassPermissions`; the lab launcher permits it only in an allocated worker. |
| Codex | `codex`, `codex app-server` | Default for host bootstrap, with workspace sandbox and noninteractive approval policy. |
| Cursor | `agent-client-protocol`, `agent acp` | Supported by pinned Sortie 1.24.1; unattended tool/permission behavior needs worker qualification. |

The worker launcher sets `BUSYBEE_SORTIE_WORKER=1` inside its allocated guest.
Do not set that on a shared host to bypass the environment check. This is an
execution-profile condition, not a substitute for VM isolation. Any runner can
use the same skills and completion record from its normal authorized session.

Sortie's built-in `self_review` is not this gate: it uses the author's session
and does not establish two fresh-context reviews of a committed PR. Keep this
explicit PR loop. Native adapter configuration and successful authenticated
execution are distinct checks; validate the selected runtime before dispatch.

Sources: [Sortie adapters](https://docs.sortie-ai.com/reference/workflow-config/),
[Sortie ACP](https://docs.sortie-ai.com/reference/adapter-agent-client-protocol/),
[Cursor ACP](https://prod.cursor.com/docs/cli/acp),
[Codex skills](https://learn.chatgpt.com/docs/build-skills),
[Claude skills](https://code.claude.com/docs/en/skills), and
[Cursor skills](https://prod.cursor.com/help/customization/skills).
