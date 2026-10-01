//! One signal to a running pueue task; the caller owns the SIGTERM → SIGKILL
//! escalation.

use pueue_lib::message::{KillRequest, Request, Response, Signal, TaskSelection};
use pueue_lib::Client;

use crate::client::request;
use crate::errors::BusybeeError;

/// A refusal (task unknown or already ended) is an error so the caller can
/// tell it apart from a delivered signal.
pub async fn kill(client: &mut Client, task_id: usize, signal: Signal) -> Result<(), BusybeeError> {
    let kill = Request::Kill(KillRequest {
        tasks: TaskSelection::TaskIds(vec![task_id]),
        signal: Some(signal),
    });
    match request(client, kill).await? {
        Response::Success(_) => Ok(()),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}
