//! A build whose jobs count each other, for tests that need to know how many
//! ran at once.
//!
//! Each job drops a marker, sleeps, lists the markers into a sample file of
//! its own, and removes its marker: the largest listing is the peak
//! concurrency, and a sample's mtime is when it was taken. Markers and samples
//! carry the build's name (`COUNTER_NAME`, `a` by default) so two builds can
//! share one directory.

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
        // `ls` writes the sample itself, so its mtime is the snapshot's.
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
    let names: Vec<String> = (1..=targets).map(|i| format!("t{i}")).collect();
    for name in &names {
        file.push_str(&format!("build {name}: job\n"));
    }
    file.push_str(&format!(
        "build run: phony {}\ndefault run\n",
        names.join(" ")
    ));
    prepare(dir);
    fs::write(dir.join("build.ninja"), file).expect("write the ninja file");
}

fn prepare(dir: &Path) {
    fs::create_dir_all(dir.join("markers")).expect("create the marker directory");
    fs::create_dir_all(dir.join("samples")).expect("create the sample directory");
    // Samples are renamed into place so a reader never sees a partial one.
    fs::create_dir_all(dir.join("pending")).expect("create the pending directory");
}

/// True when `tool` is present and at least version `min`; otherwise prints
/// why the calling test skips (visible with `--nocapture`).
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

/// `(major, minor)` from the first line of `tool --version` ("GNU Make 4.4.1",
/// ninja's bare "1.13.2"). `None` only when `tool` is not in `PATH`; any other
/// failure panics, since a skip would pass while testing nothing.
fn version(tool: &str) -> Option<(u32, u32)> {
    let out = match std::process::Command::new(tool).arg("--version").output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("run `{tool} --version`: {e}"),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let last_word = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last());
    let mut parts = last_word
        .unwrap_or_default()
        .split('.')
        .map(|p| p.parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => Some((major, minor)),
        _ => panic!("no version in `{tool} --version` output {text:?}"),
    }
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
