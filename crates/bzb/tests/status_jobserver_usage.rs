//! Regression for #50: a running jobserver lease must not report `using ~0`.
//!
//! Jobserver leases hold no tokens (`cores_held == 0`); the per-lease usage
//! estimate requires compiler-process attribution that is not yet implemented.
//! Until it is, the status row must show neutral text ("sharing") and
//! `status --json` must not carry a numeric per-lease usage of zero for it.

mod common;

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    time::Instant,
};

use bzb_test_support::counter;
use common::{Busybee, PATIENCE};

const BUSYBEE: &str = env!("CARGO_BIN_EXE_busybee");

/// `None` when `make ≥ 4.4` or `pueued` is not available.
fn fixture() -> Option<Busybee> {
    if !counter::available("make", (4, 4)) {
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

    // Deadline guard so the test can't hang indefinitely.
    let deadline = Instant::now() + PATIENCE;

    let mut child = busybee
        .cmd(&["--", "make", "-C", build.to_str().unwrap(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn busybee make");

    busybee.wait_for_a_running_task();
    assert!(
        Instant::now() < deadline,
        "timed out waiting for a running task"
    );

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
