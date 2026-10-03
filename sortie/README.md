# Running Sortie against this repository

[Sortie](https://docs.sortie-ai.com) dispatches labelled issues, resumes their
existing PRs for fixes, and merges after formal approval and green CI. There
is one dispatch profile, `sortie/LAB_WORKFLOW.md`, launched with
`sortie/run-lab.sh`. It uses the repository-owned Sortie 1.24.1 runtime in
`nix develop .#agent`; no global installation or host configuration change is
needed. Its state is `build/sortie-lab/` and its dashboard listens on port 7679.

Every attempt runs in a fresh owned Linux worker of the
[VM lab](../docs/design/agent-lab.md): the `before_run` and `after_run` hooks
open and close the attempt, and every agent turn runs in the worker through
`vmctl session agent` (see [Agent sessions](../docs/design/agent-lab.md#agent-sessions)).
It needs the lab's local configuration and promoted Linux and macOS baselines
in this checkout, and the runner's model credential in the launcher's
environment (`CLAUDE_CODE_OAUTH_TOKEN` or an API key; Codex: `OPENAI_API_KEY`).
An agent's `lab handoff` reaches review only after the controller's evidence
gate has verified the pushed head against its base on both platforms
([evidence](../docs/design/agent-lab.md#evidence-required-for-a-verified-fix)).

## Start and validate

Authenticate `gh` and the chosen implementation agent first. The launcher uses
`gh` credentials for the tracker and pull requests, and for task checkouts
over HTTPS. Validation needs no account or credentials:

```sh
sortie/run-lab.sh --agent claude --validate
sortie/run-lab.sh --agent claude --dry-run
sortie/run-lab.sh --agent claude
```

`--agent claude|codex|cursor` selects the runner; Codex is the default, and
Cursor is refused until it is qualified inside workers. Stop an old
controller before starting the replacement. CI review always uses Opus 5.5
high. Per-runner qualification is in
[runner support](../docs/development/agent-review.md#runner-support).

A running process snapshots committed dispatch scripts, the VM controller,
skills, and policy from `origin/main` outside the task checkouts
(`sortie/snapshot.sh`). Fetch `main` before launch.

## Workspaces and Git transport

The operator chooses where issue checkouts live and how they reach GitHub
with environment variables of the launching process:

| Variable | Default | Effect |
|---|---|---|
| `BUSYBEE_SORTIE_WORKSPACES` | unset: `build/sortie-lab/workspaces/<issue>` | An absolute directory shared with other checkouts. Sortie keeps each issue's checkout in `<dir>/.sortie/busybee/<issue>`, and `prepare-workspace.sh` links it as `<dir>/busybee-<issue>`. |
| `BUSYBEE_SORTIE_CLONE_URL` | `https://github.com/githappens/busybee.git` | The clone, fetch and push URL. An SSH URL through a host alias, `git@<alias>:githappens/busybee.git`, uses that alias's key. |
| `BUSYBEE_SORTIE_PORT` | `7679` | The dashboard port. |
| `BUSYBEE_SORTIE_TRUSTED_REF` | `origin/main` | A deliberately reviewed bootstrap commit to run the policy from; never an automatically chosen candidate branch. |
| `BUSYBEE_CURSOR_COMMAND` | `agent acp` | Another compatible ACP command for Cursor. |

Sortie names an issue's directory by its number and refuses a workspace that
is a symbolic link, so the `busybee-<issue>` name is the link, not the
checkout. A name already used by something that is not a link is left alone,
with a warning in the hook output; the launcher removes links whose checkout
Sortie has since deleted. Change the root only while no issue is in flight:
Sortie looks for existing workspaces under the current root only. The lab
mirrors the workspace path in the guest, so it must not contain whitespace.

Over HTTPS, `prepare-workspace.sh` routes the checkout's credentials through
`gh` in its local Git configuration. Over SSH it installs no credential
helper: the alias in the operator's SSH configuration must select a dedicated
deploy key with write access to this repository, never an interactive or
hardware-backed key. The checkout runs SSH with `BatchMode=yes`, so a key
that would prompt fails the fetch or push instead of stalling a run. The
session fetches `origin/main` and pushes from the workspace, so the same
transport serves the whole session.

### What hooks see

Sortie runs every hook (and the review triage script) as `sh -c` with a
restricted environment: a small system allowlist (`PATH`, `HOME`, `USER`,
`TMPDIR`, `SSH_AUTH_SOCK`, ...) plus every `SORTIE_*` variable of its own
process. Anything else the launcher exports, including the variables above and
`GITHUB_TOKEN`, is stripped. The launcher therefore also exports its settings
as `SORTIE_BUSYBEE_*`, and each hook first sources the trusted
`sortie/hook-env.sh`, which maps them back to the names the lab scripts read
and stops the hook with a message naming the missing setting when one is
absent. The agent command is not filtered: `vmctl session agent` inherits the
launcher's whole environment, including the model credential.
`--validate` and `--dry-run` never run hooks; `sortie/tests/test_hook_env.py`
runs the real hook bodies with Sortie's filtering, and, inside
`nix develop .#agent`, one real dispatch through the pinned binary.

## Dispatch, ordering and concurrency

Issue states are `sortie:ready`, `sortie:working`, `sortie:review`, and
`sortie:done`; the `sortie` marker enables dispatch, and Sortie owns state
changes. Any open issue in the repository is a candidate once an operator
labels it `sortie:ready`, whatever its milestone. The dependency release
sidecar (`lab.py release`, once per minute) gives it the `sortie` marker, and
takes the marker back, according to:

- **Dependencies.** An issue that must wait for another lists it as *blocked
  by* (GitHub's issue dependencies). It is released only when every blocker
  was completed by a merged PR into `main`, not merely closed. This is the
  only ordering: milestones, numbers and labels do not order dispatch.
- **Capabilities.** The worker capabilities the issue needs must be available
  now. Every issue needs `worker:linux`, `worker:macos`, `controller:session`
  and `controller:gate`; a line such as `**Lab requires:** controller:terminal`
  in the issue body adds more. Tracking (`epic`) and parked (`needs-human`)
  issues are never released.
- **Profile.** An issue labelled `area:harness` runs with the `infrastructure`
  profile; any other with `product`, whose session refuses changes to the
  paths in `sortie/guard-policy.json`.

A product issue must show its regression to be handed to review: test-only
files the agent names with `lab handoff --overlay PATH...` (red on the base,
green on the head), or a scenario under `tests/scenarios/` naming the issue.
An ordinary bug fix or feature needs nothing more in its body; its agent adds
the test first. A product change that cannot fail on its base (documentation,
a pure refactor) is reported blocked rather than given an invented test, so
label such work `area:harness` only if it really is harness work, or do it by
hand. Inspect eligibility with:

```sh
nix develop .#agent -c python3 sortie/lab.py check --issue NUMBER
nix develop .#agent -c python3 sortie/lab.py release --dry-run
nix develop .#agent -c python3 sortie/lab.py profile --issue NUMBER
```

`check` and `release` read worker capabilities from the trusted controller's
`doctor`, so they need `BUSYBEE_LAB_ROOT` naming this checkout (the launcher
sets it).

Up to two issues run at once (`agent.max_concurrent_agents` in
`LAB_WORKFLOW.md`). The lab admits up to `[concurrency] linux_workers` Linux
workers (default 2) in `build/vm/local.toml`, each with the `[worker]`
allocation, which must fit the per-worker `[budget]`; the macOS slot is one
more guest. A session releases its worker before its handoff is verified, so
two sessions need two Linux workers at most; a creation or verification that
finds them all busy waits in line. `vmctl doctor` prints the cap and what the
configured workers need together, and warns when the host has less.

`prepare-workspace.sh` preserves `sortie-lab/<issue>` across continuations,
including unfinished work and remotely existing branches. That workspace is
also the checkpoint of the worker's work. Product tests must own private
Pueue/bzbd state. The Claude machine-safety hook is a cooperative guard, not
a security boundary. `isolated.sh` points `PUEUE_CONFIG_PATH` at a generated
workspace-local YAML file; `test-isolated-launcher.sh` drives a real Pueue
through it and fails when Pueue is missing. It is not a cold-start fixture.

## Operating the lab profile

`sortie/run-lab.sh` is the one entry point: it dispatches every eligible
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
authoritative for every same-repository PR. `LAB_WORKFLOW.md` owns the
runtime bounds; in particular, `reactions.bot_review.max_continuation_turns`
controls review fix rounds. A maintainer can request reevaluation with:

```sh
gh workflow run agent-review-gate.yml --ref main -f pr=NUMBER
```

## Operations and checks

Use the dashboard and `vmctl session status` for session inspection. An
existing launcher lock requires checking the previous process before removing
it. Preserve workspaces and reports when escalating; remove `needs-human` and
re-enable an issue only after its blocker is resolved. Restart a stopped
controller to adopt a newly merged policy snapshot.

```sh
nix develop .#agent -c python3 -m unittest discover -s sortie/tests
nix develop .#agent -c bash sortie/test-machine-safety-hook.sh
nix develop .#agent -c bash sortie/test-isolated-launcher.sh
bash sortie/test-harness-docs.sh
```

The ordinary Linux/macOS workflow also runs these harness checks. See the
review contract for the OAuth secret, Actions approval setting, evidence
artifacts and transcripts, and the pinned one-PR bootstrap procedure.

### Resetting an issue whose retries a harness failure burned

A hook or launcher failure fails every attempt before a turn runs, yet each
attempt still counts against `agent.max_sessions` and leaves the issue in
`sortie:working` with a growing retry backoff. Once the harness is fixed, stop
the launcher (no `build/sortie-lab/launcher.lock` may remain), then:

```sh
nix develop .#agent -c python3 sortie/lab.py reset \
  --state build/sortie-lab --issue N [--issue M ...]
```

It deletes the issues' rows from Sortie's per-issue tables in
`build/sortie-lab/sortie.db` (`run_history`, which is the session budget,
`retry_entries`, `session_metadata`, `parked_issues`, `budget_hold_notices`,
`handoff_absence_resets`, `reaction_fingerprints`), and moves the issue from
`sortie:working` back to `sortie:ready`. Lab session records under
`build/vm/sessions/` are evidence and stay. If a failed attempt left a
checkout without a `.git` directory under the workspace root, remove it so
`after_create` clones afresh. Reset only issues whose runs never reached
the agent: a reset discards real attempt history too.
