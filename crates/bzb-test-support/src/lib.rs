//! Fixtures shared by the busybee crates' integration tests.
//!
//! Everything here spawns its own daemon in a temporary directory: a test must
//! never reach the developer's own `pueued` or its `busybee` group.

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// Spawns an isolated `pueued` in a tempdir with its own socket. Kills it on
/// `Drop`. Tests skip themselves (ignored) when `pueued` is not on PATH — this
/// keeps `cargo test` green on machines without pueue installed.
pub struct PueuedFixture {
    child: Child,
    _tmp: TempDir,
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
}

impl PueuedFixture {
    pub fn try_start() -> Option<Self> {
        Self::start_program("pueued", Duration::from_secs(3))
    }

    /// Spawns `program --config <generated config>` and waits `timeout` for the
    /// socket. `None` means only "`program` is not on `PATH`"; a program that
    /// starts and never binds panics.
    fn start_program(program: &str, timeout: Duration) -> Option<Self> {
        if which::which(program).is_err() {
            eprintln!("{program} not on PATH; skipping integration test");
            return None;
        }
        let tmp = TempDir::new().expect("create tempdir");
        let config_path = tmp.path().join("pueue.yml");
        let socket_path = tmp.path().join("pueue.sock");
        let shared_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        // Minimal pueue 4.x config. Only `shared` keys needed; groups are a
        // runtime concept, not config.
        let config = format!(
            r#"shared:
  pueue_directory: {shared}
  runtime_directory: {shared}
  use_unix_socket: true
  unix_socket_path: {socket}
"#,
            socket = socket_path.display(),
            shared = shared_dir.display(),
        );
        std::fs::write(&config_path, config).unwrap();

        let mut child = Command::new(program)
            .arg("--config")
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"));

        // Wait for the socket to appear. A daemon that spawns but never binds
        // is a broken fixture, not a reason to skip the test.
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if socket_path.exists() {
                return Some(Self {
                    child,
                    _tmp: tmp,
                    socket_path,
                    config_path,
                });
            }
            sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "{program} did not create {} within {timeout:?}",
            socket_path.display()
        );
    }

    /// Kills the daemon now, for a test about what happens when pueued dies.
    /// Idempotent: `Drop` kills it again and does not mind.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PueuedFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that starts and exits without ever binding the socket is a
    /// broken fixture, not a reason to skip: `start_program` must panic, never
    /// return `None`. `true` stands in for such a daemon.
    #[test]
    #[should_panic(expected = "did not create")]
    fn start_program_panics_when_the_daemon_never_binds() {
        PueuedFixture::start_program("true", Duration::from_millis(200));
    }
}
