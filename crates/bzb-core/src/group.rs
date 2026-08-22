use crate::errors::BusybeeError;
use pueue_lib::message::{GroupRequest, Request, Response};
use pueue_lib::Client;

pub const BUSYBEE_GROUP: &str = "busybee";

/// What the group's `parallel_tasks` is held at: unlimited. `docs/design/bzbd.md`
/// §Components bypasses pueue's dispatcher — bzbd decides what runs and submits
/// admitted tasks with `start_immediately` — so a limit here would only hold
/// back tasks bzbd has already admitted.
const PARALLEL_TASKS: usize = 0;

/// Ensure a `busybee` group exists with `parallel_tasks = 0`. Safe to call
/// on every invocation; idempotent. If the group exists but its parallel
/// limit has been manually changed (e.g. `pueue parallel -g busybee 4`),
/// busybee re-enforces [`PARALLEL_TASKS`].
pub async fn ensure_busybee_group(client: &mut Client) -> Result<(), BusybeeError> {
    client
        .send_request(Request::Group(GroupRequest::List))
        .await
        .map_err(io)?;
    let resp = client.receive_response().await.map_err(io)?;

    let existing_parallel = match &resp {
        Response::Group(group_resp) => group_resp
            .groups
            .get(BUSYBEE_GROUP)
            .map(|g| g.parallel_tasks),
        _ => None,
    };

    match existing_parallel {
        Some(PARALLEL_TASKS) => Ok(()),
        Some(_) => enforce_parallel(client).await,
        None => create_group(client).await,
    }
}

async fn create_group(client: &mut Client) -> Result<(), BusybeeError> {
    client
        .send_request(Request::Group(GroupRequest::Add {
            name: BUSYBEE_GROUP.into(),
            parallel_tasks: Some(PARALLEL_TASKS),
        }))
        .await
        .map_err(io)?;
    let resp = client.receive_response().await.map_err(io)?;
    match resp {
        Response::Success(_) => Ok(()),
        // Race: another busybee created it first. Treat as success; we'll
        // re-enforce parallel=1 on the next invocation.
        Response::Failure(msg) if msg.to_lowercase().contains("already") => Ok(()),
        Response::Failure(msg) => Err(BusybeeError::EnqueueRejected(msg)),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

async fn enforce_parallel(client: &mut Client) -> Result<(), BusybeeError> {
    client
        .send_request(Request::Parallel(pueue_lib::message::ParallelRequest {
            parallel_tasks: PARALLEL_TASKS,
            group: BUSYBEE_GROUP.into(),
        }))
        .await
        .map_err(io)?;
    // A refusal here leaves the group dispatching tasks on its own, behind
    // bzbd's back. Nothing downstream would notice, so it is reported rather
    // than dropped.
    match client.receive_response().await.map_err(io)? {
        Response::Success(_) => Ok(()),
        Response::Failure(msg) => Err(BusybeeError::EnqueueRejected(msg)),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

fn io(e: impl std::fmt::Display) -> BusybeeError {
    BusybeeError::Other(format!("pueue-lib io: {e}"))
}
