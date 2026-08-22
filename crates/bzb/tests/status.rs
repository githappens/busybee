//! `busybee status` end to end, against a stand-in bzbd in a temp state
//! directory. The user's daemon is never touched: every test binds its own
//! socket and points the client at it with `BUSYBEE_STATE_DIR`.
//!
//! The stand-in exists because the real daemon does not serve `Status` yet —
//! `crates/bzbd/tests/skeleton.rs` pins it to "not implemented" until the
//! scheduler is wired in. What is under test here is the client half: the
//! request, the table, the JSON and the no-daemon case.

use std::{
    io::BufRead,
    os::fd::FromRawFd,
    path::Path,
    process::{Output, Stdio},
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
    // `slave` stays open here for the same reason.
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        master.read_line(&mut line).expect("read the pty");
        line
    });

    let status = child.wait().await.expect("wait for busybee status --json");
    assert!(status.success(), "busybee status --json exited {status}");
    let line = reader.join().expect("the pty reader thread");

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
