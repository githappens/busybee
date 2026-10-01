//! A `busybee` client with an isolated `pueued` and state directory; the
//! client auto-starts its own `bzbd` there.

// Each test binary uses a different part of this module.
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// A poll tick plus daemon latency, with room to spare.
pub const PATIENCE: Duration = Duration::from_secs(15);

pub struct Busybee {
    _pueue: PueuedFixture,
    pub tmp: TempDir,
}

impl Busybee {
    /// `None` when `pueued` is not on `PATH`, so tests self-skip.
    pub fn start() -> Option<Self> {
        Self::start_on("pool_size = 4\n")
    }

    pub fn start_on(config: &str) -> Option<Self> {
        let pueue = PueuedFixture::try_start()?;
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

    /// Like [`Self::run`], but kills busybee and panics after [`PATIENCE`].
    pub fn run_timed(&self, args: &[&str]) -> Output {
        let mut cmd = self.cmd(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().expect("spawn busybee");
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(PATIENCE) {
            Ok(result) => result.expect("wait for busybee"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                panic!(
                    "busybee did not finish within {PATIENCE:?}; nested gating likely deadlocked"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the waiter thread dropped before busybee exited")
            }
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir().join("bzbd.sock")
    }

    /// Errors are returned, not raised: to the waiters they mean "not yet".
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

    pub fn wait_for_leases(&self, count: usize) {
        self.wait_for(&format!("{count} lease(s)"), |status| {
            status.leases.len() == count
        });
    }

    pub fn wait_for_a_running_task(&self) {
        self.wait_for("a running task", |status| {
            status.leases.iter().any(|l| l.state == "running")
        });
    }

    pub fn wait_for(&self, what: &str, ready: impl Fn(&StatusReply) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        loop {
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
        // The auto-started daemon is in its own session; find it by pid file.
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

/// Binds `socket`, answers one handshake, then holds the connection silently.
pub fn spawn_wedged_daemon(socket: &Path) {
    let listener = tokio::net::UnixListener::bind(socket).expect("bind");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(reader).lines();
        lines.next_line().await.expect("read").expect("a hello");
        let pong = Response::Pong {
            version: "test".into(),
            pid: std::process::id(),
        };
        let line = format!("{}\n", serde_json::to_string(&pong).expect("encode"));
        writer.write_all(line.as_bytes()).await.expect("write");
        std::future::pending::<()>().await;
    });
}
