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
    signal(pid, libc::SIGTERM);
}

/// Sends an arbitrary signal to any pid — a test's own `pueued` as much as its
/// daemon, which is why this is a free function and not only a [`Fixture`]
/// method.
pub fn signal(pid: u32, signal: libc::c_int) {
    assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0, "kill failed");
}

/// A foreground `bzbd` with its own state directory, killed on drop.
pub struct Fixture {
    pub child: Child,
    state: PathBuf,
    config: PathBuf,
    /// What the daemon was started with, so `restart` starts the same one.
    env: Vec<(String, String)>,
    /// Kept for its drop: it takes the state directory with it.
    _tmp: TempDir,
}

impl Fixture {
    /// Starts a daemon whose config file does not exist, i.e. on the defaults.
    pub fn start() -> Self {
        Self::start_with(None, &[])
    }

    /// Starts a daemon on `config`, written to a file of its own.
    pub fn start_on(config: &str) -> Self {
        Self::start_with(Some(config), &[])
    }

    /// Same as [`Fixture::start`], but pointed at an isolated `pueued` through
    /// the config path its fixture generated.
    pub fn start_with_pueue(config: &Path) -> Self {
        Self::start_with(None, &[("PUEUE_CONFIG_PATH", config.display().to_string())])
    }

    /// Same again, with a `PATH` of the test's choosing: it is where bzbd
    /// looks for `pueued` when it has to spawn one.
    pub fn start_with_pueue_and_path(config: &Path, path: &Path) -> Self {
        Self::start_with(
            None,
            &[
                ("PUEUE_CONFIG_PATH", config.display().to_string()),
                ("PATH", path.display().to_string()),
            ],
        )
    }

    /// A daemon on `config` — the defaults when there is none — with the given
    /// environment on top of the test's own: `PUEUE_CONFIG_PATH`, `PATH`, and
    /// so on.
    pub fn start_with(config: Option<&str>, env: &[(&str, String)]) -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        // A directory bzbd has to create itself, so its mode is the daemon's
        // doing rather than tempfile's.
        let state = tmp.path().join("state");
        let config_path = tmp.path().join("config.toml");
        if let Some(config) = config {
            std::fs::write(&config_path, config).expect("write the config");
        }
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect();
        let child = spawn(&state, &config_path, &env);
        let fixture = Self {
            child,
            state,
            config: config_path,
            env,
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

    /// SIGKILL: the daemon gets no chance to clean up, which is what a crash
    /// looks like. The state directory stays for `restart`.
    pub fn kill(&mut self) {
        self.child.kill().expect("kill bzbd");
        self.child.wait().expect("wait for bzbd");
    }

    /// Starts a new daemon on the same state directory, config and
    /// environment, and waits until it accepts connections — the socket file
    /// alone proves nothing after a kill, since the dead daemon's is still
    /// there.
    pub fn restart(&mut self) {
        self.child = spawn(&self.state, &self.config, &self.env);
        wait_for_listener(&self.socket_path());
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

    /// The token pool: `jobserver-<pid>` in the state directory, and under
    /// `--foreground` the pid is the child's own.
    pub fn fifo_path(&self) -> PathBuf {
        self.state.join(format!("jobserver-{}", self.child.id()))
    }
}

fn spawn(state: &Path, config: &Path, env: &[(String, String)]) -> Child {
    let mut command = Command::new(BZBD);
    command
        .arg("--foreground")
        .env("BUSYBEE_STATE_DIR", state)
        .env("BUSYBEE_CONFIG", config);
    for (name, value) in env {
        command.env(name, value);
    }
    command.spawn().expect("spawn bzbd")
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

/// Waits up to 3 s for something to accept connections on `socket`.
pub fn wait_for_listener(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("nothing was listening on {} after 3s", socket.display());
}
