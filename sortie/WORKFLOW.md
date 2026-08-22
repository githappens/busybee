---
# Sortie workflow for githappens/busybee.
#
# Launch with sortie/run.sh (pins this instance's port, reuses gh's token,
# checks the ssh alias, runs inside the dev shell so the agent inherits
# cargo/pueued/make/ninja on PATH). `sortie/run.sh --dry-run` polls once.
#
# Issues are selected by the `sortie` marker label and moved through
# sortie:ready -> sortie:working -> sortie:review -> sortie:done by sortie
# itself. The agent never touches issue state.

tracker:
  kind: github
  project: githappens/busybee
  api_key: $GITHUB_TOKEN
  query_filter: "label:sortie milestone:\"bzbd: shared CPU token pool\""
  active_states: [sortie:ready, sortie:working]
  in_progress_state: sortie:working
  handoff_state: sortie:review
  terminal_states: [sortie:done]
  handoff_evidence: observed

polling:
  interval_ms: 60000

workspace:
  root: $PWD/build/sortie-workspaces

db_path: $PWD/build/sortie.db

hooks:
  # Full clone (not --depth 1): agents rebase and inspect history.
  after_create: |
    git clone git@github.com-sortie:githappens/busybee.git .
  # Keep the issue branch across attempts and continuation turns; only
  # create it from origin/main the first time.
  before_run: |
    set -e
    git fetch -q origin main
    b="sortie/${SORTIE_ISSUE_IDENTIFIER}"
    if git rev-parse -q --verify "$b" >/dev/null; then
      git checkout -q "$b"
    else
      git checkout -q -B "$b" origin/main
    fi
    # Per-issue model override: a `model:<name>` label (e.g. model:fable)
    # becomes .sortie/model, read by sortie/agent.sh. No label = default model.
    mkdir -p .sortie
    labels="$(gh issue view "$SORTIE_ISSUE_IDENTIFIER" --repo githappens/busybee --json labels --jq '[.labels[].name] | join(" ")')"
    model=""
    for l in $labels; do case "$l" in model:*) model="${l#model:}";; esac; done
    if [ -n "$model" ]; then printf '%s\n' "$model" > .sortie/model; else rm -f .sortie/model; fi
  timeout_ms: 120000

agent:
  kind: claude-code
  command: sortie/agent.sh   # picks the model per issue; see the before_run hook
  max_turns: 20
  max_sessions: 24   # continuation turns (review fixes, rebases) count as sessions
  max_concurrent_agents: 4   # hot-reloads; drop to 1 when bringing up a fresh setup
  turn_timeout_ms: 7200000
  read_timeout_ms: 10000
  stall_timeout_ms: 900000
  max_retry_backoff_ms: 300000

claude-code:
  permission_mode: dontAsk
  allowed_tools: "Bash Edit MultiEdit Write Read Glob Grep Agent TodoWrite WebFetch(domain:docs.rs) WebFetch(domain:github.com)"
  disallowed_tools: "mcp__sortie-tools__tracker_api WebSearch"
  max_budget_usd: 20   # per turn; an estimate under subscription auth, a runaway guard not a bill
  session_persistence: true

reactions:
  # Human CHANGES_REQUESTED reviews -> continuation turn.
  review_comments:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 120000
    debounce_ms: 60000
    max_continuation_turns: 3
  # Codex (chatgpt-codex-connector[bot]) reviews every push; its inline
  # comments -> continuation turn. github-actions[bot] only posts the verdict.
  bot_review:
    provider: github
    bot_usernames: ["chatgpt-codex-connector", "chatgpt-codex-connector[bot]"]
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
    # Every new review body (Codex summary, gate verdict) changes the comment
    # set and re-triggers a turn, so rounds are cheap no-ops more often than not.
    max_continuation_turns: 30
  # Merge once the review decision is APPROVED (codex-gate approves as
  # github-actions[bot] when Codex reports no findings on the head commit; the
  # main ruleset requires that approval) and every check is green.
  auto_merge:
    provider: github
    strategy: squash
    require_ci: true
    delete_branch: true
    poll_interval_ms: 60000
    max_retries: 2
    escalation: label
    escalation_label: needs-human
  # A PR that becomes unmergeable (main moved under it) gets one rebase turn
  # per conflicting head; auto_merge defers while conflicted.
  merge_conflicts:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 60000
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

Blocked-by issues (all must already be merged on `main`): {{ range $i, $b := .issue.blocked_by }}{{ if $i }}, {{ end }}#{{ $b.identifier }}{{ end }}. Read their merged code before starting; build on it, do not duplicate it.
{{ end }}

## Ground rules

1. **Read first.** `CLAUDE.md` (if present) for build/test commands and layout, then
   `docs/design/bzbd.md` — **the design document is the specification.** Every
   rule, state machine, table and contract is decided there; conform to it, do
   not redesign. If the spec and the task conflict, the task wins for scope and
   the spec wins for semantics; say so in the PR body. If your change alters
   behaviour the spec describes, update the spec section in the same PR.
2. **Scope.** Implement exactly the issue's Scope and Acceptance criteria. No
   adjacent refactors, no speculative features. If something outside scope
   blocks you, write `blocked` to `.sortie/status` with one line explaining why,
   and stop.
3. **Use these skills when they are available (check the skill list):**
   - `ponytail:ponytail` before writing any code — simplest solution that
     works: standard library before dependencies, one function before an
     abstraction, no speculative flexibility.
   - `ponytail:ponytail-review` on your diff before opening the PR; delete what
     it flags.
   - `superpowers:test-driven-development` while implementing.
   - `superpowers:systematic-debugging` the moment a test fails unexpectedly or
     behaviour surprises you — form a hypothesis from evidence before changing
     code; no guess-and-rerun loops.
   The issue's acceptance criteria are the whole scope.
4. **TDD.** Write the failing test named in the issue first, then the code. Never
   weaken, skip, or delete an existing test to get green; if a test disagrees
   with your change, assume the code is wrong until proven otherwise.
5. **No silent fallbacks.** Errors propagate with context; degraded paths must be
   loud. Do not add `unwrap_or_default`-style masking to make something pass.
6. **Build and test through the dev shell and busybee:**
   `busybee -- cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo fmt --all`. Cargo output goes to `build/`; do not change `.cargo/config.toml`.
   Never install or replace the machine's global `busybee`/`bzbd`/`pueued`; test via
   cargo-built binaries only.
7. **Isolation.** Integration tests must spawn their own `pueued`/`bzbd` in a
   temporary state dir (see `crates/bzb/tests/common/pueued.rs`), never the user's
   instance.
8. **Public repo hygiene.** Everything you write — code, comments, tests,
   fixtures, commit messages, PR text — is public. Use generic examples only: no
   machine names, user names, local paths, or references to other projects.
   No AI co-author trailers in commits.

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

When the acceptance criteria pass locally:

1. `git add -A && git commit` with a conventional message
   (`feat(bzb-core): …`, `test(bzbd): …`, `docs: …`); reference the issue as
   `Closes #{{ .issue.identifier }}` in the body.
2. `git push -u origin HEAD`.
3. `gh pr create --repo githappens/busybee --base main --title "<type>(<area>): <summary> (#{{ .issue.identifier }})" --body "<what, why, how tested; Closes #{{ .issue.identifier }}>"`.
4. Write the PR details for the orchestrator:
   ```sh
   mkdir -p .sortie
   printf '{"branch":"%s","pr_number":%s,"owner":"githappens","repo":"busybee"}\n' \
     "$(git rev-parse --abbrev-ref HEAD)" "$(gh pr view --json number -q .number)" > .sortie/scm.json
   echo needs-human-review > .sortie/status
   ```
5. Stop. An automated reviewer (Codex) reviews every push and its findings come
   back to you as a continuation turn; the PR merges automatically once the
   review reports no findings and CI is green. Human review comments also
   come back as continuation turns.
{{ if or .run.is_continuation .review_comments .bot_review_comments .merge_conflict }}

## Continuation

You are resuming this task. Do not start over: run `git status`, `git log
--oneline -5`, and the test suite, then continue from where the previous turn
stopped. If a PR already exists, push to the same branch.
{{ if .review_comments }}

### Review feedback to address

{{ range .review_comments }}- (comment id {{ .id }}) {{ .reviewer }}{{ if .file }} on `{{ .file }}`{{ if .start_line }}:{{ .start_line }}{{ end }}{{ end }}: {{ .body }}
{{ end }}
First react 👀 on each comment (`gh api -X POST repos/githappens/busybee/pulls/comments/<id>/reactions -f content=eyes`)
so the reviewer sees you are on it. Address every point and reply on each thread
with what changed. Push only if you changed files; threads you already answered in
an earlier turn need no new push.
{{ end }}
{{ if .merge_conflict }}

### Merge conflict to resolve

PR #{{ .merge_conflict.pr_number }} (branch `{{ .merge_conflict.branch }}`, head
`{{ .merge_conflict.head_sha }}`) no longer merges into `{{ .merge_conflict.base }}`.
Rebase the branch onto the current `origin/{{ .merge_conflict.base }}`
(`git fetch origin && git rebase origin/{{ .merge_conflict.base }}`), resolve every
conflict so the result still satisfies the issue's acceptance criteria and the
spec, rerun the full test suite, and `git push --force-with-lease`. Do not merge
`{{ .merge_conflict.base }}` into the branch; history must stay linear for the
squash merge. Keep the PR's scope unchanged.
{{ end }}
{{ if .bot_review_comments }}

### Automated review findings to address (Codex)

{{ range .bot_review_comments }}- (comment id {{ .id }}) {{ if .file }}`{{ .file }}`{{ if .start_line }}:{{ .start_line }}{{ end }}: {{ end }}{{ .body }}
{{ end }}
First, acknowledge every finding so humans can see you are on it: for each comment
id above run
`gh api -X POST repos/githappens/busybee/pulls/comments/<id>/reactions -f content=eyes`.
Then, for each finding, either fix it or reply on its thread
(`gh api -X POST repos/githappens/busybee/pulls/<pr>/comments -F in_reply_to=<id> -f body='…'`)
with the reason it does not apply; reply on the thread with a one-line summary of
the fix as well.

A finding listed here may already have been handled in an earlier turn: the list
is regenerated whenever the set of open comments changes, so threads that already
carry your reply come back. Check each thread first; if the code already reflects
the reply, there is nothing to do for it.

**The scope of this turn is exactly the findings listed above that have no reply
yet — nothing else.** Do not hunt for further defects, harden adjacent code, or
polish: every push discards the review in flight and restarts the cycle, and a
PR that keeps moving never merges. Anything you notice beyond the listed findings
goes into a follow-up issue, not this PR. **Push only if you changed files for a
listed finding.** If every finding already has your reply, end the turn without
committing, amending, rebasing or pushing. A new push triggers a fresh automated
review; the PR merges automatically once that review reports no findings and CI
is green.
{{ end }}
{{ end }}
{{ if and .attempt (not .run.is_continuation) (not .review_comments) (not .bot_review_comments) (not .merge_conflict) }}

## Retry (attempt {{ .attempt }})

A previous attempt failed. Inspect the workspace and `.sortie/status` before
choosing an approach; do not repeat the one that failed.
{{ end }}

Issue: {{ .issue.url }}
