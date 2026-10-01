//! Daemon lifecycle and wire protocol.

mod common;

use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use bzb_core::{
    daemon::Connection,
    protocol::{Request, Response, MAX_LINE_BYTES, PROTOCOL_VERSION},
};
use common::{isolated_config, sigterm, wait_for, Fixture, BZBD};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
async fn the_state_directory_and_the_socket_are_owner_only() {
    let daemon = Fixture::start();

    assert_eq!(
        mode(daemon.state_dir()),
        0o700,
        "the state directory {} is not owner-only",
        daemon.state_dir().display()
    );
    // The mode the spec names.
    assert_eq!(
        mode(&daemon.socket_path()),
        0o600,
        "the socket {} is not 0600",
        daemon.socket_path().display()
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
async fn status_reports_an_untouched_pool_while_no_lease_exists() {
    let daemon = Fixture::start();
    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");

    conn.send(Request::Status).await.expect("send status");

    match conn.recv().await.expect("recv status reply") {
        Response::Status(status) => {
            assert!(status.pool_size > 0, "pool was {}", status.pool_size);
            assert_eq!(status.free, status.pool_size);
            assert_eq!(status.held, 0);
            assert!(status.leases.is_empty(), "leases were {:?}", status.leases);
        }
        other => panic!("expected a Status reply, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_instance_exits_zero_and_the_first_keeps_serving() {
    let daemon = Fixture::start();

    let second = daemon.run_second_instance();

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
async fn an_oversized_hello_is_rejected_instead_of_buffered() {
    let daemon = Fixture::start();

    let message = oversized_line_error(&daemon, &[]).await;

    assert!(
        message.contains(&MAX_LINE_BYTES.to_string()),
        "message was {message:?}"
    );
}

#[tokio::test]
async fn an_oversized_request_is_rejected_instead_of_buffered() {
    let daemon = Fixture::start();

    let message = oversized_line_error(&daemon, hello().as_bytes()).await;

    assert!(
        message.contains(&MAX_LINE_BYTES.to_string()),
        "message was {message:?}"
    );
}

fn hello() -> String {
    format!("{{\"hello\":{PROTOCOL_VERSION}}}\n")
}

/// Sends `prelude` (one reply discarded per line), then a newline-free line one
/// byte over the limit; returns the error and asserts the connection closed.
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

/// Echoing the request back would expand every quote fourfold.
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

/// Serde quotes the variant it choked on into its own error.
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

#[tokio::test]
async fn a_request_that_is_not_utf8_gets_an_error_rather_than_a_dropped_connection() {
    let message = bounded_decode_error(b"\xff\xfe not utf-8\n").await;
    assert!(message.contains("utf-8"), "message was {message:?}");
}

/// Sends `request` after the handshake; returns the error, asserting the reply
/// fits in a line.
async fn bounded_decode_error(request: &[u8]) -> String {
    let daemon = Fixture::start();
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(hello().as_bytes()).await.expect("hello");
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

#[tokio::test]
async fn an_unterminated_hello_is_refused_instead_of_answered() {
    let daemon = Fixture::start();
    let stream = tokio::net::UnixStream::connect(daemon.socket_path())
        .await
        .expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(hello().trim_end().as_bytes())
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

#[tokio::test]
async fn daemonizing_returns_only_once_the_socket_is_serving() {
    let tmp = TempDir::new().expect("create tempdir");
    let status = Command::new(BZBD)
        .env("BUSYBEE_STATE_DIR", tmp.path())
        .env("BUSYBEE_CONFIG", isolated_config(tmp.path()))
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

#[tokio::test]
async fn a_startup_failure_after_the_fork_reaches_the_caller() {
    let tmp = TempDir::new().expect("create tempdir");
    // A socket path far past sun_path's ~104 bytes: the directory is created
    // by the parent, the bind then fails in the child.
    let state = tmp.path().join("d".repeat(120));

    let out = Command::new(BZBD)
        .env("BUSYBEE_STATE_DIR", &state)
        .env("BUSYBEE_CONFIG", isolated_config(tmp.path()))
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
#[serial_test::serial]
async fn connect_or_spawn_starts_a_daemon_when_none_is_running() {
    let tmp = TempDir::new().expect("create tempdir");
    // Process-wide, hence serial.
    std::env::set_var("BUSYBEE_STATE_DIR", tmp.path());
    std::env::set_var("BUSYBEE_CONFIG", isolated_config(tmp.path()));
    std::env::set_var("PATH", Path::new(BZBD).parent().expect("bzbd's directory"));

    let mut conn = bzb_core::daemon::connect_or_spawn_bzbd()
        .await
        .expect("connect or spawn");
    conn.send(Request::Ping).await.expect("send ping");
    let pid = match conn.recv().await.expect("recv pong") {
        Response::Pong { pid, .. } => pid,
        other => panic!("expected a Pong, got {other:?}"),
    };

    assert_eq!(
        fs::read_to_string(tmp.path().join("bzbd.pid"))
            .expect("read pid file")
            .trim(),
        pid.to_string()
    );
    sigterm(pid);
    wait_for(&tmp.path().join("bzbd.sock"), false);
}

/// A departing daemon unlinks its socket before releasing the lock; a spawn in
/// that window exits "already running", so the client must spawn again.
#[tokio::test]
#[serial_test::serial]
async fn connect_or_spawn_starts_a_daemon_once_a_departing_one_releases_the_lock() {
    let tmp = TempDir::new().expect("create tempdir");
    std::env::set_var("BUSYBEE_STATE_DIR", tmp.path());
    std::env::set_var("BUSYBEE_CONFIG", isolated_config(tmp.path()));
    std::env::set_var("PATH", Path::new(BZBD).parent().expect("bzbd's directory"));

    // Stand in for that daemon: the lock is held, the socket is already gone.
    let pid_file = fs::File::create(tmp.path().join("bzbd.pid")).expect("create the pid file");
    assert_eq!(
        unsafe { libc::flock(pid_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "flock failed"
    );
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        // Teardown finishes: closing the file releases the lock.
        drop(pid_file);
    });

    let mut conn = bzb_core::daemon::connect_or_spawn_bzbd()
        .await
        .expect("connect or spawn");
    conn.send(Request::Ping).await.expect("send ping");
    let pid = match conn.recv().await.expect("recv pong") {
        Response::Pong { pid, .. } => pid,
        other => panic!("expected a Pong, got {other:?}"),
    };

    sigterm(pid);
    wait_for(&tmp.path().join("bzbd.sock"), false);
}
