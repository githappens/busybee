//! `ensure_busybee_group` against a real, isolated `pueued`.
//!
//! `docs/design/bzbd.md` §Components puts the group at `parallel_tasks = 0`:
//! pueue's dispatcher is bypassed and bzbd decides what runs, so the group must
//! never hold a task back on its own.

use bzb_core::{client, group};
use bzb_test_support::PueuedFixture;
use pueue_lib::{
    message::{GroupRequest, ParallelRequest, Request, Response},
    Client,
};

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
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.expect("connect");

    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");

    assert_eq!(parallel_tasks(&mut client).await, 0);
}

/// Every invocation re-enforces the group, so creating one that already exists
/// has to be a no-op rather than an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn ensure_group_is_idempotent() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.expect("connect");
    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group");
    group::ensure_busybee_group(&mut client)
        .await
        .expect("create the group again");
}

/// bzbd admits tasks itself and submits them with `start_immediately`. A limit
/// someone raised by hand (`pueue parallel -g busybee 4`) would leave pueue
/// dispatching queued tasks behind bzbd's back, so it is put back on every
/// invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_hand_set_parallel_limit_is_re_enforced() {
    let Some(p) = PueuedFixture::try_start() else {
        return;
    };
    std::env::set_var("PUEUE_CONFIG_PATH", &p.config_path);
    let mut client = client::connect_or_spawn().await.expect("connect");
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
