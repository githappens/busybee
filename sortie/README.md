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
  2. `.github/workflows/codex-gate.yml` reads that review for the head commit and,
     as `github-actions[bot]`, submits REQUEST_CHANGES (any inline finding) or
     APPROVE (none). The `main` ruleset requires one approval and dismisses it on
     push, so the review decision tracks the latest Codex verdict. Needs the repo
     setting "Allow GitHub Actions to create and approve pull requests".
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
  turn; on the next start sortie reconciles against tracker state.
- **Change concurrency, polling, model**: edit `WORKFLOW.md`; most `agent.*`
  and `polling.*` keys hot-reload, reactions need a restart (`sortie validate`
  tells you if the file is malformed).

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
