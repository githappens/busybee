//! End-to-end tests for pytest's capability-aware injection through the full
//! busybee → bzbd → pueued pipeline, using a pytest stub on PATH.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{stderr, Busybee};

/// Write the pytest stub to a directory inside the test's temp area.
/// The stub records its argv and relevant env vars to a file so tests can
/// verify what the daemon actually passed.
///
/// Returns `(stub_dir, record_path)`.  The caller prepends `stub_dir` to PATH
/// and passes the record path to the busybee invocation via `PYTEST_STUB_RECORD`.
fn write_pytest_stub(busybee: &Busybee) -> (std::path::PathBuf, std::path::PathBuf) {
    let stub_dir = busybee.tmp.path().join("pytest-stub");
    std::fs::create_dir_all(&stub_dir).expect("create stub dir");
    let stub = stub_dir.join("pytest");
    let record = busybee.tmp.path().join("pytest-stub-record.txt");

    // Each invocation appends to the record file:
    //   ARG\t<arg>         — one line per positional argument
    //   ADDOPTS\t<value>   — PYTEST_ADDOPTS (empty string if unset)
    //   CORES\t<value>     — BUSYBEE_CORES
    //   END
    let script = format!(
        "#!/bin/sh\n\
         printf 'ARG\\t%s\\n' \"$@\" >> '{record}'\n\
         printf 'ADDOPTS\\t%s\\n' \"${{PYTEST_ADDOPTS}}\" >> '{record}'\n\
         printf 'CORES\\t%s\\n' \"${{BUSYBEE_CORES}}\" >> '{record}'\n\
         printf 'END\\n' >> '{record}'\n",
        record = record.display(),
    );
    std::fs::write(&stub, &script).expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    (stub_dir, record)
}

/// Parse the record file.  Each entry is a tab-delimited `KEY\tVALUE` line.
fn parse_record(path: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('\t')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn args_from_record(path: &std::path::Path) -> Vec<String> {
    parse_record(path)
        .into_iter()
        .filter(|(k, _)| k == "ARG")
        .map(|(_, v)| v)
        .collect()
}

fn addopts_from_record(path: &std::path::Path) -> String {
    parse_record(path)
        .into_iter()
        .find(|(k, _)| k == "ADDOPTS")
        .map(|(_, v)| v)
        .unwrap_or_else(|| "RECORD_MISSING".to_string())
}

fn cores_from_record(path: &std::path::Path) -> String {
    parse_record(path)
        .into_iter()
        .find(|(k, _)| k == "CORES")
        .map(|(_, v)| v)
        .unwrap_or_else(|| "RECORD_MISSING".to_string())
}

/// Build a PATH string with `stub_dir` at the front.
fn prepend_path(stub_dir: &std::path::Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", stub_dir.display(), existing)
}

// --- tests -------------------------------------------------------------------

/// `busybee -- pytest -q` must pass no `-n` and leave PYTEST_ADDOPTS unset.
///
/// On main the classifier injects `-n {cores}` into PYTEST_ADDOPTS even for a
/// plain pytest invocation, which breaks pytest when xdist is not installed.
#[test]
#[serial_test::serial]
fn pytest_without_xdist_runs() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let (stub_dir, record) = write_pytest_stub(&busybee);

    let out = busybee
        .cmd(&["--", "pytest", "-q"])
        .env("PATH", prepend_path(&stub_dir))
        .output()
        .expect("run busybee");

    assert!(
        out.status.success(),
        "busybee exited non-zero; stderr: {}",
        stderr(&out)
    );

    let args = args_from_record(&record);
    assert!(
        !args.iter().any(|a| a == "-n" || a.starts_with("-n")),
        "plain pytest must not receive -n; got args: {:?}",
        args
    );
    assert_eq!(
        addopts_from_record(&record),
        "",
        "PYTEST_ADDOPTS must be empty/unset for plain pytest; got: {:?}",
        addopts_from_record(&record)
    );
}

/// `busybee -- pytest -n auto` must reach the stub as `-n <cores>`, not `-n auto`.
///
/// `<cores>` must equal the BUSYBEE_CORES the daemon granted.
#[test]
#[serial_test::serial]
fn pytest_auto_workers_get_the_grant() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let (stub_dir, record) = write_pytest_stub(&busybee);

    let out = busybee
        .cmd(&["--", "pytest", "-n", "auto"])
        .env("PATH", prepend_path(&stub_dir))
        .output()
        .expect("run busybee");

    assert!(
        out.status.success(),
        "busybee exited non-zero; stderr: {}",
        stderr(&out)
    );

    let args = args_from_record(&record);
    let cores = cores_from_record(&record);

    // Locate -n in the args the stub received.
    let n_pos = args
        .iter()
        .position(|a| a == "-n")
        .expect("-n must be present in pytest's argv");
    let n_val = args
        .get(n_pos + 1)
        .expect("-n must be followed by its value");

    assert_ne!(n_val, "auto", "-n auto must be replaced with a core count");
    assert_eq!(
        n_val, &cores,
        "-n value must equal BUSYBEE_CORES; got -n={n_val} but BUSYBEE_CORES={cores}"
    );
}

/// `--class none` and `--cores 1` must not add or rewrite a `-n`.
#[test]
#[serial_test::serial]
fn pytest_explicit_serial_stays_serial() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let (stub_dir, record) = write_pytest_stub(&busybee);

    // --class none
    let out = busybee
        .cmd(&["--class", "none", "--", "pytest", "tests/"])
        .env("PATH", prepend_path(&stub_dir))
        .output()
        .expect("run busybee --class none");

    assert!(
        out.status.success(),
        "--class none: busybee failed; stderr: {}",
        stderr(&out)
    );
    let args = args_from_record(&record);
    assert!(
        !args.iter().any(|a| a == "-n" || a.starts_with("-n")),
        "--class none must not add -n; got: {:?}",
        args
    );
    assert_eq!(
        addopts_from_record(&record),
        "",
        "--class none must not set PYTEST_ADDOPTS; got: {:?}",
        addopts_from_record(&record)
    );

    // Clear the record before the second run.
    std::fs::remove_file(&record).ok();

    // --cores 1
    let out = busybee
        .cmd(&["--cores", "1", "--", "pytest", "tests/"])
        .env("PATH", prepend_path(&stub_dir))
        .output()
        .expect("run busybee --cores 1");

    assert!(
        out.status.success(),
        "--cores 1: busybee failed; stderr: {}",
        stderr(&out)
    );
    let args = args_from_record(&record);
    assert!(
        !args.iter().any(|a| a == "-n" || a.starts_with("-n")),
        "--cores 1 must not add -n; got: {:?}",
        args
    );
    assert_eq!(
        addopts_from_record(&record),
        "",
        "--cores 1 must not set PYTEST_ADDOPTS; got: {:?}",
        addopts_from_record(&record)
    );
}

/// A caller-supplied `PYTEST_ADDOPTS` must reach pytest exactly as set.
#[test]
#[serial_test::serial]
fn pytest_preserves_caller_addopts() {
    let Some(busybee) = Busybee::start() else {
        return;
    };
    let (stub_dir, record) = write_pytest_stub(&busybee);

    let out = busybee
        .cmd(&["--", "pytest", "tests/"])
        .env("PATH", prepend_path(&stub_dir))
        .env("PYTEST_ADDOPTS", "-x --tb=short")
        .output()
        .expect("run busybee");

    assert!(
        out.status.success(),
        "busybee failed; stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        addopts_from_record(&record),
        "-x --tb=short",
        "PYTEST_ADDOPTS must arrive unchanged; got: {:?}",
        addopts_from_record(&record)
    );
}
