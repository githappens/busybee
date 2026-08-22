//! `bzbd`, busybee's broker daemon.
//!
//! This is the skeleton: it owns the state directory, the unix socket and the
//! single-instance lock, and it answers `Ping` and `Status`. Leases,
//! admission and pueue submission arrive with the scheduler.

use std::{
    fs::{File, OpenOptions, Permissions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::{
        mpsc::{self, RecvTimeoutError, Sender},
        Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use bzb_core::{
    daemon::{log_path, pid_path, socket_path, state_dir},
    protocol::{read_line, Hello, Line, Request, Response, PROTOCOL_VERSION},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener, UnixStream,
    },
    signal::unix::{signal, SignalKind},
};
use tracing::Level;

pub fn run() -> Result<()> {
    let foreground = parse_args(std::env::args().skip(1))?;

    let dir = state_dir()?;
    create_state_dir(&dir)?;
    let log = log_path()?;

    // Fork before the runtime exists: a forked child inherits no threads.
    // Under `--foreground` there is no parent waiting for a report.
    let mut ready = if foreground {
        // Nothing to report to and nothing to watch: the caller is looking at
        // our stderr and can stop us.
        Ready {
            pipe: None,
            watchdog: None,
        }
    } else {
        daemonize(&log)?
    };

    let result = start(&mut ready, &log);
    if let Err(err) = &result {
        // Our stderr is the log file by now; the parent is the only route back
        // to the terminal the user is looking at.
        ready.report(&format!("{err:#}"));
    }
    result
}

/// Creates the state directory owner-only. The socket inside it is the
/// daemon's whole control surface, so on a shared machine the usual 022 umask
/// would hand every other user a way in. The mode is part of the creation
/// rather than a chmod after it: a directory born 0755 is reachable for as
/// long as it takes to restrict it, which is long enough to plant a file in.
fn create_state_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create the state directory {}", dir.display()))?;
    // `recursive` leaves an existing directory alone, mode included, and one
    // may predate this.
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict the state directory {}", dir.display()))
}

/// Everything that happens after the fork, so a failure has one place to be
/// reported from.
fn start(ready: &mut Ready, log: &Path) -> Result<()> {
    init_logging(log)?;

    let pid_file = pid_path()?;
    let Some(_locked) = lock_pid_file(&pid_file)? else {
        // Someone else is already serving, which is what the caller wanted.
        ready.report(SERVING);
        eprintln!("bzbd: already running");
        tracing::info!(pid_file = %pid_file.display(), "another bzbd holds the lock; exiting");
        return Ok(());
    };

    tokio::runtime::Runtime::new()
        .context("cannot start the tokio runtime")?
        .block_on(serve(&socket_path()?, &pid_file, ready))
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

async fn serve(socket: &Path, pid_file: &Path, ready: &mut Ready) -> Result<()> {
    // Before the socket exists: the socket is how everyone else finds us, and
    // a SIGTERM landing before these are installed kills us on the spot,
    // leaving the socket and the pid file behind.
    let mut terminate = signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot listen for SIGINT")?;

    // Nothing else can be listening: this process holds the pid-file lock.
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove the stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot bind the socket {}", socket.display()))?;
    // Belt and braces: the owner-only state directory already keeps other
    // users from reaching this path, which is also what covers the moment
    // between the bind and this chmod.
    std::fs::set_permissions(socket, Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict the socket {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "bzbd listening");

    // The socket accepts connections and SIGTERM will be caught: only now may
    // the caller stop waiting.
    ready.report(SERVING);
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
    let mut incoming = BufReader::new(reader);

    let first = match next_line(&mut incoming).await? {
        Line::Text(line) => line,
        Line::Closed => return Ok(()),
        // Dropping the writer closes the connection: we cannot find the next
        // message on a connection whose framing we have already lost.
        Line::Malformed(reason) => return reply(&mut writer, Response::error(reason)).await,
    };
    match serde_json::from_str::<Hello>(&first) {
        Ok(hello) if hello.hello == PROTOCOL_VERSION => {}
        Ok(hello) => {
            // Dropping the writer closes the connection.
            return reply(
                &mut writer,
                Response::error(format!(
                    "protocol version {} is not supported; bzbd speaks {PROTOCOL_VERSION}",
                    hello.hello
                )),
            )
            .await;
        }
        Err(err) => {
            return reply(
                &mut writer,
                Response::error(format!(r#"expected {{"hello": <version>}} first: {err}"#)),
            )
            .await;
        }
    }
    reply(&mut writer, pong()).await?;

    loop {
        let line = match next_line(&mut incoming).await? {
            Line::Text(line) => line,
            Line::Closed => return Ok(()),
            Line::Malformed(reason) => return reply(&mut writer, Response::error(reason)).await,
        };
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Ping) => pong(),
            // The token pool arrives with the scheduler. An all-zero StatusReply
            // would be indistinguishable from a real idle pool, so refuse
            // instead of inventing one.
            Ok(Request::Status | Request::Submit(_) | Request::Cancel { .. }) => {
                Response::error("not implemented")
            }
            // Not echoing the request: escaping it and then JSON-encoding the
            // message quadruples it, so a line that fit within MAX_LINE_BYTES
            // would be answered with one that does not. The decoder still
            // quotes what it choked on, which is why every error message here
            // goes through Response::error to be cut to a length that survives
            // encoding.
            Err(err) => Response::error(format!("cannot decode the request: {err}")),
        };
        reply(&mut writer, response).await?;
    }
}

async fn next_line(reader: &mut BufReader<OwnedReadHalf>) -> Result<Line> {
    read_line(reader).await.context("cannot read a request")
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
        // A pid file that turns out to be a symlink is not ours: we truncate
        // this one, and following the link would empty whatever it points at.
        .custom_flags(libc::O_NOFOLLOW)
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

/// What the daemonized child writes down the pipe once it is serving;
/// anything else is the reason it is not.
const SERVING: &str = "serving";

/// How long a daemonized child may take to reach `report`. It holds the
/// pid-file lock the whole time, and `setsid` has put it in its own session:
/// the client that started it cannot reach it any more, and the forking parent
/// it can reach may itself be killed when the client gives up. So a child that
/// wedges during startup would keep every later daemon out of the lock
/// forever. This bound is the child's own, and generous: a healthy startup is
/// a lock, a runtime, two signal handlers and a bind.
const STARTUP_WATCHDOG: Duration = Duration::from_secs(10);

/// The daemonized child's end of the startup pipe. The forking parent stays
/// alive reading it, so a failure after the fork reaches the caller's stderr
/// instead of only `bzbd.log`.
struct Ready {
    pipe: Option<File>,
    /// Dropped on the first report; that is what disarms the watchdog.
    watchdog: Option<Sender<()>>,
}

impl Ready {
    /// Closing the pipe is what releases the parent, so it happens exactly
    /// once, on the first report.
    fn report(&mut self, message: &str) {
        self.watchdog.take();
        let Some(mut pipe) = self.pipe.take() else {
            return;
        };
        if let Err(err) = pipe.write_all(message.as_bytes()) {
            tracing::warn!("cannot report startup on the pipe: {err}");
        }
    }
}

/// Runs `expire` unless the returned sender is dropped within `timeout`.
fn watchdog(timeout: Duration, expire: impl FnOnce() + Send + 'static) -> Sender<()> {
    let (armed, disarmed) = mpsc::channel();
    std::thread::spawn(move || {
        if disarmed.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
            expire();
        }
    });
    armed
}

/// Detaches from the terminal: fork, `setsid`, and point the standard streams
/// at the log file. Same shape as pueued's `-d`, except the parent waits for
/// the child's verdict before exiting. Only the child returns.
fn daemonize(log: &Path) -> Result<Ready> {
    let log_file = open_log(log)?;
    let devnull = File::open("/dev/null").context("cannot open /dev/null")?;
    let (reading, writing) = startup_pipe()?;

    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()).context("cannot fork"),
        0 => drop(reading),
        _ => {
            drop(writing);
            await_startup(reading, log);
        }
    }
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error()).context("cannot setsid");
    }

    redirect(&devnull, libc::STDIN_FILENO)?;
    redirect(&log_file, libc::STDOUT_FILENO)?;
    redirect(&log_file, libc::STDERR_FILENO)?;
    // Our stderr is the log by now, and exiting closes the startup pipe, so
    // the parent reports the death too.
    let watchdog = watchdog(STARTUP_WATCHDOG, || {
        eprintln!(
            "bzbd: startup did not finish within {}s; exiting so the pid-file lock is released",
            STARTUP_WATCHDOG.as_secs()
        );
        std::process::exit(1);
    });
    Ok(Ready {
        pipe: Some(writing),
        watchdog: Some(watchdog),
    })
}

fn startup_pipe() -> Result<(File, File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("cannot create the startup pipe");
    }
    // Safety: `pipe` filled both fds and nothing else owns them.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// Runs in the forking parent: blocks until the child reports, then exits
/// with its verdict. An empty read means the child died without saying why.
fn await_startup(mut reading: File, log: &Path) -> ! {
    let mut message = String::new();
    let code = match reading.read_to_string(&mut message) {
        Ok(_) if message == SERVING => 0,
        Ok(_) if message.is_empty() => {
            eprintln!("bzbd: exited during startup; see {}", log.display());
            1
        }
        Ok(_) => {
            eprintln!("bzbd: {message}");
            1
        }
        Err(err) => {
            eprintln!("bzbd: cannot read the startup pipe: {err}");
            1
        }
    };
    std::process::exit(code);
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
        // Same as the pid file: our standard streams end up here, so a symlink
        // planted in the state directory would aim them somewhere else.
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)
        .with_context(|| format!("cannot open the log file {}", log.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

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

    /// A child that wedges after taking the pid-file lock locks every later
    /// daemon out of it, and no client can clear it: `setsid` put it in its own
    /// session, so the process the client spawned is not even its ancestor any
    /// more. The deadline therefore has to be the child's own.
    #[test]
    fn a_startup_that_never_reports_trips_the_watchdog() {
        let tripped = Arc::new(AtomicBool::new(false));
        let armed = watchdog(Duration::from_millis(50), {
            let tripped = tripped.clone();
            move || tripped.store(true, Ordering::SeqCst)
        });

        std::thread::sleep(Duration::from_millis(400));

        assert!(tripped.load(Ordering::SeqCst), "the watchdog never fired");
        drop(armed);
    }

    /// The mode has to come from the creation itself, not from a chmod after
    /// it: between the two, another user can reach inside a directory the
    /// umask left open. Every level counts, because a parent that stays
    /// traversable is a way to the files below it.
    #[test]
    fn every_state_directory_is_created_owner_only() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let nested = tmp.path().join("outer/state");

        create_state_dir(&nested).expect("create the state directory");

        for dir in [nested.parent().expect("outer"), nested.as_path()] {
            let mode = std::fs::metadata(dir).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{} is {mode:o}", dir.display());
        }
    }

    /// A pid file we did not create is not ours to truncate: following a
    /// symlink planted in the state directory would empty whatever it points
    /// at. Refuse instead, loudly.
    #[test]
    fn a_symlinked_pid_file_is_refused() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let target = tmp.path().join("precious");
        std::fs::write(&target, "keep me").expect("write the target");
        let pid_file = tmp.path().join("bzbd.pid");
        std::os::unix::fs::symlink(&target, &pid_file).expect("plant the symlink");

        let err = lock_pid_file(&pid_file).expect_err("a symlinked pid file was accepted");

        assert!(
            err.to_string().contains("bzbd.pid"),
            "message was {err:#}, which does not name the path"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read the target"),
            "keep me"
        );
    }

    /// And a child that did start serving must survive: the watchdog bounds
    /// startup, not the daemon.
    #[test]
    fn reporting_disarms_the_watchdog() {
        let tripped = Arc::new(AtomicBool::new(false));
        let mut ready = Ready {
            pipe: None,
            watchdog: Some(watchdog(Duration::from_millis(50), {
                let tripped = tripped.clone();
                move || tripped.store(true, Ordering::SeqCst)
            })),
        };

        ready.report(SERVING);
        std::thread::sleep(Duration::from_millis(400));

        assert!(
            !tripped.load(Ordering::SeqCst),
            "a daemon that reported it is serving was killed anyway"
        );
    }
}
