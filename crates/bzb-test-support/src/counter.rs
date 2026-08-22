//! A build whose jobs count each other, for tests that need to know how many
//! ran at once.
//!
//! Every target is one job: it drops a marker file, sleeps, records how many
//! markers exist, and removes its marker. The set of markers is the set of
//! jobs running at that instant, so the largest count any job saw is the peak
//! concurrency the tool actually reached — a number no sampler outside the
//! build can be sure of catching.
//!
//! Each sample is a file of its own rather than a line in a shared log: its
//! mtime is the instant the count was read, which is what lets a test ask what
//! the concurrency was *during* some window rather than only over the whole
//! run. Markers and samples are named after the build (`COUNTER_NAME`, `a` by
//! default) so two builds can share one directory and still be told apart:
//! each sample carries the total across both builds and the count of the
//! build's own jobs.

use std::{fs, path::Path, time::SystemTime};

/// One job's view of the machine at the moment it looked.
pub struct Sample {
    /// The build whose job wrote this (`COUNTER_NAME`).
    pub name: String,
    /// Jobs running across every build sharing the directory.
    pub total: u32,
    /// Jobs running in this sample's own build.
    pub own: u32,
    /// When the count was read.
    pub at: SystemTime,
}

/// Writes a Makefile of `targets` independent jobs into `dir`, each sleeping
/// `sleep` seconds. Build it with `make run`.
pub fn make_build(dir: &Path, targets: u32, sleep: &str) {
    let names: Vec<String> = (1..=targets).map(|i| format!("t{i}")).collect();
    let makefile = format!(
        "COUNTER_NAME ?= a\n\
         TARGETS := {targets_list}\n\
         .PHONY: run $(TARGETS)\n\
         run: $(TARGETS)\n\
         $(TARGETS):\n\
         \t@s=$(COUNTER_NAME).$@.$$$$; m=markers/$$s; touch $$m; sleep {sleep}; \\\n\
         \tprintf '%s %s\\n' \"$$(ls markers | wc -l)\" \
         \"$$(ls markers | grep -c '^$(COUNTER_NAME)\\.')\" > pending/$$s; \\\n\
         \tmv pending/$$s samples/$$s; rm $$m\n",
        targets_list = names.join(" "),
    );
    prepare(dir);
    fs::write(dir.join("Makefile"), makefile).expect("write the Makefile");
}

/// The same build for ninja, which has no second build to count against: every
/// sample's own count is the total.
pub fn ninja_build(dir: &Path, targets: u32, sleep: &str) {
    let mut file = format!(
        "rule job\n  command = m=markers/$out.$$$$; touch $$m; sleep {sleep}; \
         c=$$(ls markers | wc -l); printf '%s %s\\n' \"$$c\" \"$$c\" > pending/n.$out.$$$$; \
         mv pending/n.$out.$$$$ samples/n.$out.$$$$; rm $$m\n",
    );
    for i in 1..=targets {
        file.push_str(&format!("build t{i}: job\n"));
    }
    file.push_str("build run: phony");
    for i in 1..=targets {
        file.push_str(&format!(" t{i}"));
    }
    file.push_str("\ndefault run\n");
    prepare(dir);
    fs::write(dir.join("build.ninja"), file).expect("write the ninja file");
}

fn prepare(dir: &Path) {
    fs::create_dir_all(dir.join("markers")).expect("create the marker directory");
    fs::create_dir_all(dir.join("samples")).expect("create the sample directory");
    // A sample is written here and renamed into place, so a test reading the
    // samples of a build that is still going never catches a half-written
    // one. Rename is atomic within a filesystem and keeps the mtime.
    fs::create_dir_all(dir.join("pending")).expect("create the pending directory");
}

/// True when `tool` is present and at least version `min`; otherwise prints
/// why the calling test is skipping itself and returns false. Rust cannot
/// attach `#[ignore]` at runtime, so a skipped test still reports `ok`; the
/// reason is visible with `--nocapture`.
pub fn available(tool: &str, min: (u32, u32)) -> bool {
    match version(tool) {
        Some(v) if v >= min => true,
        Some((major, minor)) => {
            eprintln!(
                "skipping: {tool} {major}.{minor} is older than the required {}.{}",
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

/// `(major, minor)` from the first line of `tool --version`, `None` if the
/// tool cannot be run. Handles both "GNU Make 4.4.1" and ninja's bare "1.13.2".
fn version(tool: &str) -> Option<(u32, u32)> {
    let out = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let last_word = text.lines().next()?.split_whitespace().last()?;
    let mut parts = last_word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Every sample the build(s) in `dir` have written so far, oldest first.
pub fn samples(dir: &Path) -> Vec<Sample> {
    let mut samples: Vec<Sample> = fs::read_dir(dir.join("samples"))
        .expect("read the sample directory")
        .map(|entry| {
            let entry = entry.expect("read a sample");
            let at = entry
                .metadata()
                .expect("stat a sample")
                .modified()
                .expect("a sample's mtime");
            let file = entry.file_name().to_string_lossy().into_owned();
            let text = fs::read_to_string(entry.path()).expect("read a sample");
            let mut counts = text.split_whitespace().map(|count| {
                count
                    .parse()
                    .unwrap_or_else(|_| panic!("{file} is not a pair of counts: {text:?}"))
            });
            let (total, own) = (
                counts.next().expect("a total"),
                counts.next().expect("an own count"),
            );
            Sample {
                name: file.split('.').next().expect("a build name").to_string(),
                total,
                own,
                at,
            }
        })
        .collect();
    samples.sort_by_key(|sample| sample.at);
    samples
}

/// The most jobs any one of `dir`'s jobs saw running at once, across every
/// build sharing it. Panics unless `expected` jobs logged, so a build that
/// silently did less work than it was asked to cannot pass for a quiet one.
pub fn peak(dir: &Path, expected: usize) -> u32 {
    let samples = samples(dir);
    assert_eq!(samples.len(), expected, "every job must log exactly once");
    samples
        .iter()
        .map(|sample| sample.total)
        .max()
        .expect("at least one job")
}
