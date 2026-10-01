//! Fills a [`Plan`]'s `{fifo}`, `{cores}` and `{cores-1}` placeholders, only in
//! what the classifier wrote (see `docs/design/bzbd.md` §Classification).

use std::collections::BTreeMap;

use bzb_core::classify::Plan;

pub(crate) struct Injected {
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) argv: Vec<String>,
}

/// `user_args` is how many of `plan.argv` the user typed.
pub(crate) fn inject(
    plan: &Plan,
    user_args: usize,
    mut env: BTreeMap<String, String>,
    fifo: &str,
    cores: u32,
) -> Injected {
    let cores_1 = cores.saturating_sub(1).max(1).to_string();
    let cores = cores.to_string();
    let substitutions = [
        ("{fifo}", fifo),
        ("{cores}", cores.as_str()),
        ("{cores-1}", cores_1.as_str()),
    ];
    // One pass, left to right: what a substitution puts in is never itself
    // substituted, so a fifo under a directory named `{cores}` stays put.
    let fill = |mut value: &str| {
        let mut out = String::with_capacity(value.len());
        while let Some(at) = value.find('{') {
            out.push_str(&value[..at]);
            value = &value[at..];
            match substitutions
                .iter()
                .find(|(placeholder, _)| value.starts_with(placeholder))
            {
                Some((placeholder, with)) => {
                    out.push_str(with);
                    value = &value[placeholder.len()..];
                }
                None => {
                    out.push('{');
                    value = &value[1..];
                }
            }
        }
        out.push_str(value);
        out
    };
    for name in &plan.env_unset {
        env.remove(name);
    }
    for (name, value) in &plan.env_set {
        env.insert(name.clone(), fill(value));
    }
    for (name, value) in &plan.env_append {
        let value = fill(value);
        env.entry(name.clone())
            .and_modify(|existing| {
                if !existing.is_empty() {
                    existing.push(' ');
                }
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    // The classifier only appends, so the user's prefix is never filled.
    let (user, appended) = plan.argv.split_at(user_args);
    Injected {
        env,
        argv: user
            .iter()
            .cloned()
            .chain(appended.iter().map(|arg| fill(arg)))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzb_core::classify::{classify, default_table, Overrides};

    fn plan(argv: &[&str]) -> Plan {
        let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
        classify(&argv, &Overrides::default(), &default_table())
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// `{cores-1}` floors at 1.
    #[test]
    fn xcodebuild_with_one_core_gets_jobs_one() {
        let injected = inject(&plan(&["xcodebuild", "build"]), 2, env(&[]), "/run/js", 1);
        assert_eq!(injected.argv, ["xcodebuild", "build", "-jobs", "1"]);
    }

    #[test]
    fn a_placeholder_in_the_users_own_argv_is_left_alone() {
        let argv = ["sh", "-c", "echo {cores} {cores-1} {fifo}"];
        let injected = inject(&plan(&argv), argv.len(), env(&[]), "/run/js", 3);
        assert_eq!(injected.argv, argv);
    }

    #[test]
    fn a_placeholder_shaped_fifo_path_is_kept_verbatim() {
        let injected = inject(&plan(&["make"]), 1, env(&[]), "/tmp/{cores}/js", 4);
        assert_eq!(
            injected.env.get("MAKEFLAGS").map(String::as_str),
            Some("--jobserver-auth=fifo:/tmp/{cores}/js")
        );
    }

    #[test]
    fn the_fifo_and_the_share_fill_the_placeholders() {
        let injected = inject(
            &plan(&["cargo", "test"]),
            2,
            env(&[("CMAKE_BUILD_PARALLEL_LEVEL", "9")]),
            "/run/js",
            3,
        );
        assert_eq!(
            injected.env.get("MAKEFLAGS").map(String::as_str),
            Some("--jobserver-auth=fifo:/run/js")
        );
        assert_eq!(
            injected.env.get("RUST_TEST_THREADS").map(String::as_str),
            Some("3")
        );
        assert_eq!(
            injected.env.get("BUSYBEE_CLASS").map(String::as_str),
            Some("jobserver")
        );
        assert_eq!(
            injected.env.get("BUSYBEE_CORES").map(String::as_str),
            Some("3")
        );
        // Not on cargo's row: the caller's variable is left alone.
        assert_eq!(
            injected
                .env
                .get("CMAKE_BUILD_PARALLEL_LEVEL")
                .map(String::as_str),
            Some("9")
        );
    }

    #[test]
    fn cmake_build_unsets_the_callers_parallel_level() {
        let injected = inject(
            &plan(&["cmake", "--build", "."]),
            3,
            env(&[("CMAKE_BUILD_PARALLEL_LEVEL", "9")]),
            "/run/js",
            4,
        );
        assert!(!injected.env.contains_key("CMAKE_BUILD_PARALLEL_LEVEL"));
    }

    #[test]
    fn pytest_extends_the_callers_addopts_rather_than_replacing_them() {
        let injected = inject(
            &plan(&["pytest"]),
            1,
            env(&[("PYTEST_ADDOPTS", "-q")]),
            "/run/js",
            2,
        );
        assert_eq!(
            injected.env.get("PYTEST_ADDOPTS").map(String::as_str),
            Some("-q -n 2")
        );
        assert_eq!(
            inject(&plan(&["pytest"]), 1, env(&[]), "/run/js", 2)
                .env
                .get("PYTEST_ADDOPTS")
                .map(String::as_str),
            Some("-n 2")
        );
    }

    #[test]
    fn appending_to_an_empty_value_adds_no_leading_space() {
        let injected = inject(
            &plan(&["pytest"]),
            1,
            env(&[("PYTEST_ADDOPTS", "")]),
            "/run/js",
            2,
        );
        assert_eq!(
            injected.env.get("PYTEST_ADDOPTS").map(String::as_str),
            Some("-n 2")
        );
    }
}
