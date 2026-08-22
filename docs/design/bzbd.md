# bzbd: shared CPU token pool and cost-aware admission

Design specification for busybee's next iteration. Tracking issue: githappens/busybee#2
(parent of the task issues; milestone "bzbd: shared CPU token pool").

**This document is the specification.** Task issues describe scope and acceptance
criteria; semantics come from here. A pull request that changes behaviour described
in this document updates the relevant section in the same PR. Amendments that change
a decision are also announced as a comment on #2 so the decisions log stays findable.

## Problem

busybee today is strictly one-task-at-a-time (`pueue` group `busybee`, `parallel_tasks = 1`). That stops four parallel agent sessions from running four `cmake --build`s at once, but it also leaves the machine idle whenever the one running task cannot use every core, and it serialises things that could safely share. The README roadmap proposed cost-aware admission inside pueue; upstream has explicitly declined that scope (Nukesor/pueue#325: "perfectly suited for a big brother of Pueue"). busybee is that big brother.

Goal: **one task alone gets the whole machine; N tasks share it; nothing is ever over-subscribed** — with zero ceremony for callers (`busybee -- <cmd>` stays the whole API) and no silent degradation.

## Key insight: the jobserver already solves dynamic sharing

GNU make's jobserver protocol (fifo style, make ≥ 4.4) is a pool of tokens in a named pipe. A participating build reads one token before starting each compile job and writes it back when the job exits. Clients on a typical native toolchain today:

| tool | jobserver client? | how it joins |
|---|---|---|
| GNU make ≥ 4.4 | yes | `MAKEFLAGS=--jobserver-auth=fifo:PATH` |
| ninja ≥ 1.13 | yes | same env var; **only when no explicit `-j`** on its command line |
| `cmake --build` (Make/Ninja generators) | yes, via the generator | same; must not set `CMAKE_BUILD_PARALLEL_LEVEL` |
| cargo + rustc | yes (`jobserver` crate parses `fifo:PATH`) | `MAKEFLAGS` / `CARGO_MAKEFLAGS` |
| clang / gcc / ld | transparent | driven by the above |
| xcodebuild | **no** | `-jobs N` argv only; no env knob |
| go toolchain | no | `GOMAXPROCS=N` (bounds `-p`) |
| ctest | no | `CTEST_PARALLEL_LEVEL=N` |
| pytest (+xdist) | no | `PYTEST_ADDOPTS=-n N` |

So: one machine-wide fifo with `pool_size` tokens (default = logical cores). Every jobserver-aware build busybee runs gets `MAKEFLAGS` pointing at it and self-balances at compile-job granularity. Alone it drains all tokens; when a second build starts the two interleave token by token; when one finishes the other grows back within one compile unit's time. No daemon decision is involved in that rebalancing.

Tools that cannot speak jobserver get **static** treatment: busybee pulls `n` tokens out of the same fifo on the task's behalf, holds them for the task's lifetime, and tells the tool `n` (env or argv). One pool, one accounting. Static tasks are locked at `n` until they exit (their tools read the number once); jobserver tasks grow and shrink. That asymmetry is the documented reason to prefer jobserver-aware tools.

## Components

```
busybee (client)  ──unix socket──▶  bzbd (broker)  ──pueue-lib──▶  pueued (supervisor, logs)
                                      │ owns: fifo token pool, queue, leases, admission
                                      │ submits admitted tasks with start_immediately
                                      └ group `busybee` now parallel_tasks = 0
```

**bzbd** (new crate `crates/bzbd`) is the only thing clients ask to run anything, and the only thing that changes pueued's state. There is one read-only exception: a blocking client streams its own task's combined output straight from pueued (`bzb-core/src/log.rs::fetch_log_chunk`, by task id from the `Admitted` event) rather than through the broker, because pueued already serves that log from a byte offset and relaying it through bzbd would add a copy, a buffer to bound and a second place for the stream to stall. That connection reads and never spawns: bzbd has necessarily already started the task on its pueued, so an unreachable socket means the client's pueue configuration disagrees with the daemon's, and a client that quietly started a pueued of its own would read an empty queue and report missing output instead of the misconfiguration. Auto-started by the client on demand, exactly as pueued is today (`bzb-core/src/client.rs::connect_or_spawn`). State dir: `$XDG_STATE_HOME/busybee/` (default `~/.local/state/busybee/`, overridden by `BUSYBEE_STATE_DIR`): `bzbd.sock`, `bzbd.pid` (also the single-instance `flock`), `bzbd.log`, `jobserver-<pid>` fifo, `leases.json`. The state dir is created mode `0700` and the socket is `0600`: the socket is bzbd's whole control surface, so on a shared machine it belongs to its owner alone.

Clients and bzbd speak newline-delimited JSON over `bzbd.sock`, one UTF-8 message per line. The client's first line is `{"hello": <protocol_version>}`; bzbd answers `Pong { version, pid }`, or `Error` when it does not speak that version, and closes. A line may not exceed 64 KiB in either direction; bzbd answers `Error` and closes rather than buffering a message that never ends, and a client that is sent an over-long line reports a protocol error rather than buffering it. An `Error` reporting an undecodable line does not echo it back, and its message is truncated to a kilobyte — a decoder quotes the input it choked on, so neither the echo nor the quote can turn a request that fit within the limit into an answer that does not.

**pueued** keeps what it is good at: spawning, process groups, log capture, persistence. Its dispatcher is bypassed (`parallel_tasks = 0` + `start_immediately`).

**busybee / bzb** (client) keeps its contract: blocks, streams the task's combined output, mirrors the exit code, Ctrl-C cancels with exit 130. `--detach` returns immediately.

## Lease model

Every `busybee -- cmd` is a lease request:

```
LeaseRequest { argv: Vec<String>, cwd, env, label: Option<String>,
               class_override: Option<Class>, cores_wanted: Option<u32>,
               detached: bool }
```

Lifecycle: `Queued` → `Admitted { pueue_task_id, class, cores }` → `Finished { exit_code }`. A lease ends when pueued reports the task `Done`, or when the requesting client's connection drops (before admission: dropped from the queue; after: task killed, tokens returned). The client holds its socket open for the lease's whole life; connection = lease.

`detached: true` (`--detach`) is the one exception, and the reason the flag can keep its contract of returning immediately: the lease survives its connection, so bzbd runs it to completion whether or not anyone is still listening. Nothing holds it, so no Ctrl-C can reach it either — `Request::Cancel { lease }` (`busybee cancel <id>`) is the only way to end one early, and bzbd answers `Ack`, or `Error` when the lease is not there. A cancel that silently accepted an unknown id would report success for a task still on the machine.

## Admission policy (pure state machine, no IO)

Queue is FIFO. A lease at the head is admitted when:

1. `admitted_count < max_concurrent` (default 4). Needed because make/ninja/cargo each run one job *without* a token (the implicit token), so unbounded admission would add one uncounted job per task.
2. Class-specific:
   - **jobserver**: admitted as soon as (1) holds; takes no tokens up front.
   - **static**: target `n = clamp(cores_wanted, 1, ceil(pool_size / (admitted_count + 1)))` — fair share at the moment of admission. bzbd blocking-reads up to `n` tokens from the fifo within `drain_deadline_ms` (default 2000), then starts the task with whatever it collected (minimum 1, the implicit token). Because jobserver clients return tokens after every job, the drain normally completes in well under a second and throttles the running jobserver builds to `pool − n`.
   - **none**: static with `cores_wanted = pool_size`. This is today's exclusive behaviour and the default for anything unrecognised.

Head-of-line blocking is intentional: a `none` lease waits for the pool to be fully free, and everything behind it waits too. Priorities, preemption and reordering are out of scope.

## Classification (`classify(argv) -> Plan`)

Operates on the argv the client received (the shell has already handled `|`, `&&`, redirects outside busybee). Steps:

1. **Unwrap wrappers** until the first non-wrapper token: `nix develop [args] -c|--command`, `nix shell [args] -c`, `env [VAR=val...]`, `caffeinate [-flags]`, `nice [-n N]`. A shell string (`sh -c '...'`, `bash -lc '...'`) is opaque: class **none**. Unwrapped wrappers stay in `argv`, so an `env NAME=value` operand is applied by `env` *after* the daemon has set up the environment and wins over anything busybee injects under the same name (`env MAKEFLAGS=-j100 make`): drop that injection and emit a notice, exactly as for a user-supplied `-j`.
2. Look up the basename of the tool:

| tool | class | injection | notes |
|---|---|---|---|
| `make`, `gmake` | jobserver | `MAKEFLAGS` | notice if make's own option parsing sees `-j`, including inside a short-option cluster (`-ksj8`) |
| `ninja` | jobserver | `MAKEFLAGS` | notice if argv has `-j` (ninja then ignores the pool) |
| `cmake` with `--build` (build mode only) | jobserver | `MAKEFLAGS`; remove `CMAKE_BUILD_PARALLEL_LEVEL` | notice if `--parallel`/`-j` |
| `cargo` | jobserver | `MAKEFLAGS`, `CARGO_MAKEFLAGS`; plus `RUST_TEST_THREADS=<fair share>` | test threads are not token-accounted |
| `xcodebuild` | static | argv `-jobs max(1, n−1)` | measured: `-jobs N` yields N+1 concurrent `clang -cc1` in steady state on a large legacy project; skip if argv already has `-jobs` |
| `go` | static | `GOMAXPROCS=n` | |
| `ctest` | static | `CTEST_PARALLEL_LEVEL=n` | |
| `pytest` | static | `PYTEST_ADDOPTS` += `-n n` | effective only with xdist; harmless otherwise |
| `docker` with `build` | none | — | the VM has its own CPU cap |
| everything else | none | — | |

A required token (`--build`, `build`) counts only as the tool's *first* argument, the position that selects a mode. cmake dispatches on that position exactly — `--build`, `--install`, `--open`, `-E` — so a `--build` anywhere else is another mode's operand (`cmake --install --build` installs into a directory named `--build`) or an argument of a payload command (`cmake -E env ./x --build`). Those, and cmake's other non-build modes, are **none**.

For `make`/`gmake` the parallelism scan walks argv the way make's own option parser does, because only the `-j` make actually sees overrides the injected jobserver: short options cluster (`-ksj8` puts `-j8` in `MAKEFLAGS`), the first value-taking option in a cluster swallows the rest of the token (`-Cjobs` is a directory, `-EFOO=j` an expression) and, when that value is mandatory, the next argument too (`make -f -kj` builds a makefile named `-kj`), and `--` ends the options. Other tools keep the plain scan for the flags their row lists.

3. Overrides: `--class jobserver|static|none`, `--cores N` (static target; ignored with a notice for jobserver class). A user-supplied parallelism flag always wins over injection and produces a one-line notice.
4. Every task additionally gets `BUSYBEE_CLASS=<class>` and `BUSYBEE_CORES=<fair share>` in its env so opaque scripts can cooperate (`xcodebuild ... -jobs "${BUSYBEE_CORES:-8}"`). This is the only remedy for argv-only tools hidden inside scripts.

The `Plan` is data: `{ class, tool: String, env_set: Vec<(String,String)>, env_append: Vec<(String,String)>, env_unset: Vec<String>, argv: Vec<String>, cores_wanted: Option<u32>, notices: Vec<String> }`. Executing it is a separate concern.

`classify` never reads the environment or the filesystem, so values it emits carry placeholders the daemon substitutes at dispatch: `{fifo}` (fifo path), `{cores}` (fair share), `{cores-1}` (`max(1, cores − 1)`). These three are the only substitution points. `{cores}` is the task's share of the pool at admission. For static/none it is the number of tokens the drain actually collected (minimum 1, the implicit token), whose upper bound is the admission target `clamp(cores_wanted, 1, ceil(pool_size / (admitted_count + 1)))`; substituting the target after a short drain would let concurrent static tasks demand more cores than the pool has. For jobserver it is `ceil(pool_size / (admitted_count + 1))`, since it holds no tokens of its own and there is nothing collected to count. Jobserver tasks get a number at all because the threads they spawn that *don't* speak the protocol (`RUST_TEST_THREADS`) need bounding, and pool_size there would let every concurrently admitted task claim the whole machine. `env_append` exists for `PYTEST_ADDOPTS`, which must extend the caller's value rather than replace it; the daemon joins the two with a space. `cores_wanted` carries `--cores` through to admission (never set for jobserver, which is where the notice comes from). `argv` is the whole command line as received, wrappers included, so the daemon runs it as-is. An override that forces `jobserver` on a row that has no fifo injection (an opaque script, a static tool) still gets `MAKEFLAGS`; forcing `static`/`none` keeps a core-count injection and drops a fifo one.

## Client output contract

Lines the client prints to **stderr** (stdout is reserved for the task's output), so agents can read what they got:

```
busybee: queued (2 ahead)
busybee: running — cmake, jobserver, sharing 18-token pool with 1 other task
busybee: running — xcodebuild, static, holding 9/18 cores (2 other tasks active)
busybee: running — make, none, exclusive (18 cores)
busybee: note: you passed -j8; ninja will ignore the shared pool
busybee: command exited 0 (elapsed 2m14s)
```

A `none` task reports the pool rather than a held count: it is admitted alone, so the cores it holds a token for understate what it was granted.

Notices the classifier raises about the request itself (a `-j` that defeats the pool, a `--cores` the class ignores) precede `Queued`, since a `--detach` client returns on that event and would never see one raised after it. A notice raised at admission — a static drain that found no token within its deadline — follows `Queued` and precedes `Admitted`.

Exit-code mapping is unchanged (`bzb-core/src/exit_code.rs`).

`--detach` is the exception that owns stdout, because its lease id is the command's result and `busybee cancel <id>` is the only thing that can end the lease. It prints the lease id there, plus the pueue task id when the lease was admitted before the client returned:

```
busybee: lease 7 detached (pueue task assigned once admitted)
busybee: lease 7 detached (pueue task 12)
```

## Failure and recovery

No silent fallbacks anywhere: if bzbd cannot create its fifo or socket it refuses to start and the client exits non-zero with the reason, rather than running the command ungoverned.

| event | behaviour |
|---|---|
| client disconnects while queued | lease dropped |
| client disconnects while running | pueue task killed (SIGINT → SIGKILL escalation as today), tokens returned once pueued reports the task gone — not when the signal is sent: a static task that ignores the first signal is still running at its full width, and a jobserver build would take returned tokens at once and oversubscribe the pool. The admission machine forgets the lease at once, but the next admission waits for the same report: starting a replacement meanwhile would run two exclusive tasks at once. The teardown is written to `leases.json` before the signal is sent, so a daemon killed between the two resumes it rather than adopting the task as a lease whose cancellation it never heard of |
| task goes live after its lease was torn down | the drain finished and the task launched while the teardown was in flight. bzbd reports `Started` regardless; the admission machine no longer tracks the lease, so it answers with a second drop and the task is killed and its tokens returned. Never swallowed: an ignored late `Started` would leak both the process and its tokens |
| a submission to pueued goes unanswered | the task id is in the answer, so bzbd cannot tell a submission that never landed from one pueued has already started — and `start_immediately` starts it on arrival. The next poll looks for it by label and creation time; anything found is killed like any other orphan, and nothing is admitted until that is settled |
| a drain comes up short, or collects nothing at all | not a failure: the task starts with what was collected, the implicit token providing the minimum of one, and `Started` reports the real count. Only a drain that cannot run at all (fifo unreadable, submission rejected) ends the lease. Treating exhaustion as a failure would stop a second static task from ever running once the first drained the pool |
| task exits | observed by polling pueue status (1 s); tokens returned; client gets exit code |
| pueued dies | running leases marked lost; clients exit non-zero with a clear message; bzbd keeps serving and re-spawns pueued on next submit. One failed request is what counts as gone — a request over pueued's socket fails only when it is — and the lost leases are ended on the spot rather than after a grace period: a poll that waited for pueued to come back would leave the clients waiting on completions that cannot arrive, and a pueued that does come back reports every task it was running as killed, whatever the task is doing. The lost leases' tokens go back to the pool, because pueue's task status carries no child pid and nothing can tell when a task that outlived pueued exits; the error is logged, and a survivor runs oversubscribed for what is left of it |
| bzbd dies | running tasks continue (pueued's children; they hold the old fifo open). On restart, before its socket exists, bzbd reloads `leases.json`, cross-checks it against pueue status, re-adopts every lease whose task is still running and is the recorded one (in the `busybee` group, under the record's label: pueued reuses ids once its state is reset, and a lease adopted over another task would let `busybee cancel` signal it) — with no client, reported `orphaned` by `busybee status`, ended by `busybee cancel` or the task's own exit — drops and logs the rest, creates a new pid-suffixed fifo and seeds it with `pool_size − Σ cores_held(adopted)` tokens. Orphaned jobserver tasks finish on the old pipe: its file stays while any adopted task was pointed at it, since a sub-make opens the path anew, and is unlinked once they are gone. A `jobserver-<pid>` whose daemon is dead and that no adopted lease refers to is unlinked at startup. Leases that were queued when the daemon died are dropped, since their clients went with it. A lease whose submission was out when the daemon died — the record says when it went to pueued but names no task, because the id comes back in the answer and `leases.json` is written before the submission as well as after — is matched to its task by label and creation time, as the poll matches an unanswered submission, and adopted if pueued started one; otherwise it is dropped like a queued lease. A teardown in flight when the daemon died — a task signalled but not yet reported gone — is on record too, as `killing`: it is resumed rather than adopted, its `cores_held` are withheld from the new pool until pueued reports the task gone, and nothing is admitted before that. If the pool shrank under the adopted leases, the tokens they hold beyond `pool_size` are owed: the pool starts empty and that many of their releases are not put back, so the fifo never holds more than the pool has |
| bzbd is told to stop (SIGTERM) | running tasks are left alone; `leases.json` is written for the daemon that takes them over; the socket is unlinked; the fifo is left for the tasks that hold it |
| fifo accounting drift | bzbd periodically checks `FIONREAD + Σ held ≤ pool_size`; excess tokens (a tool wrote extra bytes) are drained; a deficit is logged and corrected when the holding lease ends |

## Configuration

`$XDG_CONFIG_HOME/busybee/config.toml`, falling back to
`~/.config/busybee/config.toml`; `BUSYBEE_CONFIG` names a file outright and must
be absolute, since client and daemon run from different directories. Every key
is optional and a missing file is the defaults.

```toml
pool_size = 18            # default: logical cores
max_concurrent = 4
drain_deadline_ms = 2000
[defaults]                # per-class cores_wanted
static = "fair"           # or an integer

[overrides]               # extend/replace classification rows without a release
"./build.sh" = { class = "jobserver" }
"my-bench"   = { class = "none" }
"mytool"     = { class = "static", env = { MYTOOL_THREADS = "{cores}" } }
```

Ranges: `pool_size` 1..=4096 (the pipe capacity a pool has to fit in),
`max_concurrent` ≥ 1, `drain_deadline_ms` 100..=60000, `defaults.static` `"fair"`
or ≥ 1. An override's `class` is one of the three, and its `env` values carry
only the placeholders §Classification lists. Nothing is applied in part: a file
that fails any of this is refused whole, naming the line, and bzbd refuses to
start rather than run the machine's builds under a configuration nobody wrote.

An override key is matched on the tool's basename, the same string
[`classify`](#classification) looks rows up by, so `"./build.sh"` and
`"build.sh"` are one row and two keys that collapse to one are an error. The row
replaces every built-in row for that tool — class, injection and mode gate
together — and a row forced to `jobserver` gets `MAKEFLAGS` even though the
file cannot ask for it, which is what makes `--class jobserver`'s escape hatch
available to a config file too. A row's `env` is layered on top of that
injection and never replaces it: `MAKEFLAGS` on a jobserver row is the fifo the
task is accounted through, so busybee's value stays and the row's is dropped
with a notice rather than leaving a task that reserves no tokens running
outside the pool. A jobserver row owns `CARGO_MAKEFLAGS` on the same grounds
even though the forced injection sets only `MAKEFLAGS` — cargo reads the
cargo-specific spelling first, so leaving it open would be the same escape by
another name. The same holds for the `BUSYBEE_CLASS` and `BUSYBEE_CORES`
of point 4 above, which every class injects: they are how a task reads back the
bargain it was admitted under, so a row cannot describe itself to the task as
something other than what the scheduler booked.

Reload is SIGHUP or `busybee config reload`, which is the same reload over the
socket so the client can report a refusal instead of leaving it in the log. New
`Params` go to `Scheduler::set_params`; a changed `pool_size` is applied by
releasing or acquiring the delta on the fifo, never taking it below the tokens
currently held — a shrink that cannot complete is logged, the rest booked as
owed, and finishes as the holding leases end: a static grant is withheld as it
is released, and what a jobserver build returns straight to the fifo is taken
from there on the next poll. A token taken either way pays the shrink once.
`busybee config show` prints the effective configuration, defaults merged, as
TOML.

## Observability

`busybee status [--json]`: free tokens, held tokens, each lease (id, label, tool, class, cores, state, elapsed, ahead-count). `tool` is the basename `classify` recognised, which is what decides the class; `label` is the caller's `--name` when there is one, so the two are separate fields. A `none` lease holds the machine rather than a token count, which is what its `cores` column says. The monitor TUI reads the same data: pool gauge plus one row per lease. Jobserver tasks show an *estimated* "using ~N" (`pool − FIONREAD − Σ held`, attributed by counting compiler processes in each task's process group), labelled approximate and never used for scheduling.

`--json` prints `StatusReply` verbatim as one line, plus `approx_in_use` (`pool_size − free − held`, clamped at 0) so the estimate above does not have to be re-derived. The status command does not auto-start bzbd: asking what the pool is doing should not create the pool, and reporting a daemon that failed to start as an idle machine would be the silent fallback this document rules out everywhere else. With no daemon listening there is usually no pool and nothing being gated, so the client says so on stderr and exits 0, leaving stdout empty rather than inventing an all-zero reply. The exception is a bzbd that died with leases live: its tasks keep running (§Failure and recovery) and `leases.json` still records them, so the client reports that instead and exits non-zero — the pool is not idle and it cannot say what it holds.

## Module layout (so issues can be worked in parallel without collisions)

```
crates/bzb-core/src/classify.rs    tool table, wrapper unwrap, Plan        (#3)
crates/bzb-core/src/scheduler.rs   admission state machine                 (#4)
crates/bzb-core/src/jobserver.rs   fifo create/seed/acquire/release/FIONREAD (#5)
crates/bzb-core/src/protocol.rs    client<->bzbd messages (serde, JSON lines) (#6)
crates/bzb-core/src/config.rs      config file + overrides                 (#11)
crates/bzbd/                       daemon binary: server, leases, submit, inject, recovery
crates/bzb/src/enqueue.rs          client on the bzbd protocol             (#9)
crates/bzb/src/status.rs           `busybee status`                        (#10)
crates/bzb/src/monitor/            TUI on bzbd data                        (#19)
```

## Testing strategy

| unit | style |
|---|---|
| classify | table-driven; synthetic fixture of invocation shapes (wrappers, opaque shell strings, explicit `-j`, scripts, `$VAR`-expanded binaries) |
| scheduler | pure state machine tests, no IO (same style as `bzb-core/src/wait.rs`) |
| jobserver | integration tests running real GNU make 4.4 from the dev shell on a Makefile of `sleep` targets; assert peak concurrency ≤ pool + 1 via a counter file (the `+ 1` is the implicit job every participant runs without a token, see admission rule 1); two makes sharing one fifo, bound at pool + 2; FIONREAD accounting |
| bzbd + pueue | isolated `pueued` + `bzbd` in a temp dir, extending `crates/bzb/tests/common/pueued.rs` |
| client | `crates/bzb/tests/smoke.rs` extended: exit codes, Ctrl-C, detach, preamble lines |
| recovery | `crates/bzbd/tests/recovery.rs`: kill bzbd mid-task and restart; SIGTERM bzbd mid-task; SIGKILL a client mid-task; kill pueued mid-task; stale fifos at startup |

Dev shell gains `gnumake` and `ninja`. CI (#14) runs the full suite on macOS and Linux.

## Decisions log

- **Keep pueue, add a broker** rather than replacing pueue: the broker is the novel part; pueue's supervision/logging stays useful; busybee keeps working during the transition. Replacing pueue later is a contained follow-up (give bzbd spawn duties).
- **Opt-in join** (only `busybee --`-wrapped commands see the fifo) rather than exporting `MAKEFLAGS` globally: no always-on daemon, nothing breaks when bzbd is down. Ambient mode is parked (#15).
- **Unknown → none**: every unrecognised command seen in practice was a benchmark, render, or opaque script — all of which want exclusivity. The override table and `--class` are the escape hatches.
- **User-supplied parallelism wins, class is unchanged**: `ninja -j8`, `make -j8` and the env spelling of the same thing (`env MAKEFLAGS=-j100 make`) all defeat the injection, so the task runs uncontrolled while admitted as `jobserver`, which reserves nothing. Accepted knowingly: the remedy is the notice, which tells the user to drop their flag. Demoting these to `none` (exclusive) is a defensible alternative, but it is one admission-policy decision covering every spelling — not something to apply to the env spelling alone — and it would make a plain `ninja -j8` block the whole pool. Revisit in the scheduler (#4), not in `classify`.
- **CPU only** for this iteration. RAM admission (#16) reuses the lease/admission machinery later.
- **Static tasks are locked in**: accepted; the alternatives (restarting xcodebuild with a bigger `-jobs`) are parked (#18).

## Task issues

Groundwork: #13, #14. Core: #3, #4, #5. Daemon: #6, #7, #8, #9, #10, #11, #12. Monitor: #19. Verification: #21. Docs: #20. Parked: #15, #16, #17, #18.

