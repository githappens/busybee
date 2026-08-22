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
  max_sessions: 3
  max_concurrent_agents: 3
  turn_timeout_ms: 7200000
  read_timeout_ms: 10000
  stall_timeout_ms: 900000
  max_retry_backoff_ms: 300000

claude-code:
  permission_mode: dontAsk
  allowed_tools: "Bash Edit MultiEdit Write Read Glob Grep Agent TodoWrite WebFetch(domain:docs.rs) WebFetch(domain:github.com)"
  disallowed_tools: "mcp__sortie-tools__tracker_api WebSearch"
  max_budget_usd: 5
  session_persistence: true

reactions:
  review_comments:
    provider: github
    max_retries: 2
    escalation: label
    escalation_label: needs-human
    poll_interval_ms: 120000
    debounce_ms: 60000
    max_continuation_turns: 3
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
3. **TDD.** Write the failing test named in the issue first, then the code. Never
   weaken, skip, or delete an existing test to get green; if a test disagrees
   with your change, assume the code is wrong until proven otherwise.
4. **No silent fallbacks.** Errors propagate with context; degraded paths must be
   loud. Do not add `unwrap_or_default`-style masking to make something pass.
5. **Build and test through the dev shell and busybee:**
   `busybee -- cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo fmt --all`. Cargo output goes to `build/`; do not change `.cargo/config.toml`.
   Never install or replace the machine's global `busybee`/`bzbd`/`pueued`; test via
   cargo-built binaries only.
6. **Isolation.** Integration tests must spawn their own `pueued`/`bzbd` in a
   temporary state dir (see `crates/bzb/tests/common/pueued.rs`), never the user's
   instance.
7. **Public repo hygiene.** Everything you write — code, comments, tests,
   fixtures, commit messages, PR text — is public. Use generic examples only: no
   machine names, user names, local paths, or references to other projects.
   No AI co-author trailers in commits.

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
5. Stop. A human reviews and merges; review comments come back to you as a
   continuation turn.
{{ if .run.is_continuation }}

## Continuation

You are resuming this task. Do not start over: run `git status`, `git log
--oneline -5`, and the test suite, then continue from where the previous turn
stopped. If a PR already exists, push to the same branch.
{{ if .review_comments }}

### Review feedback to address

{{ range .review_comments }}- {{ .reviewer }}{{ if .file }} on `{{ .file }}`{{ if .start_line }}:{{ .start_line }}{{ end }}{{ end }}: {{ .body }}
{{ end }}
Address every point, push, and reply on the PR with what changed.
{{ end }}
{{ end }}
{{ if and .attempt (not .run.is_continuation) }}

## Retry (attempt {{ .attempt }})

A previous attempt failed. Inspect the workspace and `.sortie/status` before
choosing an approach; do not repeat the one that failed.
{{ end }}

Issue: {{ .issue.url }}
