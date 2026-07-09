mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Config { command } => commands::config_cmd::run(command),
        Command::Ask { .. }
        | Command::Chat { .. }
        | Command::Plan { .. }
        | Command::Tools { .. }
        | Command::Threads { .. }
        | Command::Mcp { .. }
        | Command::Sync { .. }
        | Command::Db { .. } => {
            anyhow::bail!("not implemented yet — this command lands in a later phase")
        }
    }
}

/// Log to stderr; default WARN, raised by -v flags, overridable via GRAPH_LOG.
fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("GRAPH_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
