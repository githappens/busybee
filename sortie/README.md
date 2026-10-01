# Running Sortie against this repository

[Sortie](https://docs.sortie-ai.com) dispatches labelled issues, resumes their
existing PRs for fixes, and merges after formal approval and green CI. Both
profiles use the repository-owned Sortie 1.24.1 runtime in `nix develop .#agent`.
No global installation or host configuration change is needed.

| Profile | Launcher | Issue milestone | State and workspaces | Dashboard |
|---|---|---|---|---|
| Product | `sortie/run.sh` | `bzbd: shared CPU token pool` | `build/sortie.db`, `build/sortie-workspaces/` | 7678 |
| Lab | `sortie/run-lab.sh` | `agent lab: autonomous VM development` | `build/sortie-lab/` | 7679 |

The [VM lab design](../docs/design/agent-lab.md) describes planned disposable
workers, observation, and bounded scenarios. This bootstrap supplies dispatch
and review; it does not claim that those VM capabilities already exist.

## Start and validate

Authenticate `gh` and the chosen implementation agent first. The launcher uses
`gh` credentials for tracker access and HTTPS task checkouts. It does not need
a private SSH alias. Validation needs no account or credentials:

```sh
sortie/run.sh --agent codex --validate
sortie/run-lab.sh --agent claude --validate
sortie/run-lab.sh --agent cursor --validate
sortie/run-lab.sh --agent codex --dry-run
sortie/run-lab.sh --agent codex
```

Both launchers accept `--agent claude|codex|cursor`; Codex is the default.
They share `launch.sh`, with independent profile state and locks. Stop an old
controller before starting the replacement. Existing product workspaces and
database paths are preserved; per-issue `model:` labels from the old Claude
wrapper are no longer consumed. Select the implementation runner at launch;
CI review always uses Opus 5.5 high.

Claude's native adapter in Sortie 1.24 requires `bypassPermissions`, so the
launcher permits Claude only inside an allocated worker with
`BUSYBEE_SORTIE_WORKER=1`. Do not set that flag on a shared host. Cursor uses
`agent acp`; `BUSYBEE_CURSOR_COMMAND` can select another compatible ACP command.
Its unattended tools and permissions still require worker qualification.
Codex uses `codex app-server` with a workspace sandbox and noninteractive
approval policy. See [runner qualification](../docs/development/agent-review.md#runner-support).

A running process snapshots committed controller scripts, skills, and policy
from `origin/main` outside the task checkouts. Fetch `main` before launch.
`BUSYBEE_SORTIE_TRUSTED_REF` may select a deliberately reviewed bootstrap
commit; it must never point to an automatically chosen candidate branch.
Candidate edits cannot replace the policy supervising that session.
`BUSYBEE_SORTIE_CLONE_URL` and `BUSYBEE_SORTIE_PORT` override the public clone
URL and the selected profile's dashboard port when needed.

## Dispatch and handoff

Issue states are `sortie:ready`, `sortie:working`, `sortie:review`, and
`sortie:done`; the `sortie` marker enables dispatch. Sortie owns state changes.
The lab controller rejects tracking/parked issues and requires prerequisites
to be completed by a merged PR into `main`, not merely closed. Its dependency
release sidecar runs once per minute. The product profile retains its existing
`unblock.sh` dependency release behavior. Inspect lab eligibility manually with:

```sh
nix develop .#agent -c python3 sortie/lab.py check --issue NUMBER
nix develop .#agent -c python3 sortie/lab.py release --dry-run
```

`prepare-workspace.sh` preserves `sortie/<issue>` or `sortie-lab/<issue>` across
continuations, including unfinished work and remotely existing branches. It
sets credentials only inside the disposable checkout. Product tests must own
private Pueue/bzbd state. The Claude machine-safety hook is a cooperative guard,
not a security boundary; the isolated wrapper's cold-start defect remains #71.

## One review loop

The [shared review contract](../docs/development/agent-review.md) is authoritative
for all same-repository PRs, regardless of branch name or implementation runner:

1. Implement and test the issue, then push a draft PR with its closing issue.
2. Mark it ready and use the trusted `reviews.py handoff` helper immediately.
   The helper records the pushed SHA and time in `.sortie/scm.json` and writes
   `needs-human-review` to `.sortie/status`.
3. Linux/macOS CI passes, then `agent-review-gate.yml` runs independent contract
   and ponytail reviews with Claude Opus 5.5 high.
4. A separate publisher posts a formal `APPROVE` or `REQUEST_CHANGES` as
   `github-actions[bot]`, tied to the exact reviewed head.
5. Sortie's trusted triage resumes scoped fixes for `BLOCKED`, ignores settled
   `READY`/`WAITING` evidence, and escalates `UNSURE` to `needs-human`. Fixes and
   concrete author dispositions trigger follow-up reviews on the same PR.
6. Sortie squash-merges only with GitHub approval and green CI, respecting the
   repository rules. Agents do not merge or bypass protections.

`WORKFLOW.md` and `LAB_WORKFLOW.md` own the runtime bounds; in particular,
`reactions.bot_review.max_continuation_turns` controls review fix rounds.
Review and merge watch windows cover the bounded CI sessions. There is no
separate wording judge, review-reaction polling script, or gate timer sidecar.
The workflow runs after CI, on comments, on a recovery schedule, and by manual
dispatch. A maintainer can request reevaluation with:

```sh
gh workflow run agent-review-gate.yml --ref main -f pr=NUMBER
```

Unchanged evidence is reused. To retry a retained authentication/tool failure,
first resolve the cause, then add an author disposition explaining the fix.
A new head or disposition produces new review inputs; an empty commit is not
needed. A new push dismisses the old approval, so rebase only for actual conflicts.

## Operations and checks

Use the selected dashboard and `sortie/peek.sh` for product session inspection.
An existing launcher lock requires checking the previous process before
removing it. Preserve workspaces and reports when escalating; remove
`needs-human` and re-enable an issue only after its blocker is resolved.
Restart a stopped controller to adopt a newly merged policy snapshot.

```sh
nix develop .#agent -c python3 -m unittest discover -s sortie/tests
nix develop .#agent -c bash sortie/test-machine-safety-hook.sh
bash sortie/test-harness-docs.sh
```

The ordinary Linux/macOS workflow also runs these harness checks. See the
review contract for the OAuth secret, Actions approval setting, evidence
artifacts, and the pinned one-PR bootstrap procedure. Each review run retains
separate contract/ponytail transcript artifacts for seven days, including
failed sessions that produced a transcript; download them from the run to debug.
