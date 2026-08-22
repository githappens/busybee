# busybee [![CI](https://github.com/githappens/busybee/actions/workflows/ci.yml/badge.svg)](https://github.com/githappens/busybee/actions/workflows/ci.yml)

A queued runner for resource-heavy tasks, with a live CPU + queue TUI.

![busybee monitor TUI — per-core CPU gauges with a single-line queue status at the bottom](docs/images/monitor.png)

## Why

Running several agentic dev sessions in parallel is great — until they all fire
off a `cmake --build`, `cargo build`, or big test suite at the same time.
Saturating every core simultaneously means every session gets slower, agents
time out, and the fans scream. There's no coordination between them, and yet
the work is trivially serializable: one build finishes, the next starts.

busybee is a thin tool that enforces that coordination. You prefix the heavy
command with `busybee --`, it gets added to a shared local queue, your shell
blocks until the queue has a slot for you, and then the command runs with full
stdout and exit-code passthrough — as if you'd just typed it directly. Other
busybee invocations from other sessions see you're running and wait their
turn.

Under the hood it wraps [pueue](https://github.com/Nukesor/pueue) (a mature
Rust task queue + daemon) via its `pueue-lib` crate, so all the queue
semantics, persistence, and daemon lifecycle come for free. The novel piece is
the integrated TUI: a live per-core CPU monitor next to the current busybee
queue state, so you can see at a glance what's running, what's waiting, and
how hot the box is.

## Goals

- **Zero-ceremony coordination.** `busybee -- <cmd>` is the whole API for the
  common case. No daemon to configure, no queue to name — busybee auto-starts
  pueued and manages its own group.
- **Faithful passthrough.** Your command sees the same cwd and env (with
  color-forcing vars injected so compile output stays colored). busybee's exit
  code mirrors the task's exit code. Ctrl-C cancels cleanly.
- **Observability.** `busybee monitor` is a single-screen TUI that gives you
  "what's the machine doing right now" at a glance, so you can tell whether
  you're waiting because the queue is long or because one task is chewing a
  core.
- **Opt-in.** Only commands you prefix with `busybee` go through the queue.
  Ad-hoc shell commands are unaffected.

## Usage

```bash
busybee -- cmake --build build --target MyProject
busybee --name "backend tests" -- cargo test --workspace
busybee --detach -- long-running-thing     # enqueue and return immediately
busybee cancel 7                            # end a detached task
busybee monitor                             # live TUI
busybee status                              # one-shot pool + queue view
busybee status --json                       # the same, for scripts and agents
```

`busybee status` prints the token pool and one row per task; `--json` prints
the daemon's reply as a single JSON line. With no daemon running it reports an
idle pool on stderr and exits 0, unless the daemon died leaving tasks behind.

Press `q` in the monitor to quit. Press `Ctrl-C` while blocked to cancel — the
queued or running task is killed and busybee exits 130.

`--detach` prints a lease id and returns, and the task keeps running after the
client is gone. Nothing is left holding it, so `Ctrl-C` cannot reach a detached
task: `busybee cancel <id>` is the only way to end one early.

## Configuration

Optional. With no file, busybee runs on the defaults below.

```bash
busybee config show      # the effective configuration (defaults merged), as TOML
busybee config reload    # make a running bzbd re-read the file (same as SIGHUP)
```

The file lives at `$XDG_CONFIG_HOME/busybee/config.toml`, or
`~/.config/busybee/config.toml` when `XDG_CONFIG_HOME` is unset. `BUSYBEE_CONFIG`
names a different file outright (it must be an absolute path).

```toml
pool_size = 18            # default: logical cores
max_concurrent = 4
drain_deadline_ms = 2000

[defaults]
static = "fair"           # or a fixed core count

[overrides]               # add or replace classification rows without a release
"./build.sh" = { class = "jobserver" }
"my-bench"   = { class = "none" }
"mytool"     = { class = "static", env = { MYTOOL_THREADS = "{cores}" } }
```

| key | type | default | meaning |
|---|---|---|---|
| `pool_size` | integer 1–4096 | logical cores | Tokens in the shared CPU pool. |
| `max_concurrent` | integer ≥ 1 | 4 | Tasks admitted at once, whatever their size. |
| `drain_deadline_ms` | integer 100–60000 | 2000 | How long a task that cannot speak the jobserver protocol waits for its tokens before starting with what it collected. |
| `defaults.static` | `"fair"` or integer ≥ 1 | `"fair"` | Cores such a task asks for: `"fair"` is its share of the pool at the moment it starts. |
| `overrides.<tool>` | table | none | One classification row. |
| `overrides.<tool>.class` | `"jobserver"`, `"static"` or `"none"` | required | How the tool shares the pool: joins it dynamically, holds a fixed number of cores, or takes the machine. |
| `overrides.<tool>.env` | table of strings | empty | Variables to set for the task. Values may contain `{cores}`, `{cores-1}` and `{fifo}`, and nothing else. |

An override key is matched against the tool's basename, so `"./build.sh"` and
`"build.sh"` name the same row (two keys that collapse to one are an error).
The row it produces replaces the built-in one for that tool outright, class and
injection together.

A file that does not parse, names a key busybee does not read, or carries a
value out of range is refused whole, with the line to fix: nothing is applied
in part. `bzbd` then refuses to start, and a reload keeps the configuration it
was already running on.

## Roadmap

- **Hardware-cost-aware scheduling.** Today busybee is strictly
  one-at-a-time. The point of wrapping pueue was to leave room for a smarter
  admission policy: declare each task's rough cost — CPU cores, RAM, maybe
  GPU — and give busybee a budget for the box it's running on. The queue
  then admits a task only when `running_cost + task_cost ≤ budget`, which
  lets it pack a small `cargo check` alongside a big `cmake --build`, or
  schedule two RAM-light things together without OOMing. Same principle
  whether the bottleneck is cores, memory, or a single shared GPU. The goal
  is to keep hardware saturated from user-supplied estimates instead of
  idling behind one big task. Requires a small change to pueued's
  dispatcher — contributed upstream first, forked only if upstream declines.

## Install

Both channels install two executables — `busybee` (the full name) and `bzb`
(a short alias) — and both depend on [pueue](https://github.com/Nukesor/pueue)'s
`pueued` binary being on your `PATH`. busybee auto-starts `pueued` on demand
if it isn't already running, but it doesn't bundle it.

### End users

Homebrew (macOS arm64):

```bash
brew install githappens/tap/busybee
brew install pueue               # prerequisite
```

crates.io (any Rust target):

```bash
cargo install bzb
```

Nix profile (pueue as a prerequisite):

```bash
nix profile add nixpkgs#pueue
```

### From source

Build under `nix develop` and install into your nix profile:

```bash
./scripts/buildanddeploy.sh
```

Or build directly with cargo:

```bash
nix develop --command cargo install --path crates/bzb
```

## Use with agentic dev workflows

busybee exists to fix a specific problem: multiple Claude Code / Cursor /
Windsurf sessions running against different projects on the same machine,
each happily firing off `cargo build` or `cmake --build` whenever it feels
like it. Without coordination you get four parallel builds fighting for the
same cores — everyone's slower, fans max out. You don't want the agents to
stop building, you just want them to serialise.

Teach every agent session in one place by dropping a `CLAUDE.md` (or
`AGENTS.md`, `.cursorrules`, whatever your tool looks for) at the root of
your work tree. Mine lives at `~/Work/CLAUDE.md` and looks roughly like
this:

    # Work-tree-wide conventions

    ## Gate heavy commands through `busybee`

    This machine runs multiple agentic dev sessions in parallel. Route
    heavy commands (compilation, linking, large test suites) through
    `busybee` — a FIFO-gated runner that lets one heavy task run at a
    time across all sessions and blocks the rest with a friendly
    "N ahead" message.

    ### What counts as heavy
    - `cmake --build …`, `ninja …`, `make -j …`
    - `cargo build`, `cargo test`, `cargo check` on non-trivial crates
    - `go build ./…`, `go test ./…` on large modules
    - Any local Docker/image build
    - Long test suites (`pytest`, `npm test`, slow `jest --runInBand`)

    Trivial one-shots (`cargo check -p small-crate`, `cargo fmt`, `rg`,
    `ls`, most git commands) do not need busybee.

    ### Usage
    Always use blocking mode so you wait your turn and see the command
    finish:

        busybee -- cmake --build build --target MyProject
        busybee --name "backend tests" -- cargo test --workspace

    `busybee -- <cmd>` blocks until your turn, then runs in the current
    directory, streams stdout, and exits with the same exit code. Ctrl-C
    while blocked cancels (exit 130).

The details of *what* counts as heavy on your machine are yours to set —
busybee just provides the gate.

## License

MIT OR Apache-2.0.
