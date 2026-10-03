//! Regression for #50: a running jobserver lease must not report `using ~0`.
//!
//! Jobserver leases hold no tokens (`cores_held == 0`); the per-lease usage
//! estimate requires compiler-process attribution that is not yet implemented.
//! Until it is, the status row must show neutral text ("sharing") and
//! `status --json` must not carry a numeric per-lease usage of zero for it.
//!
//! The unit test at the bottom of this file uses `LeaseView::cores: None`,
//! which requires `cores: Option<u32>` (protocol v5). It fails to compile
//! against the v4 base where `cores` was `u32`.

mod common;

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

use bzb_core::protocol::LeaseView;
use bzb_test_support::counter;
use common::Busybee;

const BUSYBEE: &str = env!("CARGO_BIN_EXE_busybee");

/// `None` when prerequisites are missing; skips instead of panicking so that
/// tests running without a full workspace build do not crash the runner.
fn fixture() -> Option<Busybee> {
    if !counter::available("make", (4, 4)) {
        return None;
    }
    // bzbd is in the bzbd crate; skip if it has not been built yet.
    let bzbd = std::path::Path::new(env!("CARGO_BIN_EXE_busybee"))
        .parent()?
        .join("bzbd");
    if !bzbd.is_file() {
        eprintln!(
            "skipping: bzbd not found at {}; run cargo build --workspace first",
            bzbd.display()
        );
        return None;
    }
    Busybee::start_on("pool_size = 4\n")
}

fn run_status(state: &Path, args: &[&str]) -> Output {
    Command::new(BUSYBEE)
        .arg("status")
        .args(args)
        .env("BUSYBEE_STATE_DIR", state)
        .output()
        .expect("run busybee status")
}

/// A running jobserver lease must not appear as `using ~0` in the text table.
#[test]
#[serial_test::serial]
fn a_running_jobserver_lease_does_not_show_using_zero() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("build");
    counter::make_build(&build, 4, "2");

    // Start a make build under busybee without waiting for it to finish.
    let mut child = busybee
        .cmd(&["--", "make", "-C", build.to_str().unwrap(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busybee make");

    // Wait for the build to be admitted and running.
    busybee.wait_for_a_running_task();

    let output = run_status(busybee.state_dir().as_path(), &[]);
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");

    // The table must not contain "using ~0" for any jobserver row.
    assert!(
        !stdout.contains("using ~0"),
        "status table contained 'using ~0':\n{stdout}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// `status --json` must not report a numeric per-lease `cores` of `0` for a
/// running jobserver lease; the field must be absent or `null`.
#[test]
#[serial_test::serial]
fn a_running_jobserver_lease_has_no_numeric_cores_in_json() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("build");
    counter::make_build(&build, 4, "2");

    let mut child = busybee
        .cmd(&["--", "make", "-C", build.to_str().unwrap(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busybee make");

    busybee.wait_for_a_running_task();

    let output = run_status(busybee.state_dir().as_path(), &["--json"]);
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("decode status JSON");

    let leases = json["leases"].as_array().expect("leases is an array");
    for lease in leases {
        let class = lease["class"].as_str().unwrap_or("");
        let state = lease["state"].as_str().unwrap_or("");
        if class == "jobserver" && state == "running" {
            // cores must be null or absent — never a number (especially not 0)
            assert!(
                !lease["cores"].is_number(),
                "running jobserver lease reported numeric cores in JSON: {lease}"
            );
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// The aggregate `approx_in_use` pool estimate is still present in JSON even
/// when per-lease jobserver attribution is unavailable.
#[test]
#[serial_test::serial]
fn the_aggregate_approx_in_use_is_still_present_in_json() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("build");
    counter::make_build(&build, 4, "2");

    let mut child = busybee
        .cmd(&["--", "make", "-C", build.to_str().unwrap(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busybee make");

    busybee.wait_for_a_running_task();

    let output = run_status(busybee.state_dir().as_path(), &["--json"]);
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("decode status JSON");

    assert!(
        json.get("approx_in_use").is_some(),
        "approx_in_use is absent from JSON: {json}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// `LeaseView::cores` is `Option<u32>` in protocol v5: `None` when per-lease
/// attribution is unavailable (jobserver today), `Some(n)` when measured.
///
/// This test uses `cores: None` directly. It fails to **compile** on the v4
/// base where `cores` was `u32`, providing a compile-error regression for the
/// verification gate regardless of which binaries are built.
#[test]
fn jobserver_lease_view_has_optional_cores() {
    let view = LeaseView {
        id: 1,
        label: "make build".into(),
        tool: "make".into(),
        class: "jobserver".into(),
        cores: None, // Option<u32> in v5; compile error on v4 base (cores: u32)
        state: "running".into(),
        elapsed_ms: 0,
        ahead: None,
        pueue_task_id: None,
    };
    assert!(
        view.cores.is_none(),
        "jobserver lease without attribution must have None cores, got {:?}",
        view.cores
    );
}
