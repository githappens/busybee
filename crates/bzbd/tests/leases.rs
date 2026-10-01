//! Lease lifecycle end to end against an isolated `pueued`.

mod common;

use std::time::Duration;

use bzb_core::{daemon::Connection, protocol::LeaseEvent};
use bzb_test_support::PueuedFixture;
use common::{
    event, leases_json, request, status, task_status, wait_for_no_leases, wait_for_task_to_end,
    Fixture,
};
use pueue_lib::task::TaskStatus;
use serde_json::Value;

async fn submit(daemon: &Fixture, argv: &[&str]) -> Connection {
    common::submit(daemon, request(argv)).await
}

/// Asserts that nothing arrives within `patience`.
async fn stays_silent(conn: &mut Connection, patience: Duration) {
    if let Ok(event) = tokio::time::timeout(patience, conn.events().next()).await {
        panic!("expected no event, got {event:?}");
    }
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

/// Lives until SIGKILL. pueued signals the process group, so the shell must
/// survive: short sleeps in a loop, not one long one.
const IGNORES_SIGTERM: &[&str] = &["sh", "-c", "trap '' TERM; while :; do sleep 0.2; done"];

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

    drop(first);
    admitted(event(&mut second).await);
    let status = task_status(&pueued.config_path, stubborn).await;
    assert!(
        matches!(status, None | Some(TaskStatus::Done { .. })),
        "the second lease started while pueue task {stubborn} was still {status:?}"
    );
}

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

#[tokio::test]
async fn a_pueued_that_never_comes_back_ends_the_running_leases() {
    let Some(mut pueued) = PueuedFixture::try_start() else {
        return;
    };
    // bzbd respawns pueued off its `PATH`; an empty one means it never comes back.
    let nothing_on_path = tempfile::tempdir().expect("create tempdir");
    let daemon = Fixture::start_with(
        None,
        &[
            (
                "PUEUE_CONFIG_PATH",
                pueued.config_path.display().to_string(),
            ),
            ("PATH", nothing_on_path.path().display().to_string()),
        ],
    );

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
    wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
}
