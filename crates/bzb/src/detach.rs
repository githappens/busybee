//! `--detach` and `busybee cancel <id>`, the only way to end a detached lease.

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
    // Wait only for the lease id; the pueue task id needs admission.
    loop {
        let line = match conn.events().next().await? {
            Some(LeaseEvent::Queued { id, .. }) => {
                format!("busybee: lease {id} detached (pueue task assigned once admitted)")
            }
            Some(LeaseEvent::Admitted {
                id, pueue_task_id, ..
            }) => format!("busybee: lease {id} detached (pueue task {pueue_task_id})"),
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
