mod cli;
mod commands;
mod output;
mod runtime;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Config { command } => commands::config_cmd::run(command),
        Command::Mcp { command } => commands::mcp_cmd::run(command).await,
        Command::Tools { command } => commands::tools_cmd::run(command).await,
        Command::Ask {
            message,
            thread,
            json,
            no_stream,
        } => {
            commands::ask::run(commands::ask::AskArgs {
                message,
                thread,
                json,
                no_stream,
            })
            .await
        }
        Command::Chat { thread } => commands::chat_cmd::run(thread).await,
        Command::Threads { command } => commands::threads_cmd::run(command).await,
        Command::Db { command } => commands::db_cmd::run(command).await,
        Command::Plan { command } => commands::plan_cmd::run(command).await,
        Command::Sync { .. } => {
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
    let filter = EnvFilter::try_from_env("GRAPH_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
