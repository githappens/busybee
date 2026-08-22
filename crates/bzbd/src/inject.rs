//! Executing a [`Plan`]: the environment and argv a task is actually started
//! with. Pure — the daemon supplies the fifo path and the core count it
//! settled on at admission, and this fills the placeholders in.
//!
//! `docs/design/bzbd.md` §Classification: `{fifo}`, `{cores}` and `{cores-1}`
//! (`max(1, cores − 1)`) are the only substitution points, and only in what
//! the classifier wrote: the environment edits and the arguments it appended.
//! The user's own command line is theirs, braces included. `env_unset` is
//! applied first, then `env_set`, then `env_append` joined to the caller's
//! value with a space — none when that value is empty.

use std::collections::BTreeMap;

use bzb_core::classify::Plan;

pub struct Injected {
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
}

/// `user_args` is how many of `plan.argv` the user typed; `env` is the
/// client's environment; `cores` is the task's share of the pool.
pub fn inject(
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
    // The classifier only ever appends to the user's argv, so the prefix is
    // theirs byte for byte and the placeholders can only be in the rest.
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

    /// `-jobs N` yields N+1 concurrent compilers, so the row asks for
    /// `{cores-1}`; at one core that still has to be a usable `1`.
    #[test]
    fn xcodebuild_with_one_core_gets_jobs_one() {
        let injected = inject(&plan(&["xcodebuild", "build"]), 2, env(&[]), "/run/js", 1);
        assert_eq!(injected.argv, ["xcodebuild", "build", "-jobs", "1"]);
    }

    /// The placeholders are the classifier's, in what it appended; the user's
    /// own command line is not rewritten, braces and all.
    #[test]
    fn a_placeholder_in_the_users_own_argv_is_left_alone() {
        let argv = ["sh", "-c", "echo {cores} {cores-1} {fifo}"];
        let injected = inject(&plan(&argv), argv.len(), env(&[]), "/run/js", 3);
        assert_eq!(injected.argv, argv);
    }

    /// The fifo path is whatever the state directory is, and a directory may
    /// be named like a placeholder. Text a substitution put in is never
    /// itself substituted.
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

    /// `Plan::env_append`'s contract: no leading space when the caller's
    /// value is there but empty (`PYTEST_ADDOPTS=""`).
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
