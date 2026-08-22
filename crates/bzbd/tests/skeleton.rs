//! Lifecycle and wire-protocol tests for the daemon skeleton.
//!
//! Every test runs its own `bzbd` in a temp state directory; the user's
//! instance is never touched.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use bzb_core::{
    daemon::Connection,
    protocol::{Request, Response},
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BZBD: &str = env!("CARGO_BIN_EXE_bzbd");

/// A foreground `bzbd` with its own state directory, killed on drop.
struct Fixture {
    child: Child,
    tmp: TempDir,
}

impl Fixture {
    fn start() -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        let child = Command::new(BZBD)
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", tmp.path())
            .spawn()
            .expect("spawn bzbd");
        let fixture = Self { child, tmp };
        wait_for(&fixture.socket_path(), true);
        fixture
    }

    fn state_dir(&self) -> &Path {
        self.tmp.path()
    }

    fn socket_path(&self) -> PathBuf {
        self.tmp.path().join("bzbd.sock")
    }

    fn pid_path(&self) -> PathBuf {
        self.tmp.path().join("bzbd.pid")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits up to 3 s for `path` to exist (or to be gone, when `present` is false).
fn wait_for(path: &Path, present: bool) {
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

fn sigterm(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) },
        0,
        "kill failed"
    );
}

#[tokio::test]
async fn ping_reports_the_crate_version_and_the_daemon_pid() {
    let daemon = Fixture::start();
    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");

    conn.send(Request::Ping).await.expect("send ping");

    match conn.recv().await.expect("recv pong") {
        Response::Pong { version, pid } => {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(pid, daemon.child.id());
        }
        other => panic!("expected a Pong, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_instance_exits_zero_and_the_first_keeps_serving() {
    let daemon = Fixture::start();

    let second = Command::new(BZBD)
        .arg("--foreground")
        .env("BUSYBEE_STATE_DIR", daemon.state_dir())
        .output()
        .expect("run second bzbd");

    assert!(
        second.status.success(),
        "second instance exited {}",
        second.status
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already running"), "stderr was {stderr:?}");

    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");
    conn.send(Request::Ping).await.expect("send ping");
    assert!(matches!(
        conn.recv().await.expect("recv"),
        Response::Pong { .. }
    ));
}

#[tokio::test]
async fn sigterm_removes_the_socket_and_the_pid_file() {
    let mut daemon = Fixture::start();
    assert!(
        daemon.pid_path().exists(),
        "pid file should exist while running"
    );

    sigterm(daemon.child.id());

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if !daemon.socket_path().exists() && !daemon.pid_path().exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!daemon.socket_path().exists(), "socket outlived SIGTERM");
    assert!(!daemon.pid_path().exists(), "pid file outlived SIGTERM");
    assert!(daemon.child.wait().expect("wait").success());
}

#[tokio::test]
async fn a_protocol_version_mismatch_gets_an_error_and_the_connection_closes() {
    let daemon = Fixture::start();
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(b"{\"hello\":9999}\n")
        .await
        .expect("write hello");

    let reply = lines
        .next_line()
        .await
        .expect("read")
        .expect("a reply line");
    match serde_json::from_str::<Response>(&reply).expect("decode reply") {
        Response::Error { message } => assert!(message.contains("9999"), "message was {message:?}"),
        other => panic!("expected an Error, got {other:?}"),
    }
    assert!(
        lines.next_line().await.expect("read").is_none(),
        "connection stayed open"
    );
}

#[tokio::test]
async fn connect_or_spawn_starts_a_daemon_when_none_is_running() {
    let tmp = TempDir::new().expect("create tempdir");
    // `connect_or_spawn_bzbd` reads the environment of this process and looks
    // up `bzbd` on PATH, so both have to be set here.
    std::env::set_var("BUSYBEE_STATE_DIR", tmp.path());
    std::env::set_var("PATH", Path::new(BZBD).parent().expect("bzbd's directory"));

    let mut conn = bzb_core::daemon::connect_or_spawn_bzbd()
        .await
        .expect("connect or spawn");
    conn.send(Request::Ping).await.expect("send ping");
    let pid = match conn.recv().await.expect("recv pong") {
        Response::Pong { pid, .. } => pid,
        other => panic!("expected a Pong, got {other:?}"),
    };

    // The spawned daemon is detached, so it has to be stopped explicitly.
    assert_eq!(
        fs::read_to_string(tmp.path().join("bzbd.pid"))
            .expect("read pid file")
            .trim(),
        pid.to_string()
    );
    sigterm(pid);
    wait_for(&tmp.path().join("bzbd.sock"), false);
}
