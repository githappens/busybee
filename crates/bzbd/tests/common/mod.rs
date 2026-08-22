//! A `bzbd` of one's own: every test drives a daemon in a temporary state
//! directory, never the developer's instance.

// Every test binary compiles this module for itself and uses a different part
// of it, so what one leaves unused another needs.
#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use tempfile::TempDir;

pub const BZBD: &str = env!("CARGO_BIN_EXE_bzbd");

/// A foreground `bzbd` with its own state directory, killed on drop.
pub struct Fixture {
    pub child: Child,
    state: PathBuf,
    /// Kept for its drop: it takes the state directory with it.
    _tmp: TempDir,
}

impl Fixture {
    pub fn start() -> Self {
        Self::spawn(None)
    }

    /// Same, but pointed at an isolated `pueued` through the config path its
    /// fixture generated.
    pub fn start_with_pueue(config: &Path) -> Self {
        Self::spawn(Some(config))
    }

    fn spawn(pueue_config: Option<&Path>) -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        // A directory bzbd has to create itself, so its mode is the daemon's
        // doing rather than tempfile's.
        let state = tmp.path().join("state");
        let mut command = Command::new(BZBD);
        command.arg("--foreground").env("BUSYBEE_STATE_DIR", &state);
        if let Some(config) = pueue_config {
            command.env("PUEUE_CONFIG_PATH", config);
        }
        let child = command.spawn().expect("spawn bzbd");
        let fixture = Self {
            child,
            state,
            _tmp: tmp,
        };
        wait_for(&fixture.socket_path(), true);
        fixture
    }

    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    pub fn socket_path(&self) -> PathBuf {
        self.state.join("bzbd.sock")
    }

    pub fn pid_path(&self) -> PathBuf {
        self.state.join("bzbd.pid")
    }

    pub fn leases_path(&self) -> PathBuf {
        self.state.join("leases.json")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits up to 3 s for `path` to exist (or to be gone, when `present` is false).
pub fn wait_for(path: &Path, present: bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() == present {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "{} was still {} after 3s",
        path.display(),
        if present { "missing" } else { "present" }
    );
}

pub fn sigterm(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) },
        0,
        "kill failed"
    );
}
