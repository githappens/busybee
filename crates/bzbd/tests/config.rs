//! The config file as the daemon sees it: refuse to start on a bad one,
//! reload on SIGHUP and on request, and never apply half of one.

mod common;

use std::time::{Duration, Instant};

use bzb_core::{
    daemon::Connection,
    protocol::{Request, Response, StatusReply},
};
use common::Fixture;

/// Sends `ConfigReload` on a fresh connection and returns the daemon's answer.
async fn reload(daemon: &Fixture) -> Response {
    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");
    conn.send(Request::ConfigReload)
        .await
        .expect("send a reload request");
    conn.recv().await.expect("recv the reload reply")
}

/// Asks the daemon what pool it is running on.
async fn status(daemon: &Fixture) -> StatusReply {
    let mut conn = Connection::connect(&daemon.socket_path())
        .await
        .expect("connect");
    conn.send(Request::Status)
        .await
        .expect("send a status request");
    match conn.recv().await.expect("recv the status reply") {
        Response::Status(status) => status,
        other => panic!("expected a status reply, got {other:?}"),
    }
}

#[tokio::test]
async fn a_reload_request_picks_up_the_rewritten_file() {
    let daemon = Fixture::start_on("pool_size = 4\n");

    daemon.write_config("pool_size = 6\nmax_concurrent = 2\n");

    match reload(&daemon).await {
        Response::ConfigReloaded {
            pool_size,
            max_concurrent,
            drain_deadline_ms,
        } => {
            assert_eq!(pool_size, 6);
            assert_eq!(max_concurrent, 2);
            // Untouched by the rewrite, so it is back to its default.
            assert_eq!(drain_deadline_ms, 2000);
        }
        other => panic!("expected a reload confirmation, got {other:?}"),
    }
}

/// A signal has no reply, so the log is the only place the daemon can say what
/// it now runs on.
#[tokio::test]
async fn sighup_reloads_the_config() {
    let daemon = Fixture::start_on("pool_size = 4\n");
    wait_for_log(&daemon, "pool_size=4");

    daemon.write_config("pool_size = 6\n");
    daemon.signal(libc::SIGHUP);

    wait_for_log(&daemon, "pool_size=6");
}

/// The reply is not the only thing that moves: a reloaded `pool_size` reaches
/// the scheduler, so the pool `Status` reports is the new one and all of it is
/// free while nothing holds a lease.
#[tokio::test]
async fn a_reloaded_pool_size_reaches_the_scheduler() {
    let daemon = Fixture::start_on("pool_size = 4\n");
    assert_eq!(status(&daemon).await.pool_size, 4);

    daemon.write_config("pool_size = 6\n");
    daemon.signal(libc::SIGHUP);
    wait_for_log(&daemon, "pool_size=6");

    let status = status(&daemon).await;
    assert_eq!(status.pool_size, 6);
    assert_eq!(status.free, 6);
}

/// No partial apply: a file that does not parse leaves the daemon on the
/// configuration it already had, and the reason names the line to fix.
#[tokio::test]
async fn a_malformed_file_is_refused_on_reload_and_the_running_config_stays() {
    let daemon = Fixture::start_on("pool_size = 4\n");

    daemon.write_config("pool_size = 6\nmax_concurent = 2\n");

    let Response::Error { message } = reload(&daemon).await else {
        panic!("a malformed config must be refused");
    };
    assert!(message.contains("line 2"), "message was {message:?}");
    assert!(message.contains("pool_size 4"), "message was {message:?}");

    // Still serving, and still on the old pool: fixing only the typo must
    // reload the file the daemon has been refusing.
    daemon.write_config("pool_size = 6\n");
    match reload(&daemon).await {
        Response::ConfigReloaded { pool_size, .. } => assert_eq!(pool_size, 6),
        other => panic!("expected a reload confirmation, got {other:?}"),
    }
}

/// Running the machine's builds under a configuration the user did not write
/// is worse than not running them: refuse the file, name the line, exit.
#[test]
fn a_malformed_file_stops_the_daemon_from_starting() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "pool_size = 0\n").expect("write the config");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bzbd"))
        .arg("--foreground")
        .env("BUSYBEE_STATE_DIR", tmp.path().join("state"))
        .env("BUSYBEE_CONFIG", &config)
        .output()
        .expect("run bzbd");

    assert!(!out.status.success(), "bzbd started on a refused config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pool_size"), "stderr was {stderr:?}");
}

/// The refusal is the would-be daemon's, not every invocation's. A second
/// `bzbd` launched while one is already serving starts nothing, so a file that
/// has gone bad since the first one read it is none of its business: it reports
/// the running daemon and exits zero, and that daemon stays on the
/// configuration it started with.
#[tokio::test]
async fn a_malformed_file_leaves_the_already_running_path_alone() {
    let daemon = Fixture::start_on("pool_size = 4\n");
    daemon.write_config("pool_size = 0\n");

    let second = daemon.run_second_instance();

    assert!(
        second.status.success(),
        "second instance exited {}",
        second.status
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already running"), "stderr was {stderr:?}");
    assert_eq!(status(&daemon).await.pool_size, 4);
}

/// Waits up to 3 s for `needle` to show up in the daemon's log.
fn wait_for_log(daemon: &Fixture, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut log = String::new();
    while Instant::now() < deadline {
        // The daemon opens its log before it binds the socket the fixture
        // waited for, so a missing one here is a failure, not a race.
        log = std::fs::read_to_string(daemon.log_path()).expect("read the log");
        if log.contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{needle:?} never appeared in the log; it holds:\n{log}");
}
