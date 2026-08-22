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
//! mtime is the instant the markers were listed, which is what lets a test ask
//! what the concurrency was *during* some window rather than only over the
//! whole run. A sample holds the listing itself, written by the `ls` that took
//! it, so nothing runs between the snapshot and the timestamp that stands for
//! it; the counting is [`samples`]'s job. Markers and samples are named after
//! the build (`COUNTER_NAME`, `a` by default) so two builds can share one
//! directory and still be told apart: each listing gives both the total across
//! the builds and the count of the sampling build's own jobs.

use std::{fs, path::Path, time::SystemTime};

/// One job's view of the machine at the moment it looked.
pub struct Sample {
    /// The build whose job wrote this (`COUNTER_NAME`).
    pub name: String,
    /// Jobs running across every build sharing the directory.
    pub total: u32,
    /// Jobs running in this sample's own build.
    pub own: u32,
    /// When the markers were listed.
    pub at: SystemTime,
}

/// Writes a Makefile of `targets` independent jobs into `dir`, each sleeping
/// `sleep` seconds. Build it with `make run`.
pub fn make_build(dir: &Path, targets: u32, sleep: &str) {
    let names: Vec<String> = (1..=targets).map(|i| format!("t{i}")).collect();
    let makefile = format!(
        // The listing goes into the sample as it comes, so the file's mtime is
        // the `ls` that took it and not something later: a test that asks what
        // was running after some instant must not be handed an older snapshot
        // stamped after it.
        "COUNTER_NAME ?= a\n\
         TARGETS := {targets_list}\n\
         .PHONY: run $(TARGETS)\n\
         run: $(TARGETS)\n\
         $(TARGETS):\n\
         \t@s=$(COUNTER_NAME).$@.$$$$; m=markers/$$s; touch $$m; sleep {sleep}; \\\n\
         \tls markers > pending/$$s; mv pending/$$s samples/$$s; rm $$m\n",
        targets_list = names.join(" "),
    );
    prepare(dir);
    fs::write(dir.join("Makefile"), makefile).expect("write the Makefile");
}

/// The same build for ninja, whose jobs are all named `n.…`: it has no second
/// build to count against, so every sample's own count is the total.
pub fn ninja_build(dir: &Path, targets: u32, sleep: &str) {
    let mut file = format!(
        "rule job\n  command = s=n.$out.$$$$; m=markers/$$s; touch $$m; sleep {sleep}; \
         ls markers > pending/$$s; mv pending/$$s samples/$$s; rm $$m\n",
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
            let listing = fs::read_to_string(entry.path()).expect("read a sample");
            let name = file.split('.').next().expect("a build name").to_string();
            let prefix = format!("{name}.");
            let total = listing.lines().count() as u32;
            let own = listing
                .lines()
                .filter(|marker| marker.starts_with(&prefix))
                .count() as u32;
            // The job's own marker was there when it looked, so a listing
            // without it is a broken recipe rather than a quiet build.
            assert!(own >= 1, "{file} listed no marker of its own: {listing:?}");
            Sample {
                name,
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
