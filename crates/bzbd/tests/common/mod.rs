//! An isolated `bzbd` for integration tests: its own state directory and its
//! own config file, so no test can reach the developer's daemon or read the
//! config on their machine.

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

/// A config file for a daemon started outside [`Fixture`]: a path inside the
/// test's own directory, so the daemon reads the defaults rather than whatever
/// the developer running the suite has configured.
pub fn isolated_config(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

pub fn sigterm(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) },
        0,
        "kill failed"
    );
}

/// A foreground `bzbd` with its own state directory, killed on drop.
pub struct Fixture {
    pub child: Child,
    state: PathBuf,
    config: PathBuf,
    /// Kept for its drop: it takes the state directory with it.
    _tmp: TempDir,
}

impl Fixture {
    /// Starts a daemon whose config file does not exist, i.e. on the defaults.
    pub fn start() -> Self {
        Self::spawn(None, None, None)
    }

    /// Starts a daemon on `config`, written to a file of its own.
    pub fn start_on(config: &str) -> Self {
        Self::spawn(None, None, Some(config))
    }

    /// Same as [`Fixture::start`], but pointed at an isolated `pueued` through
    /// the config path its fixture generated.
    pub fn start_with_pueue(config: &Path) -> Self {
        Self::spawn(Some(config), None, None)
    }

    /// Same again, with a `PATH` of the test's choosing: it is where bzbd
    /// looks for `pueued` when it has to spawn one.
    pub fn start_with_pueue_and_path(config: &Path, path: &Path) -> Self {
        Self::spawn(Some(config), Some(path), None)
    }

    fn spawn(pueue_config: Option<&Path>, path: Option<&Path>, config: Option<&str>) -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        // A directory bzbd has to create itself, so its mode is the daemon's
        // doing rather than tempfile's.
        let state = tmp.path().join("state");
        let config_path = tmp.path().join("config.toml");
        if let Some(config) = config {
            std::fs::write(&config_path, config).expect("write the config");
        }
        let mut command = Command::new(BZBD);
        command
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", &state)
            .env("BUSYBEE_CONFIG", &config_path);
        if let Some(pueue_config) = pueue_config {
            command.env("PUEUE_CONFIG_PATH", pueue_config);
        }
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let child = command.spawn().expect("spawn bzbd");
        let fixture = Self {
            child,
            state,
            config: config_path,
            _tmp: tmp,
        };
        wait_for(&fixture.socket_path(), true);
        fixture
    }

    /// Runs a second `bzbd` against the same state directory and config, to
    /// completion.
    pub fn run_second_instance(&self) -> std::process::Output {
        Command::new(BZBD)
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", &self.state)
            .env("BUSYBEE_CONFIG", &self.config)
            .output()
            .expect("run second bzbd")
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

    pub fn log_path(&self) -> PathBuf {
        self.state.join("bzbd.log")
    }

    pub fn config_path(&self) -> &Path {
        &self.config
    }

    pub fn write_config(&self, body: &str) {
        std::fs::write(&self.config, body).expect("rewrite the config");
    }

    /// Sends `signal` to the daemon.
    pub fn signal(&self, signal: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, signal) },
            0,
            "kill failed"
        );
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
