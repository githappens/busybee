//! Integration tests for `bzb_core::jobserver` against real GNU make and ninja.
//!
//! Each build fixture has 16 independent targets that count each other; see
//! [`bzb_test_support::counter`] for how, and `crates/bzb/tests/e2e_pool.rs`
//! for the same fixture under the whole daemon.
//!
//! Tests that need an external tool print why they are skipped when the tool
//! is missing or too old (visible with `--nocapture`); see `tests/README.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bzb_core::jobserver::Jobserver;
use bzb_test_support::counter::{self, available};

/// Targets in a build fixture, which is also how many samples it leaves.
const JOBS: usize = 16;

/// Long enough that the tool has time to reach its peak before the first job
/// finishes, short enough to keep the suite quick.
const SLEEP: &str = "0.3";

/// Fresh per-test directory, empty but for whatever the caller puts in it.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bzb-jobserver-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build_cmd(tool: &str, dir: &Path, js: &Jobserver) -> Command {
    let mut cmd = Command::new(tool);
    cmd.current_dir(dir).env("MAKEFLAGS", js.makeflags_value());
    cmd
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "build failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn make_respects_pool_of_four() {
    if !available("make", (4, 4)) {
        return;
    }
    let dir = fixture("make");
    counter::make_build(&dir, JOBS as u32, SLEEP);
    let js = Jobserver::create(&dir, 4).unwrap();

    run(build_cmd("make", &dir, &js).arg("run"));

    let peak = counter::peak(&dir, JOBS);
    assert!(
        (2..=5).contains(&peak),
        "peak concurrency {peak}, expected 2..=5"
    );
    assert_eq!(js.free().unwrap(), 4, "make must return every token");
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn two_makes_share_one_pool() {
    if !available("make", (4, 4)) {
        return;
    }
    let dir = fixture("make2");
    counter::make_build(&dir, JOBS as u32, SLEEP);
    let js = Jobserver::create(&dir, 4).unwrap();

    // Two names, so the two builds' samples cannot collide even if the shells
    // running a target happen to be given the same pid.
    let mut a = build_cmd("make", &dir, &js)
        .args(["COUNTER_NAME=a", "run"])
        .spawn()
        .unwrap();
    let mut b = build_cmd("make", &dir, &js)
        .args(["COUNTER_NAME=b", "run"])
        .spawn()
        .unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    let peak = counter::peak(&dir, 2 * JOBS);
    assert!(peak <= 6, "combined peak concurrency {peak}, expected <= 6");
    assert_eq!(js.free().unwrap(), 4);
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn acquire_and_release_round_trip() {
    let dir = fixture("acquire");
    let js = Jobserver::create(&dir, 4).unwrap();
    assert_eq!(js.free().unwrap(), 4);

    assert_eq!(js.acquire(3, Duration::from_secs(1)).unwrap(), 3);
    assert_eq!(js.free().unwrap(), 1);

    js.release(3).unwrap();
    assert_eq!(js.free().unwrap(), 4);
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn acquire_on_empty_pool_times_out() {
    let dir = fixture("timeout");
    let js = Jobserver::create(&dir, 0).unwrap();

    let start = Instant::now();
    let got = js.acquire(2, Duration::from_millis(200)).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(got, 0);
    assert!(
        elapsed >= Duration::from_millis(190) && elapsed < Duration::from_secs(2),
        "acquire returned after {elapsed:?}, expected about 200 ms"
    );
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drain_excess_removes_extra_tokens() {
    let dir = fixture("drain");
    let js = Jobserver::create(&dir, 4).unwrap();

    // A misbehaving tool writing two bytes it never read.
    fs::write(js.path(), "++").unwrap();
    assert_eq!(js.free().unwrap(), 6);

    assert_eq!(js.drain_excess(4).unwrap(), 2);
    assert_eq!(js.free().unwrap(), 4);
    assert_eq!(js.drain_excess(4).unwrap(), 0);
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_unlinks_fifo() {
    let dir = fixture("drop");
    let js = Jobserver::create(&dir, 1).unwrap();
    let path = js.path().to_path_buf();
    assert_eq!(
        js.makeflags_value(),
        format!("--jobserver-auth=fifo:{}", path.display())
    );
    assert!(path.exists());
    drop(js);
    assert!(!path.exists());
    let _ = fs::remove_dir_all(&dir);
}

/// A daemon on its way out leaves the fifo to the builds that hold it open,
/// tokens and all. The tokens live in the pipe, and the pipe lives as long as
/// someone has it open, so a holder stands in for the build here: the
/// daemon's own descriptors still close with the handle.
#[test]
fn leave_keeps_the_fifo_and_its_tokens_for_whoever_holds_it() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let dir = fixture("leave");
    let mut js = Jobserver::create(&dir, 3).unwrap();
    let path = js.path().to_path_buf();
    let holder = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();

    js.leave();
    drop(js);

    assert!(path.exists());
    let mut n: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::ioctl(holder.as_raw_fd(), libc::FIONREAD, &mut n) },
        0
    );
    assert_eq!(n, 3, "the tokens went with the daemon");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn close_reports_unlink_failure() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, directory permissions are not enforced");
        return;
    }
    let dir = fixture("close");
    let js = Jobserver::create(&dir, 1).unwrap();
    let path = js.path().to_path_buf();

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let err = js
        .close()
        .expect_err("unlink in a read-only directory must fail");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert!(path.exists(), "fifo must survive a failed unlink");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_rejects_dir_makeflags_cannot_carry() {
    // MAKEFLAGS is split on whitespace, so make would open only "fifo:<prefix>".
    let dir = fixture("with space");
    let err = Jobserver::create(&dir, 4)
        .err()
        .expect("create must fail for a whitespace path");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with("jobserver-"))
        .collect();
    assert!(leftovers.is_empty(), "fifo left behind: {leftovers:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_rejects_a_pool_the_pipe_cannot_hold() {
    // Every token must fit in the pipe at once; 4096 is the smallest capacity
    // on the supported platforms, so one more is an error, not a panic.
    let dir = fixture("too-many-tokens");
    let err = Jobserver::create(&dir, 4097)
        .err()
        .expect("create must fail for a pool larger than the pipe");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("4097"), "{err}");
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with("jobserver-"))
        .collect();
    assert!(leftovers.is_empty(), "fifo left behind: {leftovers:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ninja_respects_pool_of_four() {
    if !available("ninja", (1, 13)) {
        return;
    }
    let dir = fixture("ninja");
    counter::ninja_build(&dir, JOBS as u32, SLEEP);
    let js = Jobserver::create(&dir, 4).unwrap();

    run(&mut build_cmd("ninja", &dir, &js));

    let peak = counter::peak(&dir, JOBS);
    assert!(
        (2..=5).contains(&peak),
        "peak concurrency {peak}, expected 2..=5"
    );
    assert_eq!(js.free().unwrap(), 4, "ninja must return every token");
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}
