# Running sortie against this repository

[sortie](https://docs.sortie-ai.com) turns labelled GitHub issues into autonomous
Claude Code sessions. `WORKFLOW.md` is the configuration; `run.sh` launches it;
`agent.sh` is the per-issue agent launcher. One sortie process serves one
workflow, so this directory is specific to this repository.

## Preflight (verify every time after a break)

| check | command | expect |
|---|---|---|
| sortie on PATH | `sortie --version` | 1.21 or newer |
| gh authenticated, scopes | `gh auth status` | scopes include `repo` (and `project` for board admin) |
| ssh alias for workspaces | `ssh -T git@github.com-sortie` | `Hi githappens/busybee! You've successfully authenticated` |
| deploy key still registered | `gh repo deploy-key list --repo githappens/busybee` | the automation key, `read-write` |
| board sync secret present | `gh secret list --repo githappens/busybee` | `PROJECT_TOKEN` (classic PAT, `project` scope; **expires** — check the date) |
| labels exist | `gh label list --repo githappens/busybee` | `sortie`, `sortie:ready/working/review/done`, `needs-human`, `epic`, `later`, `model:fable` |
| nothing already running | `pgrep -fl "sortie .*WORKFLOW"` | no output |
| one poll, no side effects | `sortie/run.sh --dry-run` | `candidates_fetched=N would_dispatch=M` |

The ssh alias `github.com-sortie` lives in the local ssh config: a dedicated,
passphrase-less key registered as a **write deploy key on this repository
only**, with `IdentityAgent none` so unattended sessions never touch a hardware
key or an agent prompt. If the alias is missing on a new machine, recreate the
key, add it as a deploy key, and add the alias; then re-run the check above.

## Launch

```sh
sortie/run.sh            # foreground; Ctrl-C stops dispatching (running sessions are reconciled on restart)
sortie/run.sh --dry-run  # one poll cycle, nothing dispatched, nothing written
```

`run.sh` pins the HTTP/metrics port (7678), reuses `gh auth token` as
`GITHUB_TOKEN`, verifies the ssh alias, refuses to start if the port is busy,
and runs inside `nix develop` so agents inherit cargo, pueued, make and ninja.

First launch after a long gap: set `agent.max_concurrent_agents: 1` in
`WORKFLOW.md`, watch one issue reach a PR, then raise it. That key hot-reloads.

Watch a session read-only: `sortie/peek.sh <issue>` (last events from the agent's
transcript plus the workspace's git state; `-f` follows).

Dashboard: http://127.0.0.1:7678 (local only). Metrics: `/metrics` on the same
port. Run history and cost: `sortie stats --since 24h` (reads `build/sortie.db`).

## How an issue moves

Only issues with the `sortie` label **and** the milestone in `query_filter` are
candidates. Labels are the state machine; sortie is the only writer:

```
sortie:ready ──dispatch──▶ sortie:working ──PR opened──▶ sortie:review ──auto-merged──▶ sortie:done (closed)
                                                              │  ▲
                                                Codex findings └──┘ continuation turn
                                  │
                                  └─ agent writes `blocked` ──▶ parked (needs-human)
```

Dependencies use GitHub's native *blocked by* relationships. **sortie 1.21 does
not honour them** (its GitHub adapter only loads blockers for single-issue
fetches, not for the candidate list), so the `sortie` marker label is the
readiness gate: blocked issues carry `sortie:ready` for the board but no
marker, and `sortie/unblock.sh` adds the marker once every blocker is closed.
`run.sh` runs it every 60 s as a sidecar; it can also be run by hand after a merge. Ordering is by creation date.

The board (Projects → "busybee · bzbd", Kanban view) mirrors these labels through
`.github/workflows/project-sync.yml`, using the `PROJECT_TOKEN` secret.

## Common operations

- **Release dependents after a merge**: `sortie/unblock.sh` (idempotent;
  `VERBOSE=1` shows what each issue still waits on).
- **Re-run an issue** (failed, parked, or you want another attempt): fix whatever
  blocked it, remove `needs-human` if present, set the label back to
  `sortie:ready`. The existing workspace and branch are reused.
- **Bump the model for one issue**: add the label `model:fable` before it is
  dispatched. The `before_run` hook writes `.sortie/model`, and `agent.sh` runs
  Claude with that model; without the label the default in `agent.sh` applies.
  Retries and continuation turns follow the label.
- **Automated review and merge** (no human in the loop by default):
  1. Codex reviews every push (ChatGPT → Codex → Code review settings: auto
     review on, trigger "on every push"; rules in `AGENTS.md` → *Code Review
     Rules*). It posts a comment-only review as `chatgpt-codex-connector[bot]`.
  2. `.github/workflows/codex-gate.yml` (every 5 min and on PR events) selects PRs
     with new Codex activity on their head commit, has Claude Haiku read that
     material and decide APPROVE / REQUEST_CHANGES / UNSURE, and posts the verdict
     as `github-actions[bot]` (UNSURE posts nothing; the PR waits). Severity
     decides what blocks: a P0/P1 finding blocks, and that block lifts only on a
     later clean Codex signal for the head — a re-review requested on the same
     head (`@codex review`, which the agent posts after declining a P0/P1) or a
     review of a new head. P2/P3 findings do not block: the gate approves the head
     once the author has answered every such thread with a reason (out of scope,
     not applicable, follow-up issue); until then it posts REQUEST_CHANGES naming
     the waiting threads, and an author reply re-triggers the judgement. The
     policy comes from an audit of 49 findings over four PRs: every
     would-have-shipped bug was a round-1/2 P1, the P2 tail was one cascade or
     edge case per round. The `main` ruleset requires one approval and dismisses
     it on push, so the review decision tracks the latest verdict. Needs the repo setting "Allow GitHub Actions to
     create and approve pull requests" and the `CLAUDE_CODE_OAUTH_TOKEN` secret
     (from `claude setup-token`; tied to the Max subscription, so it expires with
     it and shares its rate window).
  3. `reactions.bot_review` routes Codex's comments to the agent as a continuation
     turn (max 5 per issue, then `needs-human`).
  4. `reactions.auto_merge` squash-merges once the decision is APPROVED and CI is
     green; `reactions.merge_completion` sets `sortie:done` and closes the issue;
     the unblock sidecar releases dependents.
  If Codex never reviews (rate limit reached with credits off), the PR simply
  waits: the gate only acts on an existing Codex review for the head commit.
- **Human review**: request changes on the PR; sortie dispatches a continuation
  turn with your comments (`reactions.review_comments`). Your approval counts
  like the gate's.
- **Stop everything**: Ctrl-C `run.sh`. Sessions already running finish their
  turn; on the next start sortie reconciles against tracker state. Signal the
  `sortie` process, not the `run.sh` wrapper — the wrapper is the parent, so a
  signal sent to it alone does not reach the daemon. Prefer stopping when
  `sortie_sessions_running` is 0: killing an agent mid-rebase leaves its
  workspace with an unfinished rebase, which makes `before_run` fail on every
  later dispatch for that issue (see the stall table).
- **Change concurrency, polling, model**: edit `WORKFLOW.md`; most `agent.*`
  and `polling.*` keys hot-reload, reactions need a restart (`sortie validate`
  tells you if the file is malformed).

## Stall signatures (and the fix for each)

| what you see | cause | fix |
|---|---|---|
| issue is `sortie:working`, `sortie_sessions_running 0`, last log line for it is `worker exiting exit_kind=normal` with no `handoff transition succeeded` after it | sortie skipped the handoff; reactions only run for issues in `sortie:review` | relabel the issue `sortie:review`; if nothing dispatches within two polls, restart `run.sh` (the startup scan re-detects pending Codex comments) |
| `effort budget exhausted, blocking re-dispatch count=N max_sessions=N` | every continuation turn, including no-op review rounds, counts toward `agent.max_sessions` | raise it in `WORKFLOW.md` (hot-reloads), then restart `run.sh` — the blocked dispatch is not retried on its own |
| `bot review continuation turns exhausted, escalating` → `needs-human` | `reactions.bot_review.max_continuation_turns` reached; each push costs two rounds (findings, then the post-verdict no-op) | raise it (needs a restart), remove `needs-human`; the counter resets per restart |
| `no available orchestrator slots, rescheduling retry` repeating every 5 min for one issue | review-fix retries lose the slot race to fresh dispatches; each lost retry still increments `attempt` | raise `agent.max_concurrent_agents` by one (hot-reloads) |
| a PR has a clean Codex 👍 for 20+ minutes and no gate approval | the 👍 raises no workflow event and the cron trigger is throttled on quiet repositories | `gh workflow run codex-gate.yml` (the `run.sh` sidecar does this every 10 min) |
| approved PR, `mergeStateStatus UNSTABLE`, `auto_merge` idle | a check failed on the head (typically Linux-only behaviour; agents develop on macOS) | `reactions.ci_failure` hands the log excerpt to the agent; if it is missing from `WORKFLOW.md`, add it and restart |
| a PR keeps getting new findings on code added for earlier findings | review scope creep on a large PR | `AGENTS.md` → *Stay within the issue's scope*; only branches containing that rule are reviewed under it |
| PR blocked at `CHANGES_REQUESTED`, head unchanged for 10+ min, every finding carries an agent reply and no push | a declined P0/P1: a reply alone never clears that, only a Codex re-review does. (Declined P2/P3 findings approve on the reply; if the verdict still says it is waiting, the reply gave no reason — a bare acknowledgement does not count) | the prompt has the agent post `@codex review` on the same head after a P0/P1 decline; if the branch predates that, post the comment yourself, and if Codex re-raises a declined finding decide it by hand (the gate will not) |
| retry attempt climbing for one issue with no agent activity, error `worker exited: workspace preparation: hook run: exit_code=1` | the issue's workspace is mid-rebase, so `before_run`'s `git checkout sortie/<n>` cannot run; every dispatch dies before the agent starts. Stopping `run.sh` while an agent is rebasing leaves it this way | `cd build/sortie-workspaces/<n> && git status` shows a detached HEAD, `.git/rebase-merge` and `UU` paths; `git rebase --abort` restores the branch and the next dispatch succeeds. Check the reflog before assuming commits are lost — an agent that resets onto a new main and recommits has squashed them, not dropped them. Deleting the workspace also works; `after_create` re-clones |

Restart `run.sh` when `sortie_sessions_running` is 0 (`/metrics`): a restart cancels in-flight turns, and they resume as retries. Reactions (`reactions.*`) load only at startup; `agent.*`, `polling.*` and the prompt body hot-reload. GitHub's reviews and comments endpoints page at 30 and agent replies count as reviews, so always `--paginate` when inspecting a long-lived PR.

## Where state lives (all under the gitignored `build/`)

| path | what | safe to delete? |
|---|---|---|
| `build/sortie.db` | run history, sessions, cost | yes, loses `sortie stats` history |
| `build/sortie-workspaces/<issue>/` | one clone per issue, branch `sortie/<issue>` | yes once the PR is merged; sortie removes it on terminal state |
| `build/sortie-workspaces/CLAUDE.md` | optional machine-local instructions inherited by every workspace (not in the repo) | keep |
| `<workspace>/.sortie/` | per-run protocol files (`status`, `scm.json`, `model`, `mcp.json`) | managed by sortie |

## Things that expire or rot

- `PROJECT_TOKEN` (classic PAT): board sync fails silently-in-the-board when it
  expires — Actions show red runs. Rotate with `gh secret set PROJECT_TOKEN`.
- Deploy key: never expires, but revoke it if the machine is retired.
- sortie itself moves fast (minor every few weeks); re-run `sortie validate
  WORKFLOW.md` after upgrading and read the changelog for renamed keys.
- `agent.sh`'s default model alias: update when the model line-up changes.
