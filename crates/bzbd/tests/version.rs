//! `bzbd --version` exits 0, prints exactly `bzbd <version>`, and leaves no state.

mod common;

use common::BZBD;
use std::process::Command;

/// `bzbd --version` exits 0 and prints exactly one line `bzbd <MAJOR.MINOR.PATCH…>`,
/// equal to the version busybee's build derives for the same checkout.
#[test]
fn bzbd_version_flag_prints_version() {
    let out = Command::new(BZBD)
        .arg("--version")
        .output()
        .expect("run bzbd --version");

    assert!(out.status.success(), "bzbd --version exited {}", out.status);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("bzbd {}\n", env!("BUSYBEE_VERSION"));
    assert_eq!(
        stdout.as_ref(),
        expected,
        "bzbd --version printed {stdout:?}, expected {expected:?}"
    );
}

/// `bzbd --version` must not create the state directory or bind any socket,
/// even when `BUSYBEE_STATE_DIR` points at a path that does not exist.
#[test]
fn bzbd_version_flag_touches_no_state() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let state = tmp.path().join("nonexistent-state");

    let out = Command::new(BZBD)
        .arg("--version")
        .env("BUSYBEE_STATE_DIR", &state)
        .output()
        .expect("run bzbd --version");

    assert!(out.status.success(), "bzbd --version exited {}", out.status);
    assert!(
        !state.exists(),
        "bzbd --version created the state directory at {}",
        state.display()
    );
}
