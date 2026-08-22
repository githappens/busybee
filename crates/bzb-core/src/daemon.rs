//! Client side of the `bzbd` protocol: where the daemon's files live, how to
//! reach it, and how to auto-start it.

use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
    process::Command,
    time::sleep,
};

use crate::errors::BusybeeError;
use crate::protocol::{read_line, Hello, LeaseEvent, Line, Request, Response, PROTOCOL_VERSION};

/// Directory holding `bzbd.sock`, `bzbd.pid` and `bzbd.log`.
pub fn state_dir() -> Result<PathBuf, BusybeeError> {
    if let Some(dir) = env::var_os("BUSYBEE_STATE_DIR") {
        if dir.is_empty() {
            return Err(BusybeeError::Other(
                "BUSYBEE_STATE_DIR is set but empty; unset it or give it a directory".into(),
            ));
        }
        return Ok(PathBuf::from(dir));
    }
    // The XDG spec says a value that is empty or not absolute counts as unset.
    // Resolving one relative to the working directory would give each caller
    // its own socket, and each of those its own daemon.
    if let Some(dir) = env::var_os("XDG_STATE_HOME").filter(|d| Path::new(d).is_absolute()) {
        return Ok(PathBuf::from(dir).join("busybee"));
    }
    // HOME has to be absolute for the same reason: a relative one would put
    // `.local/state/busybee` under whatever directory the caller ran from.
    let home = env::var_os("HOME")
        .filter(|d| Path::new(d).is_absolute())
        .ok_or_else(|| {
            BusybeeError::Other(
                "cannot locate the busybee state directory: BUSYBEE_STATE_DIR is unset and \
                 neither XDG_STATE_HOME nor HOME holds an absolute path"
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
    incoming: BufReader<OwnedReadHalf>,
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
            incoming: BufReader::new(reader),
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
        Events {
            conn: self,
            finished: false,
        }
    }

    async fn write_json(&mut self, value: &impl serde::Serialize) -> Result<(), BusybeeError> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| BusybeeError::Protocol(format!("cannot encode a message: {e}")))?;
        line.push('\n');
        // Not `?`: the shared `Network` variant reads "pueue-lib I/O error",
        // and this socket has nothing to do with pueued.
        self.outgoing
            .write_all(line.as_bytes())
            .await
            .map_err(|e| BusybeeError::DaemonUnreachable {
                context: format!("cannot send to bzbd: {e}"),
            })?;
        Ok(())
    }

    async fn recv_opt(&mut self) -> Result<Option<Response>, BusybeeError> {
        let read =
            read_line(&mut self.incoming)
                .await
                .map_err(|e| BusybeeError::DaemonUnreachable {
                    context: format!("cannot read from bzbd: {e}"),
                })?;
        let line = match read {
            Line::Text(line) => line,
            Line::Closed => return Ok(None),
            // The same framing the daemon reads under: a peer that breaks it is
            // not speaking this protocol, and we cannot find the next message.
            Line::Malformed(reason) => {
                return Err(BusybeeError::Protocol(format!(
                    "bzbd broke the protocol framing: {reason}"
                )))
            }
        };
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| BusybeeError::Protocol(format!("cannot decode {line:?}: {e}")))
    }
}

pub struct Events<'a> {
    conn: &'a mut Connection,
    finished: bool,
}

impl Events<'_> {
    /// The next lease event, or `None` once the stream ends. A stream that
    /// ends before `Finished` lost the lease's exit code with it, so that is
    /// an error rather than a normal end.
    pub async fn next(&mut self) -> Result<Option<LeaseEvent>, BusybeeError> {
        match self.conn.recv_opt().await? {
            None if self.finished => Ok(None),
            None => Err(BusybeeError::DaemonUnreachable {
                context: "bzbd closed the connection before the lease finished".into(),
            }),
            Some(Response::Event(event)) => {
                self.finished |= matches!(event, LeaseEvent::Finished { .. });
                Ok(Some(event))
            }
            // Not `EnqueueRejected`: that one reads "pueued rejected our
            // request", and this refusal came from bzbd.
            Some(Response::Error { message }) => Err(BusybeeError::Rejected(message)),
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
            spawn_bzbd(&bzbd_program(), deadline).await?;
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

/// The daemon binary to start: the one next to the running executable, so a
/// locally built client starts its matching daemon, else whatever is on PATH.
fn bzbd_program() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("bzbd")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("bzbd"))
}

/// Starts `bzbd` in daemonize mode. The daemonizing parent exits once its
/// child is serving or has failed, so waiting for it both paces the retry and
/// surfaces startup errors; a child that never reports is bounded by
/// `deadline`.
async fn spawn_bzbd(program: &Path, deadline: Instant) -> Result<(), BusybeeError> {
    let child = Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Timing out below drops the handle, which on its own neither kills
        // nor reaps the process; without this a daemon that hangs before
        // reporting outlives every client that tried to start it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BusybeeError::DaemonUnreachable {
            context: format!("cannot spawn {}: {e}", program.display()),
        })?;

    let budget = deadline.saturating_duration_since(Instant::now());
    let Ok(output) = tokio::time::timeout(budget, child.wait_with_output()).await else {
        return Err(BusybeeError::DaemonUnreachable {
            context: format!(
                "{} did not report that it is serving within {} seconds; see {}",
                program.display(),
                STARTUP_TIMEOUT.as_secs(),
                log_path()?.display()
            ),
        });
    };
    let output = output.map_err(|e| BusybeeError::DaemonUnreachable {
        context: format!("cannot wait for {}: {e}", program.display()),
    })?;
    if !output.status.success() {
        return Err(BusybeeError::DaemonUnreachable {
            context: format!(
                "{} exited with {}: {}",
                program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Hello, MAX_LINE_BYTES, PROTOCOL_VERSION};
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

    /// All of the state-directory rules in one test: the environment is
    /// process-wide, so splitting them would race across test threads.
    #[tokio::test]
    async fn state_dir_resolves_the_environment_in_order() {
        temp_env(&[("BUSYBEE_STATE_DIR", Some("/tmp/override"))], || {
            assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/override"));
        });

        // An empty override is a misconfiguration, not a request for the
        // default: falling back silently would hide it.
        temp_env(&[("BUSYBEE_STATE_DIR", Some(""))], || {
            let err = state_dir().unwrap_err().to_string();
            assert!(err.contains("BUSYBEE_STATE_DIR"), "message was {err:?}");
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

        // An empty or relative XDG value counts as unset. Honouring a relative
        // one would put the socket in a different place per working directory,
        // and each of those would auto-start its own daemon.
        for value in ["", "relative/state"] {
            temp_env(
                &[
                    ("BUSYBEE_STATE_DIR", None),
                    ("XDG_STATE_HOME", Some(value)),
                    ("HOME", Some("/tmp/home")),
                ],
                || {
                    assert_eq!(
                        state_dir().unwrap(),
                        PathBuf::from("/tmp/home/.local/state/busybee"),
                        "XDG_STATE_HOME={value:?}"
                    );
                },
            );
        }

        // The home-relative default needs an absolute HOME for the same
        // reason: `.local/state/busybee` would resolve per working directory.
        for home in [None, Some(""), Some("relative/home")] {
            temp_env(
                &[
                    ("BUSYBEE_STATE_DIR", None),
                    ("XDG_STATE_HOME", None),
                    ("HOME", home),
                ],
                || {
                    let err = state_dir().unwrap_err().to_string();
                    assert!(err.contains("HOME"), "HOME={home:?} message was {err:?}");
                },
            );
        }
    }

    fn event_line(event: LeaseEvent) -> String {
        serde_json::to_string(&Response::Event(event)).unwrap()
    }

    fn pong() -> Response {
        Response::Pong {
            version: "0".into(),
            pid: 1,
        }
    }

    #[tokio::test]
    async fn events_yields_streamed_events_until_the_lease_finishes() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("events.sock");
        fake_daemon(
            socket.clone(),
            pong(),
            vec![
                event_line(LeaseEvent::Queued { id: 4, ahead: 1 }),
                event_line(LeaseEvent::Finished {
                    id: 4,
                    exit_code: 0,
                }),
            ],
        )
        .await;

        let mut conn = Connection::connect(&socket).await.unwrap();
        let mut events = conn.events();
        assert!(matches!(
            events.next().await.unwrap(),
            Some(LeaseEvent::Queued { id: 4, ahead: 1 })
        ));
        assert!(matches!(
            events.next().await.unwrap(),
            Some(LeaseEvent::Finished {
                id: 4,
                exit_code: 0
            })
        ));
        assert!(events.next().await.unwrap().is_none());
    }

    /// `Finished` carries the exit code, so a stream that stops before it lost
    /// the lease. Reporting that as a normal end of stream would let a
    /// `while let Some(..)` consumer exit as if the command had succeeded.
    #[tokio::test]
    async fn events_report_a_stream_that_ends_before_the_lease_finishes() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("truncated.sock");
        fake_daemon(
            socket.clone(),
            pong(),
            vec![event_line(LeaseEvent::Queued { id: 7, ahead: 0 })],
        )
        .await;

        let mut conn = Connection::connect(&socket).await.unwrap();
        let mut events = conn.events();
        assert!(events.next().await.unwrap().is_some());
        let Err(BusybeeError::DaemonUnreachable { context }) = events.next().await else {
            panic!("expected a truncated stream to be an error");
        };
        assert!(context.contains("finish"), "{context}");
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

    /// A transport failure on a bzbd socket must name bzbd. The shared
    /// `Network` variant reads "pueue-lib I/O error", which points the user at
    /// a daemon that had nothing to do with it.
    #[tokio::test]
    async fn a_broken_bzbd_connection_names_bzbd() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let (reader, outgoing) = ours.into_split();
        let mut conn = Connection {
            incoming: BufReader::new(reader),
            outgoing,
        };

        // The first write can still land in the socket buffer; the second
        // cannot, the peer's end of the pair is gone.
        let _ = conn.send(Request::Ping).await;
        let error = conn.send(Request::Ping).await;
        let Err(BusybeeError::DaemonUnreachable { context }) = error else {
            panic!("expected a write to a closed bzbd socket to name bzbd, got {error:?}");
        };
        assert!(context.contains("bzbd"), "{context}");
    }

    /// The daemon caps what it reads at [`MAX_LINE_BYTES`]; a client that does
    /// not cap what it reads back is the same unbounded buffer with the roles
    /// swapped, so a stale or buggy daemon could hang or exhaust it.
    #[tokio::test]
    async fn an_oversized_response_line_is_rejected_instead_of_buffered() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("flood.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_reader, mut writer) = stream.into_split();
            // A pong to get past the handshake, then a line that never ends.
            writer
                .write_all(format!("{}\n", serde_json::to_string(&pong()).unwrap()).as_bytes())
                .await
                .unwrap();
            let _ = writer.write_all(&vec![b'x'; MAX_LINE_BYTES + 1]).await;
            std::future::pending::<()>().await;
        });

        let mut conn = Connection::connect(&socket).await.unwrap();
        conn.send(Request::Ping).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(3), conn.recv())
            .await
            .expect("the client buffered the line instead of rejecting it");
        let Err(BusybeeError::Protocol(message)) = reply else {
            panic!("expected an oversized response line to be a protocol error, got {reply:?}");
        };
        assert!(
            message.contains(&MAX_LINE_BYTES.to_string()),
            "message was {message:?}"
        );
    }

    /// A rejection that came from bzbd has to name bzbd: `EnqueueRejected`
    /// reads "pueued rejected our request", which sends the user to the logs of
    /// a daemon that was never involved.
    #[tokio::test]
    async fn a_rejected_lease_names_bzbd() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("rejected.sock");
        fake_daemon(
            socket.clone(),
            pong(),
            vec![serde_json::to_string(&Response::Error {
                message: "not implemented".into(),
            })
            .unwrap()],
        )
        .await;

        let mut conn = Connection::connect(&socket).await.unwrap();
        let error = conn.events().next().await;
        let Err(BusybeeError::Rejected(message)) = &error else {
            panic!("expected a bzbd rejection, got {error:?}");
        };
        assert_eq!(message, "not implemented");
        assert!(
            error.unwrap_err().to_string().contains("bzbd"),
            "the rejection has to name the daemon that refused"
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

    /// A hung daemon must not survive the client that gave up on it. Timing
    /// out only drops the `Child` handle, and a dropped handle neither kills
    /// nor reaps the process on its own, so the forking parent would linger
    /// while later clients time out against it in turn.
    #[tokio::test]
    async fn a_daemon_that_never_reports_is_killed_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let program = dir.path().join("bzbd");
        // Touched only if the process outlives the deadline below.
        let survived = dir.path().join("survived");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nsleep 1\ntouch '{}'\n", survived.display()),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let deadline = Instant::now() + Duration::from_millis(200);
        let Err(BusybeeError::DaemonUnreachable { context }) = spawn_bzbd(&program, deadline).await
        else {
            panic!("expected a daemon that never reports to fail at the deadline");
        };
        assert!(context.contains("did not report"), "{context}");

        sleep(Duration::from_millis(1500)).await;
        assert!(!survived.exists(), "the timed-out daemon was left running");
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
