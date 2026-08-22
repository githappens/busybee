//! `busybee status` end to end, in a temp state directory. The user's daemon is
//! never touched: every test binds its own socket, or runs its own `bzbd`, and
//! points the client at it with `BUSYBEE_STATE_DIR`.
//!
//! One test runs the real daemon, which is what proves the command works
//! against what it talks to in production. The rest use a stand-in: the daemon
//! gives every lease `Class::None` until the classification work lands, so it
//! cannot yet produce the static and jobserver rows the table is made of.

use std::{
    io::BufRead,
    os::fd::FromRawFd,
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use bzb_core::protocol::{LeaseView, Request, Response, StatusReply, PROTOCOL_VERSION};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const BUSYBEE: &str = env!("CARGO_BIN_EXE_busybee");

/// A pool of 18 with a static lease holding 9, a jobserver lease the daemon
/// estimates at 3, and one queued behind them.
fn reply() -> StatusReply {
    StatusReply {
        pool_size: 18,
        free: 6,
        held: 9,
        leases: vec![
            LeaseView {
                id: 41,
                label: "ui build".into(),
                tool: "xcodebuild".into(),
                class: "static".into(),
                cores: 9,
                state: "running".into(),
                elapsed_ms: 132_000,
                ahead: None,
                pueue_task_id: Some(1),
            },
            LeaseView {
                id: 42,
                label: "cmake --build build --target Tests".into(),
                tool: "cmake".into(),
                class: "jobserver".into(),
                cores: 3,
                state: "running".into(),
                elapsed_ms: 40_000,
                ahead: None,
                pueue_task_id: Some(2),
            },
            LeaseView {
                id: 43,
                label: "backend tests".into(),
                tool: "cargo".into(),
                class: "jobserver".into(),
                cores: 0,
                state: "queued".into(),
                elapsed_ms: 5_000,
                ahead: Some(1),
                pueue_task_id: None,
            },
        ],
    }
}

/// The real `bzbd`, in a temp state directory, killed on drop.
struct RealBzbd {
    child: Child,
    state: TempDir,
}

impl RealBzbd {
    fn start() -> Self {
        // Cargo exports the path of `busybee` only, so the daemon is found next
        // to it — the same place `bzb-core` looks for it when auto-starting.
        let bzbd = Path::new(BUSYBEE)
            .parent()
            .expect("busybee lives in a directory")
            .join("bzbd");
        assert!(
            bzbd.is_file(),
            "{} is not built; run the tests with `cargo test --workspace`",
            bzbd.display()
        );
        let state = TempDir::new().expect("create tempdir");
        let child = Command::new(&bzbd)
            .arg("--foreground")
            .env("BUSYBEE_STATE_DIR", state.path())
            .spawn()
            .expect("spawn bzbd");
        let daemon = Self { child, state };
        let socket = daemon.state.path().join("bzbd.sock");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "bzbd did not bind {} within 3s",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn state_dir(&self) -> &Path {
        self.state.path()
    }

    /// SIGKILL and reap, so the daemon is gone before the caller looks at what
    /// it left in the state directory.
    fn kill(&mut self) {
        self.child.kill().expect("kill bzbd");
        self.child.wait().expect("reap bzbd");
    }
}

impl Drop for RealBzbd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A socket that speaks the handshake and answers every `Status` with `reply`.
struct FakeBzbd {
    state: TempDir,
}

impl FakeBzbd {
    fn start(reply: StatusReply) -> Self {
        let state = TempDir::new().expect("create tempdir");
        let listener = UnixListener::bind(state.path().join("bzbd.sock")).expect("bind");
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let reply = reply.clone();
                tokio::spawn(async move { serve(stream, reply).await });
            }
        });
        Self { state }
    }

    fn state_dir(&self) -> &Path {
        self.state.path()
    }
}

async fn serve(stream: UnixStream, reply: StatusReply) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let hello = lines.next_line().await.expect("read").expect("a hello");
    assert!(
        hello.contains(&PROTOCOL_VERSION.to_string()),
        "hello was {hello:?}"
    );
    respond(
        &mut writer,
        Response::Pong {
            version: "test".into(),
            pid: std::process::id(),
        },
    )
    .await;

    while let Some(line) = lines.next_line().await.expect("read") {
        match serde_json::from_str::<Request>(&line).expect("decode a request") {
            Request::Status => respond(&mut writer, Response::Status(reply.clone())).await,
            other => panic!("expected a Status request, got {other:?}"),
        }
    }
}

async fn respond(writer: &mut tokio::net::unix::OwnedWriteHalf, response: Response) {
    let line = format!("{}\n", serde_json::to_string(&response).expect("encode"));
    writer.write_all(line.as_bytes()).await.expect("write");
}

async fn run_status(state: &Path, args: &[&str]) -> Output {
    tokio::process::Command::new(BUSYBEE)
        .arg("status")
        .args(args)
        .env("BUSYBEE_STATE_DIR", state)
        .output()
        .await
        .expect("run busybee status")
}

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "busybee status exited {:?}",
        output
    );
    String::from_utf8(output.stdout.clone()).expect("stdout is utf-8")
}

#[tokio::test]
async fn the_table_has_the_pool_line_and_one_row_per_lease() {
    let daemon = FakeBzbd::start(reply());

    let output = run_status(daemon.state_dir(), &[]).await;

    let stdout = stdout_of(&output);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "pool: 18 tokens, 6 free, 9 held by static leases   (approx. 3 in use by jobserver tasks)",
            "#41  running  2m12s  xcodebuild   static     holding 9     label: ui build",
            "#42  running  0m40s  cmake        jobserver  using ~3      label: cmake --build build --target Tests",
            "#43  queued   0m05s  cargo        jobserver  1 ahead       label: backend tests",
        ]
    );
}

#[tokio::test]
async fn the_json_output_is_the_status_reply_the_daemon_sent() {
    let daemon = FakeBzbd::start(reply());

    let output = run_status(daemon.state_dir(), &["--json"]).await;

    let stdout = stdout_of(&output);
    let decoded: StatusReply = serde_json::from_str(&stdout).expect("decode a StatusReply");
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(reply()).unwrap()
    );
    let object: serde_json::Value = serde_json::from_str(&stdout).expect("decode");
    assert_eq!(object["approx_in_use"], 3);
}

/// The stand-in above covers the rendering; this covers the wiring, against the
/// daemon the command actually talks to. Nothing has been submitted to it, so
/// it holds no leases and the pool it reports is whole and free.
#[tokio::test]
async fn the_real_daemon_answers_the_status_request() {
    let daemon = RealBzbd::start();
    let cores = std::thread::available_parallelism()
        .expect("logical cores")
        .get();

    let json = stdout_of(&run_status(daemon.state_dir(), &["--json"]).await);
    let reply: StatusReply = serde_json::from_str(&json).expect("decode a StatusReply");
    assert_eq!(
        (reply.pool_size as usize, reply.free as usize, reply.held),
        (cores, cores, 0)
    );
    assert!(reply.leases.is_empty(), "leases were {:?}", reply.leases);

    let table = stdout_of(&run_status(daemon.state_dir(), &[]).await);
    assert_eq!(
        table.lines().collect::<Vec<_>>(),
        vec![format!(
            "pool: {cores} tokens, {cores} free, 0 held by static leases   \
             (approx. 0 in use by jobserver tasks)"
        )]
    );
}

/// A daemon killed outright cannot unlink its socket, so the file outlives it.
/// Nothing is listening on it, which is the idle pool the message describes —
/// the presence of the file is not what decides that.
#[tokio::test]
async fn a_killed_daemon_leaves_a_socket_that_still_reports_an_idle_pool() {
    let mut daemon = RealBzbd::start();
    let socket = daemon.state_dir().join("bzbd.sock");
    daemon.kill();
    assert!(socket.exists(), "the killed daemon unlinked its socket");

    let output = run_status(daemon.state_dir(), &[]).await;

    assert!(
        output.status.success(),
        "busybee status exited {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("busybee: daemon not running; pool idle"),
        "stderr was {stderr:?}"
    );
}

/// A daemon that is listening and failing is not an idle machine. Reporting it
/// as one would tell the caller the pool is free while bzbd is gating every
/// command on it.
#[tokio::test]
async fn a_daemon_that_hangs_up_mid_handshake_is_reported_as_a_failure() {
    let state = TempDir::new().expect("create tempdir");
    let listener = UnixListener::bind(state.path().join("bzbd.sock")).expect("bind");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        drop(stream);
    });

    let output = run_status(state.path(), &[]).await;

    assert!(
        !output.status.success(),
        "busybee status exited {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("pool idle"),
        "a listening daemon was reported as an idle pool: {stderr:?}"
    );
    assert!(stderr.contains("bzbd"), "stderr was {stderr:?}");
}

/// The handshake's own deadline ends at the pong. A daemon that answers it and
/// then wedges leaves this one-shot command with nothing of its own to give up
/// on, so the request-and-reply is bounded too.
#[tokio::test]
async fn a_daemon_that_never_answers_the_status_request_gives_up() {
    let state = TempDir::new().expect("create tempdir");
    let listener = UnixListener::bind(state.path().join("bzbd.sock")).expect("bind");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        lines.next_line().await.expect("read").expect("a hello");
        respond(
            &mut writer,
            Response::Pong {
                version: "test".into(),
                pid: std::process::id(),
            },
        )
        .await;
        // Handshaken, and from here on silent. Holding the connection open is
        // the point: nothing answers the `Status` and nothing closes on it.
        std::future::pending::<()>().await;
    });

    let started = Instant::now();
    let output = run_status(state.path(), &[]).await;

    assert!(
        started.elapsed() < Duration::from_secs(60),
        "busybee status waited {:?} on a wedged daemon",
        started.elapsed()
    );
    assert!(
        !output.status.success(),
        "busybee status exited {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("did not answer"), "stderr was {stderr:?}");
}

/// No daemon means no pool and nothing being gated, which is a true report and
/// not a failure. stdout stays empty so a `--json` consumer never mistakes the
/// message for a reply.
#[tokio::test]
async fn a_missing_daemon_reports_an_idle_pool_and_exits_zero() {
    let state = TempDir::new().expect("create tempdir");

    for args in [&[][..], &["--json"][..]] {
        let output = run_status(state.path(), args).await;

        assert!(
            output.status.success(),
            "busybee status {args:?} exited {}",
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "stdout was {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("busybee: daemon not running; pool idle"),
            "stderr was {stderr:?}"
        );
    }
}

/// `--json` is parsed, not read, so it must stay machine-readable when the
/// caller happens to be a terminal — a colouring library that switches on
/// `isatty` would put escape sequences through the middle of the object.
#[tokio::test]
async fn the_json_output_carries_no_ansi_escapes_on_a_terminal() {
    let daemon = FakeBzbd::start(reply());
    let (mut master, slave) = openpty();

    let mut child = tokio::process::Command::new(BUSYBEE)
        .arg("status")
        .arg("--json")
        .env("BUSYBEE_STATE_DIR", daemon.state_dir())
        .stdout(Stdio::from(slave.try_clone().expect("clone the slave")))
        .spawn()
        .expect("run busybee status --json on a pty");

    // Read while the child runs, on a thread so the fake daemon's task still
    // gets to answer. Waiting for the child first would race macOS, which
    // flushes the pty's output queue once the last slave descriptor closes;
    // `slave` stays open here for the same reason. Holding it open also means
    // the read never sees EOF, so the line is collected with a deadline rather
    // than by joining: a `status --json` that printed nothing must fail this
    // test, not hang it.
    let (send_line, line) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut read = String::new();
        master.read_line(&mut read).expect("read the pty");
        let _ = send_line.send(read);
    });

    let status = child.wait().await.expect("wait for busybee status --json");
    assert!(status.success(), "busybee status --json exited {status}");
    let line = line
        .recv_timeout(Duration::from_secs(10))
        .expect("busybee status --json wrote a line to the pty");

    assert!(
        !line.contains('\u{1b}'),
        "the json line carries an escape: {line:?}"
    );
    let decoded: StatusReply = serde_json::from_str(line.trim()).expect("decode a StatusReply");
    assert_eq!(decoded.pool_size, 18);
}

/// A pseudo-terminal, as `(master reader, slave)`. The slave is what the child
/// gets as stdout, which is what makes `isatty` true for it.
fn openpty() -> (std::io::BufReader<std::fs::File>, std::fs::File) {
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `openpty` filled both fds and nothing else owns them.
    unsafe {
        (
            std::io::BufReader::new(std::fs::File::from_raw_fd(master)),
            std::fs::File::from_raw_fd(slave),
        )
    }
}
