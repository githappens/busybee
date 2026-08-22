//! Everything bzbd says to pueued.
//!
//! bzbd is the only thing that submits to pueued now (`docs/design/bzbd.md`
//! §Components), so it holds the connection for its whole life. pueued may
//! die and be restarted underneath it: a request that fails drops the client,
//! and the next one reconnects — spawning pueued if it has to, exactly as the
//! client used to.

use bzb_core::{
    client,
    enqueue::{self, TaskSpec},
    errors::BusybeeError,
    group, kill,
};
use pueue_lib::{
    message::{Request, Response, Signal},
    state::State,
    Client,
};

/// The connection to pueued, re-established on demand.
#[derive(Default)]
pub struct Pueue {
    client: Option<Client>,
}

impl Pueue {
    /// Submits a task and returns the id pueued gave it.
    pub async fn add(&mut self, spec: TaskSpec) -> Result<usize, BusybeeError> {
        let result = match self.client().await {
            Ok(client) => enqueue::enqueue(client, spec).await,
            Err(err) => Err(err),
        };
        self.forget_on_error(&result);
        result
    }

    /// The whole task list. Polled once a second to spot tasks that ended.
    pub async fn status(&mut self) -> Result<State, BusybeeError> {
        let result = match self.client().await {
            Ok(client) => status(client).await,
            Err(err) => Err(err),
        };
        self.forget_on_error(&result);
        result
    }

    pub async fn kill(&mut self, task_id: usize, signal: Signal) -> Result<(), BusybeeError> {
        let result = match self.client().await {
            Ok(client) => kill::kill(client, task_id, signal).await,
            Err(err) => Err(err),
        };
        self.forget_on_error(&result);
        result
    }

    /// A connected client, spawning pueued and creating the group if this is
    /// the first request since a failure.
    async fn client(&mut self) -> Result<&mut Client, BusybeeError> {
        if self.client.is_none() {
            let mut client = client::connect_or_spawn().await?;
            // The group is where every busybee task lands, and a pueued that
            // just came up has no memory of it.
            group::ensure_busybee_group(&mut client).await?;
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("just connected"))
    }

    /// A failed request leaves the connection in an unknown state — a reply
    /// may still be in flight on it — so it is dropped rather than reused.
    fn forget_on_error<T>(&mut self, result: &Result<T, BusybeeError>) {
        if result.is_err() {
            self.client = None;
        }
    }
}

async fn status(client: &mut Client) -> Result<State, BusybeeError> {
    client
        .send_request(Request::Status)
        .await
        .map_err(|e| BusybeeError::Other(format!("pueue-lib io: {e}")))?;
    match client
        .receive_response()
        .await
        .map_err(|e| BusybeeError::Other(format!("pueue-lib io: {e}")))?
    {
        Response::Status(state) => Ok(*state),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}
