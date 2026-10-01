//! `~/.config/busybee/config.toml` (spec §Configuration). Every key is
//! optional; a file that does not parse, has an unknown key or an out-of-range
//! value is refused whole, naming the line.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
};

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use toml::Spanned;

use crate::{
    classify::{basename, Class, Inject, Rule, Table},
    errors::BusybeeError,
    jobserver::MAX_POOL,
    scheduler::Params,
};

const DEFAULT_MAX_CONCURRENT: u32 = 4;
const DEFAULT_DRAIN_DEADLINE_MS: u64 = 2000;
pub const DEFAULT_KILL_GRACE_MS: u64 = 1000;
const MIN_DRAIN_DEADLINE_MS: u64 = 100;
const MAX_DRAIN_DEADLINE_MS: u64 = 60_000;
const MIN_KILL_GRACE_MS: u64 = 100;
const MAX_KILL_GRACE_MS: u64 = 60_000;

/// The only substitutions the daemon performs (spec §Classification).
const PLACEHOLDERS: [&str; 3] = ["{cores}", "{cores-1}", "{fifo}"];

/// The effective configuration, defaults resolved; what `config show` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Config {
    pub pool_size: u32,
    pub max_concurrent: u32,
    pub drain_deadline_ms: u64,
    /// How long a signalled task has before SIGKILL follows.
    pub kill_grace_ms: u64,
    pub defaults: Defaults,
    /// Keyed as written, so `config show` keeps the file's spelling;
    /// [`Config::apply_overrides`] matches on the basename.
    pub overrides: BTreeMap<String, Override>,
}

/// Per-class defaults for the core count a task asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub r#static: StaticDefault,
}

/// `static = "fair"` (the share the scheduler works out at admission) or a
/// fixed core count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaticDefault {
    #[default]
    Fair,
    Cores(u32),
}

impl StaticDefault {
    /// The `cores_wanted` a static lease starts from: `None` leaves the
    /// scheduler's fair share in charge.
    pub fn cores_wanted(self) -> Option<u32> {
        match self {
            StaticDefault::Fair => None,
            StaticDefault::Cores(n) => Some(n),
        }
    }
}

const FAIR: &str = "fair";

impl Serialize for StaticDefault {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            StaticDefault::Fair => s.serialize_str(FAIR),
            StaticDefault::Cores(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for StaticDefault {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Cores(u32),
            Keyword(String),
        }
        match Written::deserialize(d)? {
            Written::Cores(n) => Ok(StaticDefault::Cores(n)),
            Written::Keyword(word) if word == FAIR => Ok(StaticDefault::Fair),
            Written::Keyword(word) => Err(D::Error::custom(format!(
                "static: expected {FAIR:?} or a core count, got {word:?}"
            ))),
        }
    }
}

/// One classification row from the file. It replaces the built-in row for the
/// same tool outright — class, injection and all — rather than editing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub class: Class,
    /// Values may carry the [`PLACEHOLDERS`] and nothing else.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl Config {
    pub fn defaults() -> Result<Self, BusybeeError> {
        Self::parse("", Path::new("<defaults>"))
    }

    pub fn load() -> Result<Self, BusybeeError> {
        Self::load_from(&Self::path()?)
    }

    /// A missing file is the defaults, not an error.
    pub fn load_from(path: &Path) -> Result<Self, BusybeeError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::defaults(),
            Err(err) => {
                return Err(BusybeeError::Other(format!(
                    "cannot read the config file {}: {err}",
                    path.display()
                )))
            }
        };
        Self::parse(&text, path)
    }

    /// `$BUSYBEE_CONFIG`, else `$XDG_CONFIG_HOME/busybee/config.toml`, else
    /// `~/.config/busybee/config.toml`.
    pub fn path() -> Result<PathBuf, BusybeeError> {
        path_from(
            env::var_os("BUSYBEE_CONFIG"),
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
        )
    }

    pub fn params(&self) -> Params {
        Params {
            pool_size: self.pool_size,
            max_concurrent: self.max_concurrent,
        }
    }

    /// Layer [`Config::overrides`] onto `table`. Each key replaces every
    /// built-in row for the tool it names, so the row that survives is the
    /// file's alone.
    pub fn apply_overrides(&self, table: &mut Table) {
        for (key, over) in &self.overrides {
            let tool = basename(key);
            // The file cannot spell parallelism flags, so the replaced row's
            // are kept: the `-j` notice is promised for every tool that has one
            // (spec §Classification).
            let parallel_flags = table
                .rows
                .iter()
                .find(|row| row.tool == tool && !row.parallel_flags.is_empty())
                .map(|row| row.parallel_flags.clone())
                .unwrap_or_default();
            table.rows.retain(|row| row.tool != tool);
            table.rows.push(Rule {
                tool: tool.to_string(),
                requires: None,
                class: over.class,
                // Forcing jobserver on an opaque script is the point of the
                // override, so it still gets the fifo.
                inject: match over.class {
                    Class::Jobserver => Inject::Jobserver,
                    Class::Static | Class::None => Inject::None,
                },
                parallel_flags,
                env_set: over
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            });
        }
    }

    /// What `config show` prints; parses back to the same config.
    pub fn to_toml(&self) -> Result<String, BusybeeError> {
        toml::to_string_pretty(self)
            .map_err(|err| BusybeeError::Other(format!("cannot render the config: {err}")))
    }

    fn parse(text: &str, path: &Path) -> Result<Self, BusybeeError> {
        let written: Written = toml::from_str(text)
            .map_err(|err| BusybeeError::Other(format!("{}: {err}", path.display())))?;
        let pool_size = match &written.pool_size {
            Some(n) => *n.get_ref(),
            None => logical_cores()?,
        };
        written.validate(pool_size).map_err(|refusal| {
            BusybeeError::Other(format!(
                "{}: {}",
                location(path, text, refusal.at),
                refusal.reason
            ))
        })?;
        Ok(Config {
            pool_size,
            max_concurrent: written
                .max_concurrent
                .map_or(DEFAULT_MAX_CONCURRENT, Spanned::into_inner),
            drain_deadline_ms: written
                .drain_deadline_ms
                .map_or(DEFAULT_DRAIN_DEADLINE_MS, Spanned::into_inner),
            kill_grace_ms: written
                .kill_grace_ms
                .map_or(DEFAULT_KILL_GRACE_MS, Spanned::into_inner),
            defaults: Defaults {
                r#static: written
                    .defaults
                    .r#static
                    .map_or_else(StaticDefault::default, Spanned::into_inner),
            },
            overrides: written
                .overrides
                .into_iter()
                .map(|(key, row)| {
                    let row = row.into_inner();
                    let env = row
                        .env
                        .into_iter()
                        .map(|(name, value)| (name, value.into_inner()))
                        .collect();
                    (
                        key,
                        Override {
                            class: row.class,
                            env,
                        },
                    )
                })
                .collect(),
        })
    }
}

/// The file as written: every key optional, unknown keys refused. Values that
/// [`Written::validate`] can refuse keep their [`Spanned`] position so the
/// refusal names a line, as a parse error does.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    #[serde(default)]
    pool_size: Option<Spanned<u32>>,
    #[serde(default)]
    max_concurrent: Option<Spanned<u32>>,
    #[serde(default)]
    drain_deadline_ms: Option<Spanned<u64>>,
    #[serde(default)]
    kill_grace_ms: Option<Spanned<u64>>,
    #[serde(default)]
    defaults: WrittenDefaults,
    #[serde(default)]
    overrides: BTreeMap<String, Spanned<WrittenOverride>>,
}

/// `env` values keep their own spans: in an expanded `[overrides.<tool>.env]`
/// table the assignment sits well below the row header.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenOverride {
    class: Class,
    #[serde(default)]
    env: BTreeMap<String, Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WrittenDefaults {
    #[serde(default)]
    r#static: Option<Spanned<StaticDefault>>,
}

impl Written {
    /// Ranges and placeholders. `pool_size` is the effective one, so a core
    /// count above the cap is refused too. Absent values are defaults and
    /// cannot be wrong.
    fn validate(&self, pool_size: u32) -> Result<(), Refusal> {
        fn refuse<T>(value: &Spanned<T>, reason: String) -> Result<(), Refusal> {
            Err(Refusal {
                reason,
                at: Some(value.span()),
            })
        }
        if !(1..=MAX_POOL).contains(&pool_size) {
            return Err(Refusal {
                reason: format!("pool_size must be between 1 and {MAX_POOL}, got {pool_size}"),
                at: self.pool_size.as_ref().map(Spanned::span),
            });
        }
        if let Some(n) = &self.max_concurrent {
            if *n.get_ref() == 0 {
                return refuse(n, "max_concurrent must be at least 1, got 0".to_string());
            }
        }
        if let Some(ms) = &self.drain_deadline_ms {
            if !(MIN_DRAIN_DEADLINE_MS..=MAX_DRAIN_DEADLINE_MS).contains(ms.get_ref()) {
                return refuse(
                    ms,
                    format!(
                        "drain_deadline_ms must be between {MIN_DRAIN_DEADLINE_MS} and \
                         {MAX_DRAIN_DEADLINE_MS}, got {}",
                        ms.get_ref()
                    ),
                );
            }
        }
        if let Some(ms) = &self.kill_grace_ms {
            if !(MIN_KILL_GRACE_MS..=MAX_KILL_GRACE_MS).contains(ms.get_ref()) {
                return refuse(
                    ms,
                    format!(
                        "kill_grace_ms must be between {MIN_KILL_GRACE_MS} and \
                         {MAX_KILL_GRACE_MS}, got {}",
                        ms.get_ref()
                    ),
                );
            }
        }
        if let Some(st) = &self.defaults.r#static {
            if st.get_ref().cores_wanted() == Some(0) {
                return refuse(
                    st,
                    "defaults.static must be \"fair\" or at least 1, got 0".to_string(),
                );
            }
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (key, row) in &self.overrides {
            // Rows are looked up by basename, so two keys that share one would
            // fight over a single row and the winner would be invisible.
            let tool = basename(key);
            if !seen.insert(tool) {
                return refuse(
                    row,
                    format!(
                        "two overrides match the tool {tool:?}; keys are matched on the \
                         basename, so only one of them can have the row"
                    ),
                );
            }
            for (name, value) in &row.get_ref().env {
                if let Err(reason) = check_placeholders(value.get_ref()) {
                    return refuse(value, format!("overrides.{key}.env.{name}: {reason}"));
                }
            }
        }
        Ok(())
    }
}

struct Refusal {
    reason: String,
    at: Option<Range<usize>>,
}

fn location(path: &Path, text: &str, at: Option<Range<usize>>) -> String {
    match at {
        Some(span) => format!("{} line {}", path.display(), line_of(text, span.start)),
        None => path.display().to_string(),
    }
}

/// 1-based line of byte `offset`; clamped, so a bad span cannot panic.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// Rejects any `{…}` the daemon does not substitute: it would reach the task
/// verbatim, a literal `{threads}` where a number belongs.
fn check_placeholders(value: &str) -> Result<(), String> {
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        rest = &rest[open..];
        let close = rest
            .find('}')
            .ok_or_else(|| format!("{rest:?} opens a placeholder that never closes"))?;
        let found = &rest[..=close];
        if !PLACEHOLDERS.contains(&found) {
            return Err(format!(
                "{found} is not a placeholder busybee substitutes (only {})",
                PLACEHOLDERS.join(", ")
            ));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// The default pool size. No fallback: 1 would quietly serialise every build.
fn logical_cores() -> Result<u32, BusybeeError> {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .map_err(|err| {
            BusybeeError::Other(format!(
                "cannot read the machine's logical core count ({err}); \
                 set pool_size in the config file"
            ))
        })
}

/// [`Config::path`] with the environment passed in, for tests.
fn path_from(
    busybee_config: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, BusybeeError> {
    locate(
        "config file",
        ("BUSYBEE_CONFIG", busybee_config),
        ("XDG_CONFIG_HOME", xdg_config_home),
        home,
        ".config",
        Some("config.toml"),
    )
}

/// `$var` outright, else `$xdg/busybee[/leaf]`, else
/// `$HOME/<home_base>/busybee[/leaf]`. Relative paths are refused (`$var`) or
/// count as unset (XDG, HOME): client and daemon run from different
/// directories, and would resolve different files and each auto-start a
/// daemon of its own.
pub(crate) fn locate(
    what: &str,
    (var, explicit): (&str, Option<OsString>),
    (xdg_var, xdg): (&str, Option<OsString>),
    home: Option<OsString>,
    home_base: &str,
    leaf: Option<&str>,
) -> Result<PathBuf, BusybeeError> {
    if let Some(path) = explicit {
        if !Path::new(&path).is_absolute() {
            return Err(BusybeeError::Other(format!(
                "{var} must be an absolute path, got {:?}",
                Path::new(&path)
            )));
        }
        return Ok(PathBuf::from(path));
    }
    let absolute = |d: &OsString| Path::new(d).is_absolute();
    let base = match (xdg.filter(absolute), home.filter(absolute)) {
        (Some(xdg), _) => PathBuf::from(xdg),
        (None, Some(home)) => PathBuf::from(home).join(home_base),
        (None, None) => {
            return Err(BusybeeError::Other(format!(
                "cannot locate the busybee {what}: {var} is unset and neither \
                 {xdg_var} nor HOME holds an absolute path"
            )))
        }
    };
    let dir = base.join("busybee");
    Ok(match leaf {
        Some(leaf) => dir.join(leaf),
        None => dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{classify, default_table, Class, Overrides};
    use std::ffi::OsString;

    /// The example from `docs/design/bzbd.md` §Configuration.
    const EXAMPLE: &str = r#"
pool_size = 18
max_concurrent = 4
drain_deadline_ms = 2000

[defaults]
static = "fair"

[overrides]
"./build.sh" = { class = "jobserver" }
"my-bench"   = { class = "none" }
"mytool"     = { class = "static", env = { MYTOOL_THREADS = "{cores}" } }
"#;

    fn write(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).expect("write the config");
        (dir, path)
    }

    fn load(body: &str) -> Result<Config, crate::errors::BusybeeError> {
        let (_dir, path) = write(body);
        Config::load_from(&path)
    }

    fn error(body: &str) -> String {
        load(body)
            .expect_err("the config should have been refused")
            .to_string()
    }

    #[test]
    fn the_documented_example_parses() {
        let config = load(EXAMPLE).expect("the documented example must parse");

        assert_eq!(config.pool_size, 18);
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.drain_deadline_ms, 2000);
        assert_eq!(config.defaults.r#static, StaticDefault::Fair);
        assert_eq!(config.overrides["./build.sh"].class, Class::Jobserver);
        assert_eq!(config.overrides["my-bench"].class, Class::None);
        assert_eq!(
            config.overrides["mytool"].env["MYTOOL_THREADS"],
            "{cores}".to_string()
        );
    }

    #[test]
    fn an_absent_file_is_the_default_config() {
        let dir = tempfile::tempdir().expect("create tempdir");

        let config = Config::load_from(&dir.path().join("nothing-here.toml"))
            .expect("an absent config file is not an error");

        assert_eq!(config, Config::defaults().expect("the built-in defaults"));
        assert_eq!(config.pool_size, logical_cores().expect("logical cores"));
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(config.drain_deadline_ms, DEFAULT_DRAIN_DEADLINE_MS);
        assert!(config.overrides.is_empty());
    }

    #[test]
    fn a_misspelled_key_is_refused_with_its_line() {
        let message = error("pool_size = 4\nmax_concurent = 2\n");

        assert!(message.contains("line 2"), "message was {message:?}");
        assert!(message.contains("max_concurent"), "message was {message:?}");
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused_with_its_line() {
        let message = error("pool_size = \"lots\"\n");

        assert!(message.contains("line 1"), "message was {message:?}");
    }

    /// The leading blank line keeps a message naming no line from passing.
    #[test]
    fn out_of_range_values_are_refused_with_their_line() {
        for (body, key, line) in [
            ("\npool_size = 0\n", "pool_size", 2),
            ("\npool_size = 4097\n", "pool_size", 2),
            ("\nmax_concurrent = 0\n", "max_concurrent", 2),
            ("\ndrain_deadline_ms = 99\n", "drain_deadline_ms", 2),
            ("\ndrain_deadline_ms = 60001\n", "drain_deadline_ms", 2),
            ("\n[defaults]\nstatic = 0\n", "static", 3),
        ] {
            let message = error(body);
            assert!(
                message.contains(key),
                "{body:?} was refused with {message:?}, which does not name {key}"
            );
            assert!(
                message.contains(&format!("line {line}")),
                "{body:?} was refused with {message:?}, which does not name line {line}"
            );
        }
    }

    #[test]
    fn an_unknown_class_is_refused() {
        let message = error("[overrides]\nmytool = { class = \"statik\" }\n");

        assert!(message.contains("statik"), "message was {message:?}");
    }

    #[test]
    fn an_unknown_placeholder_in_an_override_env_is_refused() {
        let message = error(
            "\n[overrides]\nmytool = { class = \"static\", \
             env = { MYTOOL_THREADS = \"{threads}\" } }\n",
        );

        assert!(message.contains("{threads}"), "message was {message:?}");
        assert!(
            message.contains("overrides.mytool.env.MYTOOL_THREADS"),
            "message was {message:?}"
        );
        assert!(message.contains("line 3"), "message was {message:?}");
    }

    #[test]
    fn an_expanded_env_table_is_refused_at_the_offending_assignment() {
        let message = error(
            "[overrides.mytool]\nclass = \"static\"\n\n\
             [overrides.mytool.env]\nGOOD = \"{cores}\"\nBAD = \"{threads}\"\n",
        );

        assert!(
            message.contains("overrides.mytool.env.BAD"),
            "message was {message:?}"
        );
        assert!(message.contains("line 6"), "message was {message:?}");
    }

    #[test]
    fn the_three_known_placeholders_are_accepted() {
        let config = load(
            "[overrides]\nmytool = { class = \"static\", \
             env = { A = \"{cores}\", B = \"-j{cores-1}\", C = \"{fifo}\" } }\n",
        )
        .expect("the documented placeholders must be accepted");

        assert_eq!(config.overrides["mytool"].env.len(), 3);
    }

    #[test]
    fn two_keys_with_the_same_basename_are_refused() {
        let message = error(
            "[overrides]\n\"./build.sh\" = { class = \"none\" }\n\
             \"build.sh\" = { class = \"jobserver\" }\n",
        );

        assert!(message.contains("build.sh"), "message was {message:?}");
        // The second key's line.
        assert!(message.contains("line 3"), "message was {message:?}");
    }

    #[test]
    fn an_override_replaces_the_whole_row_for_its_tool() {
        let config = load("[overrides]\ncargo = { class = \"none\" }\n").expect("parse");
        let mut table = default_table();

        config.apply_overrides(&mut table);

        assert_eq!(
            table.rows.iter().filter(|r| r.tool == "cargo").count(),
            1,
            "the built-in cargo row must be replaced, not shadowed"
        );
        let plan = classify(
            &["cargo".to_string(), "build".to_string()],
            &Overrides::default(),
            &table,
        );
        assert_eq!(plan.class, Class::None);
        assert!(
            !plan.env_set.iter().any(|(k, _)| k == "MAKEFLAGS"),
            "the replaced row must not keep cargo's jobserver injection: {:?}",
            plan.env_set
        );
    }

    #[test]
    fn an_override_keeps_the_replaced_rows_parallelism_flags() {
        let config = load("[overrides]\ncargo = { class = \"jobserver\" }\n").expect("parse");
        let mut table = default_table();
        config.apply_overrides(&mut table);

        let plan = classify(
            &["cargo".to_string(), "build".to_string(), "-j8".to_string()],
            &Overrides::default(),
            &table,
        );

        assert!(
            plan.notices.iter().any(|n| n.contains("-j8")),
            "notices were {:?}",
            plan.notices
        );
    }

    #[test]
    fn a_path_shaped_key_matches_the_script_it_names() {
        let config =
            load("[overrides]\n\"./build.sh\" = { class = \"jobserver\" }\n").expect("parse");
        let mut table = default_table();
        config.apply_overrides(&mut table);

        let plan = classify(&["./build.sh".to_string()], &Overrides::default(), &table);

        assert_eq!(plan.class, Class::Jobserver);
        assert!(
            plan.env_set
                .iter()
                .any(|(k, v)| k == "MAKEFLAGS" && v.contains("{fifo}")),
            "a forced jobserver row still gets the fifo: {:?}",
            plan.env_set
        );
    }

    #[test]
    fn override_env_reaches_the_plan() {
        let config = load(
            "[overrides]\nmytool = { class = \"static\", env = { MYTOOL_THREADS = \"{cores}\" } }\n",
        )
        .expect("parse");
        let mut table = default_table();
        config.apply_overrides(&mut table);

        let plan = classify(&["mytool".to_string()], &Overrides::default(), &table);

        assert_eq!(plan.class, Class::Static);
        assert!(
            plan.env_set
                .contains(&("MYTOOL_THREADS".to_string(), "{cores}".to_string())),
            "env_set was {:?}",
            plan.env_set
        );
    }

    /// The task would keep the class that reserves no tokens and lose the pool
    /// that bounds it.
    #[test]
    fn override_env_cannot_take_the_jobserver_authentication() {
        let config = load(
            "[overrides]\nmytool = { class = \"jobserver\", env = { MAKEFLAGS = \"-j16\" } }\n",
        )
        .expect("parse");
        let mut table = default_table();
        config.apply_overrides(&mut table);

        let plan = classify(&["mytool".to_string()], &Overrides::default(), &table);

        assert_eq!(plan.class, Class::Jobserver);
        let makeflags: Vec<&String> = plan
            .env_set
            .iter()
            .filter(|(k, _)| k == "MAKEFLAGS")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            makeflags,
            vec!["--jobserver-auth=fifo:{fifo}"],
            "the configured value must not reach the plan at all: {:?}",
            plan.env_set
        );
        assert!(
            plan.notices.iter().any(|n| n.contains("MAKEFLAGS")),
            "dropping a configured value has to say so: {:?}",
            plan.notices
        );
    }

    /// Cargo prefers `CARGO_MAKEFLAGS` over `MAKEFLAGS`, so a jobserver row
    /// owns both keys.
    #[test]
    fn override_env_cannot_take_the_cargo_jobserver_authentication() {
        let config = load(
            "[overrides]\ncargo = { class = \"jobserver\", \
             env = { CARGO_MAKEFLAGS = \"--jobserver-auth=fifo:/elsewhere\" } }\n",
        )
        .expect("parse");
        let mut table = default_table();
        config.apply_overrides(&mut table);

        let plan = classify(
            &["cargo".to_string(), "build".to_string()],
            &Overrides::default(),
            &table,
        );

        assert_eq!(plan.class, Class::Jobserver);
        assert!(
            !plan.env_set.iter().any(|(k, _)| k == "CARGO_MAKEFLAGS"),
            "the configured authentication must not reach the plan: {:?}",
            plan.env_set
        );
        assert!(
            plan.notices.iter().any(|n| n.contains("CARGO_MAKEFLAGS")),
            "dropping a configured value has to say so: {:?}",
            plan.notices
        );
    }

    #[test]
    fn the_effective_config_round_trips_through_toml() {
        let config = load(EXAMPLE).expect("parse");

        let shown = config.to_toml().expect("render the effective config");

        assert!(shown.contains("pool_size = 18"), "shown was {shown}");
        assert!(shown.contains("static = \"fair\""), "shown was {shown}");
        let (_dir, path) = write(&shown);
        assert_eq!(Config::load_from(&path).expect("reparse"), config);
    }

    #[test]
    fn the_defaults_are_printed_too() {
        let shown = Config::defaults()
            .expect("the built-in defaults")
            .to_toml()
            .expect("render");

        for key in ["pool_size", "max_concurrent", "drain_deadline_ms", "static"] {
            assert!(shown.contains(key), "{key} is missing from {shown}");
        }
    }

    #[test]
    fn a_fixed_static_default_is_a_cores_count() {
        let config = load("[defaults]\nstatic = 3\n").expect("parse");

        assert_eq!(config.defaults.r#static, StaticDefault::Cores(3));
        assert_eq!(config.defaults.r#static.cores_wanted(), Some(3));
        assert_eq!(StaticDefault::Fair.cores_wanted(), None);
    }

    #[test]
    fn the_params_come_from_the_file() {
        let config = load("pool_size = 18\nmax_concurrent = 2\n").expect("parse");

        let params = config.params();

        assert_eq!(params.pool_size, 18);
        assert_eq!(params.max_concurrent, 2);
    }

    #[test]
    fn busybee_config_names_the_file_outright() {
        let path = path_from(
            Some(OsString::from("/tmp/somewhere/busybee.toml")),
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/someone")),
        )
        .expect("an absolute override is a path");

        assert_eq!(path, std::path::Path::new("/tmp/somewhere/busybee.toml"));
    }

    #[test]
    fn a_relative_override_is_refused() {
        let err = path_from(Some(OsString::from("config.toml")), None, None)
            .expect_err("a relative override must be refused");

        assert!(
            err.to_string().contains("BUSYBEE_CONFIG"),
            "message was {err}"
        );
    }

    #[test]
    fn xdg_config_home_wins_over_home() {
        let path = path_from(
            None,
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/someone")),
        )
        .expect("an absolute XDG_CONFIG_HOME is a path");

        assert_eq!(path, std::path::Path::new("/xdg/busybee/config.toml"));
    }

    #[test]
    fn a_relative_xdg_config_home_falls_back_to_home() {
        let path = path_from(
            None,
            Some(OsString::from("xdg")),
            Some(OsString::from("/home/someone")),
        )
        .expect("HOME is a path");

        assert_eq!(
            path,
            std::path::Path::new("/home/someone/.config/busybee/config.toml")
        );
    }

    #[test]
    fn nowhere_to_look_is_an_error_rather_than_a_guess() {
        let err = path_from(None, None, None).expect_err("there is no config path without HOME");

        assert!(err.to_string().contains("HOME"), "message was {err}");
    }
}
