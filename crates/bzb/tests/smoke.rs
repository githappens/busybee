//! The client end to end, against daemons of the test's own.

mod common;

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use common::{spawn_wedged_daemon, stderr, stdout, Busybee, PATIENCE};
use tempfile::TempDir;

#[test]
#[serial_test::serial]
fn the_task_s_exit_code_is_the_client_s_exit_code() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    // No self-signalled task: pueued wraps tasks in `sh -c`, and whether that
    // reports 130 or 128+N depends on the platform's /bin/sh.
    for (command, expected) in [
        ("exit 0", 0),
        ("exit 42", 42),
        ("this-command-does-not-exist", 127),
    ] {
        let out = busybee.run(&["--", "sh", "-c", command]);
        assert_eq!(
            out.status.code(),
            Some(expected),
            "`{command}` exited {:?}; stderr: {}",
            out.status.code(),
            stderr(&out)
        );
    }
}

#[test]
#[serial_test::serial]
fn a_cancelled_task_gives_its_client_130() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let mut client = busybee
        .cmd(&["--", "sleep", "30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the client");
    busybee.wait_for_a_running_task();

    let lease = busybee.status().expect("bzbd is up").leases[0].id;
    let cancelled = busybee.run(&["cancel", &lease.to_string()]);
    assert!(cancelled.status.success(), "stderr: {}", stderr(&cancelled));

    let status = client.wait().expect("wait for the cancelled client");
    assert_eq!(
        status.code(),
        Some(130),
        "exit code was {:?}",
        status.code()
    );
}

#[test]
#[serial_test::serial]
fn busybee_s_own_lines_go_to_stderr_in_lease_order() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let out = busybee.run(&["--", "sh", "-c", "printf hello"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    assert_eq!(stdout(&out), "hello", "stderr was: {}", stderr(&out));

    let stderr = stderr(&out);
    let mut rest = stderr.as_str();
    for expected in [
        "busybee: queued (0 ahead)\n",
        "busybee: running — ",
        "busybee: command exited 0 (elapsed ",
    ] {
        let at = rest
            .find(expected)
            .unwrap_or_else(|| panic!("{expected:?} is missing or out of order in {stderr:?}"));
        rest = &rest[at + expected.len()..];
    }
}

#[test]
#[serial_test::serial]
fn a_static_task_is_told_how_many_cores_it_holds() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let out = busybee.run(&[
        "--class",
        "static",
        "--cores",
        "2",
        "--",
        "sh",
        "-c",
        "echo $BUSYBEE_CORES",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "2");
}

#[test]
#[serial_test::serial]
fn sigint_while_queued_exits_130_and_drops_the_lease() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let mut running = busybee
        .cmd(&["--", "sleep", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the task that holds the machine");
    busybee.wait_for_a_running_task();

    let mut queued = busybee
        .cmd(&["--", "echo", "second"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the queued client");
    busybee.wait_for_leases(2);

    unsafe { libc::kill(queued.id() as i32, libc::SIGINT) };
    let interrupted = Instant::now();
    let status = queued.wait().expect("wait for the interrupted client");
    assert_eq!(
        status.code(),
        Some(130),
        "exit code was {:?}",
        status.code()
    );
    assert!(
        interrupted.elapsed() < Duration::from_secs(1),
        "the client took {:?} to exit",
        interrupted.elapsed()
    );

    busybee.wait_for_leases(1);
    let _ = running.kill();
    let _ = running.wait();
}

#[test]
#[serial_test::serial]
fn sigint_while_running_exits_130_and_kills_the_task() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let marker = busybee.tmp.path().join("survived");
    let mut client = busybee
        .cmd(&[
            "--",
            "sh",
            "-c",
            &format!("sleep 30; touch {}", marker.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the client");
    busybee.wait_for_a_running_task();

    unsafe { libc::kill(client.id() as i32, libc::SIGINT) };
    let status = client.wait().expect("wait for the interrupted client");

    assert_eq!(
        status.code(),
        Some(130),
        "exit code was {:?}",
        status.code()
    );
    busybee.wait_for_leases(0);
    assert!(!marker.exists(), "the cancelled task ran to the end");
}

#[test]
#[serial_test::serial]
fn a_task_that_owns_the_machine_makes_the_next_one_wait() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let mut first = busybee
        .cmd(&["--", "sleep", "2"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the first client");
    busybee.wait_for_a_running_task();

    let started = Instant::now();
    let second = busybee.run(&["--", "echo", "second"]);

    assert!(second.status.success(), "stderr: {}", stderr(&second));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the second task ran after {:?}; it should have waited",
        started.elapsed()
    );
    assert!(
        stderr(&second).contains("busybee: queued (1 ahead)"),
        "the queue position is missing from {:?}",
        stderr(&second)
    );
    first.wait().expect("wait for the first client");
}

/// githappens/busybee#64.
#[test]
#[serial_test::serial]
fn a_nested_busybee_passes_through_instead_of_deadlocking() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(&["--", bin, "--", "true"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("nested under lease"),
        "the pass-through line is missing from stdout (the parent task's stream): {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("busybee: running — "),
        "the outer client still takes a lease; stderr was {}",
        stderr(&out)
    );
}

#[test]
#[serial_test::serial]
fn a_gated_shell_string_that_gates_again_completes() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let inner = format!("{bin:?} -- true");
    let out = busybee.run_timed(&["--", "sh", "-c", &inner]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("nested under lease"),
        "stdout was {}",
        stdout(&out)
    );
}

#[test]
#[serial_test::serial]
fn a_self_gating_script_runs_under_an_outer_wrapper() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let script = busybee.tmp.path().join("build.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec {bin:?} -- printf hello\n"),
    )
    .expect("write build.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let out = busybee.run_timed(&["--", script.to_str().expect("utf-8 path")]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("hello"),
        "the script's output is missing from {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("nested under lease"),
        "stdout was {}",
        stdout(&out)
    );
}

#[test]
#[serial_test::serial]
fn a_nested_command_s_exit_code_is_the_outer_client_s() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(&["--", bin, "--", "sh", "-c", "exit 42"]);
    assert_eq!(out.status.code(), Some(42), "stderr: {}", stderr(&out));
}

#[test]
#[serial_test::serial]
fn a_nested_command_does_not_take_a_second_lease() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let mut outer = busybee
        .cmd(&["--", bin, "--", "sleep", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the nested client");
    busybee.wait_for_a_running_task();
    // Give the inner client time to submit, if it wrongly would.
    std::thread::sleep(Duration::from_millis(500));
    let status = busybee.status().expect("bzbd is up");
    assert_eq!(
        status.leases.len(),
        1,
        "nested busybee queued a second lease: {:?}",
        status.leases
    );

    let _ = outer.kill();
    let _ = outer.wait();
}

/// After bzbd dies and is restarted, it adopts the parent lease as `orphaned`
/// (bzbd.md §Failure and recovery); nesting must still find it.
#[test]
#[serial_test::serial]
fn a_nested_busybee_under_an_orphaned_parent_passes_through() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let go = busybee.tmp.path().join("go");
    let done = busybee.tmp.path().join("done");
    // The outer client dies with the daemon, so report through a file.
    let script = format!("while [ ! -e {go:?} ]; do sleep 0.1; done; {bin:?} -- touch {done:?}");
    let mut outer = busybee
        .cmd(&["--", "sh", "-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the outer client");
    busybee.wait_for_a_running_task();

    let pid = std::fs::read_to_string(busybee.state_dir().join("bzbd.pid"))
        .expect("read the daemon's pid file");
    let pid: i32 = pid.trim().parse().expect("a pid");
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = outer.wait();
    std::fs::write(&go, "").expect("release the task");

    let deadline = Instant::now() + PATIENCE;
    while !done.exists() {
        assert!(
            Instant::now() < deadline,
            "the nested call never ran; bzbd holds {:?}",
            busybee.status().map(|s| s.leases)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[serial_test::serial]
fn a_stale_lease_marker_does_not_skip_the_daemon() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let out = busybee
        .cmd(&["--", "true"])
        .env("BUSYBEE_LEASE", "999")
        .output()
        .expect("run busybee");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("busybee: queued"),
        "a stale marker skipped the daemon; stderr was {}",
        stderr(&out)
    );
    assert!(
        !stdout(&out).contains("nested under lease"),
        "stdout was {}",
        stdout(&out)
    );
}

/// `--detach` returns on `Queued`, so it can safely queue a second lease.
#[test]
#[serial_test::serial]
fn a_nested_detach_still_queues_its_own_lease() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(&[
        "--",
        "sh",
        "-c",
        &format!("{bin:?} --detach -- true; printf after"),
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("after"),
        "the script should continue after detach; stdout was {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("busybee: lease "),
        "detach still prints a lease id; stdout was {}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("nested under lease"),
        "detach must not pass through; stdout was {}",
        stdout(&out)
    );
}

#[test]
#[serial_test::serial]
fn a_detached_task_outlives_the_client_that_asked_for_it() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let marker = busybee.tmp.path().join("done");
    let start = Instant::now();
    let out = busybee.run(&[
        "--detach",
        "--",
        "sh",
        "-c",
        &format!("sleep 1; touch {}", marker.display()),
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "--detach blocked for {:?}",
        start.elapsed()
    );
    assert!(
        stdout(&out).starts_with("busybee: lease "),
        "stdout was: {}",
        stdout(&out)
    );

    busybee.wait_for_leases(0);
    assert!(marker.exists(), "the detached task never ran to the end");
}

#[test]
#[serial_test::serial]
fn cancel_ends_a_detached_lease() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let marker = busybee.tmp.path().join("finished");
    let out = busybee.run(&[
        "--detach",
        "--",
        "sh",
        "-c",
        &format!("sleep 30; touch {}", marker.display()),
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let lease = lease_id(&stdout(&out));
    busybee.wait_for_leases(1);

    let cancelled = busybee.run(&["cancel", &lease.to_string()]);
    assert!(cancelled.status.success(), "stderr: {}", stderr(&cancelled));

    busybee.wait_for_leases(0);
    assert!(!marker.exists(), "the cancelled task ran to the end");

    let again = busybee.run(&["cancel", &lease.to_string()]);
    assert!(!again.status.success(), "cancelling twice was accepted");
    assert!(
        stderr(&again).contains(&lease.to_string()),
        "the refusal must name the lease: {}",
        stderr(&again)
    );
}

fn lease_id(line: &str) -> u64 {
    line.trim()
        .strip_prefix("busybee: lease ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("no lease id in {line:?}"))
}

#[test]
#[serial_test::serial]
fn the_client_starts_the_daemon_it_needs() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    assert!(!busybee.socket().exists(), "bzbd was already running");

    let out = busybee.run(&["--", "sh", "-c", "printf up"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "up");
    assert!(busybee.socket().exists(), "bzbd was never started");
}

#[test]
#[serial_test::serial]
fn a_daemon_that_cannot_start_stops_the_command() {
    let tmp = TempDir::new().expect("create tempdir");
    // A state directory under a regular file cannot be created, even as root.
    let blocked = tmp.path().join("a-file");
    std::fs::write(&blocked, "not a directory").expect("write the file in the way");
    let marker = tmp.path().join("ran");

    let out = Command::new(env!("CARGO_BIN_EXE_busybee"))
        .env("BUSYBEE_STATE_DIR", blocked.join("state"))
        .args(["--", "touch"])
        .arg(&marker)
        .output()
        .expect("run busybee");

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("state directory"),
        "the reason is missing from {:?}",
        stderr(&out)
    );
    assert!(!marker.exists(), "the command ran without a daemon");
}

/// Every key, not only the ones the file mentions.
#[test]
fn config_show_prints_the_effective_config() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "pool_size = 7\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_busybee"))
        .env("BUSYBEE_CONFIG", &path)
        .args(["config", "show"])
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pool_size = 7"), "stdout was {stdout}");
    assert!(stdout.contains("max_concurrent = 4"), "stdout was {stdout}");
}

#[test]
fn config_reload_without_a_daemon_is_an_error_that_says_why() {
    let tmp = tempfile::tempdir().unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_busybee"))
        .env("BUSYBEE_STATE_DIR", tmp.path())
        .env("BUSYBEE_CONFIG", tmp.path().join("config.toml"))
        .args(["config", "reload"])
        .output()
        .unwrap();

    assert!(!out.status.success(), "reload succeeded with no daemon");
    assert!(
        stderr(&out).contains("bzbd is not running"),
        "stderr was {:?}",
        stderr(&out)
    );
}

/// Bounded, so a hang regression fails instead of stalling the suite.
async fn run_config_reload(state: &Path) -> Output {
    let run = tokio::process::Command::new(env!("CARGO_BIN_EXE_busybee"))
        .env("BUSYBEE_STATE_DIR", state)
        .env("BUSYBEE_CONFIG", state.join("config.toml"))
        .args(["config", "reload"])
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(PATIENCE, run)
        .await
        .expect("busybee config reload never exited")
        .expect("run busybee config reload")
}

/// A daemon that accepts and then fails (e.g. version refusal) is running.
#[tokio::test]
async fn config_reload_against_a_listening_daemon_does_not_call_it_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(tmp.path().join("bzbd.sock")).expect("bind");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        drop(stream);
    });

    let out = run_config_reload(tmp.path()).await;

    assert!(!out.status.success(), "reload succeeded against a failure");
    assert!(
        !stderr(&out).contains("is not running"),
        "a listening daemon was reported absent: {:?}",
        stderr(&out)
    );
}

#[tokio::test]
async fn config_reload_against_a_wedged_daemon_gives_up() {
    let tmp = tempfile::tempdir().unwrap();
    spawn_wedged_daemon(&tmp.path().join("bzbd.sock"));

    let out = run_config_reload(tmp.path()).await;

    assert!(!out.status.success(), "reload succeeded against silence");
    assert!(
        stderr(&out).contains("did not answer"),
        "stderr was {:?}",
        stderr(&out)
    );
}
