mod cli;
mod config;
mod detach;
mod enqueue;
mod monitor;
mod status;
#[cfg(test)]
mod version_parse;

use clap::Parser;
use cli::{Cli, ConfigAction, Mode};

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.mode() {
        Mode::Blocking => rt.block_on(enqueue::run(cli.cmd, cli.name, cli.class, cli.cores)),
        Mode::Detach => rt.block_on(detach::run(cli.cmd, cli.name, cli.class, cli.cores)),
        Mode::Cancel(lease) => rt.block_on(detach::cancel(lease)),
        Mode::Monitor => rt.block_on(monitor::app::run()),
        Mode::Status { json } => rt.block_on(status::run(json)),
        Mode::Config(ConfigAction::Show) => config::show(),
        Mode::Config(ConfigAction::Reload) => rt.block_on(config::reload()),
        Mode::MissingCommand => {
            let argv0 = std::env::args().next().unwrap_or_default();
            let prog = std::path::Path::new(&argv0)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("busybee");
            eprintln!(
                "busybee: no command given. Use `{prog} -- <cmd> [args...]` or `{prog} monitor`."
            );
            std::process::exit(2);
        }
    }
}
