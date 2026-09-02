mod cli;
mod commands;
mod interlocutor;
mod mcp_server;
mod output;
mod runtime;
mod workbench;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use output::SilentExit;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // The workbench owns the terminal, so it routes tracing to a log file
    // itself instead of stderr.
    if !matches!(cli.command, Command::Workbench { .. }) {
        init_tracing(cli.verbose);
    }

    // The one place a command's outcome becomes a process exit status. Commands
    // signal a specific code with `SilentExit` rather than `process::exit`, so
    // that everything they own is dropped — MCP children shut down, stdout
    // flushed — before the process ends. See `output::SilentExit`.
    match dispatch(cli.command, cli.verbose).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => match error.downcast::<SilentExit>() {
            // Already reported on the command's own terms; don't print twice.
            Ok(exit) => ExitCode::from(exit.code as u8),
            // Same rendering anyhow's own `Termination` gives: message chain,
            // plus a backtrace when RUST_BACKTRACE is set.
            Err(error) => {
                eprintln!("Error: {error:?}");
                ExitCode::FAILURE
            }
        },
    }
}

async fn dispatch(command: Command, verbose: u8) -> Result<()> {
    match command {
        Command::Config { command } => commands::config_cmd::run(command),
        // `serve` owns stdio for the whole process, so it never reaches the
        // McpCommand dispatcher (which opens a client manager first).
        Command::Mcp {
            command: cli::McpCommand::Serve { dir },
        } => mcp_server::serve(dir).await,
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
        Command::Shapes { command } => commands::shapes_cmd::run(command).await,
        Command::Plan { command } => commands::plan_cmd::run(command).await,
        Command::Workbench { command } => workbench::run(command, verbose).await,
        Command::Version { json } => commands::version_cmd::run(json),
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
