//! `bzbd`, busybee's broker daemon.
//!
//! This is the skeleton: it owns the state directory, the unix socket and the
//! single-instance lock, and it answers `Ping` and `Status`. Leases,
//! admission and pueue submission arrive with the scheduler.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    path::Path,
    sync::Mutex,
};

use anyhow::{anyhow, bail, Context, Result};
use bzb_core::{
    daemon::{log_path, pid_path, socket_path, state_dir},
    protocol::{Hello, Request, Response, StatusReply, PROTOCOL_VERSION},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    signal::unix::{signal, SignalKind},
};
use tracing::Level;

pub fn run() -> Result<()> {
    let foreground = parse_args(std::env::args().skip(1))?;

    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create the state directory {}", dir.display()))?;
    let log = log_path()?;

    // Fork before the runtime exists: a forked child inherits no threads.
    if !foreground {
        daemonize(&log)?;
    }
    init_logging(&log)?;

    let pid_file = pid_path()?;
    let Some(_locked) = lock_pid_file(&pid_file)? else {
        eprintln!("bzbd: already running");
        tracing::info!(pid_file = %pid_file.display(), "another bzbd holds the lock; exiting");
        return Ok(());
    };

    tokio::runtime::Runtime::new()
        .context("cannot start the tokio runtime")?
        .block_on(serve(&socket_path()?, &pid_file))
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<bool> {
    let mut foreground = false;
    for arg in args {
        match arg.as_str() {
            "--foreground" => foreground = true,
            other => bail!("unknown argument {other:?} (usage: bzbd [--foreground])"),
        }
    }
    Ok(foreground)
}

async fn serve(socket: &Path, pid_file: &Path) -> Result<()> {
    // Nothing else can be listening: this process holds the pid-file lock.
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove the stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot bind the socket {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "bzbd listening");

    let mut terminate = signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot listen for SIGINT")?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("cannot accept a connection")?;
                tokio::spawn(async move {
                    if let Err(err) = handle(stream).await {
                        tracing::warn!("connection failed: {err:#}");
                    }
                });
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }

    tracing::info!("shutting down");
    drop(listener);
    std::fs::remove_file(socket)
        .with_context(|| format!("cannot remove the socket {}", socket.display()))?;
    std::fs::remove_file(pid_file)
        .with_context(|| format!("cannot remove the pid file {}", pid_file.display()))?;
    Ok(())
}

async fn handle(stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut incoming = BufReader::new(reader).lines();

    let Some(first) = incoming.next_line().await? else {
        return Ok(());
    };
    match serde_json::from_str::<Hello>(&first) {
        Ok(hello) if hello.hello == PROTOCOL_VERSION => {}
        Ok(hello) => {
            // Dropping the writer closes the connection.
            return reply(
                &mut writer,
                Response::Error {
                    message: format!(
                        "protocol version {} is not supported; bzbd speaks {PROTOCOL_VERSION}",
                        hello.hello
                    ),
                },
            )
            .await;
        }
        Err(err) => {
            return reply(
                &mut writer,
                Response::Error {
                    message: format!(r#"expected {{"hello": <version>}} first: {err}"#),
                },
            )
            .await;
        }
    }
    reply(&mut writer, pong()).await?;

    while let Some(line) = incoming.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Ping) => pong(),
            // The token pool arrives with the scheduler; until then there is
            // nothing under management to report.
            Ok(Request::Status) => Response::Status(StatusReply {
                pool_size: 0,
                free: 0,
                held: 0,
                leases: Vec::new(),
            }),
            Ok(Request::Submit(_) | Request::Cancel { .. }) => Response::Error {
                message: "not implemented".into(),
            },
            Err(err) => Response::Error {
                message: format!("cannot decode {line:?}: {err}"),
            },
        };
        reply(&mut writer, response).await?;
    }
    Ok(())
}

fn pong() -> Response {
    Response::Pong {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    }
}

async fn reply(writer: &mut OwnedWriteHalf, response: Response) -> Result<()> {
    let mut line = serde_json::to_string(&response).context("cannot encode a response")?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .context("cannot write a response")?;
    Ok(())
}

/// Takes the exclusive lock on the pid file and records our pid. `Ok(None)`
/// means another instance holds it. The lock lives as long as the returned
/// file, i.e. as long as the process.
fn lock_pid_file(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open the pid file {}", path.display()))?;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(err).with_context(|| format!("cannot lock the pid file {}", path.display()));
    }

    file.set_len(0)
        .with_context(|| format!("cannot truncate {}", path.display()))?;
    writeln!(&file, "{}", std::process::id())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(file))
}

/// Detaches from the terminal: fork, `setsid`, and point the standard streams
/// at the log file. Same shape as pueued's `-d`.
fn daemonize(log: &Path) -> Result<()> {
    let log_file = open_log(log)?;
    let devnull = File::open("/dev/null").context("cannot open /dev/null")?;

    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()).context("cannot fork"),
        0 => {}
        _ => std::process::exit(0),
    }
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error()).context("cannot setsid");
    }

    redirect(&devnull, libc::STDIN_FILENO)?;
    redirect(&log_file, libc::STDOUT_FILENO)?;
    redirect(&log_file, libc::STDERR_FILENO)?;
    Ok(())
}

fn redirect(source: &File, fd: i32) -> Result<()> {
    if unsafe { libc::dup2(source.as_raw_fd(), fd) } == -1 {
        return Err(io::Error::last_os_error()).context(format!("cannot redirect fd {fd}"));
    }
    Ok(())
}

fn init_logging(log: &Path) -> Result<()> {
    let level = match std::env::var("BUSYBEE_LOG") {
        Ok(value) => value
            .parse::<Level>()
            .map_err(|_| anyhow!("BUSYBEE_LOG={value:?} is not a level (try info or debug)"))?,
        Err(std::env::VarError::NotPresent) => Level::INFO,
        Err(err) => bail!("BUSYBEE_LOG: {err}"),
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(Mutex::new(open_log(log)?))
        .init();
    Ok(())
}

fn open_log(log: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("cannot open the log file {}", log.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_is_off_unless_asked_for() {
        assert!(!parse_args(std::iter::empty()).unwrap());
        assert!(parse_args(["--foreground".to_string()].into_iter()).unwrap());
    }

    #[test]
    fn an_unknown_argument_is_fatal() {
        let err = parse_args(["--nope".to_string()].into_iter()).unwrap_err();
        assert!(err.to_string().contains("--nope"), "message was {err}");
    }
}
