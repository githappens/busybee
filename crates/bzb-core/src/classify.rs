//! Turn a command line into an execution [`Plan`].
//!
//! `classify` is a pure function over argv: no IO, no environment access, no
//! knowledge of the daemon. It answers two questions — which admission class
//! the command belongs to, and which env/argv edits make the command respect
//! busybee's core budget — and hands both back as data for the daemon to
//! apply (see `docs/design/bzbd.md` §Classification).
//!
//! # Placeholders
//!
//! Neither the jobserver fifo path nor the granted core count is known at
//! classification time, so emitted values may carry exactly three
//! placeholders. They are the only substitution points; the daemon replaces
//! them verbatim when it dispatches the task:
//!
//! | placeholder | replaced with |
//! |---|---|
//! | `{fifo}` | absolute path of the jobserver fifo |
//! | `{cores}` | the task's fair share of the pool at admission |
//! | `{cores-1}` | `max(1, cores - 1)` |
//!
//! `{cores}` is a fair share for *every* class, jobserver included. A jobserver
//! task holds no tokens of its own, but the threads it spawns that do not speak
//! the protocol (rustc's test harness) still have to be bounded by something,
//! and the pool size is exactly the wrong number: every concurrently admitted
//! task would claim all of it.
//!
//! # Shape of a plan
//!
//! [`Plan::argv`] is the *whole* command line as received, wrappers included,
//! with any injected tokens appended — the daemon runs it as-is.
//! [`Plan::env_set`] overwrites, [`Plan::env_append`] appends to whatever the
//! caller's environment already had (space-separated, no leading space when
//! the existing value is empty), and [`Plan::env_unset`] removes.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Value written into `MAKEFLAGS` / `CARGO_MAKEFLAGS` for pool members.
const JOBSERVER_AUTH: &str = "--jobserver-auth=fifo:{fifo}";
/// `Plan::tool` for an opaque shell string (`sh -c '…'`).
const TOOL_SHELL: &str = "<shell>";
/// `Plan::tool` when there is nothing to look at (empty argv).
const TOOL_UNKNOWN: &str = "<unknown>";

const SHELLS: [&str; 4] = ["sh", "bash", "zsh", "dash"];

/// GNU make short options whose value is mandatory: in a cluster it is the rest
/// of the token (`-Cout`, `-EFOO=1`), and when the token ends there it is the
/// next argument (`-C out`).
const MAKE_REQUIRED_VALUE_OPTIONS: &str = "CEfIoW";
/// Short options whose value is optional. It still swallows the rest of the
/// token (`-Ojobs`), but never the next argument: `make -j 8` runs make with no
/// job limit and a target named `8`.
const MAKE_OPTIONAL_VALUE_OPTIONS: &str = "jlO";
/// Long options whose value is mandatory, so it may be the next argument
/// (`--file out.mk`). Only these can turn an option-shaped argument into an
/// operand; the optional-value long forms need `=` (`--jobs=8`).
const MAKE_REQUIRED_VALUE_LONG_OPTIONS: [&str; 10] = [
    "--assume-new",
    "--assume-old",
    "--directory",
    "--eval",
    "--file",
    "--include-dir",
    "--makefile",
    "--new-file",
    "--old-file",
    "--what-if",
];

/// How a task is admitted against the token pool. Also the wire form of
/// `LeaseRequest::class_override`, which is why it serialises as the same
/// lowercase names `--class` and [`Class::as_str`] use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Speaks the GNU make jobserver protocol; shares the fifo dynamically.
    Jobserver,
    /// Cannot speak jobserver; holds a fixed number of tokens for its lifetime.
    Static,
    /// Unrecognised or explicitly exclusive; takes the whole pool.
    None,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Jobserver => "jobserver",
            Class::Static => "static",
            Class::None => "none",
        }
    }
}

impl FromStr for Class {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "jobserver" => Ok(Class::Jobserver),
            "static" => Ok(Class::Static),
            "none" => Ok(Class::None),
            other => Err(format!(
                "unknown class {other:?}: expected jobserver, static or none"
            )),
        }
    }
}

/// The injection recipe attached to a table row. Each variant is a fixed set
/// of edits; the table decides which tool gets which recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inject {
    /// No edits beyond the always-present `BUSYBEE_*` variables.
    None,
    /// `MAKEFLAGS` pointing at the fifo.
    Jobserver,
    /// `MAKEFLAGS`, and clear `CMAKE_BUILD_PARALLEL_LEVEL` so the generator
    /// does not get an explicit `-j`.
    Cmake,
    /// `MAKEFLAGS`, `CARGO_MAKEFLAGS`, and `RUST_TEST_THREADS`.
    Cargo,
    /// Append `-jobs {cores-1}` to argv.
    Xcodebuild,
    /// `GOMAXPROCS`.
    Go,
    /// `CTEST_PARALLEL_LEVEL`.
    Ctest,
    /// Append `-n {cores}` to `PYTEST_ADDOPTS`.
    Pytest,
}

/// One row of the classification table.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Basename the row matches, after wrapper unwrapping.
    pub tool: String,
    /// Token that must be the tool's first argument for the row to match
    /// (`--build` for cmake, `build` for docker).
    pub requires: Option<String>,
    pub class: Class,
    pub inject: Inject,
    /// Parallelism flags the user may pass. When one is present the user wins:
    /// a notice is emitted, and for [`Inject::Xcodebuild`] the injection is
    /// skipped entirely (argv injection would otherwise duplicate the flag).
    pub parallel_flags: Vec<String>,
}

/// Ordered list of classification rows; the first match wins.
#[derive(Debug, Clone)]
pub struct Table {
    pub rows: Vec<Rule>,
}

impl Table {
    /// First row matching `tool` whose `requires` token (if any) is the tool's
    /// first argument — the position that selects a mode. cmake dispatches on
    /// it exactly (`--build`, `--install`, `--open`, `-E`), so a `--build`
    /// anywhere else is another mode's operand (`cmake --install --build`) or
    /// an argument of a payload command (`cmake -E env ./x --build`), and every
    /// non-build cmake mode falls through to `none`.
    pub fn lookup(&self, tool: &str, args: &[String]) -> Option<&Rule> {
        self.rows.iter().find(|row| {
            row.tool == tool
                && match &row.requires {
                    Some(needle) => args.first().is_some_and(|arg| arg == needle),
                    None => true,
                }
        })
    }
}

/// User-supplied overrides (`--class`, `--cores`).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub class: Option<Class>,
    pub cores: Option<u32>,
}

/// Everything the daemon needs to dispatch one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub class: Class,
    /// Basename after unwrapping, or `<shell>` / `<unknown>`.
    pub tool: String,
    /// Variables to set; values may contain `{fifo}` / `{cores}`.
    pub env_set: Vec<(String, String)>,
    /// Variables to append to (space-separated); values may contain `{cores}`.
    pub env_append: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    /// Full command line to run, possibly with `{cores}` / `{cores-1}` tokens.
    pub argv: Vec<String>,
    /// Static core count the user asked for, for the scheduler to clamp.
    /// Never set for [`Class::Jobserver`], which rebalances on its own.
    pub cores_wanted: Option<u32>,
    /// One-liners for the client to print.
    pub notices: Vec<String>,
}

/// The built-in table. `#11` layers user config rows on top of this.
pub fn default_table() -> Table {
    fn rule(
        tool: &str,
        requires: Option<&str>,
        class: Class,
        inject: Inject,
        flags: &[&str],
    ) -> Rule {
        Rule {
            tool: tool.to_string(),
            requires: requires.map(str::to_string),
            class,
            inject,
            parallel_flags: flags.iter().map(|f| f.to_string()).collect(),
        }
    }

    Table {
        rows: vec![
            rule(
                "make",
                None,
                Class::Jobserver,
                Inject::Jobserver,
                &["-j", "--jobs"],
            ),
            rule(
                "gmake",
                None,
                Class::Jobserver,
                Inject::Jobserver,
                &["-j", "--jobs"],
            ),
            rule("ninja", None, Class::Jobserver, Inject::Jobserver, &["-j"]),
            rule(
                "cmake",
                Some("--build"),
                Class::Jobserver,
                Inject::Cmake,
                &["--parallel", "-j"],
            ),
            rule(
                "cargo",
                None,
                Class::Jobserver,
                Inject::Cargo,
                &["-j", "--jobs"],
            ),
            rule(
                "xcodebuild",
                None,
                Class::Static,
                Inject::Xcodebuild,
                &["-jobs"],
            ),
            rule("go", None, Class::Static, Inject::Go, &["-p"]),
            rule(
                "ctest",
                None,
                Class::Static,
                Inject::Ctest,
                &["-j", "--parallel"],
            ),
            rule("pytest", None, Class::Static, Inject::Pytest, &["-n"]),
            rule("docker", Some("build"), Class::None, Inject::None, &[]),
        ],
    }
}

/// Classify `argv` into a [`Plan`]. Total: any argv, including an empty one,
/// yields a plan.
pub fn classify(argv: &[String], overrides: &Overrides, table: &Table) -> Plan {
    let (tool, args, env_assigned) = unwrap_wrappers(argv);

    let rule = table.lookup(&tool, args);
    let table_class = rule.map_or(Class::None, |r| r.class);
    let class = overrides.class.unwrap_or(table_class);

    // Keep the table's injection when it still makes sense for the effective
    // class: fifo recipes only suit Jobserver, core-count recipes only suit
    // Static/None (both hold a fixed number of tokens). A forced jobserver
    // class always gets the fifo handed to it, which is the only way
    // `--class jobserver ./build.sh` can do anything.
    let table_inject = rule.map_or(Inject::None, |r| r.inject);
    let inject = match (class, table_inject) {
        (Class::Jobserver, Inject::Jobserver | Inject::Cmake | Inject::Cargo) => table_inject,
        (Class::Jobserver, _) => Inject::Jobserver,
        (
            Class::Static | Class::None,
            Inject::Xcodebuild | Inject::Go | Inject::Ctest | Inject::Pytest,
        ) => table_inject,
        _ => Inject::None,
    };

    let mut notices = Vec::new();
    // Only make reads clusters and option operands; other tools take the plain
    // scan, which is all their flags need.
    let user_flag = rule.and_then(|r| match tool.as_str() {
        "make" | "gmake" => find_make_jobs_flag(args),
        _ => find_parallel_flag(args, &r.parallel_flags),
    });
    if let (Some(flag), Some(rule)) = (&user_flag, rule) {
        notices.push(flag_notice(&tool, flag, rule.inject, class));
    }

    let mut plan = Plan {
        class,
        tool,
        env_set: Vec::new(),
        env_append: Vec::new(),
        env_unset: Vec::new(),
        argv: argv.to_vec(),
        cores_wanted: None,
        notices,
    };

    apply_injection(&mut plan, inject, user_flag.is_some());

    match (class, overrides.cores) {
        (Class::Jobserver, Some(_)) => plan.notices.push(
            "--cores is ignored for jobserver commands; the shared pool rebalances on its own"
                .to_string(),
        ),
        (_, cores) => plan.cores_wanted = cores,
    }

    plan.env_set
        .push(("BUSYBEE_CLASS".to_string(), class.as_str().to_string()));
    plan.env_set
        .push(("BUSYBEE_CORES".to_string(), "{cores}".to_string()));

    drop_shadowed_env(&mut plan, &env_assigned);

    plan
}

/// `env NAME=value` operands survive in [`Plan::argv`], and `env` applies them
/// *after* the daemon has set up the task's environment — so they win. Drop the
/// edits they would override rather than emitting a plan that provably does not
/// take effect, and say which ones went.
fn drop_shadowed_env(plan: &mut Plan, assigned: &[&str]) {
    let shadowed = |name: &str| assigned.contains(&name);

    let hit: Vec<String> = plan
        .env_set
        .iter()
        .chain(plan.env_append.iter())
        .map(|(k, _)| k)
        .chain(plan.env_unset.iter())
        .filter(|k| shadowed(k))
        .cloned()
        .collect();

    plan.env_set.retain(|(k, _)| !shadowed(k));
    plan.env_append.retain(|(k, _)| !shadowed(k));
    plan.env_unset.retain(|k| !shadowed(k));

    for name in hit {
        plan.notices.push(format!(
            "your env sets {name}; it takes precedence over busybee's value"
        ));
    }
}

fn apply_injection(plan: &mut Plan, inject: Inject, user_flag: bool) {
    let set = |plan: &mut Plan, key: &str, value: &str| {
        plan.env_set.push((key.to_string(), value.to_string()));
    };

    match inject {
        Inject::None => {}
        Inject::Jobserver => set(plan, "MAKEFLAGS", JOBSERVER_AUTH),
        Inject::Cmake => {
            set(plan, "MAKEFLAGS", JOBSERVER_AUTH);
            plan.env_unset
                .push("CMAKE_BUILD_PARALLEL_LEVEL".to_string());
        }
        Inject::Cargo => {
            set(plan, "MAKEFLAGS", JOBSERVER_AUTH);
            set(plan, "CARGO_MAKEFLAGS", JOBSERVER_AUTH);
            // Test threads are not token-accounted; bound them to the fair
            // share so concurrent `cargo test` runs do not each take the pool.
            set(plan, "RUST_TEST_THREADS", "{cores}");
        }
        Inject::Xcodebuild => {
            // argv injection would duplicate a user-supplied -jobs.
            if !user_flag {
                plan.argv.push("-jobs".to_string());
                plan.argv.push("{cores-1}".to_string());
            }
        }
        Inject::Go => set(plan, "GOMAXPROCS", "{cores}"),
        Inject::Ctest => set(plan, "CTEST_PARALLEL_LEVEL", "{cores}"),
        Inject::Pytest => plan
            .env_append
            .push(("PYTEST_ADDOPTS".to_string(), "-n {cores}".to_string())),
    }
}

/// First token in `args` that is one of `flags`. Short flags also match when
/// the value is glued on (`-j8`) or attached with `=` (`-j=8`); a longer flag
/// that merely starts with the same letters (`-pkgdir` vs `-p`) does not.
fn find_parallel_flag<'a>(args: &'a [String], flags: &[String]) -> Option<&'a str> {
    args.iter()
        .find(|arg| flags.iter().any(|flag| flag_matches(flag, arg)))
        .map(String::as_str)
}

fn flag_matches(flag: &str, arg: &str) -> bool {
    if arg == flag {
        return true;
    }
    let Some(rest) = arg.strip_prefix(flag) else {
        return false;
    };
    if let Some(value) = rest.strip_prefix('=') {
        return !value.is_empty();
    }
    // Only short flags glue their value on: `-j8`, never `--jobs8`.
    !flag.starts_with("--") && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// The jobs flag GNU make itself will see, which the plain scan cannot find:
/// make clusters short options, so `make -ksj8` puts `-j8` in `MAKEFLAGS` and
/// overrides the injected jobserver just as a standalone `-j8` would. Walking
/// argv the way getopt does is what keeps the cluster reading honest — `--`
/// ends the options, and an option whose value is mandatory takes the next
/// argument as an operand, so it is not a cluster at all (`make -f -kj` builds
/// a makefile named `-kj`).
fn find_make_jobs_flag(args: &[String]) -> Option<&str> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        // Targets and variable assignments select nothing.
        let Some(option) = arg.strip_prefix('-') else {
            continue;
        };
        if option == "-" {
            return None; // `--`: only operands follow
        }
        if option.starts_with('-') {
            // An empty value is not a job count; `--jobs=` never runs anyway.
            if make_long_option_matches("--jobs", arg) && !arg.ends_with('=') {
                return Some(arg);
            }
            // Without `=` the mandatory value is the next argument, whatever it
            // looks like (`make --inc -j8` includes a directory named `-j8`).
            if !arg.contains('=')
                && MAKE_REQUIRED_VALUE_LONG_OPTIONS
                    .iter()
                    .any(|long| make_long_option_matches(long, arg))
            {
                rest.next();
            }
            continue;
        }
        // The first option in a cluster that takes a value swallows the rest of
        // the token (`-Cjobs` is `-C jobs`, not a job count), so the cluster
        // ends there — either at `-j` or at an option that hides it.
        let Some((at, opt)) = option.char_indices().find(|(_, c)| {
            MAKE_REQUIRED_VALUE_OPTIONS.contains(*c) || MAKE_OPTIONAL_VALUE_OPTIONS.contains(*c)
        }) else {
            continue;
        };
        if opt == 'j' {
            return Some(arg);
        }
        // The option is ASCII, so the token ends right after it at `at + 1`.
        if MAKE_REQUIRED_VALUE_OPTIONS.contains(opt) && at + 1 == option.len() {
            rest.next(); // the value is the next argument
        }
    }
    None
}

/// Whether `arg` names the long option `name`. GNU make takes any unambiguous
/// prefix (`--inc` is `--include-dir`, `--jo` is `--jobs`), optionally with a
/// glued `=value`. Prefixes that are ambiguous in real make (`--j` is both
/// `--jobs` and `--just-print`) are matched here too, but make rejects those
/// command lines outright, so nothing runs on the wrong reading.
fn make_long_option_matches(name: &str, arg: &str) -> bool {
    let option = arg.split_once('=').map_or(arg, |(option, _)| option);
    // `--` alone is the option terminator, not an abbreviation of everything.
    option.len() > 2 && name.starts_with(option)
}

fn flag_notice(tool: &str, flag: &str, inject: Inject, class: Class) -> String {
    match (tool, inject, class) {
        ("ninja", _, _) => {
            format!("you passed {flag}; ninja ignores the pool when -j is explicit")
        }
        (_, Inject::Xcodebuild, _) => {
            format!("you passed {flag}; busybee will not add its own -jobs")
        }
        (_, _, Class::Jobserver) => {
            format!("you passed {flag}; {tool} will use it instead of the shared pool")
        }
        _ => format!("you passed {flag}; it takes precedence over busybee's core count"),
    }
}

/// Skip wrapper commands until the first token that actually runs something.
/// Returns the tool's basename, the tokens after it, and the variable names any
/// `env` wrapper assigns along the way. An opaque command (empty argv, a shell
/// string, a wrapper we refuse to parse) yields a label and no arguments, so no
/// table row can match it.
fn unwrap_wrappers(argv: &[String]) -> (String, &[String], Vec<&str>) {
    let mut rest = argv;
    let mut assigned = Vec::new();

    loop {
        let Some(first) = rest.first() else {
            return (TOOL_UNKNOWN.to_string(), &[], assigned);
        };
        let name = basename(first);
        let args = &rest[1..];

        if SHELLS.contains(&name) {
            return (TOOL_SHELL.to_string(), &[], assigned);
        }

        let skipped = match name {
            "nix" => skip_nix(args),
            "env" => skip_env(args),
            "caffeinate" => skip_flags(args, &["-t", "-w"]),
            "nice" => skip_flags(args, &["-n"]),
            _ => None,
        };

        match skipped {
            // A wrapper that swallowed the rest of the line (`nix develop`
            // with no `-c`, `env -i …`) is opaque: we cannot say what runs.
            Some(0) => return (name.to_string(), &[], assigned),
            Some(n) => {
                if name == "env" {
                    assigned.extend(args[..n].iter().filter_map(|a| Some(a.split_once('=')?.0)));
                }
                rest = &args[n..];
            }
            None => return (name.to_string(), args, assigned),
        }
    }
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// `nix develop|shell [args] -c|--command <cmd>`: number of tokens after
/// `nix` to skip, or `Some(0)` when there is no `-c` to unwrap past.
fn skip_nix(args: &[String]) -> Option<usize> {
    let sub = args.first()?.as_str();
    if sub != "develop" && sub != "shell" {
        return None;
    }
    let at = args
        .iter()
        .position(|a| a == "-c" || a == "--command")
        .unwrap_or(usize::MAX);
    if at == usize::MAX || at + 1 >= args.len() {
        return Some(0);
    }
    Some(at + 1)
}

/// `env [NAME=value …] <cmd>`: assignments are skipped, anything else (`-i`,
/// `-u NAME`) makes the invocation opaque.
fn skip_env(args: &[String]) -> Option<usize> {
    let mut n = 0;
    while let Some(arg) = args.get(n) {
        if arg.starts_with('-') {
            return Some(0);
        }
        if !arg.contains('=') {
            break;
        }
        n += 1;
    }
    if n >= args.len() {
        return Some(0);
    }
    Some(n)
}

/// Leading `-flags` of a wrapper, where the flags in `with_value` consume the
/// following token as well.
fn skip_flags(args: &[String], with_value: &[&str]) -> Option<usize> {
    let mut n = 0;
    while let Some(arg) = args.get(n) {
        if !arg.starts_with('-') {
            break;
        }
        n += if with_value.contains(&arg.as_str()) {
            2
        } else {
            1
        };
    }
    if n >= args.len() {
        return Some(0);
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_for(argv: &[&str]) -> Plan {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        classify(&argv, &Overrides::default(), &default_table())
    }

    #[test]
    fn empty_argv_is_total() {
        let plan = plan_for(&[]);
        assert_eq!(plan.class, Class::None);
        assert_eq!(plan.tool, TOOL_UNKNOWN);
        assert!(plan.argv.is_empty());
    }

    #[test]
    fn odd_quoting_and_unknown_flags_do_not_panic() {
        for argv in [
            vec!["--"],
            vec!["-"],
            vec![""],
            vec!["/"],
            vec!["nix"],
            vec!["nix", "develop", "-c"],
            vec!["env"],
            vec!["env", "FOO=1"],
            vec!["nice", "-n"],
            vec!["caffeinate", "-t"],
            vec!["make", "--jobs="],
            vec!["ninja", "-j"],
            vec!["sh"],
            vec!["'quoted arg'", "\"another\""],
        ] {
            let plan = plan_for(&argv);
            assert!(plan.env_set.iter().any(|(k, _)| k == "BUSYBEE_CLASS"));
        }
    }

    #[test]
    fn busybee_env_is_always_present() {
        let plan = plan_for(&["go", "test", "./..."]);
        assert!(plan
            .env_set
            .contains(&("BUSYBEE_CLASS".to_string(), "static".to_string())));
        assert!(plan
            .env_set
            .contains(&("BUSYBEE_CORES".to_string(), "{cores}".to_string())));
    }

    #[test]
    fn short_flags_match_glued_values_only_when_numeric() {
        assert!(flag_matches("-j", "-j8"));
        assert!(flag_matches("-j", "-j=8"));
        assert!(flag_matches("-j", "-j"));
        assert!(!flag_matches("-j", "-jobs"));
        assert!(!flag_matches("-p", "-pkgdir"));
        assert!(!flag_matches("--jobs", "--jobs8"));
        assert!(flag_matches("--jobs", "--jobs=8"));
        assert!(!flag_matches("--jobs", "--jobs="));
    }

    #[test]
    fn class_round_trips_through_strings() {
        for class in [Class::Jobserver, Class::Static, Class::None] {
            assert_eq!(Class::from_str(class.as_str()), Ok(class));
        }
        assert!(Class::from_str("parallel").is_err());
    }
}
