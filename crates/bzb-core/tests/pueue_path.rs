//! Regression test for issue #99: a task whose command uses an external binary
//! must have PATH in its environment, or pueued's sh cannot find it on NixOS.

use bzb_core::{
    client::{self, Client},
    enqueue::{enqueue, TaskSpec},
    log::fetch_log_chunk,
};
use bzb_test_support::{path_env, PueuedFixture};

async fn connected() -> Option<(PueuedFixture, Client)> {
    let p = PueuedFixture::try_start()?;
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.expect("connect");
    bzb_core::group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");
    Some((p, client))
}

/// A task with PATH in its environment can run external commands.
///
/// `seq` is an external binary (not a shell built-in) available on both
/// Linux and macOS. Without PATH in the task env, pueued's sh cannot find it
/// on NixOS (compiled-in default is `/no-such-path`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn task_with_path_in_env_can_run_external_commands() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };

    let id = enqueue(
        &mut client,
        TaskSpec {
            command: "seq 1 5".into(),
            cwd: std::env::current_dir().unwrap(),
            env: path_env(),
            label: None,
            start_immediately: false,
        },
    )
    .await
    .unwrap();

    let expected = "1\n2\n3\n4\n5\n";
    let mut last = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (bytes, _) = fetch_log_chunk(&mut client, id, 0).await.unwrap();
        last = String::from_utf8_lossy(&bytes).into_owned();
        if last.len() >= expected.len() {
            break;
        }
    }
    assert_eq!(last, expected, "seq output should be 1..5, one per line");
}
