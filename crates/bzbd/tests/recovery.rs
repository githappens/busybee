//! Spec §Failure and recovery, the rows bzbd owns, against an isolated
//! `pueued` and a pool of four. Tests that need a drained grant or a jobserver
//! task's `MAKEFLAGS` stage them in `leases.json` and the request's env.

mod common;

use std::{
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
use common::{
    event, leases_json, pueue, request, sigterm, status, submit, task_status, wait_for,
    wait_for_no_leases, wait_for_task_to_end, Fixture,
};
use pueue_lib::{
    message::{Request as PueueRequest, ShutdownRequest},
    task::TaskStatus,
};
use serde_json::Value;

const POOL: &str = "pool_size = 4\n";

/// A grace no test can race: killing the daemon "inside the grace" at the
/// default second can lose to a loaded CI runner.
const POOL_PATIENT_KILL: &str = "pool_size = 4\nkill_grace_ms = 30000\n";

fn pool_of_four(config: &Path) -> Fixture {
    Fixture::start_with(
        Some(POOL),
        &[("PUEUE_CONFIG_PATH", config.display().to_string())],
    )
}

fn pool_of_four_patient_kill(config: &Path) -> Fixture {
    Fixture::start_with(
        Some(POOL_PATIENT_KILL),
        &[("PUEUE_CONFIG_PATH", config.display().to_string())],
    )
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

/// A teardown holds its tokens a poll after the lease leaves the books.
async fn wait_for_pool_restored(daemon: &Fixture, patience: Duration) -> StatusReply {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = status(daemon).await;
        if status.held == 0 && status.free == status.pool_size {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pool was still {} free and {} held after {patience:?}",
            status.free,
            status.held
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Edits the one record in `leases.json`.
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

/// `FIONREAD` on the fifo, as the daemon counts its own.
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

/// pueued reports a `start_immediately` task `Queued` for a moment; a signal
/// then would end it outright.
async fn wait_for_task_to_run(config: &Path, task_id: usize, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let status = task_status(config, task_id).await;
        if matches!(status, Some(TaskStatus::Running { .. })) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pueue task {task_id} was still {status:?} after {patience:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spec table row "bzbd dies", static half.
#[tokio::test]
async fn a_restarted_daemon_adopts_the_static_lease_it_left_running() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;

    daemon.kill();
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
    assert_eq!(orphan.cores, Some(3));
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

/// Spec table row "bzbd dies", between `pueue.add` and recording its id.
#[tokio::test]
async fn a_restarted_daemon_matches_the_submission_it_was_killed_in() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 5"])).await;

    daemon.kill();
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

/// Each target logs how many are running, so the peak shows pool use.
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

/// Spec table row "bzbd dies", jobserver half.
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
    make.env.insert(
        "MAKEFLAGS".into(),
        format!("--jobserver-auth=fifo:{}", old_fifo.display()),
    );
    let (_conn, id, _task) = run(&daemon, make).await;
    // Wait until make holds the fifo open (a second job means it read a
    // token): the tokens die with the pipe's last reader.
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

fn wait_for_records(daemon: &Fixture, want: impl Fn(&[Value]) -> bool, patience: Duration) {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let records = leases_json(daemon);
        if want(&records) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "leases.json was not as expected within {patience:?}; records were {records:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_teardown_record(daemon: &Fixture, patience: Duration) {
    wait_for_records(
        daemon,
        |records| records.iter().any(|r| r["killing"] == true),
        patience,
    );
}

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

/// Spec table rows "client disconnects while running" then "bzbd dies".
#[tokio::test]
#[serial_test::serial]
async fn a_restarted_daemon_finishes_the_teardown_it_was_killed_in() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four_patient_kill(&pueued.config_path);
    // The marker says the trap is installed: pueued reports `Running` before
    // the shell has run anything, and a SIGTERM in that gap kills it.
    let armed = daemon.state_dir().join("trap-armed");
    let script = format!("trap '' TERM; touch {}; sleep 300", armed.display());
    let (conn, _, survivor) = run(&daemon, request(&["sh", "-c", &script])).await;
    wait_for(&armed, true);

    drop(conn);
    wait_for_teardown_record(&daemon, Duration::from_secs(2));
    daemon.kill();
    let survived = task_status(&pueued.config_path, survivor).await;
    assert!(
        matches!(survived, Some(TaskStatus::Running { .. })),
        "the task did not survive SIGTERM; pueued reports {survived:?}"
    );
    stage_record(&daemon, |record| {
        record["class"] = Value::from("static");
        record["cores_held"] = Value::from(2);
    });
    // Long enough to observe the seed below before the escalation returns
    // the survivor's tokens.
    daemon.write_config("pool_size = 4\nkill_grace_ms = 5000\n");
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

/// "bzbd dies" between booking a teardown and sending SIGTERM.
#[tokio::test]
#[serial_test::serial]
async fn a_restarted_daemon_resends_sigterm_for_a_teardown_it_booked_but_never_signalled() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let marker = daemon.state_dir().join("got-term");
    // The shell notes SIGTERM and leaves; a SIGKILL leaves no note.
    let script = format!(
        "trap 'touch {}; exit 0' TERM; sleep 5 & wait",
        marker.display()
    );
    let (conn, _, task) = run(&daemon, request(&["sh", "-c", &script])).await;
    wait_for_task_to_run(&pueued.config_path, task, Duration::from_secs(5)).await;

    daemon.kill();
    drop(conn);
    stage_record(&daemon, |record| {
        record["killing"] = Value::from(true);
    });
    assert!(
        matches!(
            task_status(&pueued.config_path, task).await,
            Some(TaskStatus::Running { .. })
        ),
        "the task did not outlive the daemon"
    );
    daemon.restart();

    wait_for_task_to_end(&pueued.config_path, task, Duration::from_secs(4)).await;
    assert!(
        marker.exists(),
        "the task never saw SIGTERM: the restarted daemon went straight to SIGKILL"
    );
    // The poll that confirms the task gone is up to a tick behind pueued.
    wait_for_records(
        &daemon,
        |records| records.is_empty(),
        Duration::from_secs(3),
    );
    assert_eq!(fifo_tokens(&daemon.fifo_path()), 4);
}

/// Spec table row "pueued dies".
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

    // The respawned pueued is isolated too; shut it down rather than leak it.
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

const CLIENT_HELPER: &str = "client_helper";

const CLIENT_HELPER_SOCKET: &str = "BZBD_RECOVERY_TEST_CLIENT_SOCKET";

/// Not a test: the client process that
/// `a_client_killed_with_sigkill_takes_its_task_with_it` re-execs and SIGKILLs.
/// A no-op without [`CLIENT_HELPER_SOCKET`].
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
    wait_for_task_to_run(&pueued.config_path, task, Duration::from_secs(5)).await;

    client.kill().expect("SIGKILL the client");
    client.wait().expect("reap the client");

    wait_for_task_to_end(&pueued.config_path, task, Duration::from_secs(2)).await;
    wait_for_no_leases(&daemon, Duration::from_secs(2)).await;
    let status = wait_for_pool_restored(&daemon, Duration::from_secs(5)).await;
    assert_eq!((status.held, status.free), (0, 4));
}

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

#[tokio::test]
#[serial_test::serial]
async fn sigterm_leaves_the_running_task_its_record_and_the_fifo_alone() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let mut daemon = pool_of_four(&pueued.config_path);
    let (_conn, id, task) = run(&daemon, request(&["sh", "-c", "sleep 300"])).await;
    wait_for_task_to_run(&pueued.config_path, task, Duration::from_secs(5)).await;
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
