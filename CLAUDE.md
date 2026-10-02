# busybee — notes for agents and contributors

busybee is a Rust CLI that gates resource-heavy commands (`busybee -- cargo
build`) across parallel dev sessions. The `bzbd` broker admits work against a
shared CPU token pool and submits it to `pueued`, which spawns and logs it.
[`docs/design/bzbd.md`](docs/design/bzbd.md) **is the specification**: conform
to it rather than redesigning. The README is for users; this file is for people
changing the code. Review rules and the agent workflow are in
[`AGENTS.md`](AGENTS.md); the planned VM lab is
[`docs/design/agent-lab.md`](docs/design/agent-lab.md).

## Build and test

Everything runs inside the nix dev shell, which supplies cargo, rustc, clippy,
rustfmt, rust-analyzer and `pueued`. Either enter it once (`nix develop`) or
prefix single commands with `nix develop -c`:

```sh
nix develop -c cargo build
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
nix develop -c cargo fmt --all --check
```

Builds and the test suite are the kind of load busybee exists to serialise, so
put them through an installed busybee when there is one:

```sh
busybee -- cargo test --workspace
```

`.cargo/config.toml` sets `target-dir = "build"`, so artifacts land in
`build/debug/` and `build/release/` rather than `target/`. Do not change that
setting and do not commit `build/` — it is gitignored.

CI gates fmt, clippy and the tests on Linux and macOS. Format the files you
touch; do not reformat the workspace as a side effect of something else.

The VM lab controller (`docs/design/agent-lab.md`) is Python under
`scripts/vm/`, with its regression scenarios under `tests/scenarios/`, outside
the Cargo workspace. Its preflight and tests need no
Parallels; `doctor` reads the host's Parallels when one is installed:

```sh
nix develop -c python3 scripts/vm/vmctl.py doctor
nix develop -c python3 -m unittest discover -s scripts/vm/tests
nix develop -c python3 -m unittest discover -s tests/scenarios/tests
```

## Crate layout

- `crates/bzb-core/`: shared library. Pure logic (`classify`, `scheduler`,
  `wait`, `nest`, `exit_code`, `config`), the bzbd wire `protocol` and client
  side (`daemon`), the fifo `jobserver`, and thin pueue-lib wrappers (`client`,
  `group`, `enqueue`, `kill`, `log`).
- `crates/bzb/`: the `busybee` and `bzb` binaries (one entry point): clap CLI,
  blocking `enqueue`, `detach`/`cancel`, `status`, `config`, and the ratatui
  `monitor`.
- `crates/bzbd/`: the broker daemon (state dir and socket in `lib`, the lease
  actor in `leases`, startup `recovery`, pure `inject`, pueued `submit`);
  module layout fixed by the spec.
- `crates/bzb-test-support/`: fixtures shared by the integration tests.

## Integration tests

`bzb-test-support`'s `PueuedFixture` spawns an isolated `pueued`: its own temp
config dir, its own unix socket, killed on `Drop`. Reuse it — tests must never
touch a developer's real pueue instance or its `busybee` group. bzbd's own
tests add `crates/bzbd/tests/common/mod.rs`, which does the same for a daemon
in a temporary `BUSYBEE_STATE_DIR`.

```rust
use bzb_test_support::PueuedFixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn my_test() {
    let Some(p) = PueuedFixture::try_start() else { return };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    // ... talk to the isolated daemon
}
```

`try_start` returns `None` only when `pueued` is not on `PATH`, so the test
self-skips outside the dev shell; a daemon that spawns but never binds its
socket panics rather than skipping. `PUEUE_CONFIG_PATH` is process-wide, hence
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
  Older code may not obey it; new code propagates.
- **Pure state machines, IO at the edges.** `wait.rs` is the model: it takes a
  status snapshot and returns events, with no sockets or clocks inside, so it
  is testable without a daemon. New scheduling and classification logic follows
  the same shape.
- **stdout belongs to the wrapped task.** busybee's own messages go to stderr,
  prefixed `busybee: ` — except a fatal error, which `main` returns as an
  `anyhow::Result` for Rust to print unprefixed as `Error: …`. Two commands own
  stdout as their result: `--detach` prints `busybee: lease <id> detached (…)`
  there — the lease id, because that is what `busybee cancel <id>` takes
  (`crates/bzb/tests/smoke.rs` asserts that channel), and `monitor` renders its
  ratatui TUI to it.
- **`exit_code.rs` is the single source of truth** for translating a task
  result into a process exit code. Do not map results anywhere else.

## Versioning and release

`crates/bzb/build.rs` derives `--version` from `git describe`
(`MAJOR.MINOR.<PATCH+N>` from the nearest semver tag); parsing lives in
`version_parse.rs`, shared with the tests.

`scripts/buildanddeploy.sh` is the release pipeline: it builds release binaries
under `nix develop`, checks `build/release/{busybee,bzb}` exist, then installs
them into the nix profile from the flake's binary-only derivation (`--impure`,
with `BUSYBEE_REPO` pointing the flake at the working tree, since `build/` is
gitignored). Deliberately non-hermetic; do not change it in unrelated work.

## What not to do

- Do not introduce new external daemons or require system-level configuration.
  The broker described in the design document is the only planned daemon.
- Do not widen the `busybee` pueue group's semantics beyond the design. That
  means one group at `parallel_tasks = 0`, re-enforced on every invocation:
  pueue's dispatcher is bypassed and bzbd decides what runs, submitting
  admitted tasks with `start_immediately`.
- Do not install over an existing `busybee`/`bzb`/`pueued` to test a change;
  use the cargo-built binaries under `build/`.
