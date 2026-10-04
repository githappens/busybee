//! Pure argv → [`Plan`]: the admission class plus the env/argv edits that
//! keep the command inside its core budget, with `{fifo}`, `{cores}` and
//! `{cores-1}` placeholders for the daemon to fill. See `docs/design/bzbd.md`
//! §Classification.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Value written into `MAKEFLAGS` / `CARGO_MAKEFLAGS` for pool members.
const JOBSERVER_AUTH: &str = "--jobserver-auth=fifo:{fifo}";
/// Every variable a jobserver task's fifo authentication can arrive in. Cargo
/// reads `CARGO_MAKEFLAGS` before `MAKEFLAGS`, so a recipe that sets only the
/// latter still owns both — see the collision guard in [`classify`].
const JOBSERVER_AUTH_VARS: [&str; 2] = ["MAKEFLAGS", "CARGO_MAKEFLAGS"];
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
/// `LeaseRequest::class_override`.
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
    /// [`Inject::Cargo`] without the fifo: the test-thread cap a `cargo` row
    /// keeps when forced off the pool.
    CargoCores,
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
    /// Extra variables on top of [`Rule::inject`]: a config `[overrides]`
    /// row's `env` table. Empty for built-in rows.
    pub env_set: Vec<(String, String)>,
}

/// Ordered list of classification rows; the first match wins.
#[derive(Debug, Clone)]
pub struct Table {
    pub rows: Vec<Rule>,
}

impl Table {
    /// First row matching `tool` whose `requires` token (if any) is the first
    /// argument, the position cmake dispatches its mode on: a `--build`
    /// anywhere else is another mode's operand (`cmake --install --build`).
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
    pub env_unset: Vec<String>,
    /// Full command line to run, possibly with `{cores}` / `{cores-1}` tokens.
    pub argv: Vec<String>,
    /// Classifier-placed replacements within the user-written prefix of `argv`.
    /// Each `(index, template)` instructs the daemon to fill `template` (which
    /// may contain `{cores}`) and store the result in `argv[index]`.  Only used
    /// when the classifier rewrites a flag value the caller wrote (e.g. pytest's
    /// `-n auto` → `-n {cores}`); the appended suffix of `argv` is always filled.
    pub argv_replacements: Vec<(usize, String)>,
    /// Static core count the user asked for, for the scheduler to clamp.
    /// Never set for [`Class::Jobserver`], which rebalances on its own.
    pub cores_wanted: Option<u32>,
    /// One-liners for the client to print.
    pub notices: Vec<String>,
}

/// The built-in table; `Config::apply_overrides` layers the file's rows on top.
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
            env_set: Vec::new(),
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
            rule("pytest", None, Class::Static, Inject::Pytest, &[]),
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

    // Fifo recipes only suit Jobserver, core-count recipes only Static/None.
    // A forced jobserver class always gets the fifo (spec §Classification).
    let table_inject = rule.map_or(Inject::None, |r| r.inject);
    let inject = match (class, table_inject) {
        (Class::Jobserver, Inject::Jobserver | Inject::Cmake | Inject::Cargo) => table_inject,
        (Class::Jobserver, _) => Inject::Jobserver,
        (
            Class::Static | Class::None,
            Inject::Xcodebuild | Inject::Go | Inject::Ctest | Inject::Pytest,
        ) => table_inject,
        // Cargo's recipe carries both, so it degrades rather than disappearing.
        (Class::Static | Class::None, Inject::Cargo) => Inject::CargoCores,
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
        env_unset: Vec::new(),
        argv: argv.to_vec(),
        argv_replacements: Vec::new(),
        cores_wanted: None,
        notices,
    };

    apply_injection(&mut plan, inject, user_flag.is_some());
    // Pytest: inspect argv for -n / --numprocesses and update cores_wanted or
    // argv_replacements accordingly.  Runs after apply_injection so the plan is
    // otherwise complete; the overrides check below can still override it.
    if matches!(inject, Inject::Pytest) {
        apply_pytest_plan(&mut plan, args, overrides);
    }
    // Before the row's own variables, so the collision guard covers these.
    plan.env_set
        .push(("BUSYBEE_CLASS".to_string(), class.as_str().to_string()));
    plan.env_set
        .push(("BUSYBEE_CORES".to_string(), "{cores}".to_string()));
    // A row may not replace a variable busybee set, the fifo authentication
    // of a jobserver task (it would run outside the pool while reserving no
    // tokens), or `BUSYBEE_LEASE`, which the daemon writes at admission. The
    // dropped value is named.
    if let Some(rule) = rule {
        for (name, value) in &rule.env_set {
            let taken = plan.env_set.iter().any(|(set, _)| set == name);
            let reserved =
                class == Class::Jobserver && JOBSERVER_AUTH_VARS.contains(&name.as_str());
            let owned = name == crate::nest::LEASE_ENV;
            if taken || reserved || owned {
                plan.notices.push(format!(
                    "the {} row for {} sets {name}; busybee owns that variable and the \
                     configured value is dropped",
                    class.as_str(),
                    plan.tool
                ));
                continue;
            }
            plan.env_set.push((name.clone(), value.clone()));
        }
    }

    match (class, overrides.cores) {
        (Class::Jobserver, Some(_)) => plan.notices.push(
            "--cores is ignored for jobserver commands; the shared pool rebalances on its own"
                .to_string(),
        ),
        // Exclusive drains the whole pool; left unset so `busybee status`
        // cannot report a number nothing acts on.
        (Class::None, Some(_)) => plan.notices.push(
            "--cores is ignored for an exclusive command; it holds the whole pool until it ends"
                .to_string(),
        ),
        (_, Some(cores)) => plan.cores_wanted = Some(cores),
        // Explicit `--cores` was not given.  Keep whatever `apply_pytest_plan`
        // (or future per-tool logic) set; for all other tools it stays `None`.
        (_, None) => {}
    }

    drop_shadowed_env(&mut plan, &env_assigned);

    plan
}

/// `env NAME=value` operands in [`Plan::argv`] apply after the daemon's
/// environment and win, so drop the edits they shadow and say so.
fn drop_shadowed_env(plan: &mut Plan, assigned: &[&str]) {
    let shadowed = |name: &str| assigned.contains(&name);

    let hit: Vec<String> = plan
        .env_set
        .iter()
        .map(|(k, _)| k)
        .chain(plan.env_unset.iter())
        .filter(|k| shadowed(k))
        .cloned()
        .collect();

    plan.env_set.retain(|(k, _)| !shadowed(k));
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
        Inject::CargoCores => set(plan, "RUST_TEST_THREADS", "{cores}"),
        Inject::Go => set(plan, "GOMAXPROCS", "{cores}"),
        Inject::Ctest => set(plan, "CTEST_PARALLEL_LEVEL", "{cores}"),
        // Pytest injection is handled by `apply_pytest_plan` after this call,
        // because it needs to inspect argv and may set `cores_wanted`.
        Inject::Pytest => {}
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

/// The jobs flag GNU make will see, walking argv as getopt does: clusters
/// (`-ksj8`), `--` ending options, and mandatory values taking the next
/// argument (`make -f -kj` builds a makefile named `-kj`).
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

/// Result of scanning a pytest argv for `-n` / `--numprocesses`.
enum PytestNAnalysis {
    /// No `-n` or `--numprocesses` flag found; run serially.
    Absent,
    /// Value is `auto` or `logical`.  `args_idx` is the index within the
    /// *args* slice (i.e. relative to the tool, not the full argv) of the
    /// token that holds the value; `template` is the replacement string to
    /// store there (e.g. `"{cores}"` or `"-n{cores}"`).
    AutoOrLogical { args_idx: usize, template: String },
    /// Explicit numeric count; `0` is treated as serial by the caller.
    Count(u32),
}

/// Scan the args that follow the `pytest` tool for a `-n` / `--numprocesses`
/// flag and return how the classifier should respond.
fn analyze_pytest_n(args: &[String]) -> PytestNAnalysis {
    for (i, arg) in args.iter().enumerate() {
        let (prefix, value, at): (&str, &str, usize) = if arg == "-n" || arg == "--numprocesses" {
            match args.get(i + 1) {
                Some(v) => ("", v.as_str(), i + 1),
                None => continue,
            }
        } else if let Some(v) = arg.strip_prefix("--numprocesses=") {
            ("--numprocesses=", v, i)
        } else if let Some(v) = arg.strip_prefix("-n=") {
            ("-n=", v, i)
        } else if let Some(v) = arg.strip_prefix("-n") {
            ("-n", v, i)
        } else {
            continue;
        };

        match value {
            "auto" | "logical" => {
                return PytestNAnalysis::AutoOrLogical {
                    args_idx: at,
                    template: format!("{prefix}{{cores}}"),
                }
            }
            v => {
                if let Ok(k) = v.parse::<u32>() {
                    return PytestNAnalysis::Count(k);
                }
            }
        }
    }
    PytestNAnalysis::Absent
}

/// Update `plan` with pytest's capability-aware, opt-in injection rules.
///
/// Called when the table matched pytest and produced `Inject::Pytest`.  The
/// `args` slice is the portion of `argv` after the tool name (wrappers
/// included up-front, but already consumed by `unwrap_wrappers`).
fn apply_pytest_plan(plan: &mut Plan, args: &[String], overrides: &Overrides) {
    // --class none or --cores 1: caller opted out of any -n injection.
    if plan.class == Class::None || overrides.cores == Some(1) {
        return;
    }

    // args_offset: first element of `args` lives at plan.argv[args_offset].
    let args_offset = plan.argv.len() - args.len();

    match analyze_pytest_n(args) {
        PytestNAnalysis::Absent | PytestNAnalysis::Count(0) => {
            // No -n, or -n 0: hold exactly one token (serial run).
            plan.cores_wanted = Some(1);
        }
        PytestNAnalysis::AutoOrLogical { args_idx, template } => {
            // Replace the value token with {cores} so the daemon fills it in.
            plan.argv_replacements
                .push((args_offset + args_idx, template));
        }
        PytestNAnalysis::Count(k) => {
            // Caller specified an explicit count; respect it.
            plan.cores_wanted = Some(k);
        }
    }
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

/// What the table and `config`'s override keys are matched on.
pub(crate) fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// `nix develop|shell [args] -c|--command <cmd>`: number of tokens after
/// `nix` to skip, or `Some(0)` when there is no `-c` to unwrap past.
fn skip_nix(args: &[String]) -> Option<usize> {
    let sub = args.first()?.as_str();
    if sub != "develop" && sub != "shell" {
        return None;
    }
    match args.iter().position(|a| a == "-c" || a == "--command") {
        Some(at) if at + 1 < args.len() => Some(at + 1),
        _ => Some(0),
    }
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
    Some(if n < args.len() { n } else { 0 })
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
    Some(if n < args.len() { n } else { 0 })
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
    fn a_forced_jobserver_class_keeps_the_authentication_over_a_rows_own_env() {
        let table = Table {
            rows: vec![Rule {
                tool: "mytool".to_string(),
                requires: None,
                class: Class::Static,
                inject: Inject::None,
                parallel_flags: Vec::new(),
                env_set: vec![("MAKEFLAGS".to_string(), "-j16".to_string())],
            }],
        };
        let overrides = Overrides {
            class: Some(Class::Jobserver),
            cores: None,
        };

        let plan = classify(&["mytool".to_string()], &overrides, &table);

        assert_eq!(
            plan.env_set
                .iter()
                .filter(|(k, _)| k == "MAKEFLAGS")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>(),
            vec![JOBSERVER_AUTH],
            "env_set was {:?}",
            plan.env_set
        );
        assert!(plan.notices.iter().any(|n| n.contains("MAKEFLAGS")));
    }

    /// `BUSYBEE_LEASE` is filled at admission, so it is reserved by name.
    #[test]
    fn a_rows_own_env_cannot_take_the_busybee_variables() {
        let table = Table {
            rows: vec![Rule {
                tool: "mytool".to_string(),
                requires: None,
                class: Class::Static,
                inject: Inject::None,
                parallel_flags: Vec::new(),
                env_set: vec![
                    ("BUSYBEE_CLASS".to_string(), "none".to_string()),
                    ("BUSYBEE_CORES".to_string(), "64".to_string()),
                    (
                        crate::nest::LEASE_ENV.to_string(),
                        "not-a-real-lease".to_string(),
                    ),
                ],
            }],
        };

        let plan = classify(&["mytool".to_string()], &Overrides::default(), &table);

        assert_eq!(
            plan.env_set
                .iter()
                .filter(|(k, _)| k == "BUSYBEE_CLASS")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>(),
            vec!["static"],
            "env_set was {:?}",
            plan.env_set
        );
        assert_eq!(
            plan.env_set
                .iter()
                .filter(|(k, _)| k == "BUSYBEE_CORES")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>(),
            vec!["{cores}"],
            "env_set was {:?}",
            plan.env_set
        );
        assert!(plan.notices.iter().any(|n| n.contains("BUSYBEE_CLASS")));
        assert!(plan.notices.iter().any(|n| n.contains("BUSYBEE_CORES")));
        assert!(plan.notices.iter().any(|n| n.contains("BUSYBEE_LEASE")));
        assert!(
            !plan
                .env_set
                .iter()
                .any(|(k, _)| k == crate::nest::LEASE_ENV),
            "the daemon owns BUSYBEE_LEASE; env_set was {:?}",
            plan.env_set
        );
    }

    /// Spec §Classification: forcing static/none keeps a core-count injection
    /// and drops a fifo one; cargo's recipe has both.
    #[test]
    fn forcing_cargo_off_the_pool_keeps_its_test_thread_cap() {
        for class in [Class::Static, Class::None] {
            let plan = classify(
                &["cargo".to_string(), "test".to_string()],
                &Overrides {
                    class: Some(class),
                    cores: Some(2),
                },
                &default_table(),
            );

            assert_eq!(plan.class, class);
            let name = |k: &str| plan.env_set.iter().any(|(key, _)| key == k);
            assert!(
                name("RUST_TEST_THREADS"),
                "{class:?} lost the cap: {:?}",
                plan.env_set
            );
            assert!(
                !name("MAKEFLAGS") && !name("CARGO_MAKEFLAGS"),
                "{class:?} kept the fifo authentication: {:?}",
                plan.env_set
            );
        }
    }

    #[test]
    fn a_cores_count_an_exclusive_lease_cannot_honour_is_announced() {
        let plan = classify(
            &["unknown-tool".to_string()],
            &Overrides {
                class: None,
                cores: Some(2),
            },
            &default_table(),
        );

        assert_eq!(plan.class, Class::None);
        assert_eq!(
            plan.cores_wanted, None,
            "a count nothing acts on is not carried"
        );
        assert!(
            plan.notices
                .iter()
                .any(|n| n.contains("--cores is ignored")),
            "the caller was not told: {:?}",
            plan.notices
        );
    }

    #[test]
    fn class_round_trips_through_strings() {
        for class in [Class::Jobserver, Class::Static, Class::None] {
            assert_eq!(Class::from_str(class.as_str()), Ok(class));
        }
        assert!(Class::from_str("parallel").is_err());
    }
}
