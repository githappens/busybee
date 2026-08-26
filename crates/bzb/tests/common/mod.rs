//! A `busybee` client with daemons of its own: an isolated `pueued` and a
//! `bzbd` in a temporary state directory.
//!
//! `busybee` starts bzbd itself when the socket is unreachable, so no test
//! spawns one by hand — the auto-start is the path under test. What every test
//! does is point `BUSYBEE_STATE_DIR` and `PUEUE_CONFIG_PATH` somewhere of its
//! own, so nothing here can reach a developer's daemon or queue.

// Every test binary compiles this module for itself and uses a different part
// of it, so what one leaves unused another needs.
#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use bzb_core::{
    daemon::Connection,
    protocol::{Request, Response, StatusReply},
};
use bzb_test_support::PueuedFixture;
use tempfile::TempDir;

/// Long enough for a poll tick (1 s) plus the daemons' own latency, short
/// enough that a task which never runs fails the test instead of hanging it.
pub const PATIENCE: Duration = Duration::from_secs(15);

pub struct Busybee {
    _pueue: PueuedFixture,
    pub tmp: TempDir,
}

impl Busybee {
    /// `None` when `pueued` is not on `PATH`, which is how these tests skip
    /// themselves outside the dev shell.
    pub fn start() -> Option<Self> {
        Self::start_on("pool_size = 4\n")
    }

    /// The same, on a configuration of the test's choosing: the pool size is
    /// pinned rather than left to the core count of whatever runs the tests,
    /// and the developer's config stays out of it.
    pub fn start_on(config: &str) -> Option<Self> {
        let pueue = PueuedFixture::try_start()?;
        // bzbd is started by the client, from next to the client's own binary.
        let bzbd = Path::new(env!("CARGO_BIN_EXE_busybee"))
            .parent()
            .expect("the client binary has a directory")
            .join("bzbd");
        assert!(
            bzbd.is_file(),
            "{} is missing; build the whole workspace (cargo build --workspace) \
             so the client has a daemon to start",
            bzbd.display()
        );
        let tmp = TempDir::new().expect("create tempdir");
        std::fs::write(tmp.path().join("config.toml"), config).expect("write the config");
        Some(Self { _pueue: pueue, tmp })
    }

    /// A `busybee` invocation pointed at this test's daemons. The bzbd it
    /// auto-starts inherits this environment, config file included.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_busybee"));
        cmd.env("BUSYBEE_STATE_DIR", self.state_dir())
            .env("BUSYBEE_CONFIG", self.tmp.path().join("config.toml"))
            .env("PUEUE_CONFIG_PATH", &self._pueue.config_path)
            .args(args);
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("run busybee")
    }

    /// Like [`Self::run`], but kills the process and panics if it outlives
    /// `timeout`. Nested gating used to deadlock; a regression must fail the
    /// test rather than stall the suite.
    pub fn run_timed(&self, args: &[&str], timeout: Duration) -> Output {
        run_with_timeout(self.cmd(args), timeout)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir().join("bzbd.sock")
    }

    /// bzbd is started by the client under test, so a test that only spawned
    /// one has a window in which there is no socket to ask yet. The error is
    /// returned rather than raised: to the waiters below it is a "not yet".
    pub fn status(&self) -> anyhow::Result<StatusReply> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut conn = Connection::connect(&self.socket()).await?;
            conn.send(Request::Status).await?;
            match conn.recv().await? {
                Response::Status(status) => Ok(status),
                other => anyhow::bail!("expected a status reply, got {other:?}"),
            }
        })
    }

    /// Waits for bzbd to hold `count` leases, or fails saying what it held.
    pub fn wait_for_leases(&self, count: usize) {
        self.wait_for(&format!("{count} lease(s)"), |status| {
            status.leases.len() == count
        });
    }

    /// Waits for a task to be running, which is the point at which bzbd has it
    /// on the machine rather than merely on the queue.
    pub fn wait_for_a_running_task(&self) {
        self.wait_for("a running task", |status| {
            status.leases.iter().any(|l| l.state == "running")
        });
    }

    pub fn wait_for(&self, what: &str, ready: impl Fn(&StatusReply) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            // Kept for the failure message, so a wait that never reached bzbd
            // at all says that instead of blaming the condition.
            let why = match self.status() {
                Ok(status) if ready(&status) => return,
                Ok(status) => format!("bzbd holds {:?}", status.leases),
                Err(err) => format!("bzbd is unreachable: {err}"),
            };
            assert!(Instant::now() < deadline, "waited for {what}; {why}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Busybee {
    fn drop(&mut self) {
        // The daemon the client auto-started is in its own session; the pid
        // file is what is left to find it by. A test that never started one
        // has no file and nothing to kill.
        let Ok(pid) = std::fs::read_to_string(self.state_dir().join("bzbd.pid")) else {
            return;
        };
        if let Ok(pid) = pid.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs `cmd` with piped output, killing it if it has not exited by `timeout`.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Output {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn busybee");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result.expect("wait for busybee"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("busybee did not finish within {timeout:?}; nested gating likely deadlocked");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the waiter thread dropped before busybee exited")
        }
    }
}
