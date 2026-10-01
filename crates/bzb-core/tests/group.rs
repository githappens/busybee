//! `ensure_busybee_group` against a real, isolated `pueued`; the group sits at
//! `parallel_tasks = 0` (spec §Components).

use bzb_core::{client, group};
use bzb_test_support::PueuedFixture;
use pueue_lib::{
    message::{GroupRequest, ParallelRequest, Request, Response},
    Client,
};

/// A client to a fresh isolated pueued, or `None` (skip) without pueued.
/// `PUEUE_CONFIG_PATH` is process-wide, hence `serial` on every caller.
async fn connected() -> Option<(PueuedFixture, Client)> {
    let p = PueuedFixture::try_start()?;
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let client = client::connect_or_spawn().await.expect("connect");
    Some((p, client))
}

async fn parallel_tasks(client: &mut Client) -> usize {
    client
        .send_request(Request::Group(GroupRequest::List))
        .await
        .expect("send a group list request");
    match client
        .receive_response()
        .await
        .expect("group list response")
    {
        Response::Group(groups) => {
            groups
                .groups
                .get(group::BUSYBEE_GROUP)
                .unwrap_or_else(|| panic!("the {} group is missing", group::BUSYBEE_GROUP))
                .parallel_tasks
        }
        other => panic!("expected a group listing, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_group_is_created_with_pueue_scheduling_disabled() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };

    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");

    assert_eq!(parallel_tasks(&mut client).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn ensure_group_is_idempotent() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };
    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");
    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group again");
}

/// A hand-raised limit would let pueue dispatch behind bzbd's back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_hand_set_parallel_limit_is_re_enforced() {
    let Some((_p, mut client)) = connected().await else {
        return;
    };
    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");

    client
        .send_request(Request::Parallel(ParallelRequest {
            parallel_tasks: 4,
            group: group::BUSYBEE_GROUP.into(),
        }))
        .await
        .expect("send a parallel request");
    client.receive_response().await.expect("parallel response");
    assert_eq!(parallel_tasks(&mut client).await, 4, "fixture check");

    group::ensure_busybee_group(&mut client)
        .await
        .expect("re-enforce the group");

    assert_eq!(parallel_tasks(&mut client).await, 0);
}
