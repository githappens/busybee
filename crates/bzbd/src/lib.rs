//! `bzbd`, busybee's broker daemon: state directory, socket, single-instance
//! lock and config (see `docs/design/bzbd.md` §Components).
mod inject;
mod leases;
mod recovery;
mod submit;

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
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use bzb_core::{
    config::Config,
    daemon::{log_path, pid_path, socket_path, state_dir},
    protocol::{read_line, Hello, LeaseEvent, Line, Request, Response, PROTOCOL_VERSION},
    scheduler::LeaseId,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener, UnixStream,
    },
    signal::unix::{signal, SignalKind},
    sync::mpsc::unbounded_channel,
};
use tracing::Level;

use crate::leases::{Handle, Leases};

pub fn run() -> Result<()> {
    match parse_args(std::env::args().skip(1))? {
        Invocation::Version => {
            println!("bzbd {}", env!("BUSYBEE_VERSION"));
            Ok(())
        }
        Invocation::Daemon { foreground } => run_daemon(foreground),
    }
}

fn run_daemon(foreground: bool) -> Result<()> {
    // Capture the caller's mask before restricting it for the control surface.
    // pueued — spawned later, on the first task submission — must run under the
    // caller's mask so its tasks can create directories and executables normally.
    let original_umask = restrict_umask(DIR_UMASK);
    bzb_core::client::set_spawn_umask(original_umask);
    let dir = state_dir()?;
    create_state_dir(&dir)?;
    restrict_umask(FILE_UMASK);
    let log = log_path()?;

    // Fork before the runtime exists: a forked child inherits no threads.
    let mut ready = if foreground {
        Ready::default()
    } else {
        daemonize(&log)?
    };

    let result = start(&mut ready, &log);
    if let Err(err) = &result {
        // Our stderr is the log by now; the parent reaches the user's terminal.
        ready.report(&format!("{err:#}"));
    }
    result
}

/// Owner-only, keeping the execute bit a directory needs.
const DIR_UMASK: libc::mode_t = 0o077;

/// Owner-only without execute: `bind` derives the socket's mode from 0777, and
/// the spec puts `bzbd.sock` at 0600.
const FILE_UMASK: libc::mode_t = 0o177;

/// The mode comes with each creation rather than a chmod after it: a socket is
/// connectable the moment it is bound. Returns the previous mask.
fn restrict_umask(mask: libc::mode_t) -> libc::mode_t {
    // SAFETY: no preconditions. Process-wide and racy against concurrent file
    // creation, hence called before any thread exists.
    unsafe { libc::umask(mask) }
}

fn create_state_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create the state directory {}", dir.display()))?;
    // `recursive` leaves an existing directory's mode alone.
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict the state directory {}", dir.display()))
}

/// Everything after the fork, so a failure has one place to be reported from.
fn start(ready: &mut Ready, log: &Path) -> Result<()> {
    init_logging(log)?;

    let pid_file = pid_path()?;
    let Some(locked) = lock_pid_file(&pid_file)? else {
        ready.report(SERVING);
        eprintln!("bzbd: already running");
        tracing::info!(pid_file = %pid_file.display(), "another bzbd holds the lock; exiting");
        return Ok(());
    };

    // Only once holding the lock: a second bzbd that finds one serving must
    // not fail on a config that went bad since.
    let config = Config::load()?;
    tracing::info!(
        pool_size = config.pool_size,
        max_concurrent = config.max_concurrent,
        drain_deadline_ms = config.drain_deadline_ms,
        "config loaded"
    );

    let runtime = tokio::runtime::Runtime::new().context("cannot start the tokio runtime")?;
    let served = runtime.block_on(serve(
        &socket_path()?,
        ready,
        Arc::new(tokio::sync::Mutex::new(config)),
    ));
    // Unlink the pid file only once the runtime (and every connection task) is
    // gone: unlinked earlier, a new bzbd could lock a fresh inode and serve
    // alongside us.
    drop(runtime);
    served?;
    std::fs::remove_file(&pid_file)
        .with_context(|| format!("cannot remove the pid file {}", pid_file.display()))?;
    drop(locked);
    Ok(())
}

#[derive(Debug)]
enum Invocation {
    Daemon { foreground: bool },
    Version,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Invocation> {
    let mut foreground = false;
    for arg in args {
        match arg.as_str() {
            "--foreground" => foreground = true,
            "--version" => return Ok(Invocation::Version),
            other => bail!("unknown argument {other:?} (usage: bzbd [--foreground] [--version])"),
        }
    }
    Ok(Invocation::Daemon { foreground })
}

/// Serves until SIGTERM or SIGINT, then unlinks the socket.
async fn serve(socket: &Path, ready: &mut Ready, config: SharedConfig) -> Result<()> {
    // Before the socket exists: an earlier SIGTERM would leave it behind.
    let mut terminate = signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot listen for SIGINT")?;
    let mut hangup = signal(SignalKind::hangup()).context("cannot listen for SIGHUP")?;

    let loaded = config.lock().await.clone();
    // Before the socket exists, so no lease is sized as if the machine were idle.
    let leases_path = bzb_core::daemon::leases_path()?;
    let recovered = recovery::recover(&state_dir()?, &leases_path, loaded.pool_size).await?;
    let (actor, leases, commands) = Leases::new(&loaded, recovered, leases_path);
    tokio::spawn(actor.run(commands));

    // Nothing else can be listening: this process holds the pid-file lock.
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove the stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot bind the socket {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "bzbd listening");

    ready.report(SERVING);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("cannot accept a connection")?;
                let leases = leases.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle(stream, leases, config).await {
                        tracing::warn!("connection failed: {err:#}");
                    }
                });
            }
            // A signal has no reply; a finished reload has already logged.
            _ = hangup.recv() => if let Err(refusal) = reload(&config, &leases).await {
                tracing::warn!("{refusal}");
            },
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }

    tracing::info!("shutting down");
    leases.shutdown().await?;
    drop(listener);
    std::fs::remove_file(socket)
        .with_context(|| format!("cannot remove the socket {}", socket.display()))?;
    Ok(())
}

/// A tokio mutex: [`reload`] holds it across the await on the actor, which is
/// what keeps two reloads from interleaving.
type SharedConfig = Arc<tokio::sync::Mutex<Config>>;

/// Re-reads the config file and hands it to the actor; a refused file changes
/// nothing. The lock is held across read, apply and store so reloads apply in
/// order, and the applied config is returned rather than read back, since the
/// next reload may already hold the lock.
async fn reload(config: &SharedConfig, leases: &Handle) -> Result<Config, String> {
    let mut running = config.lock().await;
    match Config::load() {
        Ok(reloaded) => {
            // Stored and logged only once the actor took it: on a SIGHUP the
            // log line is the whole reply.
            leases.set_config(reloaded.clone()).await.map_err(|err| {
                format!("config reloaded but the scheduler did not take it: {err:#}")
            })?;
            *running = reloaded.clone();
            tracing::info!(
                pool_size = reloaded.pool_size,
                max_concurrent = reloaded.max_concurrent,
                drain_deadline_ms = reloaded.drain_deadline_ms,
                "config reloaded"
            );
            Ok(reloaded)
        }
        Err(err) => Err(format!(
            "config reload refused, keeping the running configuration \
             (pool_size {}): {err}",
            running.pool_size
        )),
    }
}

async fn handle(stream: UnixStream, leases: Handle, config: SharedConfig) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut incoming = BufReader::new(reader);

    let first = match next_line(&mut incoming).await? {
        Line::Text(line) => line,
        Line::Closed => return Ok(()),
        // Returning drops the writer: framing is lost, so the connection closes.
        Line::Malformed(reason) => return reply(&mut writer, Response::error(reason)).await,
    };
    match serde_json::from_str::<Hello>(&first) {
        Ok(hello) if hello.hello == PROTOCOL_VERSION => {}
        Ok(hello) => {
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
            Ok(Request::Status) => match leases.status().await {
                Ok(status) => Response::Status(status),
                Err(err) => Response::error(format!("{err:#}")),
            },
            Ok(Request::Submit(request)) => {
                return stream_lease(&mut incoming, &mut writer, &leases, request).await
            }
            Ok(Request::Cancel { lease }) => match leases.cancel(LeaseId(lease)).await? {
                true => Response::Ack,
                false => Response::error(format!("there is no lease {lease}")),
            },
            Ok(Request::ConfigReload) => match reload(&config, &leases).await {
                Ok(applied) => Response::ConfigReloaded {
                    pool_size: applied.pool_size,
                    max_concurrent: applied.max_concurrent,
                    drain_deadline_ms: applied.drain_deadline_ms,
                },
                Err(refusal) => {
                    tracing::warn!("{refusal}");
                    Response::error(refusal)
                }
            },
            // Not echoing the request: escaped and re-encoded, a line within
            // MAX_LINE_BYTES would be answered with one over it. Response::error
            // truncates what the decoder quotes.
            Err(err) => Response::error(format!("cannot decode the request: {err}")),
        };
        reply(&mut writer, response).await?;
    }
}

/// Connection = lease (spec §Lease model): however this returns, the actor
/// hears the hangup.
async fn stream_lease(
    incoming: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    leases: &Handle,
    request: bzb_core::protocol::LeaseRequest,
) -> Result<()> {
    let (events, mut incoming_events) = unbounded_channel();
    let lease = leases.submit(request, events).await?;
    let streamed = stream_events(incoming, writer, &mut incoming_events).await;
    leases.hangup(lease).await?;
    streamed
}

async fn stream_events(
    incoming: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<LeaseEvent>,
) -> Result<()> {
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                let finished = matches!(event, LeaseEvent::Finished { .. });
                reply(writer, Response::Event(event)).await?;
                if finished {
                    return Ok(());
                }
            }
            // Not cancel safe: an event can cut a half-read line, which the next
            // read sees as a framing error. Only a client sending requests this
            // connection refuses anyway can hit that.
            line = next_line(incoming) => match line? {
                Line::Closed => return Ok(()),
                Line::Malformed(reason) => return reply(writer, Response::error(reason)).await,
                Line::Text(_) => {
                    reply(
                        writer,
                        Response::error(
                            "this connection is streaming a lease and takes no further requests",
                        ),
                    )
                    .await?;
                }
            },
        }
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

/// `Ok(None)` means another instance holds the lock. The lock lives as long as
/// the returned file.
fn lock_pid_file(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        // We truncate it, so a planted symlink would empty its target.
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

/// Written down the startup pipe once serving; anything else is the error.
const SERVING: &str = "serving";

/// A child wedged in startup holds the pid-file lock and, after `setsid`, is
/// out of every client's reach, so the bound has to be its own.
const STARTUP_WATCHDOG: Duration = Duration::from_secs(10);

/// The child's end of the startup pipe; `None`s under `--foreground`.
#[derive(Default)]
struct Ready {
    pipe: Option<File>,
    /// Dropping it disarms the watchdog.
    watchdog: Option<Sender<()>>,
}

impl Ready {
    /// Only the first report counts: closing the pipe releases the parent.
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

/// Fork, `setsid`, standard streams to the log. The parent waits for the
/// child's verdict and exits; only the child returns.
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

/// An empty read means the child died without saying why.
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
        // Our standard streams end up here; do not follow a planted symlink.
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)
        .with_context(|| format!("cannot open the log file {}", log.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{leases::Command, recovery::Recovered};
    use bzb_core::jobserver::Jobserver;
    use pueue_lib::task::{Task, TaskResult, TaskStatus};
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, MutexGuard, PoisonError,
        },
    };

    /// Every test that creates a file or directory takes this: `umask` is
    /// process-wide, and a leaked mask would leave a neighbour's tempdir
    /// untraversable.
    static UMASK: Mutex<()> = Mutex::new(());

    pub(crate) struct Umask {
        _lock: MutexGuard<'static, ()>,
        previous: libc::mode_t,
    }

    impl Drop for Umask {
        fn drop(&mut self) {
            restrict_umask(self.previous);
        }
    }

    /// Poison is ignored: the drop restores the mask either way.
    pub(crate) fn hold_umask(mask: libc::mode_t) -> Umask {
        let lock = UMASK.lock().unwrap_or_else(PoisonError::into_inner);
        Umask {
            previous: restrict_umask(mask),
            _lock: lock,
        }
    }

    /// A `busybee`-group task created now.
    pub(crate) fn task(id: usize, label: &str, status: TaskStatus) -> Task {
        let mut task = Task::new(
            "sleep 1".into(),
            PathBuf::from("/tmp"),
            Default::default(),
            "busybee".into(),
            status,
            Vec::new(),
            0,
            Some(label.into()),
        );
        task.id = id;
        task
    }

    pub(crate) fn tasks(tasks: Vec<Task>) -> BTreeMap<usize, Task> {
        tasks.into_iter().map(|t| (t.id, t)).collect()
    }

    pub(crate) fn running() -> TaskStatus {
        let now = chrono::Local::now();
        TaskStatus::Running {
            enqueued_at: now,
            start: now,
        }
    }

    pub(crate) fn done(result: TaskResult) -> TaskStatus {
        let now = chrono::Local::now();
        TaskStatus::Done {
            enqueued_at: now,
            start: now,
            end: now,
            result,
        }
    }

    #[test]
    fn foreground_is_off_unless_asked_for() {
        assert!(matches!(
            parse_args(std::iter::empty()).unwrap(),
            Invocation::Daemon { foreground: false }
        ));
        assert!(matches!(
            parse_args(["--foreground".to_string()].into_iter()).unwrap(),
            Invocation::Daemon { foreground: true }
        ));
    }

    #[test]
    fn version_flag_is_recognized() {
        assert!(matches!(
            parse_args(["--version".to_string()].into_iter()).unwrap(),
            Invocation::Version
        ));
    }

    #[test]
    fn an_unknown_argument_is_fatal() {
        let err = parse_args(["--nope".to_string()].into_iter()).unwrap_err();
        assert!(err.to_string().contains("--nope"), "message was {err}");
    }

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

    /// One test, not two: `umask` is process-wide, so both masks are checked
    /// in `run`'s order without racing each other.
    #[test]
    fn the_state_directory_and_the_socket_are_born_owner_only() {
        let _umask = hold_umask(DIR_UMASK);
        let tmp = tempfile::tempdir().expect("create tempdir");
        let nested = tmp.path().join("outer/state");

        create_state_dir(&nested).expect("create the state directory");

        for dir in [nested.parent().expect("outer"), nested.as_path()] {
            let mode = std::fs::metadata(dir).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{} is {mode:o}", dir.display());
        }

        restrict_umask(FILE_UMASK);
        let socket = nested.join("bzbd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind the socket");

        let mode = std::fs::metadata(&socket)
            .expect("stat")
            .permissions()
            .mode();
        drop(listener);
        assert_eq!(mode & 0o777, 0o600, "the socket was born {mode:o}");
    }

    #[test]
    fn a_symlinked_pid_file_is_refused() {
        // A mask a tempdir survives, held so the owner-only test cannot set
        // its own while this one creates files.
        let _umask = hold_umask(0o022);
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

    /// A config file at `pool_size = 4` in `dir`, `BUSYBEE_CONFIG` pointed at
    /// it, and an actor that is never spawned. Returns the file's path.
    fn reload_setup(
        dir: &Path,
    ) -> (
        PathBuf,
        SharedConfig,
        Handle,
        tokio::sync::mpsc::Receiver<Command>,
    ) {
        let path = dir.join("config.toml");
        std::fs::write(&path, "pool_size = 4\n").expect("write the config");
        std::env::set_var("BUSYBEE_CONFIG", &path);
        let loaded = Config::load().expect("load the config");
        let (_actor, leases, commands) = Leases::new(
            &loaded,
            Recovered {
                jobserver: Jobserver::create(dir, loaded.pool_size).expect("a fifo"),
                adopted: Vec::new(),
                killing: Vec::new(),
                debt: 0,
            },
            dir.join("leases.json"),
        );
        (
            path,
            Arc::new(tokio::sync::Mutex::new(loaded)),
            leases,
            commands,
        )
    }

    /// `BUSYBEE_CONFIG` is process-wide, hence the serial.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_reload_the_scheduler_never_took_leaves_the_running_config_alone() {
        let _umask = hold_umask(0o022);
        let tmp = tempfile::tempdir().expect("create tempdir");
        let (path, running, leases, commands) = reload_setup(tmp.path());
        // Every command now fails, as on a daemon on its way down.
        drop(commands);
        std::fs::write(&path, "pool_size = 6\n").expect("rewrite the config");

        let refusal = reload(&running, &leases)
            .await
            .expect_err("a scheduler that never took the parameters reported success");

        std::env::remove_var("BUSYBEE_CONFIG");
        assert!(refusal.contains("scheduler"), "message was {refusal:?}");
        assert_eq!(running.lock().await.pool_size, 4);
    }

    /// The test plays the actor, so it can keep one reload in flight while the
    /// next one tries to start.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_reload_in_flight_holds_the_next_one_off() {
        let _umask = hold_umask(0o022);
        let tmp = tempfile::tempdir().expect("create tempdir");
        let (path, running, leases, mut commands) = reload_setup(tmp.path());

        std::fs::write(&path, "pool_size = 6\n").expect("rewrite the config");
        let first = tokio::spawn({
            let (running, leases) = (running.clone(), leases.clone());
            async move { reload(&running, &leases).await }
        });
        let done = match commands
            .recv()
            .await
            .expect("the first reload sent nothing")
        {
            Command::SetConfig { config, done } => {
                assert_eq!(config.pool_size, 6);
                done
            }
            _ => panic!("the first reload sent something other than set-config"),
        };

        std::fs::write(&path, "pool_size = 8\n").expect("rewrite the config again");
        let second = tokio::spawn({
            let (running, leases) = (running.clone(), leases.clone());
            async move { reload(&running, &leases).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), commands.recv())
                .await
                .is_err(),
            "a second reload reached the scheduler while the first was still in flight"
        );

        done.send(()).expect("the first reload stopped waiting");
        first
            .await
            .expect("join the first reload")
            .expect("the first reload was refused");
        match commands
            .recv()
            .await
            .expect("the second reload sent nothing")
        {
            Command::SetConfig { config, done } => {
                assert_eq!(config.pool_size, 8);
                done.send(()).expect("the second reload stopped waiting");
            }
            _ => panic!("the second reload sent something other than set-config"),
        }
        second
            .await
            .expect("join the second reload")
            .expect("the second reload was refused");

        std::env::remove_var("BUSYBEE_CONFIG");
        assert_eq!(running.lock().await.pool_size, 8);
    }
}
