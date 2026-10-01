# Autonomous development and verification workflow

This document specifies the intended workflow for agents fixing busybee in
disposable Parallels VMs. An agent must be able to reproduce an issue, change
the code, inspect the running terminal UI, and verify the fix on Linux and
macOS without asking a person to operate its development environment.

**Status: design for implementation.** Only the read-only `doctor` preflight
(`scripts/vm/vmctl.py`) has shipped; every other operation returns `unsupported`.
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
2. **Allocate a worker.** Validate the selected template, reserve the configured
   CPU and memory budget, create a clone (macOS: lease the single guest, see
   §macOS workers), and transfer the checkout. Start with
   one active worker; concurrency is a controller setting bounded by a total
   resource budget. Assign a run ID and an overall deadline before execution.
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
| `worker create` | Clone an explicit baseline, allocate a run ID, and return the worker identity and deadline. For macOS, wait for and lease the single guest instead of cloning (§macOS workers). |
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

Long commands return a run handle with status, elapsed time, last-output time,
and artifact locations. Agents can await completion and read output from a byte
offset rather than repeatedly guessing how much log to tail. An idle output
stream is not by itself evidence that a process is hung.

Ownership is recorded when a clone is created. Cleanup targets that worker's
identity, not arbitrary VM names, process-name matches, or every daemon visible
on the host. The registry survives controller restarts so an interrupted
controller can reconcile its workers and their deadlines.

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

## Make UI behavior observable

The terminal driver captures the actual binary's output through a PTY with
explicit dimensions, locale, terminal type, color settings, and font metadata.
It supports `q`, Ctrl-C, resize events, and screens captured before and after
state changes. Preserve the raw terminal recording as well as decoded screen
text and rendered images. Label images rendered from terminal cells separately
from VM console screenshots.

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
