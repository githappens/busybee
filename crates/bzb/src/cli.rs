use bzb_core::classify::Class;
use clap::{Parser, Subcommand};

/// busybee — queued runner for resource-heavy tasks, with a live monitor.
#[derive(Debug, Parser)]
#[command(version = env!("BUSYBEE_VERSION"), about, long_about = None)]
pub struct Cli {
    /// Human-readable label for the task (shown in queue / monitor).
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// Enqueue and exit immediately; do not block or stream output.
    #[arg(long, global = true)]
    pub detach: bool,

    /// Admission class to use instead of the one busybee infers.
    #[arg(long, global = true, value_name = "jobserver|static|none")]
    pub class: Option<Class>,

    /// Cores to hold for a static task; ignored for jobserver tasks.
    #[arg(long, global = true, value_name = "N")]
    pub cores: Option<u32>,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// The command to run, passed after `--` (when no subcommand is given).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch the live TUI monitor (CPU + queue).
    Monitor,

    /// End a lease `--detach` left running (Ctrl-C cannot reach those).
    Cancel {
        /// The lease id `--detach` printed.
        lease: u64,
    },

    /// Print the token pool and one row per lease, then exit.
    ///
    /// With no daemon running there is no pool and nothing is being gated:
    /// stdout stays empty, the reason goes to stderr, and the exit code is 0.
    Status {
        /// Print the daemon's reply as one JSON object instead of a table.
        ///
        /// The fields are `protocol::StatusReply` verbatim — `pool_size`,
        /// `free`, `held` and `leases`, each lease carrying `id`, `label`,
        /// `tool`, `class`, `cores`, `state`, `elapsed_ms`, `ahead` and
        /// `pueue_task_id` — plus `approx_in_use`, which is
        /// `pool_size - free - held` clamped at 0: an estimate of what the
        /// jobserver tasks are using, never a scheduling input.
        #[arg(long)]
        json: bool,
    },

    /// Inspect or reload ~/.config/busybee/config.toml.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum ConfigAction {
    /// Print the effective configuration (defaults merged) as TOML.
    Show,
    /// Make the running bzbd re-read the config file.
    Reload,
}

impl Cli {
    /// The effective mode this invocation represents.
    pub fn mode(&self) -> Mode {
        match (&self.command, self.detach, self.cmd.is_empty()) {
            (Some(Command::Monitor), _, _) => Mode::Monitor,
            (Some(Command::Status { json }), _, _) => Mode::Status { json: *json },
            (Some(Command::Cancel { lease }), _, _) => Mode::Cancel(*lease),
            (Some(Command::Config { action }), _, _) => Mode::Config(*action),
            (None, true, false) => Mode::Detach,
            (None, false, false) => Mode::Blocking,
            (None, _, true) => Mode::MissingCommand,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Blocking,
    Detach,
    Monitor,
    Status { json: bool },
    Cancel(u64),
    Config(ConfigAction),
    MissingCommand,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("busybee").chain(args.iter().copied()))
    }

    #[test]
    fn blocking_mode_with_trailing_command() {
        let cli = parse(&["--", "echo", "hi"]);
        assert_eq!(cli.mode(), Mode::Blocking);
        assert_eq!(cli.cmd, vec!["echo", "hi"]);
        assert_eq!(cli.name, None);
        assert!(!cli.detach);
    }

    #[test]
    fn name_is_captured() {
        let cli = parse(&["--name", "my build", "--", "cargo", "build"]);
        assert_eq!(cli.name.as_deref(), Some("my build"));
        assert_eq!(cli.cmd, vec!["cargo", "build"]);
    }

    #[test]
    fn detach_mode() {
        let cli = parse(&["--detach", "--", "echo", "hi"]);
        assert_eq!(cli.mode(), Mode::Detach);
        assert!(cli.detach);
    }

    #[test]
    fn monitor_subcommand() {
        let cli = parse(&["monitor"]);
        assert_eq!(cli.mode(), Mode::Monitor);
    }

    #[test]
    fn status_subcommand_defaults_to_the_human_table() {
        assert_eq!(parse(&["status"]).mode(), Mode::Status { json: false });
        assert_eq!(
            parse(&["status", "--json"]).mode(),
            Mode::Status { json: true }
        );
    }

    #[test]
    fn cancel_subcommand_takes_a_lease_id() {
        assert_eq!(parse(&["cancel", "7"]).mode(), Mode::Cancel(7));
    }

    #[test]
    fn class_and_cores_are_captured() {
        let cli = parse(&["--class", "static", "--cores", "2", "--", "xcodebuild"]);
        assert_eq!(cli.mode(), Mode::Blocking);
        assert_eq!(cli.class, Some(Class::Static));
        assert_eq!(cli.cores, Some(2));
        assert_eq!(cli.cmd, vec!["xcodebuild"]);
    }

    /// The class vocabulary is closed, so a typo is refused rather than
    /// silently ignored on the way to the daemon.
    #[test]
    fn an_unknown_class_is_refused() {
        let parsed = Cli::try_parse_from(["busybee", "--class", "statik", "--", "make"]);
        assert!(parsed.is_err(), "an unknown class was accepted");
    }

    /// Neither flag is required: `busybee -- <cmd>` stays the whole API.
    #[test]
    fn class_and_cores_default_to_unset() {
        let cli = parse(&["--", "make"]);
        assert_eq!(cli.class, None);
        assert_eq!(cli.cores, None);
    }

    #[test]
    fn config_subcommands() {
        assert_eq!(
            parse(&["config", "show"]).mode(),
            Mode::Config(ConfigAction::Show)
        );
        assert_eq!(
            parse(&["config", "reload"]).mode(),
            Mode::Config(ConfigAction::Reload)
        );
    }

    #[test]
    fn no_args_is_missing_command() {
        let cli = parse(&[]);
        assert_eq!(cli.mode(), Mode::MissingCommand);
    }

    #[test]
    fn hyphen_values_in_user_cmd_are_preserved() {
        let cli = parse(&["--", "cmake", "--build", "build", "--target", "x"]);
        assert_eq!(cli.cmd, vec!["cmake", "--build", "build", "--target", "x"]);
    }
}
