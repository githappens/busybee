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
  max_sessions: 80   # continuation turns (review fixes, rebases) count as sessions
  max_concurrent_agents: 4   # hot-reloads; drop to 1 when bringing up a fresh setup
  turn_timeout_ms: 7200000
  read_timeout_ms: 10000
  stall_timeout_ms: 900000
  max_retry_backoff_ms: 300000

claude-code:
  permission_mode: dontAsk
  allowed_tools: "Bash Edit MultiEdit Write Read Glob Grep Agent TodoWrite WebFetch(domain:docs.rs) WebFetch(domain:github.com)"
  disallowed_tools: "mcp__sortie-tools__tracker_api WebSearch"
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
    # A daemon or config PR that draws one genuine finding per round burns these
    # fast: #8 reached 15 in ninety minutes with every finding legitimate, so 30
    # would have escalated work that was converging. Needs a restart to apply.
    max_continuation_turns: 60
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
  # A failing check on the PR head -> continuation turn with the log excerpt.
  # CI runs on Linux and macOS while agents develop on one of them, so
  # platform-specific failures are expected and must be fixed by the agent.
  ci_failure:
    provider: github
    max_retries: 3
    max_log_lines: 80
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

When the acceptance criteria pass locally, review the whole change before anyone
else does. The automated reviewer surfaces one or two findings per pass and
re-reviews every push, so each defect that reaches it costs a full round trip;
the ones you catch here cost nothing.

Dispatch a subagent with a fresh context and give it only the diff
(`git diff origin/main...HEAD`), `AGENTS.md` (§Code Review Rules is what the
reviewer applies), the issue text, and the spec sections the change touches. Ask
it to review as that reviewer would: spec conformance, silent fallbacks, resource
accounting on every exit path, tests as the contract, scope. Fix what it finds
that is real and within the issue's scope; rerun the suite. One pass — do not
loop on it, and do not widen the change to satisfy a suggestion outside the
acceptance criteria.

Then:

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
   back to you as a continuation turn; the PR merges automatically once CI is
   green and the head carries no P0/P1 finding and every P2/P3 finding has your
   answer. Human review comments also come back as continuation turns.
{{ if or .run.is_continuation .review_comments .bot_review_comments .merge_conflict .ci_failure }}

## Continuation

You are resuming this task. Do not start over: run `git status` and `git log
--oneline -5`, plus the test suite unless this is an automated-review turn, then
continue from where the previous turn stopped. Automated-review turns run the
review-status predicate below before tests. If a PR already exists, push to the
same branch.
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
Before tests, acknowledgements or replies, run
`sortie/review-status.sh -v <pr>`. Its one-line verdict is authoritative:

- `approvable`: end the turn now. A new `@codex review` would replace the clean
  signal with 👀 and throw away an approval that is about to land.
- `waiting:<ids>`: answer only those P2/P3 threads.
- `blocked:<ids>`: fix or decline only those P0/P1 or unbadged findings.
- `unknown:<reason>`: do not change files. A review or re-review is in flight, the gate
  still needs to classify Codex's freeform wording, or answered P2/P3 threads still
  await the gate's reply-quality judgement (`unknown:judge-replies`).

Acknowledge each `waiting` or `blocked` id with 👀:
`gh api -X POST repos/githappens/busybee/pulls/comments/<id>/reactions -f content=eyes`.
P0/P1 findings block until the reviewer clears the head. P2/P3 findings clear
when their threads carry an author reply; fix one only when the fix is small and
inside the acceptance criteria, otherwise defer it to a follow-up issue. A bare
acknowledgement is not an answer: state the fix, or why it does not apply here.

Reply with
`gh api -X POST repos/githappens/busybee/pulls/<pr>/comments -F in_reply_to=<id> -f body='…'`.
Write enough that a reader with only that thread understands the decision.

**The scope of this turn is exactly the ids reported by the script — nothing
else.** Do not hunt for defects or harden adjacent code. Push only when fixing a
reported finding changed files; replies and declines alone get no commit, amend,
rebase or push.

After declining a blocking finding, rerun the script. If it remains `blocked`
and the evidence shows no re-review request for this head, ask once:

```sh
gh api -X POST repos/githappens/busybee/issues/<pr>/comments -f body='@codex review

<one line per declined finding: which thread it is, and why it does not apply>'
```

The script owns the once-per-head bookkeeping. If its evidence shows that the
same finding was re-raised after that request, write `blocked` to `.sortie/status`
with one line naming it and stop; a human settles it. A clean re-review appears as
`approvable`.

A new push triggers a fresh automated review. The PR merges automatically once
CI is green and the review-status predicate is approvable.
{{ end }}
{{ if .ci_failure }}

### CI failure to fix

{{ .ci_failure.failing_count }} check(s) on the PR head `{{ .ci_failure.ref }}` failed:
{{ range .ci_failure.check_runs }}- {{ .name }}: {{ .conclusion }} ({{ .details_url }})
{{ end }}
Log excerpt from the first failing check:

```
{{ .ci_failure.log_excerpt }}
```

CI runs the suite on both Linux and macOS; you develop on one of them, so the
usual cause is platform-specific behaviour (socket semantics, error kinds,
signals, filesystem details). Reproduce from the log, fix the code or the test so
it holds on both platforms (never gate a test on one OS to make it pass), rerun
the full test suite, and push.
{{ end }}
{{ end }}
{{ if and .attempt (not .run.is_continuation) (not .review_comments) (not .bot_review_comments) (not .merge_conflict) (not .ci_failure) }}

## Retry (attempt {{ .attempt }})

A previous attempt failed. Inspect the workspace and `.sortie/status` before
choosing an approach; do not repeat the one that failed.
{{ end }}

Issue: {{ .issue.url }}
