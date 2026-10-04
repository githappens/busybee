//! Real GNU make builds taking, sharing and giving back a six-token pool. Peak
//! concurrency comes from the jobs' own counts ([`bzb_test_support::counter`]).
//! Builds run without `-j`: a user `-j` bypasses the injected jobserver.

mod common;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Output, Stdio},
    time::{Duration, SystemTime},
};

use bzb_test_support::counter;
use common::{stderr, stdout, Busybee};
use regex::Regex;

/// The drain deadline is 15x the default: make re-reads a token as soon as it
/// returns one, so draining off a busy build is a race — harder still when two
/// builds are competing. At 2 s a 3-token drain fell short 6/25 times; at 10 s,
/// 0/25 (slowest ~3 s) against a single build. Two competing builds require a
/// wider budget to stay reliable on loaded CI runners (macOS in particular).
const CONFIG: &str = "pool_size = 6\nmax_concurrent = 4\ndrain_deadline_ms = 30000\n";
const POOL: u32 = 6;

/// The pool plus the one job a jobserver build runs without a token.
const CEILING: u32 = POOL + 1;

const SHARED_CEILING: u32 = POOL + 2;

/// While a static task holds three tokens.
const THROTTLED_CEILING: u32 = POOL - 3 + 1;

/// `None` (self-skip) without `pueued` or a fifo-jobserver `make`.
fn fixture() -> Option<Busybee> {
    if !counter::available("make", (4, 4)) {
        return None;
    }
    Busybee::start_on(CONFIG)
}

#[test]
#[serial_test::serial]
fn one_build_alone_gets_the_whole_pool() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("alone");
    counter::make_build(&build, 16, "0.4");

    let out = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .output()
        .expect("run the build");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let peak = counter::peak(&build, 16);
    assert!(
        (5..=CEILING).contains(&peak),
        "peak concurrency {peak}, expected 5..={CEILING}: \
         a build alone must actually use the pool it was given"
    );
    assert_eq!(stdout(&out), "", "stdout belongs to the tool");
    assert_preamble(
        &out,
        &running(
            "make",
            &format!("jobserver, sharing {POOL}-token pool with 0 other tasks"),
        ),
    );
}

#[test]
#[serial_test::serial]
fn two_builds_share_the_pool_and_neither_starves() {
    let Some(busybee) = fixture() else {
        return;
    };
    // One directory, so each sample carries the combined total too.
    let build = busybee.tmp.path().join("shared");
    counter::make_build(&build, 24, "0.4");

    let clients: Vec<_> = ["a", "b"]
        .iter()
        .map(|name| {
            busybee
                .cmd(&["--", "make", "run"])
                .current_dir(&build)
                .env("COUNTER_NAME", name)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start a build")
        })
        .collect();
    let outs: Vec<Output> = clients
        .into_iter()
        .map(|client| client.wait_with_output().expect("wait for a build"))
        .collect();

    for out in &outs {
        assert!(out.status.success(), "stderr: {}", stderr(out));
        assert_preamble(
            out,
            &running(
                "make",
                &format!(r"jobserver, sharing {POOL}-token pool with \d+ other tasks?"),
            ),
        );
    }

    let samples = counter::samples(&build);
    assert_eq!(samples.len(), 48, "both builds must run all their targets");
    let combined = samples.iter().map(|s| s.total).max().expect("samples");
    assert!(
        combined <= SHARED_CEILING,
        "combined peak concurrency {combined}, expected at most {SHARED_CEILING}"
    );
    // Both conditions in one sample: otherwise taking turns would also pass.
    for name in ["a", "b"] {
        assert!(
            samples
                .iter()
                .any(|s| s.name == name && s.own >= 2 && s.total > s.own),
            "build {name} never held a token while a job of the other build \
             was running: the two were serialised, not sharing"
        );
    }
}

#[test]
#[serial_test::serial]
fn a_static_task_drains_the_pool_and_hands_it_back() {
    let Some(busybee) = fixture() else {
        return;
    };
    // Outlasts a full 30 s drain plus the 2 s static task.
    const TARGETS: u32 = 400;
    let build = busybee.tmp.path().join("drained");
    counter::make_build(&build, TARGETS, "0.5");

    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for_a_running_task();

    let held = busybee.tmp.path().join("held");
    let handed_back = busybee.tmp.path().join("handed-back");
    let out = busybee
        .cmd(&[
            "--class",
            "static",
            "--cores",
            "3",
            "--",
            "sh",
            "-c",
            &format!(
                "echo $BUSYBEE_CORES; touch {}; sleep 2; touch {}",
                held.display(),
                handed_back.display()
            ),
        ])
        .output()
        .expect("run the static task");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "3\n",
        "the task is told the cores it holds, and nothing else is on its stdout"
    );
    assert_preamble(
        &out,
        &running(
            "<shell>",
            &format!(r"static, holding 3/{POOL} cores \(1 other task active\)"),
        ),
    );

    // Relies on sub-second mtimes (APFS, ext4).
    let (from, to) = (mtime(&held), mtime(&handed_back));
    let during: Vec<u32> = counter::samples(&build)
        .iter()
        .filter(|s| s.at >= from && s.at <= to)
        .map(|s| s.total)
        .collect();
    assert!(
        during.len() >= 2,
        "the build logged {} job(s) while the static task held its cores; \
         it has to be running through the window for this to mean anything",
        during.len()
    );
    let peak = during.iter().copied().max().expect("samples in the window");
    assert!(
        peak <= THROTTLED_CEILING,
        "the build ran {peak} jobs at once while three of {POOL} tokens were held, \
         expected at most {THROTTLED_CEILING}"
    );

    let make = make.wait_with_output().expect("wait for the build");
    assert!(make.status.success(), "stderr: {}", stderr(&make));
    let overall = counter::peak(&build, TARGETS as usize);
    assert!(
        (THROTTLED_CEILING + 1..=CEILING).contains(&overall),
        "peak concurrency {overall} over the whole build, expected \
         {}..={CEILING}: a build that never exceeded {THROTTLED_CEILING} \
         with the pool to itself would make the throttled window above vacuous",
        THROTTLED_CEILING + 1
    );

    assert_pool_idle(&busybee);
}

/// The static fair share counts every admitted lease: with two builds live it
/// is `ceil(6 / 3) = 2`, so a request for three is clamped to two.
#[test]
#[serial_test::serial]
fn a_static_task_beside_two_builds_gets_a_third_of_the_pool() {
    let Some(busybee) = fixture() else {
        return;
    };
    /// What `--cores 3` is clamped to beside two admitted builds.
    const GRANTED: u32 = 2;
    /// Two builds' implicit jobs plus the tokens the static task leaves.
    const THREE_WAY_CEILING: u32 = POOL - GRANTED + 2;
    // Per build; together they outlast a 30 s drain plus the 2 s task.
    const TARGETS: u32 = 200;
    // One directory, so each sample carries the combined total.
    let build = busybee.tmp.path().join("three-way");
    counter::make_build(&build, TARGETS, "0.5");

    let builds: Vec<_> = ["a", "b"]
        .iter()
        .map(|name| {
            busybee
                .cmd(&["--", "make", "run"])
                .current_dir(&build)
                .env("COUNTER_NAME", name)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start a build")
        })
        .collect();
    busybee.wait_for("both builds to be admitted", |status| {
        status
            .leases
            .iter()
            .filter(|l| l.tool == "make" && l.state == "running")
            .count()
            == 2
    });

    let held = busybee.tmp.path().join("held");
    let handed_back = busybee.tmp.path().join("handed-back");
    let out = busybee
        .cmd(&[
            "--class",
            "static",
            "--cores",
            "3",
            "--",
            "sh",
            "-c",
            &format!(
                "echo $BUSYBEE_CORES; touch {}; sleep 2; touch {}",
                held.display(),
                handed_back.display()
            ),
        ])
        .output()
        .expect("run the static task");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("{GRANTED}\n"),
        "a request for 3 beside two admitted leases is clamped to the fair share"
    );
    assert_preamble(
        &out,
        &running(
            "<shell>",
            &format!(r"static, holding {GRANTED}/{POOL} cores \(2 other tasks active\)"),
        ),
    );

    let (from, to) = (mtime(&held), mtime(&handed_back));
    let during: Vec<u32> = counter::samples(&build)
        .iter()
        .filter(|s| s.at >= from && s.at <= to)
        .map(|s| s.total)
        .collect();
    assert!(
        during.len() >= 2,
        "the builds logged {} job(s) while the static task held its cores; \
         they have to be running through the window for this to mean anything",
        during.len()
    );
    let peak = during.iter().copied().max().expect("samples in the window");
    assert!(
        peak <= THREE_WAY_CEILING,
        "the two builds ran {peak} jobs at once while {GRANTED} of {POOL} tokens \
         were held, expected at most {THREE_WAY_CEILING}"
    );

    for build in builds {
        let out = build.wait_with_output().expect("wait for a build");
        assert!(out.status.success(), "stderr: {}", stderr(&out));
    }
    assert_eq!(
        counter::samples(&build).len(),
        2 * TARGETS as usize,
        "both builds must run all their targets"
    );

    assert_pool_idle(&busybee);
}

#[test]
#[serial_test::serial]
fn an_unrecognised_command_waits_for_the_pool_then_has_it_alone() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("queued");
    counter::make_build(&build, 40, "0.5");
    let started = build.join("script-started");
    let script = build.join("opaque-script.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\necho alone\n", started.display()),
    )
    .expect("write the script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("make it executable");

    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for_a_running_task();

    let out = busybee
        .cmd(&["--", "./opaque-script.sh"])
        .current_dir(&build)
        .output()
        .expect("run the script");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "alone\n", "stdout belongs to the tool");
    assert!(
        stderr(&out).contains("busybee: queued (1 ahead)"),
        "the script was not told it was behind the build: {:?}",
        stderr(&out)
    );
    assert_preamble(
        &out,
        &running(
            "opaque-script.sh",
            &format!(r"none, exclusive \({POOL} cores\)"),
        ),
    );

    let make = make.wait_with_output().expect("wait for the build");
    assert!(make.status.success(), "stderr: {}", stderr(&make));
    let last_job = counter::samples(&build)
        .last()
        .expect("the build logged its jobs")
        .at;
    assert!(
        mtime(&started) > last_job,
        "the script started before the build's last job finished; it did not run alone"
    );
}

#[test]
#[serial_test::serial]
fn interrupting_a_queued_client_leaves_the_running_build_alone() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("interrupted");
    counter::make_build(&build, 40, "0.5");

    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for_a_running_task();

    let mut queued = busybee
        .cmd(&["--", "echo", "second"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the client that queues behind it");
    busybee.wait_for_leases(2);

    // SAFETY: an unreaped child's pid.
    unsafe { libc::kill(queued.id() as i32, libc::SIGINT) };
    let status = queued.wait().expect("wait for the interrupted client");
    assert_eq!(
        status.code(),
        Some(130),
        "exit code was {:?}",
        status.code()
    );

    busybee.wait_for("only the build to be left", |status| {
        status.leases.len() == 1 && status.leases[0].tool == "make"
    });

    // Only samples after the lease is gone can show tokens leaked with it;
    // the whole-build peak predates the interruption.
    let gone = busybee.tmp.path().join("gone");
    fs::write(&gone, "").expect("mark when the interrupted lease was gone");
    let gone = mtime(&gone);

    let make = make.wait_with_output().expect("wait for the build");
    assert!(make.status.success(), "stderr: {}", stderr(&make));
    let samples = counter::samples(&build);
    assert_eq!(samples.len(), 40, "every job must log exactly once");
    let after: Vec<u32> = samples
        .iter()
        .filter(|s| s.at >= gone)
        .map(|s| s.total)
        .collect();
    assert!(
        after.len() >= 6,
        "the build logged {} job(s) after the interruption; it has to still be \
         going for this to mean anything",
        after.len()
    );
    // Two-sided: leaked tokens would show as running under its share.
    let peak = after.iter().copied().max().expect("samples after the wait");
    assert!(
        (5..=CEILING).contains(&peak),
        "peak concurrency {peak} after the interruption, expected 5..={CEILING}: \
         the build kept the whole pool across it"
    );

    // One lost token can hide inside the peak, but not in the free count.
    assert_pool_idle(&busybee);
}

/// A queued lease holds nothing: the build keeps the whole pool for as long as
/// an exclusive client waits behind it, not only after the client is gone.
#[test]
#[serial_test::serial]
fn a_queued_lease_holds_no_tokens_while_it_waits() {
    let Some(busybee) = fixture() else {
        return;
    };
    /// Long enough for several rounds of the build's jobs.
    const WINDOW: Duration = Duration::from_secs(3);
    // Outlasts the queueing plus the window at full width.
    const TARGETS: u32 = 80;
    let build = busybee.tmp.path().join("waiting");
    counter::make_build(&build, TARGETS, "0.5");

    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for_a_running_task();

    // `none` wants the whole pool, so it waits for the build to end.
    let mut queued = busybee
        .cmd(&["--", "echo", "exclusive"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the client that queues behind the build");
    busybee.wait_for("the exclusive client to be queued", |status| {
        status.leases.iter().any(|l| l.state == "queued")
    });
    let from = mark(&busybee, "queued-from");
    std::thread::sleep(WINDOW);
    let to = mark(&busybee, "queued-to");

    // The window only counts if the client was queued through all of it.
    let status = busybee.status().expect("bzbd answers status");
    let waiting: Vec<_> = status.leases.iter().filter(|l| l.tool != "make").collect();
    assert!(
        waiting.len() == 1 && waiting[0].state == "queued" && waiting[0].cores == 0,
        "the exclusive client was not still queued, holding nothing, when the \
         window closed: {:?}",
        status.leases
    );

    let during: Vec<u32> = counter::samples(&build)
        .iter()
        .filter(|s| s.at >= from && s.at <= to)
        .map(|s| s.total)
        .collect();
    assert!(
        during.len() >= 6,
        "the build logged {} job(s) while the client was queued; it has to be \
         running through the window for this to mean anything",
        during.len()
    );
    // A queued lease that drained tokens early would throttle the build here.
    let peak = during.iter().copied().max().expect("samples in the window");
    assert!(
        (5..=CEILING).contains(&peak),
        "peak concurrency {peak} while a client was queued, expected 5..={CEILING}: \
         a queued lease must leave the whole pool to the build"
    );

    // SAFETY: an unreaped child's pid.
    unsafe { libc::kill(queued.id() as i32, libc::SIGINT) };
    let status = queued.wait().expect("wait for the interrupted client");
    assert_eq!(
        status.code(),
        Some(130),
        "the client was admitted before it was interrupted (exit {:?}); \
         the build ended inside the window",
        status.code()
    );

    let make = make.wait_with_output().expect("wait for the build");
    assert!(make.status.success(), "stderr: {}", stderr(&make));
    assert_pool_idle(&busybee);
}

/// Line shapes from bzbd.md §Client output contract; admission is [`running`].
const QUEUED: &str = r"^busybee: queued \(\d+ ahead\)$";
const MOVED: &str = r"^busybee: (?:\d+ ahead…|still queued \(\d+ ahead\))$";
const NOTE: &str = r"^busybee: note: .+$";
const EXITED: &str = r"^busybee: command exited -?\d+ \(elapsed (?:\d+s|\d+m\d\ds|\d+h\d\dm)\)$";

fn running(tool: &str, tail: &str) -> String {
    format!("^busybee: running — {}, {tail}$", regex::escape(tool))
}

/// Every stderr line fits the contract: queued first, one `running`, exit last.
/// Nothing here warrants a notice, so one is a failure.
fn assert_preamble(out: &Output, running: &str) {
    let text = stderr(out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "busybee said nothing about the lease");
    let running = Regex::new(running).expect("a valid running pattern");
    let note = Regex::new(NOTE).expect("a valid shape");
    let allowed: Vec<Regex> = [QUEUED, MOVED, NOTE, EXITED]
        .iter()
        .map(|shape| Regex::new(shape).expect("a valid shape"))
        .collect();
    for line in &lines {
        assert!(
            running.is_match(line) || allowed.iter().any(|shape| shape.is_match(line)),
            "{line:?} is not a line the output contract allows; stderr was {text:?}"
        );
    }
    assert!(
        !lines.iter().any(|line| note.is_match(line)),
        "busybee raised a notice about a request that should not need one; \
         stderr was {text:?}"
    );
    assert!(
        Regex::new(QUEUED)
            .expect("a valid shape")
            .is_match(lines[0]),
        "the first line is not the queue position; stderr was {text:?}"
    );
    assert_eq!(
        lines.iter().filter(|line| running.is_match(line)).count(),
        1,
        "expected exactly one admission line matching {}; stderr was {text:?}",
        running.as_str()
    );
    assert!(
        Regex::new(EXITED)
            .expect("a valid shape")
            .is_match(lines[lines.len() - 1]),
        "the last line is not the exit code; stderr was {text:?}"
    );
}

/// Waits on the leases only, so the free count is checked, not waited for.
fn assert_pool_idle(busybee: &Busybee) {
    busybee.wait_for("an idle pool", |status| status.leases.is_empty());
    let status = busybee.run(&["status", "--json"]);
    assert!(status.status.success(), "stderr: {}", stderr(&status));
    let reply: serde_json::Value =
        serde_json::from_str(stdout(&status).trim()).expect("status --json prints one JSON line");
    assert_eq!(
        reply["free"].as_u64(),
        Some(u64::from(POOL)),
        "status was {}",
        stdout(&status)
    );
}

/// Touches a file in the test's tempdir and returns its mtime, a timestamp on
/// the same clock as the build's samples.
fn mark(busybee: &Busybee, name: &str) -> SystemTime {
    let path = busybee.tmp.path().join(name);
    fs::write(&path, "").expect("write a time marker");
    mtime(&path)
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .unwrap_or_else(|err| panic!("{} was never written: {err}", path.display()))
        .modified()
        .expect("a modification time")
}
