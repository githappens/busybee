use pueue_lib::message::{GroupRequest, ParallelRequest, Request, Response};
use pueue_lib::Client;

use crate::client::request;
use crate::errors::BusybeeError;

pub const BUSYBEE_GROUP: &str = "busybee";

/// Unlimited: bzbd decides what runs and submits with `start_immediately`
/// (spec §Components), so any limit would only hold back admitted tasks.
const PARALLEL_TASKS: usize = 0;

/// Ensure the `busybee` group exists at [`PARALLEL_TASKS`], re-enforcing it
/// if someone changed it (`pueue parallel -g busybee 4`). Idempotent.
pub async fn ensure_busybee_group(client: &mut Client) -> Result<(), BusybeeError> {
    let existing_parallel = match request(client, Request::Group(GroupRequest::List)).await? {
        Response::Group(groups) => groups.groups.get(BUSYBEE_GROUP).map(|g| g.parallel_tasks),
        other => return Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    };
    match existing_parallel {
        Some(PARALLEL_TASKS) => Ok(()),
        Some(_) => enforce_parallel(client).await,
        None => create_group(client).await,
    }
}

async fn create_group(client: &mut Client) -> Result<(), BusybeeError> {
    let add = Request::Group(GroupRequest::Add {
        name: BUSYBEE_GROUP.into(),
        parallel_tasks: Some(PARALLEL_TASKS),
    });
    match request(client, add).await {
        Ok(Response::Success(_)) => Ok(()),
        // Another busybee created it first; the next invocation re-enforces
        // the limit.
        Err(BusybeeError::EnqueueRejected(msg)) if msg.to_lowercase().contains("already") => Ok(()),
        Ok(other) => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
        Err(e) => Err(e),
    }
}

async fn enforce_parallel(client: &mut Client) -> Result<(), BusybeeError> {
    let parallel = Request::Parallel(ParallelRequest {
        parallel_tasks: PARALLEL_TASKS,
        group: BUSYBEE_GROUP.into(),
    });
    // A refusal leaves the group dispatching behind bzbd's back, so it is
    // reported, not dropped.
    match request(client, parallel).await? {
        Response::Success(_) => Ok(()),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}
