use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use pueue_lib::network::socket::ConnectionSettings;
use pueue_lib::settings::Settings;
use pueue_lib::Client;
use tokio::time::sleep;

use crate::errors::BusybeeError;

/// Connects to pueued, spawning it in the background if the socket is
/// unreachable. Returns a ready-to-use `Client` (handshake complete).
pub async fn connect_or_spawn() -> Result<Client, BusybeeError> {
    let (settings, _from_file) =
        Settings::read(&None).map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("failed to read pueue settings: {e}"),
        })?;

    let socket_path = settings
        .shared
        .unix_socket_path()
        .map_err(|e| BusybeeError::Other(format!("unix_socket_path: {e}")))?;

    // Try to connect first; if it works, we're done.
    if let Ok(client) = try_connect(&socket_path, &settings).await {
        return Ok(client);
    }

    // Not reachable — spawn pueued detached and retry for up to 3s.
    spawn_pueued()?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(client) = try_connect(&socket_path, &settings).await {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err(BusybeeError::DaemonUnreachable {
                context: "pueued did not become reachable within 3 seconds of auto-spawn".into(),
            });
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn try_connect(socket_path: &Path, settings: &Settings) -> Result<Client, BusybeeError> {
    let conn = ConnectionSettings::UnixSocket {
        path: socket_path.to_path_buf(),
    };
    let secret = std::fs::read(settings.shared.shared_secret_path()).map_err(|e| {
        BusybeeError::DaemonUnreachable {
            context: format!("cannot read pueued's shared_secret: {e}"),
        }
    })?;
    Client::new(conn, &secret, false)
        .await
        .map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("pueued handshake failed: {e}"),
        })
}

fn spawn_pueued() -> Result<(), BusybeeError> {
    // Spawn pueued in daemonize mode. It picks up PUEUE_CONFIG_PATH if set
    // (our fixture sets it; in real use the user's default config is fine).
    Command::new("pueued")
        .arg("-d")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("pueued is not running and auto-start failed: {e}"),
        })?;
    Ok(())
}
