# Running Sortie against this repository

[Sortie](https://docs.sortie-ai.com) dispatches labelled issues, resumes their
existing PRs for fixes, and merges after formal approval and green CI. Both
profiles use the repository-owned Sortie 1.24.1 runtime in `nix develop .#agent`.
No global installation or host configuration change is needed.

| Profile | Launcher | Issue milestone | State and workspaces | Dashboard |
|---|---|---|---|---|
| Product | `sortie/run.sh` | `bzbd: shared CPU token pool` | `build/sortie.db`, `build/sortie-workspaces/` | 7678 |
| Lab | `sortie/run-lab.sh` | `agent lab: autonomous VM development` | `build/sortie-lab/` | 7679 |

The lab profile dispatches and reviews issues for the
[VM lab](../docs/design/agent-lab.md). Each of its attempts runs in a fresh
owned Linux worker: the `before_run` and `after_run` hooks open and close the
attempt, and every agent turn runs in the worker through `vmctl session
agent` (see [Agent sessions](../docs/design/agent-lab.md#agent-sessions)).
It needs the lab's local configuration and promoted Linux and macOS baselines
in this checkout, and the runner's model credential in the launcher's
environment (`CLAUDE_CODE_OAUTH_TOKEN` or an API key; Codex: `OPENAI_API_KEY`).
An agent's `lab handoff` reaches review only after the controller's evidence
gate has verified the pushed head against its base on both platforms
([evidence](../docs/design/agent-lab.md#evidence-required-for-a-verified-fix)).

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
controller before starting the replacement. CI review always uses Opus 5.5 high.
Per-runner qualification, including why Claude is allowed only inside an
allocated worker, is in [runner support](../docs/development/agent-review.md#runner-support).
`BUSYBEE_CURSOR_COMMAND` can select another compatible ACP command for Cursor.

A running process snapshots committed dispatch scripts, the VM controller,
skills, and policy from `origin/main` outside the task checkouts
(`sortie/snapshot.sh`). Fetch `main` before launch.
`BUSYBEE_SORTIE_TRUSTED_REF` may select a deliberately reviewed bootstrap
commit; it must never point to an automatically chosen candidate branch.
Candidate edits cannot replace the policy supervising that session.
`BUSYBEE_SORTIE_CLONE_URL` and `BUSYBEE_SORTIE_PORT` override the public clone
URL and the selected profile's dashboard port when needed.

## Dispatch and handoff

Issue states are `sortie:ready`, `sortie:working`, `sortie:review`, and
`sortie:done`; the `sortie` marker enables dispatch. Sortie owns state changes.
The lab controller rejects tracking/parked issues and requires prerequisites
to be completed by a merged PR into `main`, not merely closed, and the worker
capabilities the issue needs (a `Lab requires:` line adds to the defaults
`worker:linux`, `worker:macos`, `controller:session` and `controller:gate`) to
be available. Its dependency release sidecar runs once per
minute. An issue labelled `area:harness` runs with the `infrastructure`
profile; any other with `product`, whose session refuses changes to the paths
in `sortie/guard-policy.json`. The product profile retains its existing
`unblock.sh` dependency release behavior. Inspect lab eligibility manually with:

```sh
nix develop .#agent -c python3 sortie/lab.py check --issue NUMBER
nix develop .#agent -c python3 sortie/lab.py release --dry-run
nix develop .#agent -c python3 sortie/lab.py profile --issue NUMBER
```

`check` and `release` read worker capabilities from the trusted controller's
`doctor`, so they need `BUSYBEE_LAB_ROOT` naming this checkout (the launcher
sets it).

`prepare-workspace.sh` preserves `sortie/<issue>` or `sortie-lab/<issue>` across
continuations, including unfinished work and remotely existing branches. In the
lab profile that workspace is also the checkpoint of the worker's work, and
`vmctl session status --issue NUMBER` lists an issue's attempts and how each
ended. It
sets credentials only inside the disposable checkout. Product tests must own
private Pueue/bzbd state. The Claude machine-safety hook is a cooperative guard,
not a security boundary. `isolated.sh` points `PUEUE_CONFIG_PATH` at a generated
workspace-local YAML file; `test-isolated-launcher.sh` drives a real Pueue
through it and fails when Pueue is missing. It is not a cold-start fixture.

## Operating the lab profile

`sortie/run-lab.sh` is the one entry point: it dispatches every eligible lab
issue, and each attempt runs, verifies and hands off on its own. To see how an
issue's runs ended, without opening a dashboard or a worker:

```sh
nix develop -c python3 scripts/vm/vmctl.py session status --issue NUMBER
```

It lists every attempt with its turns, worker and outcome, then the latest
verification: head and base, verdict, and the findings it rests on. `--json`
adds the records' paths under `build/vm/` (attempts, verifications, gates and
the platform matrices with their public evidence). `vmctl status` accounts for
every owned worker. To judge a revision by hand as the session would:

```sh
nix develop -c python3 scripts/vm/vmctl.py gate --issue NUMBER --revision REV [--base REV]
```

## Review loop

The [shared review contract](../docs/development/agent-review.md) is
authoritative for every same-repository PR. `WORKFLOW.md` and `LAB_WORKFLOW.md`
own the runtime bounds; in particular,
`reactions.bot_review.max_continuation_turns` controls review fix rounds. A
maintainer can request reevaluation with:

```sh
gh workflow run agent-review-gate.yml --ref main -f pr=NUMBER
```

## Operations and checks

Use the selected dashboard and `sortie/peek.sh` for product session inspection.
An existing launcher lock requires checking the previous process before
removing it. Preserve workspaces and reports when escalating; remove
`needs-human` and re-enable an issue only after its blocker is resolved.
Restart a stopped controller to adopt a newly merged policy snapshot.

```sh
nix develop .#agent -c python3 -m unittest discover -s sortie/tests
nix develop .#agent -c bash sortie/test-machine-safety-hook.sh
nix develop .#agent -c bash sortie/test-isolated-launcher.sh
bash sortie/test-harness-docs.sh
```

The ordinary Linux/macOS workflow also runs these harness checks. See the
review contract for the OAuth secret, Actions approval setting, evidence
artifacts and transcripts, and the pinned one-PR bootstrap procedure.
