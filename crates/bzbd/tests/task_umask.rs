//! Task umask: bzbd's control-surface umask must not reach the pueued it
//! spawns, or the tasks pueued runs (docs/design/bzbd.md §Components;
//! githappens/busybee#69).
//!
//! All three tests go through the cold client startup path: neither daemon is
//! running when the test starts, the configuration is private, and no runtime
//! directories are precreated.

mod common;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use bzb_core::{daemon::Connection, protocol::LeaseEvent};
use common::{file_mode, request_in, submit, Fixture, PATIENCE};
use tempfile::TempDir;

/// Whether `pueued` is on `PATH`, checked without starting a daemon.
fn pueued_available() -> bool {
    !matches!(
        std::process::Command::new("pueued")
            .arg("--help")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Writes an isolated pueue config without starting pueued. Returns the
/// config path, the shared-state directory (used for pueued's PID file), and
/// the temp dir that owns them (must live for the whole test).
fn cold_pueue_config() -> (PathBuf, PathBuf, TempDir) {
    let tmp = TempDir::new().expect("pueue tempdir");
    let (config, _socket, shared) = bzb_test_support::pueue_config_in_dir(tmp.path());
    (config, shared, tmp)
}

/// Drain events until `Finished`; return its exit code.
async fn wait_for_finished(conn: &mut Connection) -> i32 {
    loop {
        match common::event(conn).await {
            LeaseEvent::Finished { exit_code, .. } => return exit_code,
            LeaseEvent::Queued { .. } | LeaseEvent::Admitted { .. } | LeaseEvent::Notice { .. } => {
            }
        }
    }
}

/// Send SIGTERM to the pueued that bzbd auto-spawned, using its PID file in
/// the shared dir. Best-effort: skips quietly when no PID file exists.
fn kill_pueued_in(shared: &Path) {
    let pid_path = shared.join("pueue.pid");
    if let Ok(content) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            if pid > 0 {
                // SAFETY: `kill(2)` has no preconditions.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                // Short pause before the temp dir is removed so pueued can
                // finish any in-flight writes.
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
}

/// A task creates a directory and a nested file inside it.
///
/// On the base: bzbd's restrictive umask (0o177) reaches pueued, so the
/// task's `mkdir` produces a directory with mode 0600 (no execute bit).
/// `touch d/f` then fails with EACCES, and sh exits 1.
#[tokio::test]
async fn cold_start_creates_traversable_task_directories() {
    if !pueued_available() {
        return;
    }
    let (config, shared, _pueue_tmp) = cold_pueue_config();
    let work = TempDir::new().expect("work dir");
    let daemon = Fixture::start_with_pueue(&config);

    let req = request_in(&["sh", "-c", "mkdir d && touch d/f"], work.path());
    let mut conn = submit(&daemon, req).await;
    let finished = tokio::time::timeout(PATIENCE, wait_for_finished(&mut conn)).await;
    // Before any assertion, so a timed-out task leaves no pueued behind.
    kill_pueued_in(&shared);
    let exit_code = finished.expect("task timed out");
    assert_eq!(
        exit_code, 0,
        "task failed: directory was not traversable — umask reached pueued (exit {exit_code})"
    );
}

/// A task copies an executable and then runs the copy.
///
/// On the base: pueued inherits umask 0o177, so `cp source t` produces `t`
/// with mode `0755 & ~0177 = 0600` (not executable). Exec fails.
#[tokio::test]
async fn cold_start_preserves_executable_task_output() {
    if !pueued_available() {
        return;
    }
    let (config, shared, _pueue_tmp) = cold_pueue_config();
    let work = TempDir::new().expect("work dir");

    // Executable that the task will copy and run.
    let tool = work.path().join("tool");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").expect("write tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod tool");

    let daemon = Fixture::start_with_pueue(&config);

    let req = request_in(&["sh", "-c", "cp tool t && ./t"], work.path());
    let mut conn = submit(&daemon, req).await;
    let finished = tokio::time::timeout(PATIENCE, wait_for_finished(&mut conn)).await;
    // Before any assertion, so a timed-out task leaves no pueued behind.
    kill_pueued_in(&shared);
    let exit_code = finished.expect("task timed out");
    assert_eq!(
        exit_code, 0,
        "task failed: executable copy was not runnable — umask reached pueued (exit {exit_code})"
    );
}

/// bzbd's own control surface stays owner-only even after the fix. Checked on
/// the cold path so the fixture cannot mask a state-dir permission regression.
#[tokio::test]
async fn daemon_control_surface_remains_private() {
    let (config, _, _pueue_tmp) = cold_pueue_config();
    let daemon = Fixture::start_with_pueue(&config);

    assert_eq!(
        file_mode(daemon.state_dir()),
        0o700,
        "state directory is not 0700"
    );
    assert_eq!(
        file_mode(&daemon.socket_path()),
        0o600,
        "socket is not 0600"
    );
    assert_eq!(
        file_mode(&daemon.fifo_path()),
        0o600,
        "jobserver fifo is not 0600"
    );
    assert_eq!(
        file_mode(&daemon.leases_path()),
        0o600,
        "leases.json is not 0600"
    );
}
