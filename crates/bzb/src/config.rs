//! `busybee config show` and `busybee config reload`.

use std::time::Duration;

use anyhow::{bail, Result};
use bzb_core::{
    config::Config,
    daemon::{socket_path, Connection},
    protocol::{Request, Response},
};

/// How long the daemon has to answer, once it has handshaken. The handshake's
/// own deadline ends at the pong, and this command has none of its own — a
/// daemon that wedges after it would hold `config reload` open for as long as
/// it stays wedged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

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
    // Only nothing-listening is "there is no daemon". A connection that was
    // accepted and then failed — a hang-up, a refused protocol version — is a
    // daemon that *is* running, and saying it is not would put a false
    // diagnosis on top of the real error.
    let Some(mut conn) = Connection::connect_if_listening(&socket).await? else {
        bail!(
            "bzbd is not running on {}, so there is nothing to reload",
            socket.display()
        );
    };

    let exchange = async {
        conn.send(Request::ConfigReload).await?;
        conn.recv().await
    };
    let response = match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
        Ok(result) => result?,
        Err(_) => bail!(
            "bzbd took the reload request but did not answer within {} seconds",
            REPLY_TIMEOUT.as_secs()
        ),
    };
    match response {
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
