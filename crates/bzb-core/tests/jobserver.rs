//! Integration tests for `bzb_core::jobserver` against real GNU make and ninja.
//!
//! Each build fixture has 16 independent targets. A target creates a marker
//! file under `run/`, sleeps 0.3 s, appends the number of markers currently
//! present to `counts.log`, and removes its marker. The maximum value in
//! `counts.log` is the peak concurrency the tool actually reached. Markers
//! are per-process (`$$`) so two builds can share one fixture directory.
//!
//! Tests that need an external tool print why they are skipped when the tool
//! is missing or too old (visible with `--nocapture`); see `tests/README.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bzb_core::jobserver::Jobserver;

const MAKEFILE: &str = "\
T := t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 t11 t12 t13 t14 t15 t16
all: $(T)
$(T):
\t@f=run/$@.$$$$; touch $$f; sleep 0.3; ls run | wc -l >> counts.log; rm $$f
";

fn ninja_file() -> String {
    let mut s = String::from(
        "rule job\n  command = f=run/$out.$$$$; touch $$f; sleep 0.3; ls run | wc -l >> counts.log; rm $$f\n",
    );
    for i in 1..=16 {
        s.push_str(&format!("build t{i}: job\n"));
    }
    s.push_str("build all: phony");
    for i in 1..=16 {
        s.push_str(&format!(" t{i}"));
    }
    s.push_str("\ndefault all\n");
    s
}

/// `(major, minor)` from the first line of `tool --version`, `None` if the
/// tool cannot be run. Handles both "GNU Make 4.4.1" and ninja's bare "1.13.2".
fn version(tool: &str) -> Option<(u32, u32)> {
    let out = Command::new(tool).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let last_word = text.lines().next()?.split_whitespace().last()?;
    let mut parts = last_word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// True when `tool` is present and at least `min`; otherwise prints the
/// reason the calling test is being skipped and returns false.
fn available(tool: &str, min: (u32, u32)) -> bool {
    match version(tool) {
        Some(v) if v >= min => true,
        Some((maj, min_)) => {
            eprintln!(
                "skipping: {tool} {maj}.{min_} is older than the required {}.{}",
                min.0, min.1
            );
            false
        }
        None => {
            eprintln!("skipping: {tool} not found in PATH");
            false
        }
    }
}

/// Fresh per-test directory containing `run/` and the build file.
fn fixture(name: &str, file: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bzb-jobserver-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("run")).unwrap();
    fs::write(dir.join(file), content).unwrap();
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

fn peak_concurrency(dir: &Path) -> u32 {
    let log = fs::read_to_string(dir.join("counts.log")).unwrap();
    let counts: Vec<u32> = log.lines().map(|l| l.trim().parse().unwrap()).collect();
    assert_eq!(counts.len() % 16, 0, "every target must log exactly once");
    counts.into_iter().max().unwrap()
}

#[test]
fn make_respects_pool_of_four() {
    if !available("make", (4, 4)) {
        return;
    }
    let dir = fixture("make", "Makefile", MAKEFILE);
    let js = Jobserver::create(&dir, 4).unwrap();

    run(&mut build_cmd("make", &dir, &js));

    let peak = peak_concurrency(&dir);
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
    let dir = fixture("make2", "Makefile", MAKEFILE);
    let js = Jobserver::create(&dir, 4).unwrap();

    let mut a = build_cmd("make", &dir, &js).spawn().unwrap();
    let mut b = build_cmd("make", &dir, &js).spawn().unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    let peak = peak_concurrency(&dir);
    assert!(peak <= 6, "combined peak concurrency {peak}, expected <= 6");
    assert_eq!(js.free().unwrap(), 4);
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn acquire_and_release_round_trip() {
    let dir = fixture("acquire", ".keep", "");
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
    let dir = fixture("timeout", ".keep", "");
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
    let dir = fixture("drain", ".keep", "");
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
    let dir = fixture("drop", ".keep", "");
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

    let dir = fixture("leave", ".keep", "");
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
    let dir = fixture("close", ".keep", "");
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
    let dir = fixture("with space", ".keep", "");
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
    let dir = fixture("too-many-tokens", ".keep", "");
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
    let dir = fixture("ninja", "build.ninja", &ninja_file());
    let js = Jobserver::create(&dir, 4).unwrap();

    run(&mut build_cmd("ninja", &dir, &js));

    let peak = peak_concurrency(&dir);
    assert!(
        (2..=5).contains(&peak),
        "peak concurrency {peak}, expected 2..=5"
    );
    assert_eq!(js.free().unwrap(), 4, "ninja must return every token");
    drop(js);
    let _ = fs::remove_dir_all(&dir);
}
