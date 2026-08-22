//! Client side of the `bzbd` protocol: where the daemon's files live, how to
//! reach it, and how to auto-start it.

use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
    time::sleep,
};

use crate::errors::BusybeeError;
use crate::protocol::{Hello, LeaseEvent, Request, Response, PROTOCOL_VERSION};

/// Directory holding `bzbd.sock`, `bzbd.pid` and `bzbd.log`.
pub fn state_dir() -> Result<PathBuf, BusybeeError> {
    if let Some(dir) = env::var_os("BUSYBEE_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("busybee"));
    }
    let home = env::var_os("HOME").ok_or_else(|| {
        BusybeeError::Other(
            "cannot locate the busybee state directory: none of BUSYBEE_STATE_DIR, \
             XDG_STATE_HOME or HOME is set"
                .into(),
        )
    })?;
    Ok(PathBuf::from(home).join(".local/state/busybee"))
}

pub fn socket_path() -> Result<PathBuf, BusybeeError> {
    Ok(state_dir()?.join("bzbd.sock"))
}

pub fn pid_path() -> Result<PathBuf, BusybeeError> {
    Ok(state_dir()?.join("bzbd.pid"))
}

pub fn log_path() -> Result<PathBuf, BusybeeError> {
    Ok(state_dir()?.join("bzbd.log"))
}

/// An open, handshaken connection to `bzbd`.
pub struct Connection {
    incoming: Lines<BufReader<OwnedReadHalf>>,
    outgoing: OwnedWriteHalf,
}

impl Connection {
    /// Connects to an already-running daemon and completes the handshake.
    pub async fn connect(socket: &Path) -> Result<Self, BusybeeError> {
        let stream =
            UnixStream::connect(socket)
                .await
                .map_err(|e| BusybeeError::DaemonUnreachable {
                    context: format!("cannot connect to bzbd at {}: {e}", socket.display()),
                })?;
        let (reader, outgoing) = stream.into_split();
        let mut conn = Self {
            incoming: BufReader::new(reader).lines(),
            outgoing,
        };
        conn.write_json(&Hello {
            hello: PROTOCOL_VERSION,
        })
        .await?;
        match conn.recv().await? {
            Response::Pong { .. } => Ok(conn),
            // Reachable but incompatible: a protocol error, not an absent
            // daemon, so callers do not try to spawn a replacement.
            Response::Error { message } => Err(BusybeeError::Protocol(format!(
                "bzbd rejected protocol version {PROTOCOL_VERSION}: {message}"
            ))),
            other => Err(BusybeeError::Protocol(format!(
                "expected a pong after the handshake, got {other:?}"
            ))),
        }
    }

    pub async fn send(&mut self, request: Request) -> Result<(), BusybeeError> {
        self.write_json(&request).await
    }

    pub async fn recv(&mut self) -> Result<Response, BusybeeError> {
        self.recv_opt()
            .await?
            .ok_or_else(|| BusybeeError::DaemonUnreachable {
                context: "bzbd closed the connection".into(),
            })
    }

    /// Reader over the events the daemon streams on a `Submit` connection.
    pub fn events(&mut self) -> Events<'_> {
        Events { conn: self }
    }

    async fn write_json(&mut self, value: &impl serde::Serialize) -> Result<(), BusybeeError> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| BusybeeError::Protocol(format!("cannot encode a message: {e}")))?;
        line.push('\n');
        self.outgoing.write_all(line.as_bytes()).await?;
        Ok(())
    }

    async fn recv_opt(&mut self) -> Result<Option<Response>, BusybeeError> {
        let Some(line) = self.incoming.next_line().await? else {
            return Ok(None);
        };
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| BusybeeError::Protocol(format!("cannot decode {line:?}: {e}")))
    }
}

pub struct Events<'a> {
    conn: &'a mut Connection,
}

impl Events<'_> {
    /// The next lease event, or `None` once the daemon closes the connection.
    pub async fn next(&mut self) -> Result<Option<LeaseEvent>, BusybeeError> {
        match self.conn.recv_opt().await? {
            None => Ok(None),
            Some(Response::Event(event)) => Ok(Some(event)),
            Some(Response::Error { message }) => Err(BusybeeError::EnqueueRejected(message)),
            Some(other) => Err(BusybeeError::Protocol(format!(
                "expected an event, got {other:?}"
            ))),
        }
    }
}

/// How long `connect_or_spawn_bzbd` spends reaching a daemon, spawn included.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Connects and handshakes, giving up at `deadline`. A daemon that accepts the
/// connection but never answers would otherwise block the caller forever.
async fn connect_by(socket: &Path, deadline: Instant) -> Result<Connection, BusybeeError> {
    let budget = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(budget, Connection::connect(socket)).await {
        Ok(result) => result,
        Err(_) => Err(BusybeeError::DaemonUnreachable {
            context: format!(
                "bzbd at {} did not complete the handshake within {} seconds",
                socket.display(),
                STARTUP_TIMEOUT.as_secs()
            ),
        }),
    }
}

/// Connects to `bzbd`, starting it if the socket is unreachable.
pub async fn connect_or_spawn_bzbd() -> Result<Connection, BusybeeError> {
    let socket = socket_path()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut spawned = false;
    loop {
        let last = match connect_by(&socket, deadline).await {
            Ok(conn) => return Ok(conn),
            // The daemon answered and refused us; a second one would exit as
            // "already running" and we would lose the reason.
            Err(refusal @ BusybeeError::Protocol(_)) => return Err(refusal),
            Err(other) => other,
        };
        if !spawned {
            spawn_bzbd()?;
            spawned = true;
            continue;
        }
        if Instant::now() >= deadline {
            return Err(BusybeeError::DaemonUnreachable {
                context: format!(
                    "bzbd did not start listening on {} within {} seconds of auto-spawn \
                     ({last}); see {}",
                    socket.display(),
                    STARTUP_TIMEOUT.as_secs(),
                    log_path()?.display()
                ),
            });
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Starts `bzbd` in daemonize mode. Prefers the binary next to the running
/// executable so a locally built client starts its matching daemon, and falls
/// back to PATH. The daemonizing parent exits as soon as it has forked, so
/// waiting for it costs nothing and surfaces its startup errors.
fn spawn_bzbd() -> Result<(), BusybeeError> {
    let neighbour = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("bzbd")))
        .filter(|path| path.is_file());
    let program = neighbour.unwrap_or_else(|| PathBuf::from("bzbd"));

    let status = Command::new(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("cannot spawn {}: {e}", program.display()),
        })?;
    if !status.success() {
        return Err(BusybeeError::DaemonUnreachable {
            context: format!(
                "{} exited with {status}; see {}",
                program.display(),
                log_path()?.display()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Hello, PROTOCOL_VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A stand-in daemon that answers the handshake with `handshake`, then
    /// writes `lines` verbatim and closes.
    async fn fake_daemon(socket: PathBuf, handshake: Response, lines: Vec<String>) {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut incoming = BufReader::new(reader).lines();
            let hello: Hello =
                serde_json::from_str(&incoming.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(hello.hello, PROTOCOL_VERSION);
            writer
                .write_all(format!("{}\n", serde_json::to_string(&handshake).unwrap()).as_bytes())
                .await
                .unwrap();
            for line in lines {
                writer
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
    }

    #[tokio::test]
    async fn state_dir_prefers_the_override_then_xdg() {
        temp_env(&[("BUSYBEE_STATE_DIR", Some("/tmp/override"))], || {
            assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/override"));
        });
        temp_env(
            &[
                ("BUSYBEE_STATE_DIR", None),
                ("XDG_STATE_HOME", Some("/tmp/xdg")),
            ],
            || {
                assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/xdg/busybee"));
            },
        );
    }

    #[tokio::test]
    async fn events_yields_streamed_events_until_the_daemon_closes() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("events.sock");
        let queued =
            serde_json::to_string(&Response::Event(LeaseEvent::Queued { id: 4, ahead: 1 }))
                .unwrap();
        fake_daemon(
            socket.clone(),
            Response::Pong {
                version: "0".into(),
                pid: 1,
            },
            vec![queued],
        )
        .await;

        let mut conn = Connection::connect(&socket).await.unwrap();
        let mut events = conn.events();
        assert!(matches!(
            events.next().await.unwrap(),
            Some(LeaseEvent::Queued { id: 4, ahead: 1 })
        ));
        assert!(events.next().await.unwrap().is_none());
    }

    /// A daemon that answered and refused our version is reachable: reporting
    /// it as unreachable would send `connect_or_spawn_bzbd` off to spawn a
    /// replacement that immediately exits as "already running", hiding the
    /// real reason behind a startup timeout.
    #[tokio::test]
    async fn a_refused_handshake_is_a_protocol_error_not_an_unreachable_daemon() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("refuse.sock");
        fake_daemon(
            socket.clone(),
            Response::Error {
                message: "unsupported protocol version 99".into(),
            },
            vec![],
        )
        .await;

        let Err(BusybeeError::Protocol(message)) = Connection::connect(&socket).await else {
            panic!("expected the refusal to surface as a protocol error");
        };
        assert!(
            message.contains("unsupported protocol version 99"),
            "{message}"
        );
    }

    /// A daemon that accepts connections but never answers must not hang the
    /// client past its startup deadline.
    #[tokio::test]
    async fn a_stalled_daemon_fails_the_handshake_at_the_deadline() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("stall.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let _accepted = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let deadline = Instant::now() + Duration::from_millis(200);
        let Err(BusybeeError::DaemonUnreachable { .. }) = connect_by(&socket, deadline).await
        else {
            panic!("expected the stalled handshake to fail at the deadline");
        };
        assert!(Instant::now() < deadline + Duration::from_secs(1));
    }

    /// Sets environment variables around `body`, restoring them afterwards.
    fn temp_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (key, value) in vars {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        body();
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
