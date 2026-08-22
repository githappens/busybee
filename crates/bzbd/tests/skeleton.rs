//! Lifecycle and wire-protocol tests for the daemon skeleton.
//!
//! Every test runs its own `bzbd` in a temp state directory; the user's
//! instance is never touched.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use bzb_core::{
    daemon::Connection,
    protocol::{Request, Response, MAX_LINE_BYTES},
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BZBD: &str = env!("CARGO_BIN_EXE_bzbd");

/// A foreground `bzbd` with its own state directory, killed on drop.
struct Fixture {
    child: Child,
    state: PathBuf,
    /// Kept for its drop: it takes the state directory with it.
    _tmp: TempDir,
}

impl Fixture {
    fn start() -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        // A directory bzbd has to create itself, so its mode is the daemon's
        // doing rather than tempfile's.
        let state = tmp.path().join("state");
        let child = Command::new(BZBD)
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", &state)
            .spawn()
            .expect("spawn bzbd");
        let fixture = Self {
            child,
            state,
            _tmp: tmp,
        };
        wait_for(&fixture.socket_path(), true);
        fixture
    }

    fn state_dir(&self) -> &Path {
        &self.state
    }

    fn socket_path(&self) -> PathBuf {
        self.state.join("bzbd.sock")
    }

    fn pid_path(&self) -> PathBuf {
        self.state.join("bzbd.pid")
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

/// The socket is the daemon's whole control surface, and it lives wherever the
/// state directory does. Under the usual 022 umask both would otherwise be
/// readable and connectable by every other user on the machine.
#[tokio::test]
async fn the_state_directory_and_the_socket_are_owner_only() {
    let daemon = Fixture::start();

    assert_eq!(
        mode(daemon.state_dir()),
        0o700,
        "the state directory {} is not owner-only",
        daemon.state_dir().display()
    );
    // Not an exact mode: `bind` derives the socket's from 0777, so the daemon's
    // umask leaves it 0700. What has to hold is that nobody else is in it, and
    // that it held from the moment the socket was bound: a chmod afterwards
    // would leave a window in which any user could connect.
    let socket = mode(&daemon.socket_path());
    assert_eq!(
        socket & 0o077,
        0,
        "the socket is {socket:o}, not owner-only"
    );
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn status_is_refused_while_there_is_no_pool_to_report() {
    let daemon = Fixture::start();
    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");

    conn.send(Request::Status).await.expect("send status");

    match conn.recv().await.expect("recv status reply") {
        Response::Error { message } => assert!(
            message.contains("not implemented"),
            "expected a not-implemented error, got {message:?}"
        ),
        other => panic!("expected an Error, got {other:?}"),
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

/// A line with no newline in sight must not be buffered without limit: the
/// daemon outlives every client, so one hostile connection would otherwise take
/// it down with the whole machine's memory.
#[tokio::test]
async fn an_oversized_hello_is_rejected_instead_of_buffered() {
    let daemon = Fixture::start();

    let message = oversized_line_error(&daemon, &[]).await;

    assert!(
        message.contains(&MAX_LINE_BYTES.to_string()),
        "message was {message:?}"
    );
}

/// Same limit after the handshake: the request loop reads from the same socket.
#[tokio::test]
async fn an_oversized_request_is_rejected_instead_of_buffered() {
    let daemon = Fixture::start();

    let message = oversized_line_error(&daemon, b"{\"hello\":1}\n").await;

    assert!(
        message.contains(&MAX_LINE_BYTES.to_string()),
        "message was {message:?}"
    );
}

/// Sends `prelude` (discarding one reply per line in it), then a newline-free
/// line one byte over the limit, and returns the error the daemon answers with.
/// Panics unless the daemon then closes the connection.
async fn oversized_line_error(daemon: &Fixture, prelude: &[u8]) -> String {
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(prelude).await.expect("write prelude");
    for _ in prelude.iter().filter(|byte| **byte == b'\n') {
        lines
            .next_line()
            .await
            .expect("read")
            .expect("a reply line");
    }
    // No trailing newline: the daemon has to stop on the byte count alone.
    writer
        .write_all(&vec![b'x'; MAX_LINE_BYTES + 1])
        .await
        .expect("write an oversized line");

    // A daemon that buffers the line instead of rejecting it never answers, so
    // this has to fail rather than hang.
    let reply = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .expect("the daemon did not answer within 3s")
        .expect("read")
        .expect("a reply line");
    let message = match serde_json::from_str::<Response>(&reply).expect("decode reply") {
        Response::Error { message } => message,
        other => panic!("expected an Error, got {other:?}"),
    };
    assert!(
        lines.next_line().await.expect("read").is_none(),
        "connection stayed open"
    );
    message
}

/// The limit binds both directions. Echoing an undecodable request back into
/// the error message expands it: every quote and backslash costs two bytes
/// once, then two more when the message is itself JSON-encoded, so a request
/// that fits would be answered with a line that does not.
#[tokio::test]
async fn the_error_for_an_undecodable_request_stays_within_the_line_limit() {
    let mut request = vec![b'"'; MAX_LINE_BYTES - 1];
    request.push(b'\n');
    let message = bounded_decode_error(&request).await;
    assert!(
        message.contains("decode"),
        "expected a decode error, got {message:?}"
    );
}

/// Serde quotes the input it choked on, so an in-limit line naming an unknown
/// variant comes back embedded in the error the daemon would otherwise relay
/// verbatim. The prefix matters more than the offending name.
#[tokio::test]
async fn the_error_for_an_unknown_request_variant_stays_within_the_line_limit() {
    let mut request = vec![b'"'];
    request.extend_from_slice(&vec![b'x'; MAX_LINE_BYTES - 3]);
    request.extend_from_slice(b"\"\n");
    assert_eq!(request.len(), MAX_LINE_BYTES);
    let message = bounded_decode_error(&request).await;
    assert!(
        message.contains("decode"),
        "expected a decode error, got {message:?}"
    );
}

/// Sends `request` to a fresh daemon after the handshake and returns the error
/// message, having checked that the reply itself stayed inside the frame the
/// client is willing to read.
async fn bounded_decode_error(request: &[u8]) -> String {
    let daemon = Fixture::start();
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(b"{\"hello\":1}\n").await.expect("hello");
    lines.next_line().await.expect("read").expect("a pong");

    writer.write_all(request).await.expect("write request");

    let reply = lines
        .next_line()
        .await
        .expect("read")
        .expect("a reply line");
    assert!(
        reply.len() <= MAX_LINE_BYTES,
        "the daemon answered with {} bytes, over the {MAX_LINE_BYTES} byte limit",
        reply.len()
    );
    match serde_json::from_str::<Response>(&reply).expect("decode reply") {
        Response::Error { message } => message,
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// The frame ends at the newline. A hello that arrives without one was never
/// finished, however complete its JSON looks, so the daemon must refuse it
/// rather than serve a peer whose framing it cannot follow.
#[tokio::test]
async fn an_unterminated_hello_is_refused_instead_of_answered() {
    let daemon = Fixture::start();
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(b"{\"hello\":1}")
        .await
        .expect("write hello");
    writer.shutdown().await.expect("close the write half");

    let reply = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .expect("the daemon did not answer within 3s")
        .expect("read")
        .expect("a reply line");
    match serde_json::from_str::<Response>(&reply).expect("decode reply") {
        Response::Error { message } => assert!(
            message.contains("newline"),
            "expected a framing error, got {message:?}"
        ),
        other => panic!("expected an Error, got {other:?}"),
    }
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

/// Daemon mode forks; the parent may only exit once the child is serving,
/// otherwise a client that waited for it still finds no socket.
#[tokio::test]
async fn daemonizing_returns_only_once_the_socket_is_serving() {
    let tmp = TempDir::new().expect("create tempdir");
    let status = Command::new(BZBD)
        .env("BUSYBEE_STATE_DIR", tmp.path())
        .status()
        .expect("run bzbd");
    assert!(status.success(), "bzbd exited {status}");

    // Deliberately no waiting: the socket has to be there already.
    let mut conn = Connection::connect(&tmp.path().join("bzbd.sock"))
        .await
        .expect("connect");
    conn.send(Request::Ping).await.expect("send ping");
    let Response::Pong { pid, .. } = conn.recv().await.expect("recv pong") else {
        panic!("expected a Pong");
    };

    sigterm(pid);
    wait_for(&tmp.path().join("bzbd.sock"), false);
}

/// A daemon that dies after the fork must say why on the caller's stderr; the
/// forking parent has no other way to report it.
#[tokio::test]
async fn a_startup_failure_after_the_fork_reaches_the_caller() {
    let tmp = TempDir::new().expect("create tempdir");
    // A socket path far past sun_path's ~104 bytes: the directory is created
    // by the parent, the bind then fails in the child.
    let state = tmp.path().join("d".repeat(120));

    let out = Command::new(BZBD)
        .env("BUSYBEE_STATE_DIR", &state)
        .output()
        .expect("run bzbd");

    assert!(!out.status.success(), "bzbd exited {}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot bind the socket"),
        "stderr was {stderr:?}"
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
