//! Client side of the `bzbd` protocol: where the daemon's files live, how to
//! reach it, and how to auto-start it.

use std::{
    env,
    io::ErrorKind,
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
use crate::protocol::{
    read_line, Hello, LeaseEvent, Line, Request, Response, MAX_LINE_BYTES, PROTOCOL_VERSION,
};

/// Directory holding `bzbd.sock`, `bzbd.pid` and `bzbd.log`:
/// `$BUSYBEE_STATE_DIR`, else `$XDG_STATE_HOME/busybee`, else
/// `~/.local/state/busybee`.
pub fn state_dir() -> Result<PathBuf, BusybeeError> {
    crate::config::locate(
        "state directory",
        ("BUSYBEE_STATE_DIR", env::var_os("BUSYBEE_STATE_DIR")),
        ("XDG_STATE_HOME", env::var_os("XDG_STATE_HOME")),
        env::var_os("HOME"),
        ".local/state",
        None,
    )
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

/// The leases bzbd holds, rewritten on every change and read on restart.
pub fn leases_path() -> Result<PathBuf, BusybeeError> {
    Ok(state_dir()?.join("leases.json"))
}

async fn open(socket: &Path) -> Result<UnixStream, BusybeeError> {
    UnixStream::connect(socket)
        .await
        .map_err(|e| unreachable_at(socket, e))
}

fn unreachable_at(socket: &Path, error: std::io::Error) -> BusybeeError {
    BusybeeError::DaemonUnreachable {
        context: format!("cannot connect to bzbd at {}: {error}", socket.display()),
    }
}

/// An open, handshaken connection to `bzbd`.
pub struct Connection {
    incoming: BufReader<OwnedReadHalf>,
    outgoing: OwnedWriteHalf,
}

impl Connection {
    /// Connects to an already-running daemon and completes the handshake.
    pub async fn connect(socket: &Path) -> Result<Self, BusybeeError> {
        Self::handshake(open(socket).await?).await
    }

    /// Connects only to a daemon that is already listening. `Ok(None)` means
    /// the socket is absent or stale: the one failure that means "no daemon".
    /// Anything after the connection was accepted is a running daemon not
    /// answering, so it propagates. Bounded by [`STARTUP_TIMEOUT`], since a
    /// wedged daemon accepts and then says nothing.
    pub async fn connect_if_listening(socket: &Path) -> Result<Option<Self>, BusybeeError> {
        let attempt = async {
            match UnixStream::connect(socket).await {
                Ok(stream) => Self::handshake(stream).await.map(Some),
                Err(e)
                    if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) =>
                {
                    Ok(None)
                }
                Err(e) => Err(unreachable_at(socket, e)),
            }
        };
        within(socket, STARTUP_TIMEOUT, attempt).await
    }

    async fn handshake(stream: UnixStream) -> Result<Self, BusybeeError> {
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
            // Protocol, not DaemonUnreachable: see `connect_or_spawn_bzbd`.
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

    /// The events the daemon streams on a `Submit` connection.
    pub fn events(&mut self) -> Events<'_> {
        Events {
            conn: self,
            finished: false,
        }
    }

    async fn write_json(&mut self, value: &impl serde::Serialize) -> Result<(), BusybeeError> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| BusybeeError::Protocol(format!("cannot encode a message: {e}")))?;
        // The daemon closes at the limit, so writing past it would surface as
        // our own `EPIPE`, blaming an unreachable daemon for our message.
        if line.len() > MAX_LINE_BYTES {
            return Err(BusybeeError::Protocol(format!(
                "a message of {} bytes does not fit the {MAX_LINE_BYTES}-byte line limit",
                line.len()
            )));
        }
        line.push('\n');
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
            // Including non-UTF-8: the daemon did answer, so Protocol, not
            // DaemonUnreachable (see `connect_or_spawn_bzbd`).
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
    /// The next lease event, or `None` once the stream ends. Ending before
    /// `Finished` lost the exit code, so that is an error.
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
            Some(Response::Error { message }) => Err(BusybeeError::Rejected(message)),
            Some(other) => Err(BusybeeError::Protocol(format!(
                "expected an event, got {other:?}"
            ))),
        }
    }
}

/// How long a client spends reaching a handshaken daemon, spawn included.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Bounds a connect-and-handshake `attempt`: a daemon that accepts and never
/// answers would otherwise block the caller forever.
async fn within<T>(
    socket: &Path,
    budget: Duration,
    attempt: impl std::future::Future<Output = Result<T, BusybeeError>>,
) -> Result<T, BusybeeError> {
    tokio::time::timeout(budget, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(BusybeeError::DaemonUnreachable {
                context: format!(
                    "bzbd at {} did not answer the connection and handshake within {} seconds",
                    socket.display(),
                    STARTUP_TIMEOUT.as_secs()
                ),
            })
        })
}

async fn connect_by(socket: &Path, deadline: Instant) -> Result<Connection, BusybeeError> {
    let budget = deadline.saturating_duration_since(Instant::now());
    within(socket, budget, Connection::connect(socket)).await
}

/// Connects to `bzbd`, starting it if the socket is unreachable.
///
/// A daemon that answered but refused (a `Protocol` error) is returned as is:
/// a spawned replacement would exit as "already running" and the caller would
/// see a startup timeout instead of the real reason.
pub async fn connect_or_spawn_bzbd() -> Result<Connection, BusybeeError> {
    let socket = socket_path()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut spawned = false;
    loop {
        let last = match connect_by(&socket, deadline).await {
            Ok(conn) => return Ok(conn),
            Err(refusal @ BusybeeError::Protocol(_)) => return Err(refusal),
            Err(other) => other,
        };
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
        // Pace the retries, but not the first spawn: `spawn_bzbd` already
        // waits for the daemon it starts to report that it is serving.
        if spawned {
            sleep(Duration::from_millis(100)).await;
        }
        // Spawning again is not redundant: a daemon shutting down unlinks its
        // socket while still holding the pid-file lock, so a spawn in that
        // window exits "already running" and leaves nothing to connect to.
        spawn_bzbd(&bzbd_program(), deadline).await?;
        spawned = true;
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

/// Starts `bzbd`. The daemonizing parent exits once its child is serving or
/// has failed, so waiting for it surfaces startup errors.
async fn spawn_bzbd(program: &Path, deadline: Instant) -> Result<(), BusybeeError> {
    let child = Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // A dropped handle neither kills nor reaps: without this a daemon
        // hanging before it reports outlives every client that tried.
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
    use crate::protocol::{Hello, LeaseRequest};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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

    /// One test because the environment is process-wide.
    #[tokio::test]
    async fn state_dir_resolves_the_environment_in_order() {
        temp_env(&[("BUSYBEE_STATE_DIR", Some("/tmp/override"))], || {
            assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/override"));
        });

        // A misconfiguration, not a request for the default.
        for value in ["", "relative/state"] {
            temp_env(&[("BUSYBEE_STATE_DIR", Some(value))], || {
                let err = state_dir().unwrap_err().to_string();
                assert!(
                    err.contains("BUSYBEE_STATE_DIR"),
                    "BUSYBEE_STATE_DIR={value:?} message was {err:?}"
                );
            });
        }

        temp_env(
            &[
                ("BUSYBEE_STATE_DIR", None),
                ("XDG_STATE_HOME", Some("/tmp/xdg")),
            ],
            || {
                assert_eq!(state_dir().unwrap(), PathBuf::from("/tmp/xdg/busybee"));
            },
        );

        // An empty or relative XDG value counts as unset.
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

    /// A normal end here would let a `while let Some(..)` consumer exit as if
    /// the command had succeeded.
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

    #[tokio::test]
    async fn connect_if_listening_separates_an_absent_daemon_from_a_broken_one() {
        let dir = tempfile::TempDir::new().unwrap();

        let missing = dir.path().join("missing.sock");
        assert!(Connection::connect_if_listening(&missing)
            .await
            .unwrap()
            .is_none());

        // The socket a killed daemon leaves behind refuses connections the same
        // way, which `crates/bzb/tests/status.rs` covers against a real one:
        // closing a listener in this process leaves the socket accepting until
        // the kernel frees it, so it cannot be staged here.
        let broken = dir.path().join("broken.sock");
        let listener = tokio::net::UnixListener::bind(&broken).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });
        let refusal = Connection::connect_if_listening(&broken).await;
        let Err(err) = refusal else {
            panic!("a daemon that hung up mid-handshake was reported as absent");
        };
        assert!(err.to_string().contains("bzbd"), "{err}");
    }

    /// Paused clock: the runtime jumps to the timeout once every task blocks.
    #[tokio::test(start_paused = true)]
    async fn connect_if_listening_gives_up_on_a_daemon_that_never_answers() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("wedged.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let _accepted = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let stalled = Connection::connect_if_listening(&socket).await.err();
        let Some(BusybeeError::DaemonUnreachable { context }) = stalled else {
            panic!("expected a wedged daemon to fail the handshake, got {stalled:?}");
        };
        assert!(context.contains("handshake"), "{context}");
    }

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

    #[tokio::test]
    async fn a_broken_bzbd_connection_names_bzbd() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let (reader, mut outgoing) = ours.into_split();
        // Shutting our own end down for writing is the one way to make the
        // next write fail on every platform: POSIX specifies `EPIPE` for it.
        // How long writes keep succeeding to a socket whose *peer* is gone is
        // up to the kernel's buffering, and the two CI platforms disagree.
        outgoing.shutdown().await.unwrap();
        let mut conn = Connection {
            incoming: BufReader::new(reader),
            outgoing,
        };

        let error = conn.send(Request::Ping).await;
        let Err(BusybeeError::DaemonUnreachable { context }) = error else {
            panic!("expected a write to a closed bzbd socket to name bzbd, got {error:?}");
        };
        assert!(context.contains("bzbd"), "{context}");
    }

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

    #[tokio::test]
    async fn an_oversized_request_is_refused_before_it_reaches_the_socket() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (reader, outgoing) = ours.into_split();
        let mut conn = Connection {
            incoming: BufReader::new(reader),
            outgoing,
        };

        let error = conn
            .send(Request::Submit(LeaseRequest {
                argv: vec!["x".repeat(MAX_LINE_BYTES)],
                cwd: PathBuf::from("/somewhere"),
                env: BTreeMap::new(),
                label: None,
                class_override: None,
                cores_wanted: None,
                detached: false,
            }))
            .await;
        let Err(BusybeeError::Protocol(message)) = error else {
            panic!("expected an oversized request to be a protocol error, got {error:?}");
        };
        assert!(
            message.contains(&MAX_LINE_BYTES.to_string()),
            "message was {message:?}"
        );

        // Dropping our end closes the pair, so this read ends at whatever the
        // refused send managed to put on the wire: nothing.
        drop(conn);
        let mut written = Vec::new();
        BufReader::new(theirs)
            .read_to_end(&mut written)
            .await
            .unwrap();
        assert!(written.is_empty(), "the refused request was still written");
    }

    #[tokio::test]
    async fn an_invalid_utf8_reply_is_a_protocol_error_not_an_unreachable_daemon() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("garbled.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_reader, mut writer) = stream.into_split();
            writer.write_all(b"\xff\xfe not utf-8\n").await.unwrap();
            std::future::pending::<()>().await;
        });

        let refusal = Connection::connect(&socket).await.err();
        let Some(BusybeeError::Protocol(message)) = refusal else {
            panic!("expected a garbled handshake reply to be a protocol error, got {refusal:?}");
        };
        assert!(message.contains("utf-8"), "message was {message:?}");
    }

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
