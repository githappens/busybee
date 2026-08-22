//! Compute the binary's displayed version from git state and expose it as
//! the `BUSYBEE_VERSION` compile-time env var.
//!
//! Scheme: `MAJOR.MINOR.<PATCH+N>` where `MAJOR.MINOR.PATCH` is the nearest
//! semver-shaped tag reachable from HEAD and `N` is commits since that tag.
//! No tag yet → `0.0.<total-commit-count>`.
//! No `.git` (source tarball, crates.io, etc.) → fall back to
//! `CARGO_PKG_VERSION`.

use std::path::Path;
use std::process::Command;

#[path = "src/version_parse.rs"]
mod version_parse;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let repo = Path::new(&manifest_dir).join("../..");

    // Rebuild when refs or tags change. Missing paths cause Cargo to always
    // rerun — acceptable since this script is cheap.
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

    // No matching tag reachable from HEAD: patch = total commit count.
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
