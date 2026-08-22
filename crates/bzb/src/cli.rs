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
}

impl Cli {
    /// The effective mode this invocation represents.
    pub fn mode(&self) -> Mode {
        match (&self.command, self.detach, self.cmd.is_empty()) {
            (Some(Command::Monitor), _, _) => Mode::Monitor,
            (Some(Command::Status { json }), _, _) => Mode::Status { json: *json },
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
