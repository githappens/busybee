# Autonomous development and verification workflow

This document specifies the intended workflow for agents fixing busybee in
disposable Parallels VMs. An agent must be able to reproduce an issue, change
the code, inspect the running terminal UI, and verify the fix on Linux and
macOS without asking a person to operate its development environment.

**Status: design for implementation.** `doctor`, the Linux `template`
operations, and Linux workers with `exec`, `inspect`, `signal`, `console
capture`, `collect`, run supervision, exec handles (`status`, `wait`, `read`),
the public `export`, Linux regression `scenario` runs and the `terminal`
operations (`scripts/vm/vmctl.py`, `tests/scenarios/`), macOS workers and
`verify`, issue sessions that run the lab dispatcher's agents in their own
workers (§Agent sessions), and the evidence `gate` every lab handoff passes
(§Evidence required for a verified fix) have shipped.
Until the rest lands, [CLAUDE.md](../../CLAUDE.md) and the
[Sortie workflow](../../sortie/README.md) remain the operational instructions.

This is a development facility, not a runtime dependency of busybee. Broker
semantics remain defined by [bzbd.md](bzbd.md). A product change that alters
those semantics must update that specification in the same pull request.

## Ownership and boundaries

All project-specific Nix definitions, provisioning scripts, and test scenarios
live in this repository. The host supplies existing Nix and Parallels
installations. The workflow must not import a personal configuration repository,
edit or activate host system configuration, or install into the host's global
tool profile. `nix develop` supplies temporary
controller tooling; normal Nix store and cache writes are expected.

The public repository contains generic configuration and examples. Actual VM
identifiers, snapshot IDs, machine paths, and credential references are supplied
through arguments or ignored local configuration. Credentials and private host
configuration must not appear in committed files or published run artifacts.

| Component | Owns |
|---|---|
| Template builder | Reproducible guest provisioning, validation, and baseline versions. |
| Host controller | Parallels lifecycle, resource limits, deadlines, source transfer, and artifact retention. |
| Agent | One issue, its branch, and administrative control inside its assigned worker. |
| Scenario runner | Reproduction steps, assertions, daemon fixtures, and terminal interaction. |
| Review and merge runner | The shared [review workflow](../development/agent-review.md). |

An agent may install tools, restart daemons, signal processes, and deliberately
break its guest while investigating. It operates through a controller that can
address only the worker assigned to that run. The controller and review gate
use a trusted revision outside the guest; editing a branch does not change the
controller that is supervising it. Their own changes follow the repository's
existing infrastructure review policy.

## Repository layout

The intended layout is:

```text
flake.nix                      Existing project development shell
infra/vm/flake.nix              Pinned template and controller tooling
infra/vm/flake.lock             Template dependency lock
infra/vm/linux/                NixOS guest and unattended bootstrap definitions
infra/vm/macos/                Prepared baseline requirements and guest setup
infra/vm/local.example.toml     Generic local configuration schema
scripts/vm/                    Controller and its tests
tests/scenarios/               Executable reproductions and scenario metadata
build/vm/local.toml            Ignored local configuration
build/vm/workers/              Disposable VM files and worker registry
build/vm/runs/<run-id>/         Exported source changes and verification artifacts
```

The root development shell remains the source of project tool versions. The VM
flake adds provisioning and observation tools without duplicating a separate
Rust, make, Ninja, or Pueue toolchain for each platform. Ordinary contributors
can continue using the existing development shell and tests without Parallels.

Guest checkouts and build output live on the guest's own filesystem. The
controller transfers an explicit source revision into each worker and exports
changes and evidence afterwards. Workers do not receive a shared host home
directory or a writable mount of the host checkout. Gitignored `build/vm/`
holds local operational state; published evidence is a separate, sanitized
export from that state.

## Build and validate templates

Linux workers use a NixOS guest defined in this repository. A Linux bootstrap
VM builds the Linux image; the macOS host drives that operation without adding
a system-wide remote-builder configuration. The bootstrap procedure must also
be scripted, with pinned installer inputs and unattended authentication.
Ansible is not a second source of guest package and service configuration.

macOS verification starts from a prepared Parallels baseline with Nix, the
required Apple development tools, and unattended command access. Project tools
come from the repository's development shell. Routine runs require no
system-configuration activation. Any OS setup or consent that requires human
interaction belongs to baseline preparation and is reported by template validation before
the template becomes eligible for workers.

Template preparation follows this sequence:

1. Provision a dedicated template candidate from versioned definitions.
2. Verify boot, unattended command access, file transfer, terminal control,
   console capture, required tools, shutdown, and snapshot restoration.
3. Warm dependency caches without starting busybee or Pueue. Remove task state,
   sockets, leases, and credentials introduced by validation.
4. Shut down cleanly and create a baseline snapshot. Record the guest OS,
   architecture, provisioning revision, lockfile hashes, tool versions,
   Parallels version, and snapshot identity in a local template manifest.
5. Clone that snapshot and run a validation scenario. Only a validated
   candidate becomes an eligible baseline. Retain the previous baseline until
   workers that depend on it have finished.

Use linked clones where the guest and installed Parallels version support them.
If that capability is unavailable, report it and require an explicit full-clone
configuration; do not silently change the storage or reset strategy. macOS
guests do not support them: Parallels accepts `prlctl clone --linked`, but its
macOS engine attaches only the clone's empty overlay disk and the guest does not
boot. A macOS template manifest never lists `linked` in `clone_modes`.

Nix generations describe guest system configuration. Parallels snapshots
provide the reset of mutable files and runtime state. Updating packages alone
does not establish a clean test environment. Snapshot restoration targets an
explicit baseline identity rather than whichever snapshot happens to be latest.

### macOS workers: one leased guest

Apple's licence and its Virtualization framework allow at most two running
macOS guests per host; starting another fails, and Parallels reports the reason
only in that VM's log. Operators also run macOS VMs of their own, so the lab
owns a single macOS slot: one long-lived guest, prepared from the validated
macOS baseline, leased to one run at a time. macOS workers are not cloned per run.

- **Lease.** `worker create` for macOS waits for the slot and grants it
  exclusively. Exclusion is a lock held by the controller process for the
  lease, so a holder that exits for any reason, including being killed, frees
  the slot. Waiters are served in arrival order and see their queue position.
  The lease carries the usual run ID, ownership record, and deadline.
- **Reset before every grant.** Each holder starts from the recorded baseline
  snapshot: stop, restore that snapshot, boot, and wait until command access
  and the Nix store are ready. A guest that may have been changed since its
  last reset, including after a holder that never released, is reset before
  the next grant. Resetting eagerly on release is an allowed optimisation.
- **Started on demand.** The guest may be stopped while idle, by an operator or
  after a host restart. Granting a lease starts it when it is not running.
  Holders do not shut it down; releasing the lease is enough.
- **Evidence first.** A holder halted with uncollected evidence (`retained`,
  `stopped`, `expired`) keeps the slot until it is destroyed, so no grant
  resets its guest under it.
- **Independent of busybee.** The lease, its queue, and its deadlines are
  controller functions. Busybee is not installed in the baseline and does not
  schedule the lab's own work.

The guest has no Parallels guest tools; SSH is its control channel, and its
address comes from Parallels' DHCP leases for the VM's MAC address. A guest
cannot run a macOS release newer than the host. Every reset discards what the
previous holder downloaded, so the macOS baseline is snapshotted after the
dependency-warming step above. A reset then costs about the boot time.

## Run an issue from reproduction to review

1. **Prepare the task.** Read its scope, acceptance criteria, named regressions,
   and relevant broker specification. Record the base revision and the required
   platform checks. Reuse an existing issue branch and PR when continuing work.
2. **Allocate a worker.** Validate the selected template, reserve the
   configured worker allocation, which must fit the per-worker `[budget]`,
   create a clone (macOS: lease the single guest, see §macOS workers), and
   transfer the checkout. Up to `[concurrency] linux_workers` Linux workers
   (default 2) are active at once, plus the macOS slot; a further creation
   waits in line. Assign a run ID and an overall deadline before execution.
   The lab dispatcher does this per attempt (§Agent sessions).
3. **Reproduce the failure.** Build the workspace from the recorded source.
   Run a bounded reproduction and save the failing assertion, command, output,
   and state. Distinguish a product failure from missing tools, failed boot,
   inaccessible daemons, and exhausted test deadlines.
4. **Implement the fix.** Add the regression first, then make the smallest
   change that satisfies it. Follow [AGENTS.md](../../AGENTS.md) for scope,
   resource accounting, public repository hygiene, and review. Checkpoint code
   changes outside the disposable guest during the work.
5. **Verify the candidate.** Rebuild the final revision in fresh Linux and macOS
   workers. Run the relevant regressions and the repository's required build,
   formatting, lint, and test checks. Platform-specific scenarios declare their
   applicable platform; an unavailable required platform is incomplete
   verification. A rebase or new commit invalidates evidence for the old head.
6. **Inspect the interface.** For terminal behavior changes, run the real CLI
   and monitor, inspect the captured screens, and exercise relevant keys and
   resize behavior. Record both visual evidence and assertions on the terminal
   contents. Unit rendering tests remain useful alongside these checks.
7. **Export and review.** Export commits or a patch, the result manifest, logs,
   and visual evidence. Open or reuse a draft PR explaining the original
   failure, resulting behavior, specification changes, and checks. Mark the
   implementation ready and hand off to Sortie, which follows the shared
   [review workflow](../development/agent-review.md).
8. **Continue or dispose.** Review findings, CI failures, or conflicts resume
   the same task with its branch and evidence in a new worker. A settled review
   with unchanged evidence does not launch another judgement. When the task
   finishes, collect final artifacts and destroy the worker or restore its
   baseline before reuse.

The agent does not need permission for normal edits, tests, resets, or fault
injection inside its assigned worker. An inability to proceed produces a
durable blocked result with the exact cause and attempted checks. It must not
turn into repeated requests for someone to run commands or capture screenshots.
New capabilities outside the assigned environment remain a separate decision.

## Controller interface

The following are proposed operations. Each produces a machine-readable result
and a short human-readable summary; implementation may expose them through a
CLI usable by different agent runners.

| Operation | Contract |
|---|---|
| `doctor` | Check host prerequisites, local configuration, template eligibility, and available resource budget without changing host configuration. |
| `template build`, `validate`, `promote` | Provision a candidate, prove its capabilities, and register its baseline version. |
| `worker create` | Clone an explicit baseline, allocate a run ID, and return the worker identity and deadline, waiting in line while the configured number of Linux workers is active. For macOS, wait for and lease the single guest instead of cloning (§macOS workers). |
| `exec` | Run argv in the guest with a working directory, environment, deadline, separate stdout/stderr, and exit status. |
| `terminal open`, `send`, `resize`, `capture` | Operate a real PTY; expose input, dimensions, screen cells, images, and a timestamped terminal recording. |
| `inspect`, `signal` | Read process and daemon state and signal processes belonging to the worker. |
| `console capture` | Capture the VM display through Parallels, including when command access is unavailable. |
| `collect` | Export source changes and evidence, then acknowledge which artifacts were saved durably. |
| `worker reset`, `destroy` | Collect first, then restore the recorded baseline or remove the owned clone. For the leased macOS guest, `destroy` releases the lease; the guest itself is kept. |

The shipped entry point is `python3 scripts/vm/vmctl.py [--json] <operation>`
inside the project development shell, configured by ignored
`build/vm/local.toml` (copied from `infra/vm/local.example.toml`). Result,
template and worker schemas and exit codes are documented in
`scripts/vm/vmctl.py` and `scripts/vm/contracts.py`.

`doctor` and `template build`, `validate` and `promote` have shipped for the
Linux and macOS templates. `template build linux --arch aarch64` installs NixOS from the
installer pinned in `infra/vm/linux/installer.json` into a dedicated candidate
VM with a 16 GiB expanding disk: a typed console command authorizes a run-scoped key on the installer, whose
live environment then evaluates and builds `infra/vm/flake.nix`. The candidate
is warmed, cleaned, shut down and snapshotted. `validate` clones that snapshot
with the configured strategy and checks each capability on the clone;
`promote` accepts only a validated candidate and keeps the baseline it
replaces. Every VM the controller creates is claimed in
`build/vm/registry.json` before it exists, and no Parallels call changes a VM
that is not claimed there.

macOS has no unattended installer, so its one-time setup is an operator's
prepared VM, named with the lab account and a bootstrap key in `[templates.macos]`:
the OS, an account with passwordless sudo, Determinate Nix with an
unencrypted store, and SSH. `template build macos --arch arm64` requires that
source to be stopped, full-clones it into an owned candidate (the source is
only read), removes the clone's host devices and sharing, and provisions it
over SSH from `infra/vm/macos`: the Command Line Tools pinned in
`baseline.json` through `softwareupdate`, a neutral host name, and the run's
own SSH host key and access key in place of the source's, after which the
bootstrap key no longer opens the candidate. Warming adds `nix develop -c
cargo fetch` and installs the development shell's coreutils into the
account's profile for GNU `timeout`, which bounds every exec. A macOS guest
answers an ACPI stop with a confirmation dialog, so it is shut down from
inside. Its `validate` adds `nix_store`, which waits for determinate-nixd to
mount `/nix` after SSH answers, and `dev_tools`, which names each missing
tool; a PTY there is `/dev/ttys*`. A start refused at Apple's limit is the
named finding `macos_guest_limit`, read from the VM's `parallels.log`.
`[templates.macos] clone_strategy` must be `full`; a configuration that would
clone macOS linked is invalid rather than switched.

`worker create macos` takes the slot (§macOS workers): a FIFO ticket under
`slots/macos/`, then an flock on its `slot.lock`, whose descriptor the run's
supervisor inherits and holds; a restarted supervisor takes it back only while
its run still holds the slot, and otherwise records `lease_lost`. The slot
guest is a full clone of the promoted baseline, registered with the role
`slot` and its holder, snapshotted before its first start and replaced when
another baseline is promoted. Each grant restores that snapshot, starts the
guest and transfers the source; `destroy` collects, stops the guest and
releases the lease. The guest's disk is the baseline's own and is reverted at
every grant, so only the host's free storage is checked against `storage_gib`.

`worker create linux --revision REV [--patch FILE]` claims the worker in the
registry, naming the baseline it depends on, then clones the promoted
baseline's snapshot with the configured strategy, applies the `[worker]`
allocation, and snapshots the clone before it first starts. The run record
`build/vm/runs/<run-id>/worker.json` holds that reset snapshot, the source
revision and patch hash, and the run deadline. `[budget]` bounds one VM: each
worker's `[worker]` allocation must fit it (and a template build is given
it). Up to `[concurrency] linux_workers` Linux workers (default 2, at most 16)
are active at once, each owned by its own run; the macOS slot is apart and
not counted. Every registered Linux worker counts until it is destroyed,
whatever its state, so a retained worker holds its place. A creation that
finds them all active takes a FIFO ticket under `queues/linux/` and waits,
retrying admission every few seconds without holding the controller lock,
for up to the run deadline; it then fails `worker_limit`, naming the active
runs. Admission and the claim are one step under `controller.lock`, so two
controller processes cannot both take the last place. Only the limit is
waited out: a storage or baseline refusal returns at once. Each worker's
`storage_gib` must cover the baseline's disk, which a clone can grow to, and
the host must have that storage free when it is admitted. `doctor` reports
the cap and the peak the configured workers need together (Linux workers and
the macOS slot, each with the allocation; the slot's disk is its baseline's
own) and warns (`concurrency_exceeds_host`) when the host has less.
The revision travels as a Git bundle with its history and tags, so the guest
checkout versions its build like the host's. `exec RUN --cwd DIR [--env
NAME=VALUE] [--timeout S] -- ARGV` quotes every word, bounds the command by
`timeout(1)` in the guest and by the run deadline, streams stdout and stderr to
separate files under the run directory, and records exit status, timings, the
guest checkout's revision and the digests of any built `busybee`, `bzb` and
`bzbd`. A nonzero exit is `product_failure`; an expired deadline is `timeout`.
If the guest stops answering, the controller captures its console and stops the
VM, which stays registered. `inspect` reports VM state, guest processes and
daemon state; `signal RUN SIGNAL PID` signals one guest process; `console
capture` saves the display of a running worker. `collect` exports the commits
made since the recorded revision as a bundle, uncommitted and untracked changes
as a diff, and the digests of the exec logs, each written durably, and lists
what it could not save. `worker reset` and `destroy` collect first; when
collection is incomplete they stop the clone and keep it registered, and the
result is `incomplete_collection`. Reset then restores the snapshot recorded at
creation and transfers the source again; destroy deletes the clone. Until its
source is in the guest a worker is `provisioning`: exec refuses it, and reset
and destroy have nothing to collect from it. Collection
and the cleanup it precedes run within `deadlines.cleanup`.

Every worker has a supervisor (`scripts/vm/supervisor.py`): a detached
controller process, started in its own session by `worker create` and restarted
by any later call that finds it gone, holding a lock in the run directory. It
needs no host service or system configuration. `exec` only queues the command;
the supervisor runs it, so killing the calling process or agent ends nothing.
In the guest, `timeout(1)` leads the command's process group and records its
pid. The supervisor enforces, independently of the guest and of busybee:

- the command deadline: a command still running `timeout -k` plus a margin
  after its deadline means the guest's own bound failed, and its process group
  is killed (`watchdog_deadline`);
- the artifact budget: `[worker] artifact_mib` caps the run's logs and
  evidence; a command that outgrows it is killed (`artifact_budget_exceeded`)
  and later commands are refused;
- the run deadline: unfinished commands are killed (`run_deadline`), the worker
  is collected within the cleanup window and halted, and it becomes `expired`;
- guest control: if the guest stops answering, its console and the evidence
  already on the host are kept and the VM is stopped (`guest_unresponsive`).

A supervisor that dies leaves its ssh processes writing the logs; its
successor adopts them and reads each exit status from the guest. Each finished
command checkpoints the guest's source changes to `checkpoint/`.

`exec --detach` returns a handle. `status RUN EXEC` reports its state, elapsed
time, last-output time and log sizes; quiet output alone is never treated as a
hang. `read RUN EXEC stdout|stderr --offset N` returns bytes from an offset and
the next offset, with `eof` only once the command has finished and nothing is
left; `wait RUN EXEC` returns its result. `status` with no run reconciles the
registry: it accounts for every owned worker and its allocation, restarts any
missing supervisor, and reports claims an interrupted creation never recorded.
`worker create` reconciles the same way, under a controller lock, before it
admits a worker. A creation or reset interrupted before the source was in
place is stopped and marked `failed`. Workers halted by the controller after a
collection attempt (`stopped`, `retained`, `expired`) cannot have changed: an
operation that starts one again hands it to the supervisor first, which halts
it once no operation holds it, and reconciliation does the same for one found
running. So a retried `collect`, `reset` or `destroy` completes that collection in place:
artifacts already saved with a matching digest are acknowledged and only the
rest is fetched. A repeated destroy, or one whose VM is already gone, succeeds.

`collected.json` is the versioned evidence manifest
(`busybee.vm.evidence/v3`, `contracts.evidence_errors`): source base, head,
status and patch hash, the baseline's provisioning revision, lock hashes and
tool versions, the allocation and deadline, every command with its argv,
environment, timings, result and binary digests, process and daemon
observations, the controller's cleanup events, artifact digests, acknowledged
artifacts, what is missing, `scenarios`: each scenario run's mode, status
and failed assertions, and per scenario the coverage of its required fixture
modes (§Test the real startup path), and `terminals`: per terminal handle its
command, terminal type and locale, sizes, how its program exited, and each
capture with whether its two views agree (§Make UI behavior observable).
`export RUN` writes a separate publishable copy
to `runs/<run-id>/public/`: environment values outside an allowlist, tokens and
keys, machine paths, user and host names, IP and MAC addresses, and VM,
snapshot and run identities become labelled placeholders, while exit codes,
timings and output keep their meaning. Bundles and Parallels screenshots cannot
be scanned, so the export lists them by digest instead of copying them. Terminal
images are rendered from text, so the export redacts each terminal's recording
and screens with placeholders of the same length, which keeps the timing log
and the layout valid, and renders its cells and images again from that copy.

`template prune linux|macos` deletes each retained baseline that no registered
worker or slot was cloned from, and keeps and reports the rest.

`verify --revision REV [--patch FILE] [--platform linux|macos ...]` is step 5
of §Run an issue: it checks that every required platform (default all) has a
usable baseline before creating anything, then on each platform in turn
creates a worker, runs `cargo build`, `fmt --check`, `clippy -D warnings` and
`test --workspace` in the checkout's development shell, records `busybee
--version`, runs each applicable scenario that names an issue in its required
modes, and destroys the worker, exporting its public evidence. The platform
matrix (`busybee.vm.verification/v1`, `verifications/<id>/matrix.json`) holds
per platform the head, version, binary digests and every outcome, and whether
each scenario ran the busybee that platform built. The digests are taken after
the checks, since `cargo test` can relink the binaries with test-time features. It is `verified` only when
all passed on the same source; `failed` keeps a product failure; a missing
platform, a check that could not run (exit 127 is `tool_missing`), a head,
version or binary that does not match, a worker that stopped serving mid-run
(`interrupted`; it is still destroyed), or evidence that was not collected or
exported makes it `incomplete`. The matrix is always written, with a
`matrix.public.json` beside it whose run identities, template baselines, user,
host, addresses and home paths are placeholders. A matrix records what it is
evidence for: its `role` (`candidate`, or `base` for a red run), the source
revision with the candidate's patch or, for a base, a test overlay (`verify
--overlay` is `--patch` recorded apart from the base revision), the
controller and scenario revision, per platform the template baseline its
worker came from, and per scenario whether it drives a terminal (`ui`) and
the terminals each run captured.

`gate --issue N --revision REV [--base REV] [--overlay FILE]` judges REV as
a fix of issue N (§Evidence required for a verified fix): it verifies the
candidate and its base (default: the merge base with origin/main), or reuses
complete matrices already bound to exactly that evidence, and writes
`gates/<id>/gate.json` with a `gate.public.json` beside it.

The unit tests under `scripts/vm/tests` need no Parallels. The acceptance tests
that build, validate and compare real candidates, and drive real workers, run
against the local config when opted in: `BUSYBEE_VM_LAB=1 python3 -m unittest
test_real_template test_real_worker test_real_supervision test_real_scenarios
test_real_terminal test_real_macos test_real_session test_real_gate` in that directory;
`BUSYBEE_VM_LAB_ROOT` names another worktree of the repository that holds the
lab state. The scenario runner's own tests need neither:
`python3 -m unittest discover -s tests/scenarios/tests`.

Long commands return a run handle with status, elapsed time, last-output time,
and artifact locations. Agents can await completion and read output from a byte
offset rather than repeatedly guessing how much log to tail. An idle output
stream is not by itself evidence that a process is hung.

Ownership is recorded when a clone is created. Cleanup targets that worker's
identity, not arbitrary VM names, process-name matches, or every daemon visible
on the host. The registry survives controller restarts so an interrupted
controller can reconcile its workers and their deadlines.

## Agent sessions

The lab dispatcher (`sortie/LAB_WORKFLOW.md`) runs every agent turn inside an
owned Linux worker. Issue selection stays in the dispatcher and VM lifecycle in
the controller; `scripts/vm/session.py` connects the two through `vmctl session`.

**Eligibility.** The dispatcher considers every open issue in the repository
that carries the `sortie` marker, whatever its milestone. `sortie/lab.py`
gives an open `sortie:ready` issue that marker (and takes it back) only when
every issue it is *blocked by* (GitHub's native issue dependencies) was
completed by a merged PR into `main` and every capability it needs is
available now. Those relations are the only ordering: not milestones, issue
numbers or labels. Every issue needs `worker:linux` and
`controller:session`; an issue body adds more on a `Lab requires:` line (for
example `**Lab requires:** controller:terminal`). Every issue also needs
`worker:macos` and `controller:gate`, since its handoff is verified on both
platforms (§Evidence required for a verified fix). Availability comes from the trusted
controller's read-only `doctor`: `worker:<os>` is the promoted baseline being
`ready`, and `controller:<name>` is an operation that controller provides. An
unknown capability is `unsupported`; a missing one names its cause. `release
--dry-run` prints each ready issue's decision and changes nothing; a real
release only adds or removes the `sortie` marker of ready lab issues.

**Trusted controller.** The launcher snapshots the selected reviewed revision
(`sortie/snapshot.sh`: dispatch scripts, guard policy, controller, scenario
runner, review skills) outside every workspace and runs the controller from it
with `--root` naming the checkout that holds `build/vm`. Nothing from an issue
branch runs on the host; editing the controller, dispatch or guard files in a
branch changes only the branch.

**Profiles.** `sortie/guard-policy.json` names the paths each execution profile
may not change. An issue is `infrastructure` only when it carries one of the
policy's authorized labels; otherwise it is `product`, which may not change
`sortie/`, `.github/workflows/`, the review skills, the controller or the
review contract. The profile is decided per issue by the trusted `lab.py
profile`, so one dispatcher can serve both.

**Attempts.** Sortie's `before_run` hook runs `session start`: it closes an
attempt that no `session end` closed (`interrupted`), then opens one in a fresh
worker created from the workspace's branch head with its uncommitted changes
as the patch. The worker's checkout is put on the branch with origin/main
beside it, and is also reachable at the workspace's own absolute path, so an
agent protocol that names its working directory resolves in the guest. The
agent's saved conversation state (`.claude/projects`, `.codex/sessions` in the
guest home) is restored, so a resumed turn finds its session in a new worker.
`after_run` runs `session end`. The dispatcher runs up to two sessions at
once (`agent.max_concurrent_agents`), matching the default two Linux
workers. A session holds its worker only during an attempt, never while its
PR waits for review, and releases it before its handoff is verified, so two
sessions need at most two Linux workers at a time: two attempts, or an
attempt and a verification. A verification that still finds none free (a
retained worker, an operator's own) waits in line for one, like the macOS
slot, rather than ending `incomplete`.

**Turns.** Sortie's agent command is `vmctl session agent -- <runner argv>`,
run in the workspace. It runs the runner in the worker over SSH, in the
checkout's development shell (`.#worker-agent` for Claude and Codex), with
stdin and stdout carrying the runner's own protocol and stderr kept per turn.
The runner's streams pass through a relay (`scripts/vm/turn_relay.py`), so a
daemon it leaves running cannot hold the SSH channel open: the turn ends
with the runner, whose exit status the relay records for the controller.
Model credentials named for the runner (`CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`; `OPENAI_API_KEY`,
`CODEX_API_KEY`) reach the turn through a mode-0600 file under `/run` (tmpfs)
that the turn deletes before the runner starts; a runner without one fails the
turn (`credentials_missing`) rather than running unauthenticated. Nothing
else from the dispatcher's environment enters the guest. The agent is root in
its guest, so a Claude turn runs with `IS_SANDBOX=1`, without which Claude
Code refuses to skip its permission prompts as root. No
credential is written to the worker's disk, its snapshots, the run's evidence
or the session's records. Sortie's generated `--mcp-config` names a host-side
tool server and is dropped, with a notice. A turn is bounded by `timeout(1)`
in the guest at the worker's remaining run deadline (or `--timeout`), and by
the host; SIGTERM, SIGINT or SIGHUP cancels it. Its kind is `exited`,
`timeout`, `cancelled` or `adapter_failure` (a worker that stops answering, a
runner that does not start, exit 126 or 127, or a turn that ended without the
runner's status, such as a branch whose flake lacks the agent shell). A turn that finds its worker no
longer ready (expired, stopped) closes that attempt and continues in a fresh
worker from the workspace.

**Checkpoints.** After every turn, and on `lab checkpoint`, `push` or
`handoff`, the worker's commits since the last transfer come back as a bundle
and its uncommitted changes as a diff; the workspace takes them only if it
still holds exactly what it last gave the worker. Otherwise they are kept
under `sessions/<issue>/conflicts/`. Changes touching a forbidden path are
never adopted: they are kept under `violations/`, pushes are refused, and the
attempt ends `blocked` with the paths in `.sortie/blocker.md`. Paths are
compared exactly as named, both sides of a rename; a workspace without
origin/main to compare against is a failed checkpoint, never an unchecked one. An agent's own
`.sortie/status` of `blocked` and its `blocker.md` are copied back; the review
handoff is written only by the trusted helper.

**The agent's controller.** Each turn forwards a broker socket into the guest
and stages the `lab` client (`scripts/vm/lab_client.py`) on its PATH. Every
request is served for the attempt's own run: `exec`, `status`, `wait`, `read`,
`terminal open|send|resize|capture`, `inspect`, `signal`, `console`,
`scenario` and `collect` call the worker operations above; `checkpoint`,
`reset`, `fetch`, `push`, `pr status|create|ready|comment|view`, `handoff` and
`verification` act for the session. A request naming another run (`target_not_assigned`),
any target but the worker (`host_target_refused`) or another operation
(`operation_not_permitted`) is refused. Pushes go only to the session's branch
and pull-request calls only to its PR, from the host workspace with the
dispatcher's GitHub identity; `pr create` reuses the branch's open PR, and
neither it nor `pr comment` accepts text carrying the evidence marker. Inside
its guest the agent is root and needs no permission to install tools, restart
daemons or break things. `reset` is applied when the turn ends, since it
restarts the guest the agent runs in: the work is checkpointed, the worker
restored to its recorded baseline, and the branch transferred again. When
that checkpoint did not take the work (a conflict, a guard violation or a
failure), the reset is skipped and recorded as such. A reset also marks the
run's terminals collected before it as final, since the restore removes them
from the guest.

**Handoff.** `handoff [--overlay PATH...]` is a request, checked at once
(the workspace holds the pushed head of a ready PR, with nothing uncommitted)
and carried out when the turn ends, after the checkpoint: the attempt's
worker is collected and destroyed, origin/main is fetched, and the evidence
gate judges the head against its merge base with main (`gate`, with the
candidate's changes to the overlay paths as the base's test overlay). A
product issue therefore declares its regression: test-only files named with
`--overlay` (the usual form for a bug fix or a feature; they must fail on the
base, where code they need may not even compile, and pass on the head), or a
scenario whose `issue` is the issue's number. Without either the verdict is
`checks_only`, which only an infrastructure session may hand off; a product
change that cannot fail on its base (documentation, a pure refactor) is
reported blocked by its agent rather than given an invented test. The
session's records keep each result (`sessions/<issue>/verifications/`). A
head the gate accepts gets its public record posted on the PR, once per
piece of evidence, and then the trusted `reviews.py handoff` writes
`.sortie/status`. Otherwise nothing is handed off; the next turn finds the
attempt released, closes it `unverified` and continues in a fresh worker,
where `verification` returns the verdict and its findings. A head whose
verification ends `incomplete` or `stale` twice blocks the attempt with the
cause and the retained records in `.sortie/blocker.md` rather than asking
again. A new handoff request removes an older head's `needs-human-review`.

**Exits.** `session end` checkpoints a ready worker, derives the outcome
unless one is given, destroys the worker (collecting first; a collection that
fails retains it, stopped) and exports its public evidence. Outcomes:
`success` (handoff of the current head, accepted by the gate), `blocked`,
`unverified` (a handoff the gate did not accept), `no_handoff`, `timeout`,
`cancelled`, `adapter_failure` (including a worker that could not be created)
and `interrupted`. `session status --issue N` is the run-result view: each
attempt's turns, worker and outcome, and the latest verification's verdict
and findings. `sessions/<issue>/attempts/<n>/attempt.json` records the
run, every turn, every reset and checkpoint, and the end: outcome, reason,
the worker's final state, and the collected manifest and public export.

## Bound execution and preserve failures

Every command and scenario declares a finite timeout within the overall run
budget. A watchdog outside the guest enforces that budget even if the agent,
guest, or daemon under test stops responding. Resource and timeout enforcement
must work independently of busybee.

Builds inside these allocated workers run under the controller's limits rather
than through the busybee binary under test. Busybee invocations are explicit
scenario steps. Existing guidance for shared developer machines is unchanged.

On a timeout or cancellation, collect diagnostics within a separate bounded
cleanup window, terminate the owned work, and stop the VM if guest control has
failed. Record the result as timed out, cancelled, or an environment failure;
none counts as a passing test. On collection failure, retain the stopped clone
and report which artifacts are missing instead of destroying unexported work.
Checkpointing source and streaming logs limits the evidence lost to a guest
crash. Recovery from controller termination is itself a required controller
test.

## Test the real startup path

Every scenario uses private daemon state, an explicit busybee configuration,
and a generated Pueue YAML **file**. `PUEUE_CONFIG_PATH` names that file, not a
directory. `BUSYBEE_STATE_DIR`, `BUSYBEE_CONFIG`, the Pueue data directory, and
its socket must all belong to the scenario. Resolve socket locations with the
platform's path-length limit in mind.

Use the workspace-built busybee and its matching bzbd, with executable paths
and hashes recorded. No scenario installs over global binaries or contacts a
developer's daemon. Preflight must verify required executable versions so an
integration test that returns early for a missing tool cannot produce a false
green result.

Keep two explicitly named fixture modes:

| Mode | Purpose |
|---|---|
| Cold startup | Both daemons are initially absent. Busybee starts them through its ordinary client path. Do not precreate runtime directories to work around startup bugs. |
| Prepared daemons | Start isolated daemons deliberately to test scheduling, protocol, and fault injection independently of startup. |

A minimal isolated configuration is allowed in cold mode to route all state
into the scenario. Dependencies may be cached in both modes. Their difference
is daemon and runtime state, and the result manifest records which mode ran.

For example, starting Pueue from a test fixture can give its tasks a different
umask from a Pueue started by bzbd. Permission regressions must cover cold
startup, traversable newly created directories, and executable build output.
A prepared-daemon pass cannot substitute for that check.

`scenario RUN SCENARIO --mode cold|prepared [--bin-dir DIR]` runs one scenario
in a ready worker. A scenario is `tests/scenarios/<id>.toml`
(`busybee.scenario/v1`, checked by `runner.meta_errors`): its id, applicable
platforms, the fixture modes it supports and those a verification requires,
its tools (`workspace` for the built `busybee` and the `bzbd` beside it, or a
minimum version), its deadline, and the procedure in
`tests/scenarios/procedures.py` with exactly the assertions it checks. A
scenario that reproduces a product issue records the issue and the affected
revision its red run uses. The runner and scenarios come from the controller's
checkout, staged into the guest by content digest, so a red run against an
older revision uses the current scenario. They run as one exec in the worker
checkout's development shell, bounded by the scenario deadline plus the
runner's cleanup window and the shell's startup.

The runner (`tests/scenarios/runner.py`):

- **Preflight** checks the platform, `busybee --version` against the version
  the checkout's `git describe` produces, that `bzbd` sits beside it (busybee
  starts that one; it has no version flag, so its digest is recorded), and each
  other tool's minimum version. Any failure is `environment_failure` before a
  fixture exists.
- **Fixture.** A short private root under `/tmp` holds copies of those
  binaries, `BUSYBEE_CONFIG`, and a Pueue YAML file whose runtime directory and
  socket sit inside its `pueue_directory`, since pueued creates that directory
  but not a separate runtime one. Every process the scenario starts carries a
  marker variable naming the root and a clean environment. Cold mode verifies
  that no daemon of the scenario user runs and no runtime state exists before
  the client starts; prepared mode starts pueued and bzbd itself under umask
  0022.
- **Procedure.** Steps run as an unprivileged user (root would bypass the
  directory permissions under test), under umask 0022, with output kept out of
  the runner's stdout.
- **Result.** Diagnostics come first: the modes of everything under the root,
  config and daemon logs, and process masks. Then cleanup stops every marked
  process, TERM then KILL, within its window, then removes the root and checks
  that it is gone. The result is one JSON document
  (`busybee.scenario.result/v1`). A failed assertion is `product_failure` and
  stays one. A preflight or fixture fault, an unevaluated assertion, a
  surviving process or a root that was not removed is `environment_failure`.
  The deadline gives `timeout`.

The controller checks that result against the runner's exit status and the
declared assertions. Output that is not a valid result is an environment
failure, never a product failure. It writes `runs/<run>/scenarios/<exec>/result.json`;
a scenario refused before its exec was queued ran nothing and leaves no record.
Coverage takes the latest run in each mode of the current head (the guest
checkout's head when collected, the scenario's own head when it reports): a
scenario is verified only when every required mode's latest run on that head
passed, and a prepared pass is reported alongside a missing or failing cold
run, never in its place. Runs of other heads are counted, never combined. The runner covers
Linux and macOS; on another platform it reports `platform_unsupported`. On
macOS, where the exec runs as the lab account, it starts through `sudo`, drops
privileges without `setpriv` (its interpreter clears the groups, then sets the
group and the user), and finds the fixture's processes by their marker in `ps`
output, since there is no `/proc`; BSD reports no process umask.

## Make UI behavior observable

The terminal driver captures the actual binary's output through a PTY with
explicit dimensions, locale, terminal type, color settings, and font metadata.
It supports `q`, Ctrl-C, resize events, and screens captured before and after
state changes. Preserve the raw terminal recording as well as decoded screen
text and rendered images. Label images rendered from terminal cells separately
from VM console screenshots.

The Linux guest template carries zellij, pinned through the lab flake; the
terminal host is `tests/scenarios/terminal.py`, staged like the scenarios. It
needs util-linux `script`'s advanced timing and `/proc`, so terminals run on
Linux workers only and a macOS worker refuses `terminal open`
(`platform_unsupported`); scenarios that drive one declare `platforms =
["linux"]`. macOS terminal access is validated as SSH PTY transport. On
Linux:

- **Session.** Each terminal is one zellij session with a unique name and one
  borderless pane, no bars and no session serialization; opening it checks the
  pane runs the requested command, since reusing a name would silently attach
  to an older session. A headless zellij session has a fixed size, so the host
  attaches a zellij client through a PTY it owns: dimensions are that PTY's
  window size, and a resize is a window-size change and SIGWINCH on it. No
  zellij action is sent before the pane's command is running: an action is a
  client connection, and one that ends before the session is set up crashes
  zellij 0.45.1's server. zellij older than 0.45, which lacks pane-addressed
  actions, is refused in preflight.
- **Input.** Bytes, text and named keys reach the pane through zellij's
  pane-addressed actions; named keys are encoded for the pane's current
  terminal modes, as a terminal would.
- **Recording.** The pane waits, briefly and bounded, until zellij has given
  its PTY a size, then runs the command under util-linux `script`, whose
  advanced timing log records every output and input chunk, each window-size
  change and the exit code. zellij's own screen and its report of the pane
  (`dump-screen`, `list-panes`) form a second, independent view.
- **Captures.** A capture waits for the program's output to pause briefly,
  takes zellij's screen, and is settled when no output arrived meanwhile, so
  the screen belongs to a known point of the recording even for a program
  that redraws on a timer. It is bounded by a deadline (an unsettled capture
  is reported as such), optionally after waiting for expected text. Readiness is always such a
  predicate with a deadline, never a fixed sleep.
- **Cells and images, on the host.** `scripts/vm/screen.py` replays the
  recording through pyte at the recorded sizes into a cell grid, checks its
  text against zellij's screen of the same moment, and renders the grid with
  agg in the development shell's pinned font. Each image records its font,
  cell size, renderer and the characters the font lacks; pixel output may
  differ between hosts, so no test compares pixels.
- **Lifetime.** Every process a terminal starts carries a marker naming it,
  and closing a terminal stops exactly those, within `timeout`'s kill grace.
  zellij runs in sessions of its own, so a holder killed outright (SIGKILL)
  leaves its terminal running until the worker is reset or destroyed. In a
  scenario the zellij runtime lives under the fixture root, so the fixture's
  cleanup is a second bound.

`vmctl terminal open RUN [--cols C --rows R] [--cwd D] [--env N=V] [--timeout
S] -- ARGV` starts the host as a detached exec, so the run's supervisor and
deadlines bound the terminal like any command, and returns a handle. `terminal
send RUN H (--text T | --key K... | --bytes HEX)`, `terminal resize RUN H
--cols C --rows R` and `terminal capture RUN H [--expect TEXT] [--timeout S]`
act on it; capturing a terminal whose program has exited replays the whole
recording. The terminal ends with its program, at its timeout, or when the
agent sends the holder pid that `open` reports a `signal`. Artifacts go to
`runs/<run>/terminal/<handle>/`: the controller's `handle.json`, the guest's
`state.json`, `recording/{output,input,timing}` and per capture `NNNN.json`
(zellij's view), `NNNN.cells.json` (the replayed cells and text, and whether
they agree) and `NNNN.png`. A scenario's terminals are fetched to
`runs/<run>/terminal/<exec>-<name>/`. Parallels screenshots stay under
`console/`. `collect` first brings each terminal's recording and captures up
to date from the guest, listing any it cannot fetch as missing, and `export`
carries both kinds of image apart; the capture result names the image an agent
should inspect.

The `live-monitor` scenario runs the workspace-built monitor in prepared mode
(it never starts a daemon) through the cases below at 120x40 and 60x20, pairing
each view with `busybee status --json`.

Exercise running and queued work, an idle pool, unavailable or stale daemon
data, long labels, narrow terminals, and quitting during a slow status request.
Use observable readiness conditions with deadlines rather than depending on
fixed sleeps. Capture `status --json` and daemon logs alongside UI evidence so
an incorrect view can be compared with its underlying state.

Synthetic `TestBackend` fixtures and documentation screenshots do not establish
that the live monitor works. Conversely, a screenshot alone does not prove
resource accounting: assertions must check the relevant lease and token
invariants, including the implicit jobs allowed by the broker specification.

## Evidence required for a verified fix

Each run exports:

- A manifest containing the issue, source revision or patch hash, baseline
  version, OS and architecture, lockfile and executable hashes, tool versions,
  resource budget, fixture modes, and scenario results.
- Commands, relevant environment overrides, timestamps, durations, exit codes,
  timeout classifications, and separate stdout and stderr logs. Export an
  allowlist of environment values rather than a complete credential-bearing
  environment dump.
- Daemon logs and configuration, relevant lease and pool snapshots, process
  observations, and the result of cleanup and pool-restoration checks.
- Terminal recordings, decoded screens, and images for interface checks.
- Recoverable source changes and an explicit list of missing artifacts or
  checks that could not be completed.

Raw evidence remains outside the guest. Publishing it requires an export that
removes credentials and machine-specific paths or identifiers while preserving
the diagnostic content. The review runner receives durable artifact references
rather than references to files that will disappear with the worker.

A verified fix has a regression that fails on the relevant base and passes on
the candidate, required platform checks on the candidate revision, appropriate
UI inspection, and confirmed cleanup. Skipped prerequisites, screenshots of a
fixture alone, or a success message from the agent are insufficient evidence.

The evidence gate (`scripts/vm/gate.py`, `busybee.vm.gate/v1`) applies this
to two platform matrices, the candidate's and its base's:

- **Bound.** The candidate matrix is for the committed head with no patch, the
  base matrix for its merge base with main and the declared test overlay, both
  from the current controller and scenario revision and the promoted template
  baselines. A new commit, a rebase, a moved main, another controller or a new
  baseline makes earlier evidence `stale`.
- **Complete.** Both platforms ran; every required check and every scenario
  that names an issue has an outcome in each required mode, and a scenario
  that does not apply on a platform is declared not applicable there, never
  inferred from a missing result. A skip, timeout or environment failure, a
  scenario that ran another binary, a terminal scenario without captured
  terminals, public evidence missing from disk, or a worker not collected and
  destroyed makes it `incomplete`.
- **A fix.** The regressions are the scenarios that name the issue and, with
  an overlay, `cargo test`: each must fail on the base and pass on the
  candidate. Any other candidate failure the base does not share is `failed`.
  A failure the base shares, such as a known product bug in another issue, is
  listed under `preexisting` and the verdict is `preexisting_failures`, which
  may go to review with that list in its evidence; it is never dropped.
- **No regression declared.** When no scenario names the issue and no overlay
  is given, nothing shows a red base: passing checks are `checks_only`, which
  is not a verified fix. Its evidence says so in its first lines.

`verified` and `preexisting_failures` pass for every session. `checks_only`
passes only for an `infrastructure` session (`sortie/guard-policy.json`),
whose lab or harness work owes no product regression; a product session must
name its regression, through a scenario or `handoff --overlay`. The gate reads its evidence and
changes none of it, so a refusal keeps every matrix and run for diagnosis.
Complete matrices bound to the same evidence are reused, so unchanged
evidence is not verified twice: a candidate that was verified or that a gate
accepted, and a base whether it passed or failed. A refused candidate is verified again when its handoff is
asked for again, so a flaky check does not stick to an unchanged head;
incomplete matrices are never reused. The pull-request
comment that carries the public record starts with `<!-- busybee-lab-evidence:v1
{head, base, issue, verdict, evidence_id, profile} -->`; the CI review gate
requires one by the PR's author, for the current head, with a verdict that
passes for its profile, before it reviews or approves a `sortie-lab/` PR
([review workflow](../development/agent-review.md)).

## Implementation sequence

Each milestone is a bounded infrastructure change with executable acceptance
checks. Product fixes follow in their own issue-scoped pull requests.

| Milestone | Deliverable | Acceptance |
|---|---|---|
| 1 | Repo-owned Linux template, local configuration schema, and basic controller. | From a validated baseline: clone, build, reproduce the startup/umask failure, export evidence, dispose, and reproduce it again in a fresh clone. Preserve the expected failure as evidence; do not hide it to make the infrastructure check green. |
| 2 | Interactive terminal tools, external watchdog, and durable collection. | Capture a real monitor at two sizes, send input, quit successfully, and inspect its state. A deliberately stuck command and an interrupted controller are recovered within their budgets with explicit failure results and retained artifacts. |
| 3 | macOS verification from a prepared baseline. | Run the same applicable scenarios and required project checks on both platforms for one source revision. Clone, access, capture, and reset require no routine human interaction or host configuration activation. |
| 4 | Reusable regression scenarios and fixes. | Add the cases below, retain failing-base evidence, and fix each product issue with its required specification changes. |
| 5 | Agent runner integration. | One eligible issue completes reproduction, implementation, both platform checks, independent review, and existing merge gates. A continuation retains its branch; unchanged review evidence stays settled; every exit leaves an accounted-for worker. |

### Implementation issues

[Tracking issue #68](https://github.com/githappens/busybee/issues/68) splits
these milestones into individually assignable tasks, with their order and
blocked-by dependencies, and links the separately scoped product regressions.

### Initial regression scenarios

Initial scenarios should cover:

- Cold startup and inherited umask, including file creation and execution.
- Pytest with and without xdist and explicit serial execution. Correct the
  classification specification alongside any change to automatic sharding.
- Nested calls and client/parent termination, distinguishing a nesting deadlock
  from a lease whose owner really died
  ([#64](https://github.com/githappens/busybee/issues/64)).
- Restart accounting with orphaned jobserver work, FIFO naming collisions,
  and reload during deferred admission
  ([#41](https://github.com/githappens/busybee/issues/41),
  [#39](https://github.com/githappens/busybee/issues/39),
  [#42](https://github.com/githappens/busybee/issues/42)).
- Live status/monitor attribution and incremental log processing
  ([#50](https://github.com/githappens/busybee/issues/50),
  [#49](https://github.com/githappens/busybee/issues/49)).
- Static drain behavior, exclusive handover, queued-token ownership, and
  two jobserver tasks sharing with a static task
  ([#45](https://github.com/githappens/busybee/issues/45),
  [#46](https://github.com/githappens/busybee/issues/46),
  [#47](https://github.com/githappens/busybee/issues/47),
  [#48](https://github.com/githappens/busybee/issues/48)).

Integrate with the existing agent runner through the controller interface;
keep its issue selection and review policy separate from VM lifecycle. Ordinary
product agents must not need to modify the runner, global installation, or
template to verify their issue.

The first implementation preflight must resolve unattended guest access,
Parallels guest integration, and clone/reset capabilities by exercising them.
Any unsupported capability is a named environment failure. It does not justify
quietly switching to the host or weakening a test.
