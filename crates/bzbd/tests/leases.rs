//! Lease lifecycle end to end: bzbd admits, submits to an isolated `pueued`,
//! streams the events back and tears the task down when its client goes away.
//!
//! Both daemons are the tests' own — a temporary state directory for bzbd, a
//! temporary config and socket for pueued.

mod common;

use std::{collections::BTreeMap, path::Path, time::Duration};

use bzb_core::{
    daemon::Connection,
    protocol::{LeaseEvent, LeaseRequest, Request, Response, StatusReply},
};
use bzb_test_support::PueuedFixture;
use common::Fixture;
use pueue_lib::{
    message::{Request as PueueRequest, Response as PueueResponse},
    task::TaskStatus,
};
use serde_json::Value;

/// Long enough for a poll tick (1 s) plus the daemons' own latency, short
/// enough that a lease which never arrives fails the test instead of hanging
/// the suite.
const PATIENCE: Duration = Duration::from_secs(15);

fn request(argv: &[&str]) -> LeaseRequest {
    LeaseRequest {
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        cwd: std::env::current_dir().expect("current dir"),
        // The real client sends its own environment; the task needs a PATH to
        // find `sh` with.
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        label: None,
        class_override: None,
        cores_wanted: None,
        detached: false,
    }
}

async fn connect(daemon: &Fixture) -> Connection {
    Connection::connect(&daemon.socket_path())
        .await
        .expect("connect to bzbd")
}

async fn submit(daemon: &Fixture, argv: &[&str]) -> Connection {
    let mut conn = connect(daemon).await;
    conn.send(Request::Submit(request(argv)))
        .await
        .expect("send a submit request");
    conn
}

async fn event(conn: &mut Connection) -> LeaseEvent {
    tokio::time::timeout(PATIENCE, conn.events().next())
        .await
        .expect("no lease event arrived in time")
        .expect("read a lease event")
        .expect("the event stream ended before the lease finished")
}

/// Asserts that nothing arrives within `patience` — that a lease is really
/// waiting rather than merely slow.
async fn stays_silent(conn: &mut Connection, patience: Duration) {
    if let Ok(event) = tokio::time::timeout(patience, conn.events().next()).await {
        panic!("expected no event, got {event:?}");
    }
}

async fn status(daemon: &Fixture) -> StatusReply {
    let mut conn = connect(daemon).await;
    conn.send(Request::Status).await.expect("send status");
    match conn.recv().await.expect("recv a status reply") {
        Response::Status(status) => status,
        other => panic!("expected a Status reply, got {other:?}"),
    }
}

/// Polls `status` until it holds no leases, or fails after `patience`.
async fn wait_for_no_leases(daemon: &Fixture, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let leases = status(daemon).await.leases;
        if leases.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "leases were still {leases:?} after {patience:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn leases_json(daemon: &Fixture) -> Vec<Value> {
    let raw = std::fs::read_to_string(daemon.leases_path()).expect("read leases.json");
    serde_json::from_str(&raw).expect("decode leases.json")
}

fn admitted(event: LeaseEvent) -> (u64, usize) {
    match event {
        LeaseEvent::Admitted {
            id, pueue_task_id, ..
        } => (id, pueue_task_id),
        other => panic!("expected an Admitted event, got {other:?}"),
    }
}

#[tokio::test]
async fn a_lease_runs_its_command_and_reports_the_exit_code() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut conn = submit(&daemon, &["sh", "-c", "exit 7"]).await;

    match event(&mut conn).await {
        LeaseEvent::Queued { ahead, .. } => assert_eq!(ahead, 0),
        other => panic!("expected a Queued event first, got {other:?}"),
    }
    match event(&mut conn).await {
        // A shell string is opaque to the classifier, so it runs exclusively.
        LeaseEvent::Admitted { class, .. } => assert_eq!(class, "none"),
        other => panic!("expected an Admitted event, got {other:?}"),
    }
    match event(&mut conn).await {
        LeaseEvent::Finished { exit_code, .. } => assert_eq!(exit_code, 7),
        other => panic!("expected a Finished event, got {other:?}"),
    }
}

/// Class `none` is exclusive, so the second lease is told what it is waiting
/// behind and stays there until the first one is done.
#[tokio::test]
async fn a_second_lease_waits_behind_the_running_one() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut first = submit(&daemon, &["sh", "-c", "sleep 2"]).await;
    assert!(matches!(
        event(&mut first).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    admitted(event(&mut first).await);

    let mut second = submit(&daemon, &["sh", "-c", "sleep 2"]).await;
    match event(&mut second).await {
        LeaseEvent::Queued { ahead, .. } => assert_eq!(ahead, 1),
        other => panic!("expected a Queued event, got {other:?}"),
    }
    stays_silent(&mut second, Duration::from_secs(1)).await;
    // Waiting, not merely slow to be told: the daemon's own books have it
    // queued behind the running one, with no task of its own.
    let leases = status(&daemon).await.leases;
    let waiting = leases
        .iter()
        .find(|l| l.state == "queued")
        .unwrap_or_else(|| panic!("no queued lease in {leases:?}"));
    assert_eq!(waiting.ahead, Some(1));
    assert_eq!(waiting.pueue_task_id, None);

    assert!(matches!(
        event(&mut first).await,
        LeaseEvent::Finished { exit_code: 0, .. }
    ));
    admitted(event(&mut second).await);
}

/// Connection = lease: a client that hangs up while queued never runs.
#[tokio::test]
async fn a_client_that_hangs_up_while_queued_loses_its_lease() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut first = submit(&daemon, &["sh", "-c", "sleep 30"]).await;
    assert!(matches!(
        event(&mut first).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    let (_, task) = admitted(event(&mut first).await);

    let mut second = submit(&daemon, &["sh", "-c", "sleep 30"]).await;
    let queued_id = match event(&mut second).await {
        LeaseEvent::Queued { id, ahead } => {
            assert_eq!(ahead, 1);
            id
        }
        other => panic!("expected a Queued event, got {other:?}"),
    };
    drop(second);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let leases = status(&daemon).await.leases;
        if leases.iter().all(|l| l.id != queued_id) {
            // The running lease is untouched: only the one that hung up went.
            assert_eq!(leases.len(), 1, "leases were {leases:?}");
            assert_eq!(leases[0].pueue_task_id, Some(task));
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the queued lease was still there after 5s: {leases:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// And one that hangs up while running takes its task with it, rather than
/// leaving a `sleep 30` behind holding the machine.
#[tokio::test]
#[serial_test::serial]
async fn a_client_that_hangs_up_while_running_takes_its_task_with_it() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut conn = submit(&daemon, &["sh", "-c", "sleep 30"]).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    let (_, task) = admitted(event(&mut conn).await);
    drop(conn);

    wait_for_task_to_end(&pueued.config_path, task, Duration::from_secs(2)).await;
    wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
}

/// A task that ignores SIGTERM, so it lives until the SIGKILL escalation.
/// pueued signals the whole process group, hence a loop of short sleeps rather
/// than one long one: the shell is what has to survive.
const IGNORES_SIGTERM: &[&str] = &["sh", "-c", "trap '' TERM; while :; do sleep 0.2; done"];

/// A lease admitted while the task it replaces is still being killed would put
/// two exclusive tasks on the machine at once — the thing busybee exists to
/// prevent. The next admission waits for pueued to confirm the kill.
#[tokio::test]
#[serial_test::serial]
async fn the_next_lease_waits_until_the_killed_task_is_gone() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut first = submit(&daemon, IGNORES_SIGTERM).await;
    assert!(matches!(
        event(&mut first).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    let (_, stubborn) = admitted(event(&mut first).await);

    let mut second = submit(&daemon, &["sh", "-c", "exit 0"]).await;
    assert!(matches!(
        event(&mut second).await,
        LeaseEvent::Queued { ahead: 1, .. }
    ));

    // SIGTERM goes out at once, but the shell ignores it; the second lease may
    // not start until the escalation has actually ended the first task.
    drop(first);
    admitted(event(&mut second).await);
    let status = task_status(&pueued.config_path, stubborn).await;
    assert!(
        matches!(status, None | Some(TaskStatus::Done { .. })),
        "the second lease started while pueue task {stubborn} was still {status:?}"
    );
}

/// Polls pueued directly: the task has to be gone from the machine, not just
/// from bzbd's books.
async fn wait_for_task_to_end(config: &Path, task_id: usize, patience: Duration) {
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

/// What pueued itself says a task is doing, `None` once its record is gone.
async fn task_status(config: &Path, task_id: usize) -> Option<TaskStatus> {
    // Process-wide, which is why the tests that call this are serial.
    std::env::set_var("PUEUE_CONFIG_PATH", config);
    let mut client = bzb_core::client::connect_or_spawn()
        .await
        .expect("connect to pueued");
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

/// `leases.json` is what a restarted bzbd reads to find the tasks it left
/// running, so it has to track the live set exactly.
#[tokio::test]
async fn leases_json_holds_the_running_lease_and_nothing_once_it_ends() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with_pueue(&pueued.config_path);

    let mut conn = submit(&daemon, &["sh", "-c", "exit 0"]).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    let (id, task) = admitted(event(&mut conn).await);

    let persisted = leases_json(&daemon);
    assert_eq!(persisted.len(), 1, "persisted {persisted:?}");
    let lease = &persisted[0];
    assert_eq!(lease["id"], Value::from(id));
    assert_eq!(lease["pueue_task_id"], Value::from(task));
    assert_eq!(lease["class"], Value::from("none"));
    assert_eq!(lease["label"], Value::from("sh -c 'exit 0'"));
    assert!(
        lease["started_at_unix_ms"].as_u64().unwrap_or(0) > 0,
        "started_at was {:?}",
        lease["started_at_unix_ms"]
    );
    // An exclusive lease drains the whole pool.
    let pool_size = status(&daemon).await.pool_size;
    assert_eq!(lease["cores_held"], Value::from(pool_size));

    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Finished { exit_code: 0, .. }
    ));
    assert!(
        leases_json(&daemon).is_empty(),
        "a finished lease was left in leases.json: {:?}",
        leases_json(&daemon)
    );
}

/// `docs/design/bzbd.md` §Failure and recovery: a pueued that dies takes the
/// running leases with it — their clients are told, and told why, rather than
/// waiting for a completion that can no longer arrive.
#[tokio::test]
async fn a_pueued_that_never_comes_back_ends_the_running_leases() {
    let Some(mut pueued) = PueuedFixture::try_start() else {
        return;
    };
    // bzbd respawns pueued by name off its `PATH`; an empty one is a pueued
    // that is not coming back.
    let nothing_on_path = tempfile::tempdir().expect("create tempdir");
    let daemon = Fixture::start_with_pueue_and_path(&pueued.config_path, nothing_on_path.path());

    let mut conn = submit(&daemon, &["sh", "-c", "sleep 5"]).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    admitted(event(&mut conn).await);

    pueued.kill();

    match event(&mut conn).await {
        LeaseEvent::Notice { text } => assert!(
            text.contains("pueued"),
            "the notice must name what was lost, got {text:?}"
        ),
        other => panic!("expected a Notice, got {other:?}"),
    }
    match event(&mut conn).await {
        // Non-zero: the command's own exit code went with pueued.
        LeaseEvent::Finished { exit_code, .. } => assert_ne!(exit_code, 0),
        other => panic!("expected a Finished event, got {other:?}"),
    }
    // And bzbd keeps serving: the lost lease is off its books.
    wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
}
