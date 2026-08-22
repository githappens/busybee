//! `docs/design/bzbd.md` §Failure and recovery, the rows this daemon owns: it
//! is restarted under running tasks, pueued dies under it, a client is killed
//! without closing its socket, and it is told to stop while tasks run.
//!
//! Two daemons of the tests' own — a temporary state directory for bzbd, a
//! temporary config and socket for pueued — and a pool of four tokens, so the
//! numbers are exact whatever machine this runs on.
//!
//! Two of the tests stage the state a daemon with the injection work (#8)
//! would have left: a static lease that drained three tokens, a make joined to
//! the pool through `MAKEFLAGS`. Until that work lands every lease is admitted
//! exclusive and drains nothing, so the record in `leases.json` and the
//! variable in the request's environment stand in for it. Recovery reads
//! exactly those two things, so what it is tested against is what it will see.

mod common;

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use bzb_core::{
    daemon::Connection,
    protocol::{LeaseEvent, LeaseRequest, Request, Response, StatusReply, PROTOCOL_VERSION},
};
use bzb_test_support::PueuedFixture;
use common::{sigterm, wait_for, Fixture};
use pueue_lib::{
    message::{Request as PueueRequest, Response as PueueResponse, ShutdownRequest},
    task::TaskStatus,
};
use serde_json::Value;

/// Long enough for a poll tick (1 s) plus the daemons' own latency, short
/// enough that a lease which never arrives fails the test instead of hanging
/// the suite.
const PATIENCE: Duration = Duration::from_secs(15);

/// The pool every daemon here runs on, as its config file says it.
const POOL: &str = "pool_size = 4\n";

fn request(argv: &[&str]) -> LeaseRequest {
    LeaseRequest {
        argv: argv.iter().map(|a| (*a).to_string()).collect(),
        cwd: std::env::current_dir().expect("current dir"),
        // The real client sends its own environment; the task needs a PATH to
        // find `sh` with.
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        label: None,
        class_override: None,
        cores_wanted: None,
        detached: false,
    }
}

fn pool_of_four(config: &Path) -> Fixture {
    Fixture::start_with(
        Some(POOL),
        &[("PUEUE_CONFIG_PATH", config.display().to_string())],
    )
}

async fn connect(daemon: &Fixture) -> Connection {
    Connection::connect(&daemon.socket_path())
        .await
        .expect("connect to bzbd")
}

async fn submit(daemon: &Fixture, request: LeaseRequest) -> Connection {
    let mut conn = connect(daemon).await;
    conn.send(Request::Submit(request))
        .await
        .expect("send a submit request");
    conn
}

async fn event(conn: &mut Connection) -> LeaseEvent {
    tokio::time::timeout(PATIENCE, conn.events().next())
        .await
        .expect("no lease event arrived in time")
        .expect("read a lease event")
        .expect("the event stream ended before the lease finished")
}

/// Submits and waits for admission; returns the lease and pueue task ids.
async fn run(daemon: &Fixture, request: LeaseRequest) -> (Connection, u64, usize) {
    let mut conn = submit(daemon, request).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Queued { ahead: 0, .. }
    ));
    match event(&mut conn).await {
        LeaseEvent::Admitted {
            id, pueue_task_id, ..
        } => (conn, id, pueue_task_id),
        other => panic!("expected an Admitted event, got {other:?}"),
    }
}

async fn status(daemon: &Fixture) -> StatusReply {
    let mut conn = connect(daemon).await;
    conn.send(Request::Status).await.expect("send status");
    match conn.recv().await.expect("recv a status reply") {
        Response::Status(status) => status,
        other => panic!("expected a Status reply, got {other:?}"),
    }
}

/// Polls `status` until it holds no leases, or fails after `patience`.
async fn wait_for_no_leases(daemon: &Fixture, patience: Duration) -> StatusReply {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = status(daemon).await;
        if status.leases.is_empty() {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "leases were still {:?} after {patience:?}",
            status.leases
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn leases_json(daemon: &Fixture) -> Vec<Value> {
    let raw = fs::read_to_string(daemon.leases_path()).expect("read leases.json");
    serde_json::from_str(&raw).expect("decode leases.json")
}

/// Rewrites the one record in `leases.json` the way a daemon that had done
/// the injection work would have written it (see the module comment).
fn stage_record(daemon: &Fixture, edit: impl FnOnce(&mut Value)) {
    let mut records = leases_json(daemon);
    assert_eq!(records.len(), 1, "records were {records:?}");
    edit(&mut records[0]);
    fs::write(
        daemon.leases_path(),
        serde_json::to_vec(&records).expect("encode"),
    )
    .expect("write leases.json");
}

/// Tokens sitting in a jobserver fifo (`FIONREAD`), read the way the daemon
/// reads its own.
fn fifo_tokens(path: &Path) -> u32 {
    let fifo = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|err| panic!("open {}: {err}", path.display()));
    let mut n: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::ioctl(fifo.as_raw_fd(), libc::FIONREAD, &mut n) },
        0,
        "FIONREAD on {}",
        path.display()
    );
    n as u32
}

/// What pueued itself says a task is doing, `None` once its record is gone.
/// Process-wide (`PUEUE_CONFIG_PATH`), which is why the tests that call this
/// are serial.
async fn task_status(config: &Path, task_id: usize) -> Option<TaskStatus> {
    let mut client = pueue(config).await;
    client
        .send_request(PueueRequest::Status)
        .await
        .expect("send a status request");
    let state = match client.receive_response().await.expect("status response") {
        PueueResponse::Status(state) => state,
        other => panic!("expected a status response, got {other:?}"),
    };
    state.tasks.get(&task_id).map(|t| t.status.clone())
}

async fn pueue(config: &Path) -> pueue_lib::Client {
    std::env::set_var("PUEUE_CONFIG_PATH", config);
    bzb_core::client::connect()
        .await
        .expect("connect to pueued")
}

/// Polls pueued directly: the task has to be gone from the machine, not just
/// from bzbd's books.
async fn wait_for_task_to_end(config: &Path, task_id: usize, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = task_status(config, task_id).await;
        if matches!(status, None | Some(TaskStatus::Done { .. })) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pueue task {task_id} was still {status:?} after {patience:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Table row "bzbd dies", static half: the task keeps running, so the
/// restarted daemon takes the lease back with the tokens it holds and seeds
/// the rest into a fresh pool. Once the task ends the pool is whole again and
/// the record is gone.
#[tokio::test]
async fn a_restarted_daemon_adopts_the_static_lease_it_left_running() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;

    daemon.kill();
    // A daemon with the drain (#8) would have recorded the three tokens it
    // pulled for this static task; see the module comment.
    stage_record(&daemon, |record| {
        record["class"] = Value::from("static");
        record["cores_held"] = Value::from(3);
    });
    daemon.restart();

    let status = status(&daemon).await;
    assert_eq!(status.leases.len(), 1, "leases were {:?}", status.leases);
    let orphan = &status.leases[0];
    assert_eq!(orphan.id, id);
    assert_eq!(orphan.pueue_task_id, Some(task));
    assert_eq!(orphan.state, "orphaned");
    assert_eq!(orphan.cores, 3);
    assert_eq!((status.held, status.free), (3, 1));
    assert_eq!(fifo_tokens(&daemon.fifo_path()), 1);

    let status = wait_for_no_leases(&daemon, Duration::from_secs(10)).await;
    assert_eq!((status.held, status.free), (0, 4));
    assert_eq!(fifo_tokens(&daemon.fifo_path()), 4);
    assert!(
        leases_json(&daemon).is_empty(),
        "leases.json still holds {:?}",
        leases_json(&daemon)
    );
}

/// Table row "bzbd dies", in the moment between pueued starting a task and
/// the daemon writing its id down: the record says the submission went out
/// but names no task. The restarted daemon finds the task by label and
/// creation time, as the poll finds an unanswered submission, and adopts it
/// rather than admitting the next exclusive task beside it.
#[tokio::test]
async fn a_restarted_daemon_matches_the_submission_it_was_killed_in() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;

    daemon.kill();
    // What `leases.json` says when the daemon dies between `pueue.add` and
    // the persist that follows it: the submission time is on record — the
    // lease was written before the task went out — and the task id is not.
    stage_record(&daemon, |record| {
        let submitted = record["started_at_unix_ms"].clone();
        record["submitted_at_unix_ms"] = submitted;
        record["pueue_task_id"] = Value::Null;
    });
    daemon.restart();

    let status = status(&daemon).await;
    assert_eq!(status.leases.len(), 1, "leases were {:?}", status.leases);
    let orphan = &status.leases[0];
    assert_eq!(orphan.id, id);
    assert_eq!(orphan.pueue_task_id, Some(task));
    assert_eq!(orphan.state, "orphaned");
    assert_eq!(
        leases_json(&daemon)[0]["pueue_task_id"],
        Value::from(task),
        "the matched task must be on record for the next restart"
    );
}

/// Five targets that each note how many of them are running at once: the
/// peak is what tells a make on the pool from one running one job at a time.
const MAKEFILE: &str = "\
T := t1 t2 t3 t4 t5
all: $(T)
$(T):
\t@f=run/$@; touch $$f; sleep 4; ls run | wc -l >> counts.log; rm $$f
";

/// `(major, minor)` of GNU make, `None` when it cannot be run.
fn make_version() -> Option<(u32, u32)> {
    let out = Command::new("make").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let last_word = text.lines().next()?.split_whitespace().last()?;
    let mut parts = last_word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Table row "bzbd dies", jobserver half: the make keeps the old fifo open
/// and finishes on it, the new daemon seeds a whole new pool beside it, and
/// the old fifo file goes only once nothing is left to use it.
#[tokio::test]
async fn a_restarted_daemon_leaves_an_orphaned_make_on_the_old_fifo() {
    match make_version() {
        Some(version) if version >= (4, 4) => {}
        Some((major, minor)) => {
            eprintln!("skipping: make {major}.{minor} has no fifo jobserver (need 4.4)");
            return;
        }
        None => {
            eprintln!("skipping: make not found in PATH");
            return;
        }
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let project = tempfile::tempdir().expect("create tempdir");
    fs::create_dir(project.path().join("run")).expect("mkdir run");
    fs::write(project.path().join("Makefile"), MAKEFILE).expect("write Makefile");

    let old_fifo = daemon.fifo_path();
    let mut make = request(&["make"]);
    make.cwd = project.path().to_path_buf();
    // What the injection work (#8) puts in a jobserver task's environment;
    // see the module comment.
    make.env.insert(
        "MAKEFLAGS".into(),
        format!("--jobserver-auth=fifo:{}", old_fifo.display()),
    );
    let (_conn, id, _task) = run(&daemon, make).await;
    // Not before make holds the fifo: the tokens live in the pipe, and the
    // pipe lives only while someone has it open, so a daemon dying between
    // launching make and make's first token read takes the pool with it. A
    // second job running means a token was read, i.e. the fifo is held.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while fs::read_dir(project.path().join("run"))
        .expect("list run")
        .count()
        < 2
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "make did not start two jobs within 3s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    daemon.kill();
    stage_record(&daemon, |record| {
        record["class"] = Value::from("jobserver");
    });
    daemon.restart();

    let new_fifo = daemon.fifo_path();
    assert_ne!(new_fifo, old_fifo);
    assert!(
        old_fifo.exists(),
        "the old fifo was unlinked under the make"
    );
    assert_eq!(fifo_tokens(&new_fifo), 4);
    let status = status(&daemon).await;
    assert_eq!(status.leases.len(), 1, "leases were {:?}", status.leases);
    assert_eq!(status.leases[0].id, id);
    assert_eq!(status.leases[0].state, "orphaned");
    assert_eq!((status.held, status.free), (0, 4));

    wait_for_no_leases(&daemon, Duration::from_secs(10)).await;
    wait_for(&old_fifo, false);
    assert!(
        new_fifo.exists(),
        "the daemon's own fifo went with the old one"
    );
    assert!(
        leases_json(&daemon).is_empty(),
        "leases.json still holds {:?}",
        leases_json(&daemon)
    );
    // On the pool, not one job at a time: the old fifo was still there for it.
    let counts = fs::read_to_string(project.path().join("counts.log")).expect("read counts.log");
    let peak = counts
        .lines()
        .map(|l| l.trim().parse::<u32>().expect("a count"))
        .max()
        .expect("five counts");
    assert!(
        peak >= 2,
        "make ran one job at a time: counts were {counts:?}"
    );
}

/// Polls `leases.json` until a record says `killing`, or fails after
/// `patience`: the teardown goes on record the moment the hangup is seen.
fn wait_for_teardown_record(daemon: &Fixture, patience: Duration) {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let records = leases_json(daemon);
        if records.iter().any(|r| r["killing"] == true) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no teardown was recorded within {patience:?}; records were {records:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The `Done` task's start and end as pueued recorded them.
fn span(
    status: &Option<TaskStatus>,
) -> (
    chrono::DateTime<chrono::Local>,
    chrono::DateTime<chrono::Local>,
) {
    match status {
        Some(TaskStatus::Done { start, end, .. }) => (*start, *end),
        other => panic!("expected a Done task, got {other:?}"),
    }
}

/// Table row "client disconnects while running", interrupted by "bzbd dies":
/// a task that ignores SIGTERM is still on the machine, tokens and all, when
/// the daemon is killed inside the grace period. The teardown is on record,
/// so the restarted daemon finishes it — SIGKILL, tokens back, record gone —
/// and only then admits the next lease, rather than running the two side by
/// side.
#[tokio::test]
#[serial_test::serial]
async fn a_restarted_daemon_finishes_the_teardown_it_was_killed_in() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    // `trap` makes the shell ignore SIGTERM, and `sleep` inherits that.
    let (conn, _, survivor) = run(&daemon, request(&["sh", "-c", "trap '' TERM; sleep 5"])).await;

    // The client hangs up; the daemon sends SIGTERM and waits for pueued to
    // confirm the task gone, which it will not for another second.
    drop(conn);
    wait_for_teardown_record(&daemon, Duration::from_secs(2));
    daemon.kill();
    assert!(
        matches!(
            task_status(&pueued.config_path, survivor).await,
            Some(TaskStatus::Running { .. })
        ),
        "the task did not survive SIGTERM"
    );
    // A daemon with the drain (#8) would have recorded the tokens the task
    // holds; see the module comment.
    stage_record(&daemon, |record| {
        record["class"] = Value::from("static");
        record["cores_held"] = Value::from(2);
    });
    daemon.restart();

    assert_eq!(
        fifo_tokens(&daemon.fifo_path()),
        2,
        "the new pool was seeded as if the task were gone"
    );
    let (mut conn, _, next) = run(&daemon, request(&["sh", "-c", "exit 0"])).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Finished { exit_code: 0, .. }
    ));

    let (_, survivor_ended) = span(&task_status(&pueued.config_path, survivor).await);
    let (next_started, _) = span(&task_status(&pueued.config_path, next).await);
    assert!(
        survivor_ended <= next_started,
        "the next task started at {next_started} while the survivor ran until {survivor_ended}"
    );
    assert_eq!(fifo_tokens(&daemon.fifo_path()), 4);
    assert!(
        leases_json(&daemon).is_empty(),
        "leases.json still holds {:?}",
        leases_json(&daemon)
    );
}

/// Table row "pueued dies": the lease is lost rather than left waiting for a
/// completion that cannot arrive, its client is told why and exits non-zero,
/// the tokens go back, and the next submission brings pueued back.
#[tokio::test]
#[serial_test::serial]
async fn a_pueued_that_dies_mid_task_loses_the_lease_and_is_respawned_on_the_next_submit() {
    let Some(mut pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool_of_four(&pueued.config_path);
    let (mut conn, _, _) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;

    pueued.kill();

    match event(&mut conn).await {
        LeaseEvent::Notice { text } => {
            assert!(text.contains("pueued went away"), "notice was {text:?}")
        }
        other => panic!("expected a Notice, got {other:?}"),
    }
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Finished { exit_code: 1, .. }
    ));
    let status = wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
    assert_eq!((status.held, status.free), (0, 4));

    // The daemon is on the same config, so the pueued it spawns is another
    // isolated one; it is shut down below rather than left behind.
    let (mut conn, _, _) = run(&daemon, request(&["sh", "-c", "exit 0"])).await;
    assert!(matches!(
        event(&mut conn).await,
        LeaseEvent::Finished { exit_code: 0, .. }
    ));
    let mut respawned = pueue(&pueued.config_path).await;
    respawned
        .send_request(PueueRequest::DaemonShutdown(ShutdownRequest::Graceful))
        .await
        .expect("shut the respawned pueued down");
    respawned
        .receive_response()
        .await
        .expect("the respawned pueued acknowledges the shutdown");
}

/// The name of the re-exec'd test that stands in for a client process.
const CLIENT_HELPER: &str = "client_helper";

/// The socket the helper submits to. Set only by `a_client_killed_with_sigkill_takes_its_task_with_it`.
const CLIENT_HELPER_SOCKET: &str = "BZBD_RECOVERY_TEST_CLIENT_SOCKET";

/// Not a test: the client process `a_client_killed_with_sigkill_takes_its_task_with_it`
/// SIGKILLs. Run on its own (`--ignored`) by that test with
/// [`CLIENT_HELPER_SOCKET`] set, it takes a lease on a `sleep 30`, prints the
/// pueue task id once admitted and then waits to be killed. Without the
/// variable it does nothing, so `--ignored` runs elsewhere are unaffected.
#[test]
#[ignore]
fn client_helper() {
    let Ok(socket) = std::env::var(CLIENT_HELPER_SOCKET) else {
        return;
    };
    let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
    let mut writer = stream.try_clone().expect("clone the stream");
    let mut lines = BufReader::new(stream).lines();
    let mut say = |value: String| {
        writeln!(writer, "{value}").expect("write");
    };
    say(format!("{{\"hello\":{PROTOCOL_VERSION}}}"));
    let pong = lines.next().expect("a pong").expect("read");
    assert!(
        matches!(serde_json::from_str(&pong), Ok(Response::Pong { .. })),
        "expected a Pong, got {pong}"
    );
    say(
        serde_json::to_string(&Request::Submit(request(&["sh", "-c", "sleep 30"])))
            .expect("encode"),
    );
    for line in lines {
        let line = line.expect("read");
        if let Ok(Response::Event(LeaseEvent::Admitted { pueue_task_id, .. })) =
            serde_json::from_str(&line)
        {
            println!("admitted {pueue_task_id}");
            std::io::stdout().flush().expect("flush");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    panic!("the lease ended before it was admitted");
}

/// Table row "client disconnects while running", the unclean case: a client
/// that is SIGKILLed closes nothing itself. The kernel closes its socket and
/// the daemon sees the same end of stream, so the task is killed and the
/// tokens come back just as for a client that hung up.
#[tokio::test]
#[serial_test::serial]
async fn a_client_killed_with_sigkill_takes_its_task_with_it() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool_of_four(&pueued.config_path);

    let mut client = Command::new(std::env::current_exe().expect("own path"))
        .args([CLIENT_HELPER, "--exact", "--ignored", "--nocapture"])
        .env(CLIENT_HELPER_SOCKET, daemon.socket_path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the client helper");
    let stdout = BufReader::new(client.stdout.take().expect("piped stdout"));
    let task = stdout
        .lines()
        .map(|line| line.expect("read the client's stdout"))
        .find_map(|line| line.strip_prefix("admitted ").map(|id| id.parse::<usize>()))
        .expect("the client was never admitted")
        .expect("a task id");
    assert!(
        matches!(
            task_status(&pueued.config_path, task).await,
            Some(TaskStatus::Running { .. })
        ),
        "the task was not running"
    );

    client.kill().expect("SIGKILL the client");
    client.wait().expect("reap the client");

    wait_for_task_to_end(&pueued.config_path, task, Duration::from_secs(2)).await;
    let status = wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
    assert_eq!((status.held, status.free), (0, 4));
}

/// Startup cleanup: a `jobserver-<pid>` left by a daemon that is gone is
/// unlinked, one whose pid is alive is not, and nothing else in the state
/// directory is touched.
#[tokio::test]
async fn stale_fifos_are_removed_on_start() {
    let mut daemon = Fixture::start();
    let own = daemon.fifo_path();
    daemon.kill();
    assert!(own.exists(), "SIGKILL should have left the fifo behind");
    // A pid no process can have: Linux's pid_max tops out well below it.
    let dead = daemon.state_dir().join(format!("jobserver-{}", i32::MAX));
    let alive = daemon
        .state_dir()
        .join(format!("jobserver-{}", std::process::id()));
    let unrelated = daemon.state_dir().join("notes.txt");
    for path in [&dead, &alive, &unrelated] {
        fs::write(path, "").unwrap_or_else(|err| panic!("create {}: {err}", path.display()));
    }

    daemon.restart();

    assert!(!own.exists(), "the dead daemon's fifo was kept");
    assert!(!dead.exists(), "a fifo of a dead pid was kept");
    assert!(alive.exists(), "a fifo of a live pid was unlinked");
    assert!(unrelated.exists(), "a file that is not a fifo was unlinked");
    assert!(daemon.fifo_path().exists(), "the new daemon has no fifo");
}

/// Daemon shutdown: SIGTERM ends the daemon, not the tasks. The task keeps
/// running, its record stays for the next daemon to adopt, and the fifo stays
/// for the task to hold; only the socket goes.
#[tokio::test]
#[serial_test::serial]
async fn sigterm_leaves_the_running_task_its_record_and_the_fifo_alone() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;
    let fifo = daemon.fifo_path();

    sigterm(daemon.child.id());
    wait_for(&daemon.socket_path(), false);
    assert!(daemon.child.wait().expect("wait").success());

    assert!(
        matches!(
            task_status(&pueued.config_path, task).await,
            Some(TaskStatus::Running { .. })
        ),
        "the task did not survive the daemon"
    );
    let records = leases_json(&daemon);
    assert_eq!(records.len(), 1, "records were {records:?}");
    assert_eq!(records[0]["id"], Value::from(id));
    assert_eq!(records[0]["pueue_task_id"], Value::from(task));
    assert!(fifo.exists(), "the fifo was unlinked under the task");
}
