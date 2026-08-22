//! Table-driven test over `tests/fixtures/classify_cases.toml`.

use std::str::FromStr;

use bzb_core::classify::{classify, default_table, Class, Overrides, Plan};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixture {
    case: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    argv: Vec<String>,
    class: String,
    tool: String,
    override_class: Option<String>,
    override_cores: Option<u32>,
    cores_wanted: Option<u32>,
    #[serde(default)]
    env_set_contains: Vec<String>,
    #[serde(default)]
    env_set_absent: Vec<String>,
    #[serde(default)]
    env_unset_contains: Vec<String>,
    #[serde(default)]
    env_unset_absent: Vec<String>,
    #[serde(default)]
    env_append_contains: Vec<String>,
    #[serde(default)]
    env_append_absent: Vec<String>,
    expect_argv: Option<Vec<String>>,
    #[serde(default)]
    notices_contain: Vec<String>,
    #[serde(default)]
    no_notices: bool,
}

fn parse_class(name: &str, raw: &str) -> Class {
    Class::from_str(raw).unwrap_or_else(|e| panic!("case {name}: {e}"))
}

fn assert_pairs(name: &str, label: &str, pairs: &[(String, String)], expected: &[String]) {
    for want in expected {
        let (key, value) = want
            .split_once('=')
            .unwrap_or_else(|| panic!("case {name}: {label} entry {want:?} needs a '='"));
        assert!(
            pairs.iter().any(|(k, v)| k == key && v == value),
            "case {name}: expected {label} to contain {key}={value}, got {pairs:?}"
        );
    }
}

#[test]
fn fixture_cases_classify_as_expected() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/classify_cases.toml"
    ))
    .expect("fixture file is readable");
    let fixture: Fixture = toml::from_str(&raw).expect("fixture parses");
    let table = default_table();

    assert!(!fixture.case.is_empty(), "fixture has cases");

    for case in &fixture.case {
        let overrides = Overrides {
            class: case
                .override_class
                .as_deref()
                .map(|c| parse_class(&case.name, c)),
            cores: case.override_cores,
        };
        let plan: Plan = classify(&case.argv, &overrides, &table);
        let name = &case.name;

        assert_eq!(
            plan.class,
            parse_class(name, &case.class),
            "case {name}: class"
        );
        assert_eq!(plan.tool, case.tool, "case {name}: tool");
        assert_eq!(plan.cores_wanted, case.cores_wanted, "case {name}: cores");

        assert_pairs(name, "env_set", &plan.env_set, &case.env_set_contains);
        assert_pairs(
            name,
            "env_append",
            &plan.env_append,
            &case.env_append_contains,
        );

        for key in &case.env_set_absent {
            assert!(
                !plan.env_set.iter().any(|(k, _)| k == key),
                "case {name}: expected env_set to omit {key}, got {:?}",
                plan.env_set
            );
        }
        for key in &case.env_unset_contains {
            assert!(
                plan.env_unset.contains(key),
                "case {name}: expected env_unset to contain {key}, got {:?}",
                plan.env_unset
            );
        }
        for key in &case.env_unset_absent {
            assert!(
                !plan.env_unset.contains(key),
                "case {name}: expected env_unset to omit {key}, got {:?}",
                plan.env_unset
            );
        }
        for key in &case.env_append_absent {
            assert!(
                !plan.env_append.iter().any(|(k, _)| k == key),
                "case {name}: expected env_append to omit {key}, got {:?}",
                plan.env_append
            );
        }
        if let Some(expected) = &case.expect_argv {
            assert_eq!(&plan.argv, expected, "case {name}: argv");
        }
        for needle in &case.notices_contain {
            assert!(
                plan.notices.iter().any(|n| n.contains(needle)),
                "case {name}: expected a notice containing {needle:?}, got {:?}",
                plan.notices
            );
        }
        if case.no_notices {
            assert!(
                plan.notices.is_empty(),
                "case {name}: expected no notices, got {:?}",
                plan.notices
            );
        }
    }
}
