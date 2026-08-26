//! The client end to end, against daemons of the test's own: see
//! [`common::Busybee`]. `crates/bzb/tests/e2e_pool.rs` is the same client
//! under real builds sharing the pool.

mod common;

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use bzb_core::protocol::Response;
use common::{stderr, stdout, Busybee, PATIENCE};
use tempfile::TempDir;

#[test]
#[serial_test::serial]
fn the_task_s_exit_code_is_the_client_s_exit_code() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    // A task that dies by a signal of its own is deliberately not here: pueued
    // runs every task under `sh -c`, and whether that wrapper reports the
    // signal (pueue: `Killed`, so 130) or collapses it into the shell's own
    // 128+N exit code depends on whether the platform's /bin/sh execs a single
    // simple command instead of forking it. That is a property of /bin/sh, not
    // of busybee. The exit code for a task busybee itself ends is fixed, and
    // `a_cancelled_task_gives_its_client_130` covers it.
    for (command, expected) in [
        ("exit 0", 0),
        ("exit 42", 42),
        // sh's own code for a command it cannot find.
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

/// A lease cancelled out from under a client that is still attached: bzbd
/// kills the task and reports the lease finished as killed, which is the 130
/// the client exits with. Nothing about it goes through the task's own shell,
/// so it holds wherever the tests run.
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

/// stdout is the task's (`docs/design/bzbd.md` §Client output contract), so
/// every line busybee writes about itself has to be on stderr — and in the
/// order the lease reached them.
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

/// `--class`/`--cores` reach the task through the daemon's injection: the
/// task is told the static share it actually holds.
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

/// Ctrl-C closes the connection, and the connection is the lease: bzbd drops
/// it, and the client is not waiting around for it to say so.
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

    // Only the running task is left; the interrupted one's lease went with its
    // connection.
    busybee.wait_for_leases(1);
    let _ = running.kill();
    let _ = running.wait();
}

/// The same for a task that is already on the machine: the client hangs up,
/// bzbd kills the task and the lease goes with it.
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

/// A `none` task takes the whole pool (`docs/design/bzbd.md` §Admission
/// policy), so a second one waits for it rather than running alongside.
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

/// `busybee -- <cmd>` where `<cmd>` itself invokes `busybee --` used to
/// deadlock: the outer lease holds the machine, the inner client queues
/// behind it. The nested client now sees `BUSYBEE_LEASE` and execs.
/// `docs/design/bzbd.md` §Nesting; githappens/busybee#64.
#[test]
#[serial_test::serial]
fn a_nested_busybee_passes_through_instead_of_deadlocking() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(&["--", bin, "--", "true"], PATIENCE);
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

/// The issue's reproduction: a shell string whose body gates again.
#[test]
#[serial_test::serial]
fn a_gated_shell_string_that_gates_again_completes() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let inner = format!("{bin:?} -- true");
    let out = busybee.run_timed(&["--", "sh", "-c", &inner], PATIENCE);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("nested under lease"),
        "stdout was {}",
        stdout(&out)
    );
}

/// A script that gates its own work, wrapped by a caller who also gates:
/// the recommended pattern composing with itself.
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");
    }

    let out = busybee.run_timed(&["--", script.to_str().expect("utf-8 path")], PATIENCE);
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

/// Nested pass-through still relays the inner command's exit code through
/// the outer client, the same way a non-nested lease does.
#[test]
#[serial_test::serial]
fn a_nested_command_s_exit_code_is_the_outer_client_s() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(&["--", bin, "--", "sh", "-c", "exit 42"], PATIENCE);
    assert_eq!(out.status.code(), Some(42), "stderr: {}", stderr(&out));
}

/// A nested command is not a second lease: the parent already holds the
/// machine, and status must not grow a queued sibling behind it.
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
    // The inner client starts after the task is live; give it a moment to
    // submit if it is going to. Pass-through must leave the count at one.
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

/// Exporting a lease id that is not live must not disable gating: that
/// would be the silent fallback a stale `BUSYBEE_LEASE` in the environment
/// would otherwise become.
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

/// `--detach` is not pass-through: it is asking to queue a second lease
/// and return, and it can, because it returns on `Queued`.
#[test]
#[serial_test::serial]
fn a_nested_detach_still_queues_its_own_lease() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let bin = env!("CARGO_BIN_EXE_busybee");
    let out = busybee.run_timed(
        &[
            "--",
            "sh",
            "-c",
            &format!("{bin:?} --detach -- true; printf after"),
        ],
        PATIENCE,
    );
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

/// `--detach` hands the task to bzbd and returns; the lease outlives the
/// client that asked for it.
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

    // The client is gone; the task runs to the end anyway.
    busybee.wait_for_leases(0);
    assert!(marker.exists(), "the detached task never ran to the end");
}

/// Nothing holds a detached lease, so `cancel` is the only way to end one.
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

    // A lease that is already gone is an error, not a silent success.
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

/// The client starts bzbd when there is none; every other test relies on it,
/// this one says so.
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

/// A daemon that cannot start is not a reason to run the command ungoverned
/// (`docs/design/bzbd.md` §Failure and recovery): the client says why and the
/// command never runs.
#[test]
#[serial_test::serial]
fn a_daemon_that_cannot_start_stops_the_command() {
    let tmp = TempDir::new().expect("create tempdir");
    // A state directory under a regular file: it cannot be created, whoever
    // the tests are running as.
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

/// `config show` is the one command whose result is the configuration itself,
/// so it prints to stdout — and it prints every key, not only the ones the
/// file happens to mention.
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

/// Nothing to reload without a daemon, and starting one to answer would be a
/// surprise. Say which socket was tried instead.
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

/// Runs `busybee config reload` against a state directory of the test's own,
/// and gives up on the process if it never exits — the thing under test here is
/// a command that hangs, so without a deadline of the test's own a regression
/// stalls the suite instead of failing it.
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

/// A daemon that accepts the connection and then fails — a dropped socket, a
/// refused protocol version — is running. Calling that "not running" puts a
/// false diagnosis at the top of the error, and the version refusal is exactly
/// what an upgraded client meets across an in-place upgrade.
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

/// The handshake has a deadline of its own; the request after it does not. A
/// daemon that pongs and then wedges would hold this one-shot command open for
/// as long as it stays wedged.
#[tokio::test]
async fn config_reload_against_a_wedged_daemon_gives_up() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(tmp.path().join("bzbd.sock")).expect("bind");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(reader).lines();
        lines.next_line().await.expect("read").expect("a hello");
        let pong = Response::Pong {
            version: "test".into(),
            pid: std::process::id(),
        };
        let line = format!("{}\n", serde_json::to_string(&pong).expect("encode"));
        writer.write_all(line.as_bytes()).await.expect("write");
        // Handshaken, and silent from here: nothing answers the reload request
        // and nothing closes the connection on it.
        std::future::pending::<()>().await;
    });

    let out = run_config_reload(tmp.path()).await;

    assert!(!out.status.success(), "reload succeeded against silence");
    assert!(
        stderr(&out).contains("did not answer"),
        "stderr was {:?}",
        stderr(&out)
    );
}
