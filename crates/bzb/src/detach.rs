use anyhow::Result;
use bzb_core::{client, enqueue, enqueue::shell_escape_join, group};

pub async fn run(cmd: Vec<String>, name: Option<String>) -> Result<()> {
    let mut client = client::connect_or_spawn().await?;
    group::ensure_busybee_group(&mut client).await?;
    let spec = enqueue::TaskSpec::from_current_env(shell_escape_join(&cmd), name)?;
    let id = enqueue::enqueue(&mut client, spec).await?;
    println!("busybee: enqueued task {id}");
    Ok(())
}
