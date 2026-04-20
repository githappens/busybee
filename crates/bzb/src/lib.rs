mod cli;
mod detach;
mod enqueue;
mod monitor;
mod signals;
#[cfg(test)]
mod version_parse;

use clap::Parser;
use cli::{Cli, Mode};

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.mode() {
        Mode::Blocking => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(enqueue::run(cli.cmd, cli.name))?;
            Ok(())
        }
        Mode::Detach => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(detach::run(cli.cmd, cli.name))?;
            Ok(())
        }
        Mode::Monitor => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(monitor::app::run())?;
            Ok(())
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
