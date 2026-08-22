# busybee — notes for agents and contributors

busybee is a Rust CLI that gates resource-heavy commands (`busybee -- cargo
build`) so only one runs at a time across parallel dev sessions. Today it wraps
`pueued`; the design for the `bzbd` broker that replaces the scheduling half is
[`docs/design/bzbd.md`](docs/design/bzbd.md) — **that document is the
specification**. Conform to it rather than redesigning. The README is for users;
this file is for people changing the code.

## Build and test

Everything runs inside the nix dev shell, which supplies cargo, rustc, clippy,
rustfmt, rust-analyzer and `pueued`. Either enter it once (`nix develop`) or
prefix single commands with `nix develop -c`:

```sh
nix develop -c cargo build
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Builds and the test suite are the kind of load busybee exists to serialise, so
put them through an installed busybee when there is one:

```sh
busybee -- cargo test --workspace
```

`.cargo/config.toml` sets `target-dir = "build"`, so artifacts land in
`build/debug/` and `build/release/` rather than `target/`. Do not change that
setting and do not commit `build/` — it is gitignored.

Both gates currently fail on the tree as it stands: `cargo fmt --check` reports
diffs in most files, and clippy reports one `items_after_test_module` error in
`crates/bzb/src/enqueue.rs`. Format the files you touch; do not reformat the
workspace as a side effect of an unrelated change.

The dev shell gains `gnumake` and `ninja` when the jobserver work lands — add
them to `flake.nix` in that change, not ahead of it.

## Crate layout

```
crates/bzb-core/   library: pueue-lib wrapper plus pure helpers
crates/bzb/        binaries `busybee` and `bzb` (same entry point), monitor TUI
crates/bzbd/       planned: the broker daemon; file layout is fixed by
                   the design document's module-layout section — follow it
                   so parallel work does not collide
```

`crates/bzb-core/src/`:

| module          | purpose |
|-----------------|---------|
| `client.rs`     | connect to `pueued`, spawning it if the socket is unreachable |
| `group.rs`      | create/re-enforce the `busybee` group at `parallel_tasks = 1` |
| `enqueue.rs`    | `TaskSpec` → pueue `AddRequest`; returns the new task id |
| `wait.rs`       | pure state machine turning task-status polls into `WaitEvent`s |
| `status.rs`     | `QueueSnapshot` — running/queued view of the group, plus `count_ahead` |
| `log.rs`        | fetch and decompress a task's log from a byte offset |
| `exit_code.rs`  | pueue `TaskResult` → process exit code |
| `env.rs`        | force colour env vars onto the child's environment |
| `errors.rs`     | `BusybeeError` and its error → exit-code recommendation |

`crates/bzb/src/`: `cli.rs` (clap), `enqueue.rs` (blocking mode: enqueue, wait,
stream, relay exit code), `detach.rs` (`--detach`), `signals.rs` (SIGINT
escalation), `monitor/` (ratatui TUI plus per-OS CPU sampling),
`version_parse.rs` (shared with `build.rs`).

## Integration tests

`crates/bzb/tests/common/pueued.rs` spawns an isolated `pueued`: its own temp
config dir, its own unix socket, killed on `Drop`. Reuse it — tests must never
touch a developer's real pueue instance or its `busybee` group.

```rust
mod common;
use common::pueued::PueuedFixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn my_test() {
    let Some(p) = PueuedFixture::try_start() else { return };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    // ... talk to the isolated daemon
}
```

`try_start` returns `None` when `pueued` is not on `PATH`, so the test
self-skips outside the dev shell. `PUEUE_CONFIG_PATH` is process-wide, hence
`#[serial_test::serial]` on every test that sets it. Run just these with:

```sh
nix develop -c cargo test -p bzb --test smoke
```

## Conventions

- **TDD.** Write the failing test first, then the code. Never weaken, skip or
  delete an existing test to reach green; if a test disagrees with a change,
  assume the code is wrong until shown otherwise.
- **No silent fallbacks.** Errors propagate with context. A degraded path must
  be loud — logged and visible in the result — never the quiet default. The
  design document applies this to the daemon too: if it cannot create its fifo
  or socket it refuses to start rather than running the command ungoverned.
- **Pure state machines, IO at the edges.** `wait.rs` is the model: it takes a
  status snapshot and returns events, with no sockets or clocks inside, so it
  is testable without a daemon. New scheduling and classification logic follows
  the same shape.
- **stdout belongs to the wrapped task.** Every message busybee emits about
  itself goes to stderr, prefixed `busybee: `.
- **`exit_code.rs` is the single source of truth** for translating a task
  result into a process exit code. Do not map results anywhere else.

## Versioning and release

`crates/bzb/build.rs` derives `BUSYBEE_VERSION` at compile time and exposes it
via `cargo:rustc-env`; `cli.rs` reads it for `--version`. The scheme is
`MAJOR.MINOR.<PATCH+N>` from the nearest semver-shaped git tag plus commits
since, falling back to `0.0.<commit-count>` with no tag and to
`CARGO_PKG_VERSION` with no `.git`. Parsing lives in `version_parse.rs`, shared
between the build script and the test harness.

`scripts/buildanddeploy.sh` is the release pipeline: it builds a release binary
under `nix develop`, checks `build/release/busybee` and `build/release/bzb`
exist, then installs them into the nix profile from the flake's binary-only
derivation (`--impure`, with `BUSYBEE_REPO` pointing the flake at the working
tree, because `build/` is gitignored and so absent from the flake source). It
is deliberately non-hermetic. Do not change it as part of unrelated work.

## What not to do

- Do not introduce new external daemons or require system-level configuration.
  The broker described in the design document is the only planned daemon.
- Do not commit `build/`.
- Do not widen the `busybee` pueue group's semantics beyond the design. Today
  that means one group at `parallel_tasks = 1`, re-enforced on every
  invocation. The broker changes it — the design document has bzbd set
  `parallel_tasks = 0` and admit tasks itself — so change this when
  implementing that part of the spec, not before.
- Do not install over an existing `busybee`/`bzb`/`pueued` to test a change;
  use the cargo-built binaries under `build/`.
