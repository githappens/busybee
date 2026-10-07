//! Injection end to end: GNU make on a Makefile of `sleep` targets, an
//! isolated `pueued`, and the peak concurrency each target logs to `counts.log`.

mod common;

use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use bzb_core::{
    classify::Class,
    daemon::Connection,
    protocol::{LeaseEvent, LeaseRequest, Request, Response},
};
use bzb_test_support::PueuedFixture;
use common::{event, leases_json, status, submit, Fixture, PATIENCE};
use tempfile::TempDir;

/// Sixteen independent targets plus an `env` target that records the
/// environment make hands its children.
const MAKEFILE: &str = "\
T := t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 t11 t12 t13 t14 t15 t16
all: env $(T)
env:
\t@env > env.log
$(T):
\t@f=run/$@.$$$$; touch $$f; sleep 0.5; ls run | wc -l >> counts.log; rm $$f
";

/// GNU make ≥ 4.4 (fifo jobserver) is on `PATH`; otherwise says why not.
fn make_available() -> bool {
    let version = Command::new("make")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            let text = String::from_utf8_lossy(&out.stdout);
            let word = text.lines().next()?.split_whitespace().last()?.to_string();
            let mut parts = word.split('.').map(|p| p.parse::<u32>().ok());
            Some((parts.next()??, parts.next()??))
        });
    match version {
        Some(v) if v >= (4, 4) => true,
        Some((major, minor)) => {
            eprintln!("skipping: make {major}.{minor} is older than 4.4");
            false
        }
        None => {
            eprintln!("skipping: make not found in PATH");
            false
        }
    }
}

/// A fresh build directory holding the Makefile and an empty `run/`.
fn build_dir() -> TempDir {
    let dir = TempDir::new().expect("create tempdir");
    std::fs::create_dir(dir.path().join("run")).expect("create run/");
    std::fs::write(dir.path().join("Makefile"), MAKEFILE).expect("write the Makefile");
    dir
}

fn counts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("counts.log"))
        .expect("read counts.log")
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

fn peak(lines: &[String]) -> u32 {
    lines
        .iter()
        .filter_map(|l| l.parse().ok())
        .max()
        .expect("at least one count")
}

fn env_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("env.log")).expect("read env.log")
}

fn request(argv: &[&str], cwd: &Path) -> LeaseRequest {
    common::request_in(argv, cwd)
}

async fn queued(conn: &mut Connection) -> usize {
    match event(conn).await {
        LeaseEvent::Queued { ahead, .. } => ahead,
        other => panic!("expected a Queued event, got {other:?}"),
    }
}

/// `(class, cores, pool_size, peers)` from the next event.
async fn admitted(conn: &mut Connection) -> (String, u32, u32, usize) {
    match event(conn).await {
        LeaseEvent::Admitted {
            class,
            cores,
            pool_size,
            peers,
            ..
        } => (class, cores, pool_size, peers),
        other => panic!("expected an Admitted event, got {other:?}"),
    }
}

async fn notice(conn: &mut Connection) -> String {
    match event(conn).await {
        LeaseEvent::Notice { text } => text,
        other => panic!("expected a Notice event, got {other:?}"),
    }
}

async fn finished(conn: &mut Connection) -> i32 {
    match event(conn).await {
        LeaseEvent::Finished { exit_code, .. } => exit_code,
        other => panic!("expected a Finished event, got {other:?}"),
    }
}

/// A daemon on `pool_size` tokens, talking to the isolated pueued at `config`.
fn pool(config: &Path, pool_size: u32) -> Fixture {
    Fixture::start_with(
        Some(&format!("pool_size = {pool_size}\n")),
        &[("PUEUE_CONFIG_PATH", config.display().to_string())],
    )
}

#[tokio::test]
async fn a_make_lease_joins_the_pool_and_returns_every_token() {
    if !make_available() {
        return;
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = build_dir();

    let mut conn = submit(&daemon, request(&["make"], dir.path())).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(
        admitted(&mut conn).await,
        ("jobserver".to_string(), 4, 4, 0),
        "a jobserver lease alone is told the whole pool as its share"
    );
    assert_eq!(finished(&mut conn).await, 0);

    // Four tokens plus the implicit job.
    let peak = peak(&counts(dir.path()));
    assert!(
        (2..=5).contains(&peak),
        "peak concurrency {peak}, expected 2..=5"
    );
    let status = status(&daemon).await;
    assert_eq!(status.free, 4, "make must return every token");
    assert_eq!(status.held, 0);

    let env = env_log(dir.path());
    let fifo = daemon.fifo_path();
    let makeflags = env
        .lines()
        .find(|l| l.starts_with("MAKEFLAGS="))
        .unwrap_or_else(|| panic!("no MAKEFLAGS in the task's environment:\n{env}"));
    assert!(
        makeflags.contains(&format!("--jobserver-auth=fifo:{}", fifo.display())),
        "MAKEFLAGS was {makeflags:?}, expected the fifo {}",
        fifo.display()
    );
    assert!(
        env.lines().any(|l| l == "BUSYBEE_CLASS=jobserver"),
        "no BUSYBEE_CLASS=jobserver in:\n{env}"
    );
    assert!(
        env.lines().any(|l| l == "BUSYBEE_CORES=4"),
        "no BUSYBEE_CORES=4 in:\n{env}"
    );
    assert!(
        env.lines().any(|l| l == "BUSYBEE_LEASE=1"),
        "no BUSYBEE_LEASE=1 in:\n{env}"
    );
}

/// Before `Queued`, which is where a `--detach` client stops reading.
#[tokio::test]
async fn a_plan_notice_is_streamed_before_admission() {
    if !make_available() {
        return;
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = build_dir();

    let mut conn = submit(&daemon, request(&["make", "-j16"], dir.path())).await;
    let text = notice(&mut conn).await;
    assert!(
        text.contains("-j16"),
        "the notice must name the flag, got {text:?}"
    );
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await.0, "jobserver");
    assert_eq!(finished(&mut conn).await, 0);
}

#[tokio::test]
async fn a_static_lease_holds_its_cores_for_its_lifetime() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = TempDir::new().expect("create tempdir");

    let mut request = request(
        &["sh", "-c", "echo $BUSYBEE_CORES > cores.txt; sleep 2"],
        dir.path(),
    );
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(3);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await, ("static".to_string(), 3, 4, 0));

    let running = status(&daemon).await;
    assert_eq!(running.held, 3, "status was {running:?}");
    assert_eq!(running.free, 1, "status was {running:?}");
    assert_eq!(running.leases[0].cores, Some(3), "status was {running:?}");

    assert_eq!(finished(&mut conn).await, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cores.txt")).expect("read cores.txt"),
        "3\n"
    );
    let done = status(&daemon).await;
    assert_eq!(done.free, 4, "status was {done:?}");
    assert_eq!(done.held, 0, "status was {done:?}");
}

/// Spec table row "client disconnects while running".
#[tokio::test]
async fn a_killed_static_task_keeps_its_tokens_until_it_is_gone() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = TempDir::new().expect("create tempdir");

    let mut request = request(&["sh", "-c", "trap '' TERM; sleep 30"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(3);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await, ("static".to_string(), 3, 4, 0));
    assert_eq!(status(&daemon).await.free, 1);

    drop(conn);
    let deadline = Instant::now() + PATIENCE;
    let torn_down = loop {
        let status = status(&daemon).await;
        if status.leases.is_empty() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the lease never left: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        torn_down.free, 1,
        "the tokens went back while the task was still running: {torn_down:?}"
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        let status = status(&daemon).await;
        if status.free == 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tokens never came back after the kill: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn a_teardown_in_flight_stays_in_leases_json_until_the_task_is_gone() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = TempDir::new().expect("create tempdir");

    let mut request = request(&["sh", "-c", "trap '' TERM; sleep 30"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(3);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    let task_id = match event(&mut conn).await {
        LeaseEvent::Admitted { pueue_task_id, .. } => pueue_task_id,
        other => panic!("expected an Admitted event, got {other:?}"),
    };

    drop(conn);
    let deadline = Instant::now() + PATIENCE;
    let records = loop {
        let records = leases_json(&daemon);
        if records.iter().any(|r| r["killing"] == true) {
            break records;
        }
        assert!(
            Instant::now() < deadline,
            "the teardown never reached leases.json: {records:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(records.len(), 1, "records were {records:?}");
    assert_eq!(records[0]["cores_held"], 3, "records were {records:?}");
    assert_eq!(
        records[0]["pueue_task_id"], task_id,
        "records were {records:?}"
    );
    assert_eq!(
        status(&daemon).await.held,
        3,
        "the file and the daemon disagree about what is held"
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        let records = leases_json(&daemon);
        if records.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the teardown never left leases.json: {records:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(status(&daemon).await.free, 4);
}

/// A stopped pueued leaves the submission unanswered for as long as needed.
#[tokio::test]
async fn a_drained_grant_is_in_leases_json_before_pueued_answers() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = TempDir::new().expect("create tempdir");
    common::signal(pueued.pid(), libc::SIGSTOP);

    let mut request = request(&["sh", "-c", "sleep 30"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(3);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);

    let deadline = Instant::now() + PATIENCE;
    let records = loop {
        let records = leases_json(&daemon);
        if records.iter().any(|r| r["cores_held"] == 3) {
            break records;
        }
        assert!(
            Instant::now() < deadline,
            "the grant never reached leases.json while the submission was unanswered: {records:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(records.len(), 1, "records were {records:?}");
    assert!(
        records[0]["pueue_task_id"].is_null(),
        "pueued cannot have answered while stopped: {records:?}"
    );
    assert_eq!(
        records[0]["killing"], false,
        "a grant in flight is not a teardown: {records:?}"
    );

    common::signal(pueued.pid(), libc::SIGCONT);
    let (class, cores, _, _) = admitted(&mut conn).await;
    assert_eq!((class.as_str(), cores), ("static", 3));
    let records = leases_json(&daemon);
    assert_eq!(records.len(), 1, "records were {records:?}");
    assert!(
        !records[0]["pueue_task_id"].is_null(),
        "the answer puts the task id on record: {records:?}"
    );
    assert_eq!(records[0]["killing"], false, "records were {records:?}");
    assert_eq!(records[0]["cores_held"], 3, "records were {records:?}");
    assert_eq!(status(&daemon).await.held, 3);
}

/// make's concurrency drops to `pool − held + 1`.
#[tokio::test]
async fn a_static_drain_throttles_a_running_make() {
    if !make_available() {
        return;
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = build_dir();

    let mut make = submit(&daemon, request(&["make"], dir.path())).await;
    assert_eq!(queued(&mut make).await, 0);
    assert_eq!(admitted(&mut make).await.0, "jobserver");

    let mut request = request(
        &["sh", "-c", "echo START >> counts.log; sleep 3"],
        dir.path(),
    );
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(2);
    let asked = Instant::now();
    let mut held = submit(&daemon, request).await;
    assert_eq!(queued(&mut held).await, 1);
    // Well within the 2 s drain deadline: make returns a token per 0.5 s job.
    assert_eq!(admitted(&mut held).await, ("static".to_string(), 2, 4, 1));
    let drain = asked.elapsed();
    assert!(drain < Duration::from_secs(2), "the drain took {drain:?}");

    assert_eq!(finished(&mut make).await, 0);
    assert_eq!(finished(&mut held).await, 0);

    let lines = counts(dir.path());
    let start = lines
        .iter()
        .position(|l| l == "START")
        .expect("the static task marked its start in counts.log");
    let after = &lines[start + 1..];
    assert!(
        !after.is_empty(),
        "make finished before the static task started; nothing to measure in {lines:?}"
    );
    let peak = peak(after);
    assert!(
        peak <= 3,
        "make ran {peak} jobs at once beside a lease holding 2 of 4 tokens: {lines:?}"
    );
}

/// A script named `cmake` that runs make stands in for a generated build.
#[tokio::test]
async fn a_cmake_build_loses_the_callers_parallel_level() {
    if !make_available() {
        return;
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = build_dir();
    let cmake = dir.path().join("cmake");
    std::fs::write(&cmake, "#!/bin/sh\nexec make env\n").expect("write the cmake stand-in");
    let mut mode = std::fs::metadata(&cmake).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&cmake, mode).expect("chmod the cmake stand-in");

    let mut request = request(
        &[cmake.to_str().expect("utf-8"), "--build", "."],
        dir.path(),
    );
    request
        .env
        .insert("CMAKE_BUILD_PARALLEL_LEVEL".to_string(), "9".to_string());
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await.0, "jobserver");
    assert_eq!(finished(&mut conn).await, 0);

    let env = env_log(dir.path());
    assert!(
        !env.lines()
            .any(|l| l.starts_with("CMAKE_BUILD_PARALLEL_LEVEL=")),
        "CMAKE_BUILD_PARALLEL_LEVEL survived into the task's environment:\n{env}"
    );
    assert!(
        env.lines()
            .any(|l| l.starts_with("MAKEFLAGS=") && l.contains("--jobserver-auth=fifo:")),
        "no jobserver MAKEFLAGS in:\n{env}"
    );
}

/// Spec table row "a drain comes up short, or collects nothing at all".
#[tokio::test]
async fn an_empty_drain_starts_on_the_implicit_token_and_says_so() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = Fixture::start_with(
        Some("pool_size = 1\ndrain_deadline_ms = 100\n"),
        &[(
            "PUEUE_CONFIG_PATH",
            pueued.config_path.display().to_string(),
        )],
    );
    let dir = TempDir::new().expect("create tempdir");

    let mut first = request(&["sh", "-c", "sleep 2"], dir.path());
    first.class_override = Some(Class::Static);
    let mut first = submit(&daemon, first).await;
    assert_eq!(queued(&mut first).await, 0);
    assert_eq!(admitted(&mut first).await, ("static".to_string(), 1, 1, 0));
    assert_eq!(status(&daemon).await.free, 0);

    let mut second = request(&["sh", "-c", "echo $BUSYBEE_CORES > cores.txt"], dir.path());
    second.class_override = Some(Class::Static);
    let mut second = submit(&daemon, second).await;
    assert_eq!(queued(&mut second).await, 1);
    let text = notice(&mut second).await;
    assert!(
        text.contains("implicit"),
        "the notice must say the task runs on the implicit token, got {text:?}"
    );
    assert_eq!(admitted(&mut second).await, ("static".to_string(), 1, 1, 1));
    assert_eq!(finished(&mut second).await, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cores.txt")).expect("read cores.txt"),
        "1\n"
    );
    // The first lease still holds the only token; the second held none.
    let status_while_first_runs = status(&daemon).await;
    assert_eq!(
        status_while_first_runs.held, 1,
        "status was {status_while_first_runs:?}"
    );

    assert_eq!(finished(&mut first).await, 0);
    assert_eq!(status(&daemon).await.free, 1);
}

/// `DrainFailed`: the tokens go back once the poll gives up on pueued.
#[tokio::test]
async fn a_rejected_submission_returns_the_drained_tokens() {
    let Some(mut pueued) = PueuedFixture::try_start() else {
        return;
    };
    // bzbd respawns pueued off its `PATH`; an empty one means it never comes back.
    let nothing_on_path = TempDir::new().expect("create tempdir");
    let daemon = Fixture::start_with(
        Some("pool_size = 2\n"),
        &[
            (
                "PUEUE_CONFIG_PATH",
                pueued.config_path.display().to_string(),
            ),
            ("PATH", nothing_on_path.path().display().to_string()),
        ],
    );
    pueued.kill();
    let dir = TempDir::new().expect("create tempdir");

    let mut request = request(&["sh", "-c", "true"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(2);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    let text = notice(&mut conn).await;
    assert!(text.contains("could not start"), "got {text:?}");
    assert_ne!(finished(&mut conn).await, 0);

    // Until the poll settles it, the tokens still count as held.
    let gone = status(&daemon).await;
    assert!(gone.leases.is_empty(), "status was {gone:?}");
    assert_eq!(gone.free + gone.held, 2, "status was {gone:?}");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let status = status(&daemon).await;
        if status.free == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the drained tokens never came back: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn a_grant_that_cannot_be_recorded_is_not_started() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 2);
    let dir = TempDir::new().expect("create tempdir");
    // A directory at `leases.json.tmp` fails every write like a full disk.
    std::fs::create_dir(daemon.state_dir().join("leases.json.tmp")).expect("plant the directory");

    let mut request = request(&["sh", "-c", "touch ran"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(2);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    let text = notice(&mut conn).await;
    assert!(text.contains("record"), "got {text:?}");
    assert_ne!(finished(&mut conn).await, 0);

    let after = status(&daemon).await;
    assert!(after.leases.is_empty(), "status was {after:?}");
    assert_eq!((after.free, after.held), (2, 0), "status was {after:?}");
    assert!(
        !dir.path().join("ran").exists(),
        "the task ran without its grant on record"
    );
}

#[tokio::test]
async fn a_pool_larger_than_the_fifo_is_refused_at_startup() {
    let tmp = TempDir::new().expect("create tempdir");
    let config = common::isolated_config(tmp.path());
    std::fs::write(&config, "pool_size = 4097\n").expect("write the config");
    let out = Command::new(common::BZBD)
        .env("BUSYBEE_STATE_DIR", tmp.path().join("state"))
        .env("BUSYBEE_CONFIG", &config)
        .output()
        .expect("run bzbd");

    assert!(!out.status.success(), "bzbd exited {}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pool_size") && stderr.contains("4097"),
        "stderr was {stderr:?}"
    );
    assert!(
        !stderr.contains("panicked"),
        "a configuration error must not panic: {stderr:?}"
    );
}

/// Spec §Configuration: a reload never takes back what a lease holds.
#[tokio::test]
async fn a_reloaded_pool_size_resizes_the_fifo_around_what_is_held() {
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = TempDir::new().expect("create tempdir");

    let mut request = request(&["sh", "-c", "sleep 2"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(3);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await, ("static".to_string(), 3, 4, 0));
    assert_eq!(status(&daemon).await.free, 1);

    daemon.write_config("pool_size = 6\n");
    reload(&daemon).await;
    let grown = status(&daemon).await;
    assert_eq!(
        (grown.pool_size, grown.free, grown.held),
        (6, 3, 3),
        "status was {grown:?}"
    );

    daemon.write_config("pool_size = 2\n");
    reload(&daemon).await;
    let shrunk = status(&daemon).await;
    assert_eq!(
        (shrunk.pool_size, shrunk.free, shrunk.held),
        (2, 0, 3),
        "the shrink took tokens the lease holds: {shrunk:?}"
    );
    let log = std::fs::read_to_string(daemon.log_path()).expect("read the log");
    assert!(
        log.lines()
            .any(|l| l.contains("WARN") && l.contains("shrank")),
        "no warning about the unfinished shrink in:\n{log}"
    );

    assert_eq!(finished(&mut conn).await, 0);
    // Shorter than the accounting interval on purpose: the release itself must
    // finish the shrink, not the periodic check.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = status(&daemon).await;
        assert!(
            status.free <= 2,
            "the whole grant went back to a pool that shrank under it: {status:?}"
        );
        if status.free == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the release did not finish the shrink: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A jobserver build returns tokens straight to the fifo, bypassing
/// `release`; the debt is collected from there, and only once.
#[tokio::test]
async fn a_shrink_under_a_make_is_collected_from_what_it_returns() {
    if !make_available() {
        return;
    }
    let Some(pueued) = PueuedFixture::try_start() else {
        return;
    };
    let daemon = pool(&pueued.config_path, 4);
    let dir = build_dir();

    let mut make = submit(&daemon, request(&["make"], dir.path())).await;
    assert_eq!(queued(&mut make).await, 0);
    assert_eq!(admitted(&mut make).await.0, "jobserver");
    // Only a shrink under a build holding the whole pool owes anything.
    let deadline = Instant::now() + PATIENCE;
    while status(&daemon).await.free > 0 {
        assert!(
            Instant::now() < deadline,
            "make never took every token of the pool"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    daemon.write_config("pool_size = 2\n");
    reload(&daemon).await;
    let log = std::fs::read_to_string(daemon.log_path()).expect("read the log");
    assert!(
        log.lines()
            .any(|l| l.contains("WARN") && l.contains("shrank")),
        "the shrink found its tokens free, so nothing is owed; log:\n{log}"
    );
    assert_eq!(finished(&mut make).await, 0);

    // Shorter than the accounting interval on purpose, as above.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = status(&daemon).await;
        if status.free == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the shrink was not collected from what make returned: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Collected is paid: the next static grant comes back whole.
    let mut request = request(&["true"], dir.path());
    request.class_override = Some(Class::Static);
    request.cores_wanted = Some(2);
    let mut conn = submit(&daemon, request).await;
    assert_eq!(queued(&mut conn).await, 0);
    assert_eq!(admitted(&mut conn).await, ("static".to_string(), 2, 2, 0));
    assert_eq!(finished(&mut conn).await, 0);
    let status = status(&daemon).await;
    assert_eq!(
        (status.free, status.held),
        (2, 0),
        "a debt collected from the fifo was withheld again from a release: {status:?}"
    );
}

async fn reload(daemon: &Fixture) {
    let mut conn = common::connect(daemon).await;
    conn.send(Request::ConfigReload)
        .await
        .expect("send a reload request");
    match conn.recv().await.expect("recv the reload reply") {
        Response::ConfigReloaded { .. } => {}
        other => panic!("expected a reload confirmation, got {other:?}"),
    }
}

/// Spec table row "fifo accounting drift".
#[tokio::test]
async fn extra_tokens_in_the_fifo_are_drained_by_the_accounting_check() {
    let daemon = Fixture::start_on("pool_size = 4\n");

    // A misbehaving tool writing two bytes it never read.
    std::fs::write(daemon.fifo_path(), "++").expect("write into the fifo");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let free = status(&daemon).await.free;
        if free == 4
            && std::fs::read_to_string(daemon.log_path())
                .expect("read the log")
                .contains("WARN")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "free was still {free} after 15 s; log:\n{}",
            std::fs::read_to_string(daemon.log_path()).expect("read the log")
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let log = std::fs::read_to_string(daemon.log_path()).expect("read the log");
    assert!(
        log.lines()
            .any(|l| l.contains("WARN") && l.contains("excess")),
        "no warning about the excess tokens in:\n{log}"
    );
}
