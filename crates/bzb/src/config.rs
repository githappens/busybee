//! `busybee config show` and `busybee config reload`.

use anyhow::{bail, Context, Result};
use bzb_core::{
    config::Config,
    daemon::{socket_path, Connection},
    protocol::{Request, Response},
};

/// Prints the effective configuration — the file's keys with every unset one
/// resolved to its default. This is the command's result, so it goes to
/// stdout: piping it into the config file is a valid thing to do with it.
pub fn show() -> Result<()> {
    let config = Config::load()?;
    print!("{}", config.to_toml()?);
    Ok(())
}

/// Asks the running daemon to re-read the file. No auto-start: a daemon that
/// is not running has no configuration to replace, and starting one here would
/// answer a question nobody asked.
pub async fn reload() -> Result<()> {
    let socket = socket_path()?;
    let mut conn = Connection::connect(&socket).await.with_context(|| {
        format!(
            "bzbd is not running on {}, so there is nothing to reload",
            socket.display()
        )
    })?;
    conn.send(Request::ConfigReload).await?;
    match conn.recv().await? {
        Response::ConfigReloaded {
            pool_size,
            max_concurrent,
            drain_deadline_ms,
        } => {
            eprintln!(
                "busybee: bzbd reloaded its config \
                 (pool_size {pool_size}, max_concurrent {max_concurrent}, \
                 drain_deadline_ms {drain_deadline_ms})"
            );
            Ok(())
        }
        Response::Error { message } => bail!("{message}"),
        other => bail!("expected a reload confirmation from bzbd, got {other:?}"),
    }
}
