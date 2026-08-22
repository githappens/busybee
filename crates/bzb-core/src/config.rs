//! The config file: `~/.config/busybee/config.toml`.
//!
//! Every key is optional, so a machine with no file at all runs on
//! [`Config::defaults`] (see `docs/design/bzbd.md` §Configuration). What the
//! file can say is pool geometry, the per-class default core count, and rows
//! that extend or replace the built-in classification table without waiting
//! for a release.
//!
//! Nothing here is applied partially. A file that does not parse, names a key
//! nobody reads, or carries a value outside its range is refused whole, with
//! the line it went wrong on: bzbd then refuses to start, and a reload keeps
//! the configuration it already had.

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
    scheduler::Params,
};

/// Leases admitted at once, when the file does not say.
pub const DEFAULT_MAX_CONCURRENT: u32 = 4;
/// How long a static task's token drain may take, when the file does not say.
pub const DEFAULT_DRAIN_DEADLINE_MS: u64 = 2000;

/// Largest pool the fifo can hold at once — the smallest pipe capacity on
/// macOS and Linux, which is what `jobserver::Jobserver` seeds into.
const MAX_POOL_SIZE: u32 = 4096;
const MIN_DRAIN_DEADLINE_MS: u64 = 100;
const MAX_DRAIN_DEADLINE_MS: u64 = 60_000;

/// The only substitutions the daemon performs on an injected value; see
/// `docs/design/bzbd.md` §Classification.
const PLACEHOLDERS: [&str; 3] = ["{cores}", "{cores-1}", "{fifo}"];

/// The effective configuration: what the file said, with every unset key
/// resolved to its default. `busybee config show` prints exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Config {
    pub pool_size: u32,
    pub max_concurrent: u32,
    pub drain_deadline_ms: u64,
    pub defaults: Defaults,
    /// Classification rows, keyed by the tool they match. Kept as written so
    /// `config show` prints the file's own spelling; [`Config::apply_overrides`]
    /// matches on the basename.
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
    /// Variables to set for the task. Values may carry the placeholders in
    /// [`PLACEHOLDERS`] and nothing else.
    // Skipped when empty so `config show` does not print an empty `env` table
    // under every row that does not have one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl Config {
    /// The configuration of a machine with no config file.
    pub fn defaults() -> Result<Self, BusybeeError> {
        Self::parse("", Path::new("<defaults>"))
    }

    /// Reads the file [`Config::path`] names.
    pub fn load() -> Result<Self, BusybeeError> {
        Self::load_from(&Self::path()?)
    }

    /// Reads `path`; a file that is not there is not an error, it is the
    /// defaults.
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

    /// The scheduler's view of this configuration.
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
            // How a tool spells parallelism is not something the file can say,
            // so it is not something a row replaces either: the notice a
            // user's own `-j` earns is promised for every tool that has one
            // (`docs/design/bzbd.md` §Classification), and it outlives the
            // built-in row that knew the flags.
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
                // A row forced to jobserver still gets the fifo handed to it —
                // that is the whole point of overriding an opaque script —
                // while static and none rows carry only what the file sets.
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

    /// The effective configuration as TOML — what `busybee config show`
    /// prints, and a file that parses back to the same thing.
    pub fn to_toml(&self) -> Result<String, BusybeeError> {
        toml::to_string_pretty(self)
            .map_err(|err| BusybeeError::Other(format!("cannot render the config: {err}")))
    }

    fn parse(text: &str, path: &Path) -> Result<Self, BusybeeError> {
        /// The file as written: every key optional, and nothing else allowed.
        /// A key nobody reads is a setting that silently does nothing.
        ///
        /// Every value [`Config::validate`] can refuse keeps the [`Spanned`]
        /// position it was written at, so a refusal names a line the way a
        /// parse error does. A value that is absent has no span and no way to
        /// be wrong: it is the default.
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
            defaults: WrittenDefaults,
            #[serde(default)]
            overrides: BTreeMap<String, Spanned<WrittenOverride>>,
        }

        /// [`Override`] as written. Its `env` values keep their own spans:
        /// written as an expanded `[overrides.<tool>.env]` table an assignment
        /// sits well below the row header, so the row's span would name a line
        /// the reader has to search from rather than the one to fix.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WrittenOverride {
            class: Class,
            #[serde(default)]
            env: BTreeMap<String, Spanned<String>>,
        }

        /// [`Defaults`] as written. Separate because [`Defaults`] is what the
        /// daemon and `config show` carry, and a span belongs to neither.
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct WrittenDefaults {
            #[serde(default)]
            r#static: Option<Spanned<StaticDefault>>,
        }

        let written: Written = toml::from_str(text)
            .map_err(|err| BusybeeError::Other(format!("{}: {err}", path.display())))?;
        let spans = Spans {
            pool_size: written.pool_size.as_ref().map(Spanned::span),
            max_concurrent: written.max_concurrent.as_ref().map(Spanned::span),
            drain_deadline_ms: written.drain_deadline_ms.as_ref().map(Spanned::span),
            r#static: written.defaults.r#static.as_ref().map(Spanned::span),
            overrides: written
                .overrides
                .iter()
                .map(|(key, row)| (key.clone(), row.span()))
                .collect(),
            env: written
                .overrides
                .iter()
                .flat_map(|(key, row)| {
                    row.as_ref()
                        .env
                        .iter()
                        .map(move |(name, value)| ((key.clone(), name.clone()), value.span()))
                })
                .collect(),
        };
        let config = Config {
            pool_size: match written.pool_size {
                Some(n) => n.into_inner(),
                None => logical_cores()?,
            },
            max_concurrent: written
                .max_concurrent
                .map_or(DEFAULT_MAX_CONCURRENT, Spanned::into_inner),
            drain_deadline_ms: written
                .drain_deadline_ms
                .map_or(DEFAULT_DRAIN_DEADLINE_MS, Spanned::into_inner),
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
                    (
                        key,
                        Override {
                            class: row.class,
                            env: row
                                .env
                                .into_iter()
                                .map(|(name, value)| (name, value.into_inner()))
                                .collect(),
                        },
                    )
                })
                .collect(),
        };
        config.validate(&spans).map_err(|refusal| {
            BusybeeError::Other(format!(
                "{}: {}",
                location(path, text, refusal.at),
                refusal.reason
            ))
        })?;
        Ok(config)
    }

    /// Ranges and placeholders. Anything refused here would otherwise reach
    /// the pool or a task as a value it cannot act on.
    fn validate(&self, spans: &Spans) -> Result<(), Refusal> {
        if !(1..=MAX_POOL_SIZE).contains(&self.pool_size) {
            return Err(Refusal::at(
                &spans.pool_size,
                format!(
                    "pool_size must be between 1 and {MAX_POOL_SIZE}, got {}",
                    self.pool_size
                ),
            ));
        }
        if self.max_concurrent == 0 {
            return Err(Refusal::at(
                &spans.max_concurrent,
                "max_concurrent must be at least 1, got 0".to_string(),
            ));
        }
        if !(MIN_DRAIN_DEADLINE_MS..=MAX_DRAIN_DEADLINE_MS).contains(&self.drain_deadline_ms) {
            return Err(Refusal::at(
                &spans.drain_deadline_ms,
                format!(
                    "drain_deadline_ms must be between {MIN_DRAIN_DEADLINE_MS} and \
                     {MAX_DRAIN_DEADLINE_MS}, got {}",
                    self.drain_deadline_ms
                ),
            ));
        }
        if self.defaults.r#static.cores_wanted() == Some(0) {
            return Err(Refusal::at(
                &spans.r#static,
                "defaults.static must be \"fair\" or at least 1, got 0".to_string(),
            ));
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (key, over) in &self.overrides {
            let row = spans.overrides.get(key).cloned();
            // Rows are looked up by basename, so two keys that share one would
            // fight over a single row and the winner would be invisible.
            let tool = basename(key);
            if !seen.insert(tool) {
                return Err(Refusal::at(
                    &row,
                    format!(
                        "two overrides match the tool {tool:?}; keys are matched on the \
                         basename, so only one of them can have the row"
                    ),
                ));
            }
            for (name, value) in &over.env {
                check_placeholders(value).map_err(|reason| {
                    // The assignment's own line, falling back to the row's for
                    // a config that was not parsed from a file at all.
                    let at = spans
                        .env
                        .get(&(key.clone(), name.clone()))
                        .cloned()
                        .or_else(|| row.clone());
                    Refusal::at(&at, format!("overrides.{key}.env.{name}: {reason}"))
                })?;
            }
        }
        Ok(())
    }
}

/// Where in the file each value that [`Config::validate`] can refuse was
/// written. Without it a refusal names the key but not the line, which is the
/// half of a parse error that makes a typo findable.
struct Spans {
    pool_size: Option<Range<usize>>,
    max_concurrent: Option<Range<usize>>,
    drain_deadline_ms: Option<Range<usize>>,
    r#static: Option<Range<usize>>,
    /// Keyed as the file wrote them; the span covers the whole row.
    overrides: BTreeMap<String, Range<usize>>,
    /// Each `env` value's own span, keyed by (override key, variable name).
    /// Separate from the row's because an expanded `[overrides.<tool>.env]`
    /// table puts the assignment lines away from the row header.
    env: BTreeMap<(String, String), Range<usize>>,
}

/// Why a file was refused, and where in it to look.
struct Refusal {
    reason: String,
    at: Option<Range<usize>>,
}

impl Refusal {
    fn at(span: &Option<Range<usize>>, reason: String) -> Self {
        Refusal {
            reason,
            at: span.clone(),
        }
    }
}

/// The file, plus the line to look at when the refusal knows one — the same
/// "line N" a parse error names.
fn location(path: &Path, text: &str, at: Option<Range<usize>>) -> String {
    match at {
        Some(span) => format!("{} line {}", path.display(), line_of(text, span.start)),
        None => path.display().to_string(),
    }
}

/// The 1-based line the byte at `offset` sits on. Counted over bytes rather
/// than by slicing, so a span the caller got wrong cannot panic here.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// Rejects any `{…}` span that is not one the daemon substitutes: an unknown
/// one would reach the task verbatim, as a literal `{threads}` where a number
/// belongs.
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

/// Logical cores, the default pool size. Not a guess: a machine whose core
/// count cannot be read gets an error naming the key to set by hand, because
/// falling back to 1 would quietly serialise every build on a 64-core box.
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

/// [`Config::path`]'s decision, with the environment passed in so it is
/// testable without touching the process's own.
fn path_from(
    busybee_config: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, BusybeeError> {
    if let Some(path) = busybee_config {
        // The daemon and the client run from different directories, so a
        // relative path would give them different files.
        if !Path::new(&path).is_absolute() {
            return Err(BusybeeError::Other(format!(
                "BUSYBEE_CONFIG must be an absolute path, got {:?}",
                Path::new(&path)
            )));
        }
        return Ok(PathBuf::from(path));
    }
    // The XDG spec says a value that is empty or not absolute counts as unset.
    if let Some(dir) = xdg_config_home.filter(|d| Path::new(d).is_absolute()) {
        return Ok(PathBuf::from(dir).join("busybee/config.toml"));
    }
    let home = home.filter(|d| Path::new(d).is_absolute()).ok_or_else(|| {
        BusybeeError::Other(
            "cannot locate the busybee config file: BUSYBEE_CONFIG is unset and neither \
                 XDG_CONFIG_HOME nor HOME holds an absolute path"
                .to_string(),
        )
    })?;
    Ok(PathBuf::from(home).join(".config/busybee/config.toml"))
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

    /// A key nobody reads is a setting that silently does nothing, and the
    /// user cannot see that from the file. Name the line so the typo is
    /// findable.
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

    /// A value inside its type but outside its range is as hard to find as a
    /// typo, so it is refused the same way: the key, and the line it is on.
    /// The leading blank line puts each offender past line 1, where a message
    /// that names no line at all cannot pass by accident.
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

    /// The daemon substitutes exactly three placeholders; anything else would
    /// reach the task verbatim, as a literal `{threads}` where a number
    /// belongs.
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

    /// Written as an expanded table, an `env` value sits lines below the row
    /// header. The line to fix is the assignment's own, not the row's.
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

    /// Rows are looked up by the tool's basename, so two keys that share one
    /// would fight over the same row and the winner would depend on nothing
    /// the user can see.
    #[test]
    fn two_keys_with_the_same_basename_are_refused() {
        let message = error(
            "[overrides]\n\"./build.sh\" = { class = \"none\" }\n\
             \"build.sh\" = { class = \"jobserver\" }\n",
        );

        assert!(message.contains("build.sh"), "message was {message:?}");
        // The line of the second of the two, which is the one that has no row
        // of its own to take.
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

    /// The file has no way to spell how a tool writes parallelism, so that is
    /// not part of what a row replaces: `docs/design/bzbd.md` §Classification
    /// promises a notice for every user-supplied parallelism flag, and a
    /// replaced row that lost the built-in scanner would stop earning it.
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

    /// `./build.sh` is what the user types; `classify` looks up the basename.
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

    /// A jobserver row's `MAKEFLAGS` is how the task reaches the fifo, so it is
    /// also how the task gets accounted. A row's own `env` may not take that
    /// key from under it: the task would keep the class that reserves no
    /// tokens and lose the pool that bounds it.
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

    /// Cargo reads `CARGO_MAKEFLAGS` in preference to `MAKEFLAGS`, so the
    /// generic jobserver recipe setting only the latter leaves a gap a row's
    /// own `env` could fill. Filling it would point the task at a fifo the
    /// scheduler never handed out, which is the same escape as replacing
    /// `MAKEFLAGS` outright — a jobserver row owns both keys.
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

    /// `busybee config show` prints what the daemon runs with, so every key is
    /// there whether the file mentioned it or not, and what it prints has to
    /// parse back to the same thing.
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

    /// Same reason `BUSYBEE_STATE_DIR` insists on one: the daemon and the
    /// client run from different directories, so a relative path would give
    /// them different files.
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

    /// The XDG spec says a relative value counts as unset.
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
