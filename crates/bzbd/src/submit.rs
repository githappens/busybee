//! Everything bzbd says to pueued (see `docs/design/bzbd.md` §Components). A
//! failed request drops the client; the next one reconnects, spawning pueued.

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

#[derive(Default)]
pub(crate) struct Pueue {
    client: Option<Client>,
}

impl Pueue {
    pub(crate) async fn add(&mut self, spec: TaskSpec) -> Result<usize, BusybeeError> {
        let result = enqueue::enqueue(self.client().await?, spec).await;
        self.forget_on_error(&result);
        result
    }

    pub(crate) async fn status(&mut self) -> Result<State, BusybeeError> {
        let result = status(self.client().await?).await;
        self.forget_on_error(&result);
        result
    }

    pub(crate) async fn kill(
        &mut self,
        task_id: usize,
        signal: Signal,
    ) -> Result<(), BusybeeError> {
        let result = kill::kill(self.client().await?, task_id, signal).await;
        self.forget_on_error(&result);
        result
    }

    /// Leaves `self.client` as `None` on error, so the next call retries.
    async fn client(&mut self) -> Result<&mut Client, BusybeeError> {
        if self.client.is_none() {
            let mut client = client::connect_or_spawn().await?;
            // A pueued that just came up has no memory of the group.
            group::ensure_busybee_group(&mut client).await?;
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("just connected"))
    }

    /// A reply may still be in flight on a failed connection, so it is not reused.
    fn forget_on_error<T>(&mut self, result: &Result<T, BusybeeError>) {
        if result.is_err() {
            self.client = None;
        }
    }
}

async fn status(client: &mut Client) -> Result<State, BusybeeError> {
    match client::request(client, Request::Status).await? {
        Response::Status(state) => Ok(*state),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}
