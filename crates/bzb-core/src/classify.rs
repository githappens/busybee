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
//! | `{cores}` | cores granted to the task (pool size for jobserver tasks) |
//! | `{cores-1}` | `max(1, cores - 1)` |
//!
//! # Shape of a plan
//!
//! [`Plan::argv`] is the *whole* command line as received, wrappers included,
//! with any injected tokens appended — the daemon runs it as-is.
//! [`Plan::env_set`] overwrites, [`Plan::env_append`] appends to whatever the
//! caller's environment already had (space-separated, no leading space when
//! the existing value is empty), and [`Plan::env_unset`] removes.

use std::str::FromStr;

/// Value written into `MAKEFLAGS` / `CARGO_MAKEFLAGS` for pool members.
const JOBSERVER_AUTH: &str = "--jobserver-auth=fifo:{fifo}";
/// `Plan::tool` for an opaque shell string (`sh -c '…'`).
const TOOL_SHELL: &str = "<shell>";
/// `Plan::tool` when there is nothing to look at (empty argv).
const TOOL_UNKNOWN: &str = "<unknown>";

const SHELLS: [&str; 4] = ["sh", "bash", "zsh", "dash"];

/// How a task is admitted against the token pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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

/// Which class an injection recipe is meaningful for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Nothing to keep or drop.
    Empty,
    /// Hands the task the shared fifo: only valid for [`Class::Jobserver`].
    Fifo,
    /// Tells the task a fixed core count: valid for [`Class::Static`] and
    /// [`Class::None`], both of which hold a fixed number of tokens.
    Count,
}

impl Inject {
    fn style(self) -> Style {
        match self {
            Inject::None => Style::Empty,
            Inject::Jobserver | Inject::Cmake | Inject::Cargo => Style::Fifo,
            Inject::Xcodebuild | Inject::Go | Inject::Ctest | Inject::Pytest => Style::Count,
        }
    }
}

/// One row of the classification table.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Basename the row matches, after wrapper unwrapping.
    pub tool: String,
    /// Extra token that must appear in the tool's arguments for the row to
    /// match (`--build` for cmake, `build` for docker).
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
    /// First row matching `tool` whose `requires` token (if any) appears in
    /// `args` (the tokens after the tool).
    pub fn lookup(&self, tool: &str, args: &[String]) -> Option<&Rule> {
        self.rows.iter().find(|row| {
            row.tool == tool
                && match &row.requires {
                    Some(needle) => args.iter().any(|a| a == needle),
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

/// What the wrapper-unwrapping loop found at the head of the command line.
enum Head<'a> {
    /// Nothing to classify (empty argv, or a wrapper we refuse to parse).
    Opaque(&'a str),
    Tool {
        tool: String,
        args: &'a [String],
    },
}

/// Classify `argv` into a [`Plan`]. Total: any argv, including an empty one,
/// yields a plan.
pub fn classify(argv: &[String], overrides: &Overrides, table: &Table) -> Plan {
    let head = unwrap_wrappers(argv);

    let (tool, args): (String, &[String]) = match head {
        Head::Opaque(label) => (label.to_string(), &[]),
        Head::Tool { tool, args } => (tool, args),
    };

    let rule = table.lookup(&tool, args);
    let table_class = rule.map_or(Class::None, |r| r.class);
    let class = overrides.class.unwrap_or(table_class);

    // Keep the table's injection when it still makes sense for the effective
    // class; a forced jobserver class always gets the fifo handed to it, which
    // is the only way `--class jobserver ./build.sh` can do anything.
    let table_inject = rule.map_or(Inject::None, |r| r.inject);
    let inject = match (class, table_inject.style()) {
        (Class::Jobserver, Style::Fifo) => table_inject,
        (Class::Jobserver, _) => Inject::Jobserver,
        (Class::Static | Class::None, Style::Count) => table_inject,
        _ => Inject::None,
    };

    let mut notices = Vec::new();
    let user_flag = rule.and_then(|r| find_parallel_flag(args, &r.parallel_flags));
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

    plan
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
            // Test threads are not token-accounted; bound them to the share.
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
fn unwrap_wrappers(argv: &[String]) -> Head<'_> {
    let mut rest = argv;

    loop {
        let Some(first) = rest.first() else {
            return Head::Opaque(TOOL_UNKNOWN);
        };
        let name = basename(first);
        let args = &rest[1..];

        if SHELLS.contains(&name) {
            return Head::Opaque(TOOL_SHELL);
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
            Some(0) => return Head::Opaque(name),
            Some(n) => rest = &args[n..],
            None => {
                return Head::Tool {
                    tool: name.to_string(),
                    args,
                }
            }
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
