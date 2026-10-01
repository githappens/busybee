# Autonomous PR review and handoff

Sortie owns issue dispatch, continuations, and merge completion. The implementing
agent owns code, tests, and fixes. GitHub Actions runs the two independent review
skills and publishes the formal approval or request for changes. Implementation
may use Claude, Codex, or Cursor; CI reviewers use **Claude Opus 5.5, high effort**
(`claude-opus-5-5`, `--effort high`).

## Shared skills

The canonical, runner-neutral instructions are:

- [contract-review](../../skills/contract-review/SKILL.md): verify the issue and
  specification, admitting only concrete P0/P1 blockers introduced by the delta.
- [ponytail-review](../../skills/ponytail-review/SKILL.md): identify scoped cuts
  that preserve the contract and its required tests.

Discovery aliases expose the same files under `.agents/skills/` and
`.claude/skills/`. Every runner can read the canonical files directly. CI reads
skills from its trusted default-branch revision; a PR cannot weaken the skill
reviewing itself. Both skills stay read-only. Local reviews are useful optional
early feedback; they do not grant approval or replace either CI session.

## One issue through review

1. Read the issue, applicable specification, `AGENTS.md`, and `CLAUDE.md`.
   Implement the assigned scope and named regressions. Infrastructure issues
   may change the controller/runner files explicitly in scope; ordinary product
   tasks do not gain authority over dispatch or approval policy.
2. Commit and push on the task branch (`sortie/<issue>` for product tasks,
   `sortie-lab/<issue>` for lab tasks), then create/reuse a draft PR. Include
   `Closes #<issue>`, the concrete behavior change, and verification evidence.
   Task checkouts use local Git configuration to route credentials through
   `gh`; no global credential change is needed.
3. Once implementation and local checks are ready, run `gh pr ready`. Hand off
   immediately using the trusted helper; do not keep an agent waiting for CI:

   ```sh
   python3 "$BUSYBEE_SORTIE_TRUSTED/sortie/reviews.py" handoff \
     --repo OWNER/REPO --pr NUMBER
   ```

   It verifies that the local branch/head matches the pushed, ready PR, then
   writes `.sortie/scm.json` with `branch`, `sha`, `pushed_at`, `pr_number`,
   `owner`, and `repo`, followed by `needs-human-review` in `.sortie/status`.
   That status is Sortie's protocol name; the CI gate supplies the review.
4. After the current Linux and macOS CI jobs pass, the base-controlled Actions
   workflow starts two fresh Claude sessions, one per skill. They inspect the
   full live PR packet and exact candidate tree with only read/search tools.
   A deterministic publisher validates the structured results and posts a
   formal GitHub `APPROVE` or `REQUEST_CHANGES` pinned to the reviewed commit.
5. `BLOCKED` findings reach Sortie's `bot_review` reaction. The same issue and
   PR resume in the selected implementation runner. Fix valid scoped findings,
   rerun affected checks, push, and hand off again. Explain declined findings
   with concrete evidence in an author PR comment; that changes the review
   inputs and requests follow-up review without requiring an empty commit.
6. Follow-up reviewers settle prior findings and inspect the new delta while
   preserving unchanged settled decisions. Both final reports must be `READY`
   and the latest CI jobs for both platforms must succeed on the current head.
   Sortie then uses native automerge and the existing repository protections.
   The implementing agent never merges or bypasses rules.

A CI failure or merge conflict also resumes the same PR. Unchanged evidence
requires no new model call, reply, test run, commit, or push. Required checks
must not be weakened. Product tests own private Pueue/bzbd state; allocated VM
builds use controller limits instead of the binary under test.

## CI authority, evidence, and bounds

`.github/workflows/agent-review-gate.yml` reviews every open, ready,
same-repository PR. Fork PRs require maintainer review. Linked closing issues
provide the contract; a PR without one uses its description as the scope.

Trust model:

- The workflow, controller (`sortie/ci_reviews.py`), and skills run from the
  default-branch SHA. Candidate files are read as data, never executed.
- Only human actors with write access can trigger a review; only
  owner/member/collaborator comments change the contract fingerprint. Other
  events wait for the next scheduled run.
- Review sessions use restricted, read/search-only tools with candidate
  settings and skills disabled, and a read-only GitHub token. Only the separate
  deterministic publisher can submit reviews.
- Each session runs the official Claude Code `base-action` (pinned 2.1.280, explicit
  model and effort) with a 20-minute timeout and a 100-turn limit.
- The publisher rechecks live inputs, head, and both platform jobs. Stale,
  missing, malformed, or `UNSURE` results never approve, and an earlier
  approval is revoked when its evidence is invalidated or CI fails on that head.

Each model session returns only:

```json
{
  "head": "FULL_HEAD_SHA",
  "verdict": "READY",
  "findings": [],
  "report": "Contract, evidence inspected, and disposition of prior findings."
}
```

`BLOCKED` requires concrete findings, `READY` requires none unresolved, and
`UNSURE` must explain the missing evidence. The controller adds PR/issue
identity, merge-base, skill digests, the two distinct Action session IDs,
model/effort, input fingerprint, and run URL.

Retention: the sanitized bundle is an Actions artifact kept 90 days; each
session transcript (`claude-transcript-PR-HEAD-SKILL-ATTEMPT`) is kept seven
days and is diagnostic only, never approval evidence. Results, including
failed attempts, are reused only for the same head, merge-base, contract,
description, author dispositions, and trusted policy, so an unchanged failure
does not loop. Corrupt newest evidence never falls back to an older report.

Sortie's `bot_review` reaction picks up `github-actions[bot]` reviews; the
trusted triage helper dispatches fixes only for a current `BLOCKED` and
escalates `UNSURE` or an unreadable result to `needs-human`. An operator
resolves external authentication or tool failures, then retries with an author
comment or a new head.

## Authentication and activation

Configure repository secret `CLAUDE_CODE_OAUTH_TOKEN` using `claude setup-token`
from an eligible Claude subscription. Reviews consume that subscription's
allowance. Only the controller and publisher receive the workflow's GitHub
token; the model Action needs only the Claude token. This setup does not
require installing the Claude GitHub App. Repository Actions settings
must permit GitHub Actions to create/approve PR reviews. Keep the existing
required approval and stale-review dismissal rules.

Normal triggers load this workflow from the default branch. To bootstrap the
first PR before it lands, a maintainer can create a temporary push-triggered
workflow on a separate branch, pin every policy checkout to a reviewed commit,
and fix selection to that one PR. It must run both real reviews and the same
publisher with existing protections intact. Delete the temporary branch after
qualification; do not add candidate-ref dispatch to the standard gate. The run
and formal review provide evidence that hosted OAuth and approval permissions
work. Local reviews and syntax checks alone cannot establish that.

For manual, read-only inspection, any runner can collect the same packet:

```sh
python3 sortie/reviews.py packet --repo OWNER/REPO --pr NUMBER --issue ISSUE \
  --output build/review/packet.json
```

## Runner support

Select one native adapter per Sortie process. A continuation may move to another
runner after the previous session stops, preserving the branch and PR; native
conversation IDs do not transfer between providers. CI review remains the same.

| Implementer | Sortie adapter and command | Qualification |
|---|---|---|
| Claude | `claude-code`, `claude` | Sortie 1.24 requires `bypassPermissions`; launcher restricts this profile to an allocated worker. |
| Codex | `codex`, `codex app-server` | Host bootstrap default, with workspace sandbox and noninteractive approval policy. |
| Cursor | `agent-client-protocol`, `agent acp` | Pinned Sortie 1.24.1 supports the adapter; unattended tools and permissions need worker qualification. |

The worker launcher sets `BUSYBEE_SORTIE_WORKER=1` inside its allocated guest.
Do not set it on a shared host to bypass the environment check. Sortie's
`self_review` uses the author's context and does not replace these CI sessions.

Sources: [Claude Actions](https://code.claude.com/docs/en/github-actions),
[Claude model configuration](https://code.claude.com/docs/en/model-config),
[GitHub formal reviews](https://docs.github.com/en/rest/pulls/reviews#create-a-review-for-a-pull-request),
[Sortie workflow and adapters](https://docs.sortie-ai.com/reference/workflow-config/),
[Sortie reaction triage](https://docs.sortie-ai.com/guides/triage-reactions-before-dispatch/).
