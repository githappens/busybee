---
name: contract-review
description: Review a GitHub pull request against its issue, specification, and preserved contracts. Return only concrete P0/P1 blockers introduced by the delta, or READY/UNSURE. Read-only; the implementing agent owns fixes.
---

# Contract review

Build the review packet from the live PR and its contract. Review the delta
against that contract, including a draft PR. A clean review is a successful
result; do not invent findings or widen the task to look thorough.

This skill is read-only: do not edit product files, push, post GitHub comments
or reviews, change issue state, or merge. Return the report to the implementing
agent. Writing a requested local review report is allowed.

## Establish the packet

Resolve an explicit PR URL or `owner/repo` and number. With no arguments, use
the current checkout's remote and branch PR. Do not guess a repository from a
local folder name. If no PR exists, report that and stop. GitHub is the
supported tracker for this workflow.

Read the full diff against the PR's actual base, not `git diff HEAD` or an
assumed default branch. Record the PR number, base and head SHAs. The helper
`python3 sortie/reviews.py packet --repo OWNER/REPO --pr NUMBER --output
build/review/packet.json` gathers the live packet. A supplied packet is usable
only if its identity matches the requested PR and head.

Find the contract in this order:

1. The explicitly assigned issue for this PR.
2. GitHub closing-issue references or an explicit closing reference in the PR
   body. Fetch the issue and its comments; check it is an issue, not another PR.
3. The PR title and description when no issue exists. State that this is the
   only contract; a thin description may leave the verdict `UNSURE`.

Read `AGENTS.md`, the specification sections implicated by the issue, and
prior review reports/dispositions. In this repository, `docs/design/bzbd.md`
defines broker semantics and `docs/design/agent-lab.md` defines the VM lab.
Inspect directly relevant code when necessary to establish a causal chain.
Do not search for unrelated defects. Failed retrieval of an identified issue,
diff, or required specification is incomplete evidence, not permission to
substitute a weaker contract.

Derive the required outcomes, preserved behavior, non-goals, supported inputs,
and required evidence before judging the change. Verify the packet still names
the current head before returning a final review.

## Admit findings narrowly

A finding must meet every condition:

- The current delta causes it, exposes it materially, or makes the PR's claim
  false. Pre-existing adjacent bugs are outside this review.
- It violates an issue criterion, specification, preserved contract, or a
  supported workflow implicated by changed lines.
- A concrete causal chain leads to a merge-relevant failure: wrong output,
  crash, corrupt state, broken build, invalid verification, lost work, broken
  resource accounting, or disclosure of private identifiers in public files.
- It is a distinct root cause with a bounded fix inside this issue's scope.
- It is P0 or P1 and the evidence is strong enough to direct a change.
- It anchors to a changed line in the reviewed diff. For a deletion-only
  regression, identify the deleted LEFT-side line or the nearest surviving
  relevant line and explain the removed behavior explicitly.

Omit P2/P3 suggestions, style, speculative hardening, and architectural
preferences. If an apparent blocker lacks decisive evidence, return `UNSURE`
with the missing evidence, rather than inventing a finding. Do not run broad
builds by default; a targeted check must follow the repository's isolation and
resource rules. Builds in allocated workers use their controller budget.

## Converge

On a later round, inspect resolved blockers and the changes since the prior
reviewed head. Do not reopen unchanged, settled decisions or treat a different
example of the same root cause as a new finding. Check the final combined
delta still delivers the contract. Report new defects caused by a fix when
they meet the same admission criteria.

## Report

Lead with `READY`, `BLOCKED`, or `UNSURE`, then the reviewed PR/head, the
derived contract, and findings. `BLOCKED` requires at least one finding;
`READY` requires no outstanding finding and sufficient completion evidence.
`UNSURE` names the specific missing evidence or unresolved contract decision.

For each finding include priority, path and diff line/side, violated requirement,
causal chain, and the narrow required outcome. Keep genuinely pre-existing
observations separate and nonblocking; do not turn them into additional work.

For autonomous handoff, return the review record described in
`docs/development/agent-review.md`, including the exact reviewed head, skill
digest, a distinct reviewer session ID, and your report. The parent agent fixes
accepted findings, tests, pushes, and requests the necessary follow-up review.
