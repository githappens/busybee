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
            Response::Error { message } => Err(BusybeeError::DaemonUnreachable {
                context: format!("bzbd rejected the handshake: {message}"),
            }),
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

/// Connects to `bzbd`, starting it if the socket is unreachable.
pub async fn connect_or_spawn_bzbd() -> Result<Connection, BusybeeError> {
    let socket = socket_path()?;
    if let Ok(conn) = Connection::connect(&socket).await {
        return Ok(conn);
    }

    spawn_bzbd()?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(conn) = Connection::connect(&socket).await {
            return Ok(conn);
        }
        if Instant::now() >= deadline {
            return Err(BusybeeError::DaemonUnreachable {
                context: format!(
                    "bzbd did not start listening on {} within 3 seconds of auto-spawn; \
                     see {}",
                    socket.display(),
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

    /// A stand-in daemon that completes the handshake, then writes `lines`
    /// verbatim and closes.
    async fn fake_daemon(socket: PathBuf, lines: Vec<String>) {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut incoming = BufReader::new(reader).lines();
            let hello: Hello =
                serde_json::from_str(&incoming.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(hello.hello, PROTOCOL_VERSION);
            let pong = Response::Pong {
                version: "0".into(),
                pid: 1,
            };
            writer
                .write_all(format!("{}\n", serde_json::to_string(&pong).unwrap()).as_bytes())
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
        fake_daemon(socket.clone(), vec![queued]).await;

        let mut conn = Connection::connect(&socket).await.unwrap();
        let mut events = conn.events();
        assert!(matches!(
            events.next().await.unwrap(),
            Some(LeaseEvent::Queued { id: 4, ahead: 1 })
        ));
        assert!(events.next().await.unwrap().is_none());
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
