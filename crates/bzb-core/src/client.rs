use std::{
    path::Path,
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use pueue_lib::message::{Request, Response};
use pueue_lib::network::socket::ConnectionSettings;
use pueue_lib::settings::Settings;
pub use pueue_lib::Client;
use tokio::time::sleep;

use crate::errors::BusybeeError;

/// The umask to restore in the pueued child before exec. bzbd records the
/// caller's umask here before restricting its own, so tasks run under the
/// user's mask, not the control-surface mask. Unset means no pre-exec
/// adjustment — the child inherits whatever mask is in effect at spawn time.
static SPAWN_UMASK: OnceLock<libc::mode_t> = OnceLock::new();

/// Record the umask that pueued should inherit when bzbd spawns it. Call once,
/// before `restrict_umask`, passing the value `restrict_umask` returns (the
/// previous mask). Subsequent calls are ignored; the mask is fixed for the
/// lifetime of the process.
pub fn set_spawn_umask(mask: libc::mode_t) {
    // Ignore a second set: bzbd calls this once, before any thread exists.
    let _ = SPAWN_UMASK.set(mask);
}

/// One request/response round trip. pueued's `Failure` becomes
/// [`BusybeeError::EnqueueRejected`]; every other response is the caller's.
pub async fn request(client: &mut Client, req: Request) -> Result<Response, BusybeeError> {
    let io = |e: pueue_lib::Error| BusybeeError::Other(format!("pueue-lib io: {e}"));
    client.send_request(req).await.map_err(io)?;
    match client.receive_response().await.map_err(io)? {
        Response::Failure(msg) => Err(BusybeeError::EnqueueRejected(msg)),
        other => Ok(other),
    }
}

/// Connects to a pueued that must already be running. For readers (streaming
/// a task bzbd started): a spawned replacement would show them an empty queue
/// instead of the real problem.
pub async fn connect() -> Result<Client, BusybeeError> {
    let (settings, socket_path) = settings()?;
    try_connect(&socket_path, &settings).await
}

/// Connects to pueued, spawning it in the background if the socket is
/// unreachable.
pub async fn connect_or_spawn() -> Result<Client, BusybeeError> {
    let (settings, socket_path) = settings()?;

    if let Ok(client) = try_connect(&socket_path, &settings).await {
        return Ok(client);
    }

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

fn settings() -> Result<(Settings, std::path::PathBuf), BusybeeError> {
    let (settings, _from_file) =
        Settings::read(&None).map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("failed to read pueue settings: {e}"),
        })?;
    let socket_path = settings
        .shared
        .unix_socket_path()
        .map_err(|e| BusybeeError::Other(format!("unix_socket_path: {e}")))?;
    Ok((settings, socket_path))
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
    use std::os::unix::process::CommandExt;

    let spawn_umask = SPAWN_UMASK.get().copied();
    let mut cmd = Command::new("pueued");
    cmd.arg("-d")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    if let Some(mask) = spawn_umask {
        // Restore the caller's umask in the child before exec, so pueued and
        // the tasks it runs are not subject to bzbd's control-surface mask.
        // SAFETY: `umask(2)` has no preconditions and is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    cmd.spawn().map_err(|e| BusybeeError::DaemonUnreachable {
        context: format!("pueued is not running and auto-start failed: {e}"),
    })?;
    Ok(())
}
