---
# Launch with run.sh. It selects a native adapter, pins the executing
# scripts outside issue workspaces, and uses the repo-owned agent dev shell.
tracker:
  kind: github
  project: githappens/busybee
  api_key: $GITHUB_TOKEN
  query_filter: 'label:sortie milestone:"bzbd: shared CPU token pool"'
  active_states: [sortie:ready, sortie:working]
  in_progress_state: sortie:working
  handoff_state: sortie:review
  terminal_states: [sortie:done]
  handoff_evidence: strict

workspace:
  root: $BUSYBEE_SORTIE_STATE/sortie-workspaces
db_path: $BUSYBEE_SORTIE_STATE/sortie.db

polling:
  interval_ms: 60000

hooks:
  after_create: |
    git -c credential.helper= -c credential.helper='!gh auth git-credential' clone "$BUSYBEE_SORTIE_CLONE_URL" .
  before_run: |
    bash "$BUSYBEE_SORTIE_TRUSTED/sortie/prepare-workspace.sh"
  timeout_ms: 120000

agent:
  # launch.sh selects the adapter through Sortie's supported SORTIE_AGENT_*
  # overrides. These fields do not expand arbitrary environment variables.
  kind: codex
  command: codex app-server
  max_concurrent_agents: 4
  max_turns: 20
  max_sessions: 20
  turn_timeout_ms: 7200000
  read_timeout_ms: 30000
  stall_timeout_ms: 900000
  max_retry_backoff_ms: 300000

claude-code:
  # Sortie 1.24 requires this mode; launch.sh limits it to an allocated worker.
  permission_mode: bypassPermissions
  allowed_tools: "Bash Edit MultiEdit Write Read Glob Grep Agent TodoWrite"
  disallowed_tools: "mcp__sortie-tools__tracker_api"
  session_persistence: true

codex:
  approval_policy: never
  thread_sandbox: workspaceWrite
  turn_sandbox_policy:
    type: workspaceWrite
    networkAccess: true

reactions:
  bot_review:
    provider: github
    bot_usernames: ["github-actions", "github-actions[bot]"]
    poll_interval_ms: 60000
    debounce_ms: 60000
    max_continuation_turns: 6
    watch_window_ms: 21600000
    escalation: label
    escalation_label: needs-human
    triage:
      script: |
        python3 "$BUSYBEE_SORTIE_TRUSTED/sortie/review-triage.py"
      timeout_ms: 180000
  review_comments:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 120000
    debounce_ms: 60000
    max_continuation_turns: 6
    watch_window_ms: 21600000
  ci_failure:
    provider: github
    max_retries: 3
    max_log_lines: 80
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
    watch_window_ms: 21600000
  merge_conflicts:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
  auto_merge:
    provider: github
    strategy: squash
    require_ci: true
    delete_branch: true
    poll_interval_ms: 60000
    watch_window_ms: 21600000
    max_retries: 2
    escalation: label
    escalation_label: needs-human
  merge_completion:
    provider: github
    target_state: sortie:done
    poll_interval_ms: 60000
    max_retries: 2
    escalation: label
    escalation_label: needs-human
---

You are working on **busybee**, a Rust CLI + daemon that gates resource-heavy
commands across parallel developer sessions. You receive one GitHub issue and
deliver one pull request for it.

## Task

**#{{ .issue.identifier }}: {{ .issue.title }}**
{{ if .issue.description }}

{{ .issue.description }}
{{ end }}
{{ if .issue.blocked_by }}

Blocked-by issues (all must already be merged on `main`): {{ range $i, $b := .issue.blocked_by }}{{ if $i }}, {{ end }}#{{ $b.display_id }}{{ end }}. Read their merged code before starting; build on it, do not duplicate it.
{{ end }}

## Ground rules

1. **Read first.** `CLAUDE.md` (build/test/layout) and `docs/design/bzbd.md` (the specification; `AGENTS.md` §Conform to the specification). If the spec and the task conflict, the task wins for scope and the spec wins for semantics; say so in the PR body.
2. **Scope.** The issue's Scope and Acceptance criteria are the whole scope (`AGENTS.md` §Stay within the issue's scope). If something outside that blocks you, or the criteria cannot be made to pass and remaining moves are low-confidence, write `blocked` to `.sortie/status` with one line of reasoning and stop; that is a successful escalation, not a failed attempt.
3. **Review skills.** The repository skills and CI handoff are defined in `docs/development/agent-review.md`; use the trusted copy under `$BUSYBEE_SORTIE_TRUSTED`. Local reviews are optional early feedback. CI runs both independent skill reviews.
4. **TDD.** Failing test first; never weaken, skip, or delete an existing test to get green (`CLAUDE.md` §Conventions).
5. **No silent fallbacks.** Errors propagate with context; degraded paths stay loud (`CLAUDE.md` §Conventions, `AGENTS.md` §No silent fallbacks).
6. **Build and test.** Commands live in `CLAUDE.md` §Build and test; cargo test/clippy/fmt are allowed as-is.
   Wrap Busy Bee / Pueue in `.claude/isolated.sh` so they cannot see the user's
   daemons: `.claude/isolated.sh busybee -- cargo test --workspace`,
   `.claude/isolated.sh bzb -- xcodebuild ...`, `.claude/isolated.sh bzb -- cmake --build ...`.
   Cargo output goes to `build/`; do not change `.cargo/config.toml`. Never
   install or replace the machine's global `busybee`/`bzb`/`bzbd`/`pueued`.
7. **Isolation.** Integration tests spawn their own `pueued`/`bzbd` (`CLAUDE.md` §Integration tests). Direct `bzb`/`busybee`/`bzbd`/`pueued`/`pueue` Bash is denied; use `.claude/isolated.sh`. The PreToolUse hook is a guard for cooperative agents, not a same-UID sandbox.
8. **Public repo hygiene.** Generic examples only; no machine names, user names, local paths, or other projects (`AGENTS.md` §Public repository hygiene). No AI co-author trailers in commits.

## Prohibitions

- Do not edit issue labels or state.
- Do not change files under `sortie/` or `.github/workflows/`.
- Do not touch any other PR or branch.
- Do not merge the PR.

## Keep the branch mergeable

Before finishing, and at the start of every continuation, run `git fetch origin`
and check the PR's mergeability (`gh pr view --json mergeable -q .mergeable`, or
`git merge-tree --write-tree origin/main HEAD` before the PR exists). Rebase onto
`origin/main` **only when it actually conflicts**: resolve the conflicts so the
result still satisfies the issue and the spec, rerun the full test suite, and
`git push --force-with-lease`. Do not rebase merely because `main` moved — every
push discards the current automated review and restarts the cycle, and CI already
tests the merge result. Never merge `main` into the branch; history stays linear
for the squash merge. A PR that does not merge cleanly is never merged.

## Finishing

When implementation and required checks are ready:

1. Commit, push, and create/reuse a draft PR with `Closes #{{ .issue.identifier }}`.
   Use `gh pr create --draft` and `--body-file` for the prepared description.
2. Mark the implementation ready with `gh pr ready`. CI runs both trusted
   review skills in separate Claude Opus 5.5 high sessions after Linux/macOS
   checks pass. Local skill reviews are optional early feedback; do not publish
   author review receipts or claim they can satisfy the gate.
3. Use `$BUSYBEE_SORTIE_TRUSTED/sortie/reviews.py handoff --repo OWNER/REPO
   --pr NUMBER` to write `.sortie/scm.json` (including the pushed SHA and time)
   and `.sortie/status`. Hand off immediately; do not wait in an agent session
   for CI review. `needs-human-review` is Sortie's protocol name: CI supplies
   the formal review and Sortie handles the resulting continuation or merge.
4. On a findings continuation, fix valid scoped findings, rerun affected
   checks, and push to the same PR. Explain declined findings in a PR comment
   with concrete evidence; CI re-reviews that new disposition even without a
   code change. Repeat the handoff. CI settles prior findings and reviews the
   new delta; both final reports and required CI must cover the current head.

Do not change issue labels/state or merge the PR yourself. Do not request the
external Codex review bot: these two skills provide this workflow's review.
If a prerequisite or tool genuinely blocks progress, retain source and reports,
write a specific reason to `.sortie/blocker.md`, set `.sortie/status` to
`blocked`, and stop. Do not retry unchanged evidence or ask someone to operate
the environment as a routine step.

{{ if .run.is_continuation }}
This is a continuation. Inspect branch/PR state and prior reports first. Reuse
completed work. Rerun checks when code changed or a reported failure requires
them; an unchanged settled review does not require a new test run or push.
{{ end }}
{{ if .review_comments }}
Review feedback:
{{ range .review_comments }}- {{ .reviewer }}: {{ .body }}
{{ end }}
Address the scoped feedback, then repeat affected checks and the current-head
review/handoff process. Explain declined findings with concrete reasons.
{{ end }}
{{ if .bot_review_comments }}
CI skill review findings:
{{ range .bot_review_comments }}- {{ .reviewer }}: {{ .body }}
{{ end }}
Read the latest formal CI review on this PR. Fix its scoped findings or explain
declined findings with concrete evidence in a PR comment. Retain settled
decisions. Push changes and hand off again; CI owns the follow-up reviews.
Do not run an author receipt loop, reply to a clean approval, or make an empty
commit to trigger review. Authentication/tool failures escalate through triage.
{{ end }}
{{ if .ci_failure }}
CI failed on {{ .ci_failure.ref }}:
{{ .ci_failure.log_excerpt }}
Reproduce and fix the failure; never weaken a test to pass. Push to the same PR
and hand off its new head for CI review.
{{ end }}
{{ if .merge_conflict }}
PR #{{ .merge_conflict.pr_number }} conflicts with {{ .merge_conflict.base }}.
Fetch and rebase only to resolve the actual conflict, preserve scope, run the
required checks, push with `--force-with-lease`, and review the new head.
{{ end }}
