//! bzb-core against a real, isolated `pueued`: connecting, submitting a task
//! and reading its log back.

use bzb_core::{
    client::{self, Client},
    enqueue::{enqueue, TaskSpec},
    log::fetch_log_chunk,
};
use bzb_test_support::PueuedFixture;

/// A connected client to a fresh isolated pueued with the `busybee` group in
/// place, or `None` (skip) without pueued. `PUEUE_CONFIG_PATH` is
/// process-wide, hence `serial` on every caller.
async fn connected() -> Option<(PueuedFixture, Client)> {
    let p = PueuedFixture::try_start()?;
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.expect("connect");
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");
    Some((p, client))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn connect_succeeds_when_pueued_is_running() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let client = client::connect_or_spawn()
        .await
        .expect("connect should succeed");
    drop(client);
}

/// A spawned replacement would answer log requests from an empty queue and
/// hide the real misconfiguration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn connect_fails_rather_than_spawning_a_second_pueued() {
    let Some(mut p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    p.kill();

    let err = client::connect()
        .await
        .expect_err("there is no daemon behind that socket");
    assert!(
        matches!(
            err,
            bzb_core::errors::BusybeeError::DaemonUnreachable { .. }
        ),
        "error was {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn enqueue_returns_a_task_id() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };
    let spec = TaskSpec {
        command: "true".into(),
        cwd: std::env::current_dir().unwrap(),
        env: Default::default(),
        label: Some("smoke".into()),
        start_immediately: false,
    };
    // Fresh isolated daemon: the first task gets id 0.
    assert_eq!(enqueue(&mut client, spec).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn log_chunk_accumulates_across_polls() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };
    let id = enqueue(
        &mut client,
        TaskSpec {
            command: "printf one; printf two".into(),
            cwd: std::env::current_dir().unwrap(),
            env: Default::default(),
            label: None,
            start_immediately: false,
        },
    )
    .await
    .unwrap();

    let mut seen = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (bytes, _) = fetch_log_chunk(&mut client, id, 0).await.unwrap();
        seen = String::from_utf8_lossy(&bytes).into_owned();
        if seen.contains("onetwo") {
            return;
        }
    }
    panic!("never saw full output; last: {seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn log_chunk_returns_plaintext_for_repetitive_output() {
    // pueued sends logs snappy-framed; short output survives a frame
    // literally, so a long repeated line forces back-references.
    let Some((_p, mut client)) = connected().await else {
        return;
    };

    let line = "AudioFileFormat:Multiplier:createWriterForAudioFileFormat";
    let repeats = 200;
    let id = enqueue(
        &mut client,
        TaskSpec {
            command: format!(
                "i=0; while [ \"$i\" -lt {repeats} ]; do echo '{line}'; i=$((i+1)); done"
            ),
            cwd: std::env::current_dir().unwrap(),
            env: Default::default(),
            label: None,
            start_immediately: false,
        },
    )
    .await
    .unwrap();

    let expected = {
        let mut s = String::new();
        for _ in 0..repeats {
            s.push_str(line);
            s.push('\n');
        }
        s
    };

    let mut last: Vec<u8> = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (bytes, _) = fetch_log_chunk(&mut client, id, 0).await.unwrap();
        last = bytes;
        if last.len() >= expected.len() {
            break;
        }
    }

    assert!(
        !last.windows(6).any(|w| w == b"sNaPpY"),
        "output still contains snappy frame magic; first 32 bytes: {:x?}",
        &last[..last.len().min(32)]
    );
    assert_eq!(
        String::from_utf8(last).expect("output is valid utf-8"),
        expected,
    );
}
