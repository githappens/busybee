//! The headline, end to end: **one task alone gets the whole machine, N tasks
//! share it, and nothing is ever over-subscribed** (`docs/design/bzbd.md`
//! §Problem).
//!
//! Everything else in the suite tests a part. This file spawns the real
//! `busybee` binary against daemons of its own and watches real GNU make
//! builds take, share and give back a six-token pool, so it doubles as the
//! worked example the README points at.
//!
//! How the concurrency is measured: each build target drops a marker file,
//! sleeps, records how many markers exist, and removes its marker — see
//! [`bzb_test_support::counter`]. The largest count any job saw is the peak
//! concurrency the tool actually reached, and each sample's mtime says when it
//! was read, which is what lets [`a_static_task_drains_the_pool_and_hands_it_back`]
//! ask what the build was doing *while* the static task held its cores.
//!
//! Two notes on what is asserted, and how hard:
//!
//! * Concurrency bounds are exact. They come from the jobs' own counts, not
//!   from a sampler outside the build that could miss a peak.
//! * Durations are given at least 2× slack, since a loaded CI runner is
//!   slower than a developer's machine and a tight deadline here would fail
//!   for reasons that have nothing to do with the pool.
//!
//! The builds are run as `busybee -- make run`, with no `-j` of their own.
//! `busybee -- make -j16 run` would be a different test: a user-supplied
//! parallelism flag defeats the injected jobserver by design and only earns a
//! notice (`docs/design/bzbd.md` §Decisions log), so that build would ignore
//! the pool rather than share it.

mod common;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Output, Stdio},
    time::SystemTime,
};

use bzb_test_support::counter;
use common::{stderr, stdout, Busybee};
use regex::Regex;

/// Six tokens and at most four tasks admitted at once, so every number below
/// is fixed rather than a function of whatever machine runs the tests.
const CONFIG: &str = "pool_size = 6\nmax_concurrent = 4\n";
const POOL: u32 = 6;

/// One build's ceiling: the pool, plus the one job every jobserver participant
/// runs without a token (`docs/design/bzbd.md` §Admission policy rule 1).
const CEILING: u32 = POOL + 1;

/// Two builds' ceiling: one implicit job each, on the same pool.
const SHARED_CEILING: u32 = POOL + 2;

/// What is left for a jobserver build while a static task holds three of the
/// six tokens: three tokens plus its own implicit job.
const THROTTLED_CEILING: u32 = POOL - 3 + 1;

/// The daemons and the tool every test here needs, or `None` with the reason
/// on stderr: `pueued` off `PATH`, or a `make` too old for the fifo jobserver,
/// is how these tests skip themselves outside the dev shell (see
/// `crates/bzb-core/tests/README.md`).
fn fixture() -> Option<Busybee> {
    if !counter::available("make", (4, 4)) {
        return None;
    }
    Busybee::start_on(CONFIG)
}

/// A build alone on the pool takes all of it: six tokens plus its implicit
/// job, and no ceremony beyond `busybee --`.
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

/// Two builds on one pool interleave token by token. Neither is throttled to
/// nothing and the two together never exceed the pool plus their two implicit
/// jobs — no daemon decision is involved in the rebalancing
/// (`docs/design/bzbd.md` §Key insight).
#[test]
#[serial_test::serial]
fn two_builds_share_the_pool_and_neither_starves() {
    let Some(busybee) = fixture() else {
        return;
    };
    // One directory, so the two builds' markers count against each other and
    // every sample carries the combined total as well as the build's own.
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
    assert!(
        samples.iter().any(|s| s.total > s.own),
        "no job ever saw one of the other build's running beside it: the two \
         builds were serialised, not sharing"
    );
    let combined = samples.iter().map(|s| s.total).max().expect("samples");
    assert!(
        combined <= SHARED_CEILING,
        "combined peak concurrency {combined}, expected at most {SHARED_CEILING}"
    );
    for name in ["a", "b"] {
        let own = samples
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.own)
            .max()
            .unwrap_or_else(|| panic!("build {name} logged nothing"));
        assert!(
            own >= 2,
            "build {name} never had more than {own} job(s) running; it was starved for the whole run"
        );
    }
}

/// A static task cannot speak the jobserver protocol, so bzbd pulls its cores
/// out of the same fifo and holds them (`docs/design/bzbd.md` §Key insight).
/// The running build is throttled to what is left for as long as that lasts,
/// is told nothing about it, and gets the pool back afterwards.
#[test]
#[serial_test::serial]
fn a_static_task_drains_the_pool_and_hands_it_back() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("drained");
    // Long enough that the two-second static task lands well inside it even
    // on a slow runner.
    counter::make_build(&build, 96, "0.5");

    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for_a_running_task();

    // The task marks when it had its cores and when it gave them up; both are
    // silent, so its stdout stays its own.
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
        // A shell string is opaque to the classifier, so the tool it names is
        // `<shell>` rather than anything inside the quotes.
        &running(
            "<shell>",
            &format!(r"static, holding 3/{POOL} cores \(1 other task active\)"),
        ),
    );

    // Both markers and every sample are files in the same temporary tree, so
    // this assumes the filesystem timestamps them finely enough to tell apart
    // events a second or two apart — true of APFS and ext4, which is what the
    // suite runs on.
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
    let overall = counter::peak(&build, 96);
    assert!(
        (THROTTLED_CEILING + 1..=CEILING).contains(&overall),
        "peak concurrency {overall} over the whole build, expected \
         {}..={CEILING}: a build that never exceeded {THROTTLED_CEILING} \
         with the pool to itself would make the throttled window above vacuous",
        THROTTLED_CEILING + 1
    );

    // Everything is back: the drained cores were released and the build's
    // tokens went back to the fifo. Waiting on the leases alone, so that the
    // free count below is the command's answer and not the wait's condition.
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

/// An unrecognised command is `none`: exclusive, and admitted only once
/// nothing else is (`docs/design/bzbd.md` §Admission policy). So it waits for
/// the build to finish rather than sharing with it, and the queue position it
/// was given says so.
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

/// Ctrl-C on a queued client is exit 130 and the lease goes with the
/// connection (`docs/design/bzbd.md` §Lease model). Nothing about that reaches
/// the build already on the machine.
#[test]
#[serial_test::serial]
fn interrupting_a_queued_client_leaves_the_running_build_alone() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("interrupted");
    // Long enough that the interruption is done well before the build is, so
    // the window that matters — everything after it — is most of the run.
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

    // SAFETY: kill has no preconditions beyond a pid, and this one is a child
    // of the test that has not been reaped.
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

    // The instant the interrupted lease was gone, in the same clock the samples
    // are stamped in. The peak over the whole build would prove nothing: the
    // build reaches the pool before the second client even queues, so a lease
    // that took tokens with it would leave that early peak standing and the
    // build throttled for the rest of the run. Only the samples after this
    // point can tell the two apart.
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
    // Two-sided on purpose: the interrupted lease taking tokens with it would
    // leave the build running under its share rather than over it.
    let peak = after.iter().copied().max().expect("samples after the wait");
    assert!(
        (5..=CEILING).contains(&peak),
        "peak concurrency {peak} after the interruption, expected 5..={CEILING}: \
         the build kept the whole pool across it"
    );
}

/// The lines busybee is allowed to write about itself, quoted from
/// `docs/design/bzbd.md` §Client output contract. The admission line is
/// [`running`]'s, since what it says depends on the class the lease was
/// admitted under.
const QUEUED: &str = r"^busybee: queued \(\d+ ahead\)$";
const MOVED: &str = r"^busybee: (?:\d+ ahead…|still queued \(\d+ ahead\))$";
const NOTE: &str = r"^busybee: note: .+$";
const EXITED: &str = r"^busybee: command exited -?\d+ \(elapsed (?:\d+s|\d+m\d\ds|\d+h\d\dm)\)$";

/// The admission line for `tool`, whose `tail` is the class-specific half.
fn running(tool: &str, tail: &str) -> String {
    format!("^busybee: running — {}, {tail}$", regex::escape(tool))
}

/// Checks everything the client said about itself: every line is one the
/// output contract allows, the first announces the queue, exactly one reports
/// the admission `running` describes, and the last is the exit code.
///
/// None of the invocations in this file gives busybee anything to complain
/// about — no `-j` defeating the pool, no `--cores` on a jobserver lease, no
/// drain that came up short — so a notice is a failure here even though the
/// contract has a shape for one.
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

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .unwrap_or_else(|err| panic!("{} was never written: {err}", path.display()))
        .modified()
        .expect("a modification time")
}
