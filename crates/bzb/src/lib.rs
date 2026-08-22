mod cli;
mod config;
mod detach;
mod enqueue;
mod monitor;
mod signals;
mod status;
#[cfg(test)]
mod version_parse;

use clap::Parser;
use cli::{Cli, ConfigAction, Mode};

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.mode() {
        Mode::Blocking => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(enqueue::run(cli.cmd, cli.name, cli.class, cli.cores))?;
            Ok(())
        }
        Mode::Detach => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(detach::run(cli.cmd, cli.name, cli.class, cli.cores))?;
            Ok(())
        }
        Mode::Cancel(lease) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(detach::cancel(lease))?;
            Ok(())
        }
        Mode::Monitor => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(monitor::app::run())?;
            Ok(())
        }
        Mode::Status { json } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(status::run(json))?;
            Ok(())
        }
        Mode::Config(ConfigAction::Show) => config::show(),
        Mode::Config(ConfigAction::Reload) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(config::reload())
        }
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
