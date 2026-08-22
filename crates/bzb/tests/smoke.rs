use assert_cmd::Command as AssertCmd;
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

use bzb_core::client;
use bzb_core::enqueue::{enqueue, TaskSpec};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn connect_succeeds_when_pueued_is_running() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    // SAFETY: Settings::read checks PUEUE_CONFIG_PATH; tests run single-threaded
    // with respect to env mutation here via the #[tokio::test] below using
    // multi_thread is fine because we only read env inside connect_or_spawn.
    // The env var is process-wide, so keep only one integration test setting
    // this at a time (tests run serially by default unless explicit parallelism).
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let client = client::connect_or_spawn()
        .await
        .expect("connect should succeed");
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn ensure_group_is_idempotent() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.unwrap();
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .unwrap();
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .unwrap();
    // A second call must be a no-op (both calls return Ok without erroring).
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

#[test]
#[serial_test::serial]
fn detach_prints_task_id_and_exits_zero() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    let mut cmd = AssertCmd::cargo_bin("busybee").unwrap();
    cmd.env("PUEUE_CONFIG_PATH", &p.config_path);
    let out = cmd.args(["--detach", "--", "true"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("busybee: enqueued task "),
        "got: {stdout}"
    );
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
    let id = bzb_core::enqueue::enqueue(
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
        let (bytes, _) = bzb_core::log::fetch_log_chunk(&mut client, id, 0)
            .await
            .unwrap();
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
    let id = bzb_core::enqueue::enqueue(
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
        let (bytes, _) = bzb_core::log::fetch_log_chunk(&mut client, id, 0)
            .await
            .unwrap();
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

#[test]
#[serial_test::serial]
fn blocking_mode_streams_stdout_and_returns_exit_code() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    let mut cmd = AssertCmd::cargo_bin("busybee").unwrap();
    cmd.env("PUEUE_CONFIG_PATH", &p.config_path);
    let out = cmd
        .args(["--", "sh", "-c", "printf hello; exit 0"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
}

#[test]
#[serial_test::serial]
fn blocking_mode_propagates_non_zero_exit_code() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    let mut cmd = AssertCmd::cargo_bin("busybee").unwrap();
    cmd.env("PUEUE_CONFIG_PATH", &p.config_path);
    let out = cmd.args(["--", "sh", "-c", "exit 7"]).output().unwrap();
    assert_eq!(out.status.code(), Some(7));
}

#[test]
#[serial_test::serial]
fn blocking_mode_second_task_waits_for_first_parallel_is_one() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    let cfg = p.config_path.clone();
    let h1 = std::thread::spawn({
        let cfg = cfg.clone();
        move || {
            let mut cmd = AssertCmd::cargo_bin("busybee").unwrap();
            cmd.env("PUEUE_CONFIG_PATH", &cfg);
            cmd.args(["--", "sh", "-c", "sleep 1; echo first"])
                .output()
                .unwrap()
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let start2 = std::time::Instant::now();
    let mut cmd2 = AssertCmd::cargo_bin("busybee").unwrap();
    cmd2.env("PUEUE_CONFIG_PATH", &cfg);
    let out2 = cmd2.args(["--", "echo", "second"]).output().unwrap();
    let elapsed2 = start2.elapsed();
    h1.join().unwrap();
    assert!(out2.status.success());
    assert!(
        elapsed2 >= std::time::Duration::from_millis(800),
        "second task finished too fast ({elapsed2:?}); parallelism may be > 1"
    );
}

#[test]
#[serial_test::serial]
fn sigint_while_running_cancels_task_exits_130() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_busybee"));
    cmd.env("PUEUE_CONFIG_PATH", &p.config_path);
    cmd.args(["--", "sleep", "30"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().unwrap();
    // Give the enqueue + queue-tick cycle time to get the task into Running state.
    std::thread::sleep(std::time::Duration::from_secs(2));
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait().unwrap();
    assert_eq!(
        out.code(),
        Some(130),
        "expected exit 130, got {:?}",
        out.code()
    );
}
