//! Sets `BUSYBEE_VERSION` from git: nearest semver tag plus commits since,
//! `0.0.<commits>` with no tag, `CARGO_PKG_VERSION` with no `.git`.
//! Shares the parser from `crates/bzb/src/version_parse.rs`.

use std::path::Path;
use std::process::Command;

#[path = "../bzb/src/version_parse.rs"]
mod version_parse;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let repo = Path::new(&manifest_dir).join("../..");

    // A missing path makes Cargo always rerun; the script is cheap.
    for rel in [
        ".git/HEAD",
        ".git/refs/heads",
        ".git/refs/tags",
        ".git/packed-refs",
    ] {
        println!("cargo:rerun-if-changed={}", repo.join(rel).display());
    }

    let version = git_version(&repo).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=BUSYBEE_VERSION={version}");
}

fn git_version(repo: &Path) -> Option<String> {
    // --long always appends `-N-gSHA`, so the parser is uniform at N=0.
    let desc = Command::new("git")
        .current_dir(repo)
        .args([
            "describe",
            "--tags",
            "--long",
            "--match",
            "[0-9]*.[0-9]*.[0-9]*",
            "--match",
            "v[0-9]*.[0-9]*.[0-9]*",
        ])
        .output()
        .ok()?;
    if desc.status.success() {
        let s = std::str::from_utf8(&desc.stdout).ok()?.trim();
        return version_parse::parse_describe(s);
    }

    let count = Command::new("git")
        .current_dir(repo)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;
    if !count.status.success() {
        return None;
    }
    let n: u64 = std::str::from_utf8(&count.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(format!("0.0.{n}"))
}
