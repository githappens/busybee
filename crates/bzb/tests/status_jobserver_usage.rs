//! Regression for #50: a running jobserver lease holds no tokens and has no
//! per-lease usage estimate, so status must say `sharing`, not `using ~0`.

mod common;

use std::process::Stdio;

use bzb_test_support::counter;
use common::{stderr, stdout, Busybee};

/// `None` (self-skip) without `pueued` or a fifo-jobserver `make`.
fn fixture() -> Option<Busybee> {
    if !counter::available("make", (4, 4)) {
        return None;
    }
    Busybee::start()
}

#[test]
#[serial_test::serial]
fn a_running_jobserver_lease_reports_sharing_not_a_usage_of_zero() {
    let Some(busybee) = fixture() else {
        return;
    };
    let build = busybee.tmp.path().join("build");
    // One wave on a pool of 4, long enough to read status while it runs.
    counter::make_build(&build, 4, "3");
    let make = busybee
        .cmd(&["--", "make", "run"])
        .current_dir(&build)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the build");
    busybee.wait_for("the build to run", |status| {
        status
            .leases
            .iter()
            .any(|l| l.class == "jobserver" && l.state == "running")
    });

    let table = busybee.run(&["status"]);
    let json = busybee.run(&["status", "--json"]);

    let table = stdout(&table);
    let row = table
        .lines()
        .find(|line| line.contains("make"))
        .unwrap_or_else(|| panic!("no row for the build in:\n{table}"));
    assert!(row.contains("sharing"), "build row was {row:?}");
    assert!(!row.contains("using ~"), "build row was {row:?}");

    let reply: serde_json::Value =
        serde_json::from_str(stdout(&json).trim()).expect("decode status --json");
    let leases = reply["leases"].as_array().expect("leases is an array");
    let lease = leases
        .iter()
        .find(|l| l["class"] == "jobserver" && l["state"] == "running")
        .unwrap_or_else(|| panic!("no running jobserver lease in {reply}"));
    assert!(
        lease["cores"].is_null(),
        "a jobserver lease has no per-lease usage, got {lease}"
    );

    let out = make.wait_with_output().expect("wait for the build");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
}
