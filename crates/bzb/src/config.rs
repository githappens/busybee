//! `busybee config show` and `busybee config reload`.

use std::time::Duration;

use anyhow::{bail, Result};
use bzb_core::{
    config::Config,
    daemon::socket_path,
    protocol::{Request, Response},
};

use crate::status::ask_running;

const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// The effective configuration is this command's result, so it goes to stdout.
pub fn show() -> Result<()> {
    let config = Config::load()?;
    print!("{}", config.to_toml()?);
    Ok(())
}

/// No auto-start: a daemon that is not running has no configuration to replace.
pub async fn reload() -> Result<()> {
    match ask_running(Request::ConfigReload, REPLY_TIMEOUT).await? {
        None => bail!(
            "bzbd is not running on {}, so there is nothing to reload",
            socket_path()?.display()
        ),
        Some(Response::ConfigReloaded {
            pool_size,
            max_concurrent,
            drain_deadline_ms,
        }) => {
            eprintln!(
                "busybee: bzbd reloaded its config \
                 (pool_size {pool_size}, max_concurrent {max_concurrent}, \
                 drain_deadline_ms {drain_deadline_ms})"
            );
            Ok(())
        }
        Some(other) => bail!("expected a reload confirmation from bzbd, got {other:?}"),
    }
}
