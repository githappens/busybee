//! What bzb-core says to a real, isolated `pueued`: connecting, submitting a
//! task and reading its log back.
//!
//! These used to live in the client's smoke tests, back when the client itself
//! submitted tasks. bzbd does that now (`docs/design/bzbd.md` §Components), so
//! the coverage belongs to the library both daemons share.

use bzb_core::{
    client,
    enqueue::{enqueue, TaskSpec},
    log::fetch_log_chunk,
};
use bzb_test_support::PueuedFixture;

#[test]
fn pueued_fixture_starts_and_stops() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    assert!(p.socket_path.exists());
    drop(p);
    // Socket path may or may not be removed by pueued on shutdown; that's OK.
}

/// `PUEUE_CONFIG_PATH` is process-wide, hence `serial`: two tests setting it
/// at once would send one of them to the other's daemon.
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

/// A blocked client reads its task's log from the pueued bzbd started that
/// task on, so an unreachable socket means the two are pointed at different
/// pueue configurations. Spawning a second daemon to fill the gap would answer
/// every log request from an empty queue: the task id would simply not be
/// there, and the client would report missing output instead of the real
/// misconfiguration. `connect` therefore never spawns.
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
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.unwrap();
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .unwrap();
    let spec = TaskSpec {
        command: "true".into(),
        cwd: std::env::current_dir().unwrap(),
        env: Default::default(),
        label: Some("smoke".into()),
        start_immediately: false,
    };
    // Fresh isolated daemon: first task always gets id 0.
    let id = enqueue(&mut client, spec).await.unwrap();
    let _ = id; // unwrap() above already proves the call succeeded
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn log_chunk_accumulates_across_polls() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.unwrap();
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .unwrap();
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

    // Poll up to 10s for the task to complete and the full output to appear.
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
    // Regression: pueue-lib's LogRequest returns task output compressed with
    // snappy's frame format. busybee must decompress before streaming. A
    // short literal output would survive unharmed inside a snappy frame, so
    // force back-references by printing a long repeated line.
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.unwrap();
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .unwrap();

    let line = "AudioFileFormat:Multiplier:createWriterForAudioFileFormat";
    let repeats = 200;
    let id = enqueue(
        &mut client,
        TaskSpec {
            command: format!("sh -c 'for i in $(seq 1 {repeats}); do echo {line}; done'"),
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

    // Snappy framing magic must NOT appear in a plaintext stream.
    assert!(
        !last.windows(6).any(|w| w == b"sNaPpY"),
        "output still contains snappy frame magic; first 32 bytes: {:x?}",
        &last[..last.len().min(32)]
    );
    // The decompressed bytes must match the command's output exactly.
    assert_eq!(
        String::from_utf8(last).expect("output is valid utf-8"),
        expected,
    );
}
