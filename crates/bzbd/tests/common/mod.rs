//! An isolated `bzbd` (own state directory and config file) and the client
//! helpers the integration tests share.

// Each test binary uses a different part of this module.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use bzb_core::{
    daemon::Connection,
    protocol::{LeaseEvent, LeaseRequest, Request, Response, StatusReply},
};
use pueue_lib::{
    message::{Request as PueueRequest, Response as PueueResponse},
    task::TaskStatus,
};
use serde_json::Value;
use tempfile::TempDir;

pub const BZBD: &str = env!("CARGO_BIN_EXE_bzbd");

/// A poll tick plus latency, short enough to fail rather than hang.
pub const PATIENCE: Duration = Duration::from_secs(15);

/// A config path in the test's own directory, so a daemon started outside
/// [`Fixture`] never reads the developer's config.
pub fn isolated_config(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

pub fn sigterm(pid: u32) {
    signal(pid, libc::SIGTERM);
}

pub fn signal(pid: u32, signal: libc::c_int) {
    assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0, "kill failed");
}

/// A foreground `bzbd` with its own state directory, killed on drop.
pub struct Fixture {
    pub child: Child,
    state: PathBuf,
    config: PathBuf,
    /// Kept so `restart` starts the same daemon.
    env: Vec<(String, String)>,
    _tmp: TempDir,
}

impl Fixture {
    /// On the defaults: the config file does not exist.
    pub fn start() -> Self {
        Self::start_with(None, &[])
    }

    pub fn start_on(config: &str) -> Self {
        Self::start_with(Some(config), &[])
    }

    /// On the defaults, talking to the isolated pueued behind `config`.
    pub fn start_with_pueue(config: &Path) -> Self {
        Self::start_with(None, &[("PUEUE_CONFIG_PATH", config.display().to_string())])
    }

    /// `env` goes on top of the test's own environment.
    pub fn start_with(config: Option<&str>, env: &[(&str, String)]) -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        // Created by bzbd itself, so its mode is the daemon's doing.
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

    pub fn run_second_instance(&self) -> std::process::Output {
        Command::new(BZBD)
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", &self.state)
            .env("BUSYBEE_CONFIG", &self.config)
            .output()
            .expect("run second bzbd")
    }

    /// SIGKILL, like a crash; the state directory stays for `restart`.
    pub fn kill(&mut self) {
        self.child.kill().expect("kill bzbd");
        self.child.wait().expect("wait for bzbd");
    }

    /// Waits for a listener, not the socket file: a killed daemon's is still there.
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

    pub fn signal(&self, sig: libc::c_int) {
        signal(self.child.id(), sig);
    }

    /// Under `--foreground` the fifo's pid suffix is the child's own.
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

/// Waits up to 3 s for `path` to be present (or gone).
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

pub fn request(argv: &[&str]) -> LeaseRequest {
    request_in(argv, &std::env::current_dir().expect("current dir"))
}

pub fn request_in(argv: &[&str], cwd: &Path) -> LeaseRequest {
    LeaseRequest {
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        cwd: cwd.to_path_buf(),
        // The task needs a PATH, as the real client's environment would give it.
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        label: None,
        class_override: None,
        cores_wanted: None,
        detached: false,
    }
}

pub async fn connect(daemon: &Fixture) -> Connection {
    Connection::connect(&daemon.socket_path())
        .await
        .expect("connect to bzbd")
}

pub async fn submit(daemon: &Fixture, request: LeaseRequest) -> Connection {
    let mut conn = connect(daemon).await;
    conn.send(Request::Submit(request))
        .await
        .expect("send a submit request");
    conn
}

pub async fn event(conn: &mut Connection) -> LeaseEvent {
    tokio::time::timeout(PATIENCE, conn.events().next())
        .await
        .expect("no lease event arrived in time")
        .expect("read a lease event")
        .expect("the event stream ended before the lease finished")
}

pub async fn status(daemon: &Fixture) -> StatusReply {
    let mut conn = connect(daemon).await;
    conn.send(Request::Status).await.expect("send status");
    match conn.recv().await.expect("recv a status reply") {
        Response::Status(status) => status,
        other => panic!("expected a Status reply, got {other:?}"),
    }
}

pub async fn wait_for_no_leases(daemon: &Fixture, patience: Duration) -> StatusReply {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = status(daemon).await;
        if status.leases.is_empty() {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "leases were still {:?} after {patience:?}",
            status.leases
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn leases_json(daemon: &Fixture) -> Vec<Value> {
    let raw = std::fs::read_to_string(daemon.leases_path()).expect("read leases.json");
    serde_json::from_str(&raw).expect("decode leases.json")
}

/// Connects without spawning: a test whose pueued is gone must not get a new
/// one. Sets the process-wide `PUEUE_CONFIG_PATH`, so callers are serial.
pub async fn pueue(config: &Path) -> pueue_lib::Client {
    std::env::set_var("PUEUE_CONFIG_PATH", config);
    bzb_core::client::connect()
        .await
        .expect("connect to pueued")
}

/// `None` once pueued has no record of the task. See [`pueue`].
pub async fn task_status(config: &Path, task_id: usize) -> Option<TaskStatus> {
    let mut client = pueue(config).await;
    client
        .send_request(PueueRequest::Status)
        .await
        .expect("send a status request");
    let state = match client.receive_response().await.expect("status response") {
        PueueResponse::Status(state) => state,
        other => panic!("expected a status response, got {other:?}"),
    };
    state.tasks.get(&task_id).map(|t| t.status.clone())
}

/// Gone from pueued itself, not just from bzbd's books. See [`pueue`].
pub async fn wait_for_task_to_end(config: &Path, task_id: usize, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = task_status(config, task_id).await;
        if matches!(status, None | Some(TaskStatus::Done { .. })) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pueue task {task_id} was still {status:?} after {patience:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
