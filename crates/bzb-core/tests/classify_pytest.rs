//! Classifier tests for pytest's capability-aware, opt-in parallel injection.
//! Covers every row of the decision table in `docs/design/bzbd.md` §Classification.

use bzb_core::classify::{classify, default_table, Class, Overrides};

fn plan(argv: &[&str]) -> bzb_core::classify::Plan {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    classify(&argv, &Overrides::default(), &default_table())
}

fn plan_with(argv: &[&str], class: Option<Class>, cores: Option<u32>) -> bzb_core::classify::Plan {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    classify(&argv, &Overrides { class, cores }, &default_table())
}

// --- decision table: no -n ---------------------------------------------------

/// No -n / --numprocesses: static, cores_wanted = 1, no injection.
#[test]
fn pytest_no_n_is_serial_with_one_core() {
    let p = plan(&["pytest", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(
        p.cores_wanted,
        Some(1),
        "expected cores_wanted=1 for serial pytest"
    );
    assert!(
        !p.env_set.iter().any(|(k, _)| k == "PYTEST_ADDOPTS"),
        "PYTEST_ADDOPTS must not be touched: {:?}",
        p.env_set
    );
    assert!(
        p.argv_replacements.is_empty(),
        "no argv replacement for serial pytest: {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// Plain `pytest` (no extra args at all): same serial treatment.
#[test]
fn pytest_bare_is_serial_with_one_core() {
    let p = plan(&["pytest"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, Some(1));
    assert!(
        !p.env_set.iter().any(|(k, _)| k == "PYTEST_ADDOPTS"),
        "PYTEST_ADDOPTS must not appear in env_set: {:?}",
        p.env_set
    );
    assert!(p.argv_replacements.is_empty());
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

// --- decision table: -n auto / -n logical ------------------------------------

/// -n auto: replace the value token with `{cores}` in argv; no cores_wanted (fair share).
#[test]
fn pytest_n_auto_rewrites_value_token() {
    let p = plan(&["pytest", "-n", "auto", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(
        p.cores_wanted, None,
        "no fixed count for -n auto (fair share at admission)"
    );
    // argv is unchanged except through argv_replacements
    assert_eq!(p.argv, ["pytest", "-n", "auto", "tests/"]);
    // argv[2] = "auto" should be replaced with "{cores}"
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 2 && t == "{cores}"),
        "expected argv_replacements to contain (2, \"{{cores}}\"), got {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// -n logical: same treatment as -n auto.
#[test]
fn pytest_n_logical_rewrites_value_token() {
    let p = plan(&["pytest", "-n", "logical", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, None);
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 2 && t == "{cores}"),
        "expected (2, \"{{cores}}\"), got {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// -nauto (glued form): replace the whole token with "-n{cores}".
#[test]
fn pytest_n_auto_glued_rewrites_flag_token() {
    let p = plan(&["pytest", "-nauto", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, None);
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 1 && t == "-n{cores}"),
        "expected (1, \"-n{{cores}}\"), got {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// --numprocesses=auto (long form with =): replace the whole token.
#[test]
fn pytest_numprocesses_eq_auto_rewrites_flag_token() {
    let p = plan(&["pytest", "--numprocesses=auto", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, None);
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 1 && t == "--numprocesses={cores}"),
        "expected (1, \"--numprocesses={{cores}}\"), got {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// --numprocesses logical (space-separated long form).
#[test]
fn pytest_numprocesses_logical_rewrites_value_token() {
    let p = plan(&["pytest", "--numprocesses", "logical"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, None);
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 2 && t == "{cores}"),
        "expected (2, \"{{cores}}\"), got {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

// --- decision table: -n K (explicit count) -----------------------------------

/// -n 4: static, cores_wanted = 4, argv unchanged, no injection.
#[test]
fn pytest_n_count_sets_cores_wanted() {
    let p = plan(&["pytest", "-n", "4", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, Some(4));
    assert!(
        p.argv_replacements.is_empty(),
        "no argv replacement for explicit count: {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

/// -n 1: static, cores_wanted = 1.
#[test]
fn pytest_n_one_sets_cores_wanted_one() {
    let p = plan(&["pytest", "-n", "1"]);
    assert_eq!(p.cores_wanted, Some(1));
    assert!(p.argv_replacements.is_empty());
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

// --- decision table: -n 0 ----------------------------------------------------

/// -n 0: treated as serial (cores_wanted = 1), no injection.
#[test]
fn pytest_n_zero_is_serial() {
    let p = plan(&["pytest", "-n", "0", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(
        p.cores_wanted,
        Some(1),
        "n=0 maps to serial (cores_wanted=1)"
    );
    assert!(
        p.argv_replacements.is_empty(),
        "no argv replacement for n=0: {:?}",
        p.argv_replacements
    );
    assert!(p.notices.is_empty(), "unexpected notices: {:?}", p.notices);
}

// --- --class / --cores overrides ---------------------------------------------

/// --class none: no argv rewrite and no injection, even with -n auto.
#[test]
fn pytest_class_none_skips_injection() {
    let p = plan_with(&["pytest", "-n", "auto", "tests/"], Some(Class::None), None);
    assert_eq!(p.class, Class::None);
    assert!(
        p.argv_replacements.is_empty(),
        "--class none must not rewrite argv: {:?}",
        p.argv_replacements
    );
    assert!(
        !p.env_set.iter().any(|(k, _)| k == "PYTEST_ADDOPTS"),
        "--class none must not touch PYTEST_ADDOPTS: {:?}",
        p.env_set
    );
}

/// --cores 1: no argv rewrite (even with -n auto), but cores_wanted = 1 from override.
#[test]
fn pytest_cores_one_skips_argv_rewrite() {
    let p = plan_with(&["pytest", "-n", "auto", "tests/"], None, Some(1));
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, Some(1));
    assert!(
        p.argv_replacements.is_empty(),
        "--cores 1 must not rewrite argv: {:?}",
        p.argv_replacements
    );
}

/// --cores N (N > 1) overrides a -n K cores_wanted.
#[test]
fn pytest_explicit_cores_overrides_n_count() {
    let p = plan_with(&["pytest", "-n", "4"], None, Some(2));
    assert_eq!(p.cores_wanted, Some(2), "--cores 2 should override -n 4");
}

/// --class none on plain pytest: no injection at all.
#[test]
fn pytest_class_none_no_n_no_injection() {
    let p = plan_with(&["pytest", "tests/"], Some(Class::None), None);
    assert_eq!(p.class, Class::None);
    assert!(p.argv_replacements.is_empty());
    assert!(
        !p.env_set.iter().any(|(k, _)| k == "PYTEST_ADDOPTS"),
        "PYTEST_ADDOPTS must not appear: {:?}",
        p.env_set
    );
}

// --- PYTEST_ADDOPTS is never touched -----------------------------------------

/// busybee never appends to PYTEST_ADDOPTS under any circumstances.
#[test]
fn pytest_never_appends_to_pytest_addopts() {
    for argv in [
        vec!["pytest"],
        vec!["pytest", "tests/"],
        vec!["pytest", "-n", "auto"],
        vec!["pytest", "-n", "4"],
        vec!["pytest", "-n", "0"],
    ] {
        let p = plan(&argv);
        assert!(
            !p.env_set.iter().any(|(k, _)| k == "PYTEST_ADDOPTS"),
            "PYTEST_ADDOPTS found in env_set for {:?}: {:?}",
            argv,
            p.env_set
        );
    }
}

// --- wrappers ----------------------------------------------------------------

/// nix wrapper: pytest under nix develop still gets the right classification.
#[test]
fn pytest_under_nix_develop_is_serial() {
    let p = plan(&["nix", "develop", "-c", "pytest", "tests/"]);
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.tool, "pytest");
    assert_eq!(p.cores_wanted, Some(1));
    assert!(p.argv_replacements.is_empty());
}

/// nix wrapper with -n auto: replacement index accounts for the wrappers.
#[test]
fn pytest_under_nix_develop_n_auto_uses_correct_argv_idx() {
    let p = plan(&["nix", "develop", "-c", "pytest", "-n", "auto"]);
    // argv = ["nix", "develop", "-c", "pytest", "-n", "auto"]
    // "-n" is at index 4, "auto" is at index 5
    assert_eq!(p.class, Class::Static);
    assert_eq!(p.cores_wanted, None);
    assert!(
        p.argv_replacements
            .iter()
            .any(|(i, t)| *i == 5 && t == "{cores}"),
        "expected (5, \"{{cores}}\") for wrapped argv, got {:?}",
        p.argv_replacements
    );
}
