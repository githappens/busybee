//! `--detach` and the `cancel` subcommand that goes with it.
//!
//! A detached lease outlives the connection that asked for it, so nothing is
//! left holding it: Ctrl-C has no client to interrupt, and `busybee cancel
//! <id>` is the only way to end it early.

use anyhow::{bail, Result};
use bzb_core::{
    classify::Class,
    daemon::connect_or_spawn_bzbd,
    protocol::{LeaseEvent, Request, Response},
};

use crate::enqueue::lease_request;

pub async fn run(
    cmd: Vec<String>,
    name: Option<String>,
    class: Option<Class>,
    cores: Option<u32>,
) -> Result<()> {
    let mut conn = connect_or_spawn_bzbd().await?;
    conn.send(Request::Submit(lease_request(
        cmd, name, class, cores, true,
    )?))
    .await?;
    // Far enough to know the lease exists and what to cancel it by. Its pueue
    // task id only exists once it is admitted, which is not worth waiting for:
    // that is what `--detach` is asking not to do.
    loop {
        let line = match conn.events().next().await? {
            Some(LeaseEvent::Queued { id, .. }) => {
                format!("busybee: lease {id} detached (pueue task assigned once admitted)")
            }
            Some(LeaseEvent::Admitted {
                id, pueue_task_id, ..
            }) => format!("busybee: lease {id} detached (pueue task {pueue_task_id})"),
            // Loud: the lease is already over, so nothing was detached and
            // nothing is going to run.
            Some(LeaseEvent::Finished { id, exit_code }) => {
                bail!("lease {id} ended before it was queued (exit code {exit_code})")
            }
            Some(LeaseEvent::Notice { text }) => {
                eprintln!("busybee: note: {text}");
                continue;
            }
            None => bail!("bzbd closed the connection before the lease was queued"),
        };
        // The lease id is this command's result, and the result owns stdout.
        println!("{line}");
        return Ok(());
    }
}

pub async fn cancel(lease: u64) -> Result<()> {
    let mut conn = connect_or_spawn_bzbd().await?;
    conn.send(Request::Cancel { lease }).await?;
    match conn.recv().await? {
        Response::Ack => {
            eprintln!("busybee: cancelled lease {lease}");
            Ok(())
        }
        Response::Error { message } => bail!("cannot cancel lease {lease}: {message}"),
        other => bail!("unexpected response to a cancellation: {other:?}"),
    }
}
