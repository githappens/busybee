# bzb-core integration tests

`jobserver.rs` drives real GNU make and ninja against a `Jobserver` fifo and
measures the peak concurrency they reach. It needs **make >= 4.4** (fifo-style
`--jobserver-auth`) and **ninja >= 1.13** (jobserver client support). Both are
in the dev shell: run the suite under `nix develop`.

When a tool is missing or too old, the tests that need it print
`skipping: <tool> ... ` to stderr and return without asserting. Rust cannot
attach `#[ignore]` at runtime, so such a test still reports `ok`; run with
`--nocapture` to see the reason:

```
cargo test -p bzb-core --test jobserver -- --nocapture
```

The fixtures invoke `make` and `ninja` without `-j`. An explicit `-j` on the
command line makes both tools ignore the fifo (make prints `-jN forced in
submake: resetting jobserver mode`); that is the protocol, not a test bug.
