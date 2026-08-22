//! Stopping a running pueue task.
//!
//! One signal per call, so the caller owns the escalation: busybee has always
//! sent SIGTERM first and SIGKILL only when the task ignored it. The client
//! does that across two Ctrl-Cs; bzbd does it across a grace period when a
//! lease's connection drops.

use pueue_lib::message::{KillRequest, Request, Response, Signal, TaskSelection};
use pueue_lib::Client;

use crate::errors::BusybeeError;

pub async fn kill(client: &mut Client, task_id: usize, signal: Signal) -> Result<(), BusybeeError> {
    client
        .send_request(Request::Kill(KillRequest {
            tasks: TaskSelection::TaskIds(vec![task_id]),
            signal: Some(signal),
        }))
        .await
        .map_err(io)?;
    // A refusal means the task is still out there — pueued not knowing it, or
    // it having ended already, are both things the caller has to be able to
    // tell apart from a delivered signal.
    match client.receive_response().await.map_err(io)? {
        Response::Success(_) => Ok(()),
        Response::Failure(msg) => Err(BusybeeError::EnqueueRejected(msg)),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

fn io(e: impl std::fmt::Display) -> BusybeeError {
    BusybeeError::Other(format!("pueue-lib io: {e}"))
}
