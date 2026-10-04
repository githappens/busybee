//! Fixtures shared by the busybee crates' integration tests. Every daemon runs
//! in its own temporary directory: a test must never reach the developer's
//! own `pueued` or its `busybee` group.

pub mod counter;

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// Write a minimal pueue 4.x config into `dir`, creating `dir/shared/` for
/// pueued's own state. Returns `(config_path, socket_path, shared_dir)`.
/// Does not start pueued.
pub fn pueue_config_in_dir(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let config_path = dir.join("pueue.yml");
    let socket_path = dir.join("pueue.sock");
    let shared_dir = dir.join("shared");
    std::fs::create_dir_all(&shared_dir).unwrap();
    let config = format!(
        "shared:\n  pueue_directory: {shared}\n  runtime_directory: {shared}\
         \n  use_unix_socket: true\n  unix_socket_path: {socket}\n",
        socket = socket_path.display(),
        shared = shared_dir.display(),
    );
    std::fs::write(&config_path, config).unwrap();
    (config_path, socket_path, shared_dir)
}

/// An isolated `pueued` in a tempdir with its own socket, killed on `Drop`.
pub struct PueuedFixture {
    child: Child,
    _tmp: TempDir,
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
}

impl PueuedFixture {
    /// `None` when `pueued` is not on `PATH`, so the test skips itself.
    pub fn try_start() -> Option<Self> {
        Self::start_program("pueued", Duration::from_secs(3))
    }

    /// Spawns `program --config <generated config>` and waits `timeout` for the
    /// socket. `None` means only "`program` is not on `PATH`"; a program that
    /// starts and never binds panics.
    fn start_program(program: &str, timeout: Duration) -> Option<Self> {
        let tmp = TempDir::new().expect("create tempdir");
        let (config_path, socket_path, _shared_dir) = pueue_config_in_dir(tmp.path());

        let spawned = Command::new(program)
            .arg("--config")
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("{program} not on PATH; skipping integration test");
                return None;
            }
            Err(e) => panic!("spawn {program}: {e}"),
        };

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

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Idempotent: `Drop` calls it again.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PueuedFixture {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that never binds is a broken fixture, not a skip. `true`
    /// stands in for one.
    #[test]
    #[should_panic(expected = "did not create")]
    fn start_program_panics_when_the_daemon_never_binds() {
        PueuedFixture::start_program("true", Duration::from_millis(200));
    }
}
