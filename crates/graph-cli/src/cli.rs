//! Command-line interface definition.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "graph",
    version,
    about = "A command-line agent with a plan-based execution engine"
)]
pub struct Cli {
    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Ask a one-shot question (runs one agent turn)
    Ask {
        /// The message; reads stdin when omitted and piped
        message: Option<String>,
        /// Continue a thread: `--thread <id>` for a specific one, bare
        /// `--thread` for the most recent. Omit to start a new thread.
        #[arg(long)]
        thread: Option<Option<String>>,
        /// Emit a JSON envelope instead of streaming text
        #[arg(long)]
        json: bool,
        /// Print the final answer only, without streaming
        #[arg(long)]
        no_stream: bool,
    },
    /// Interactive chat (REPL)
    Chat {
        /// Continue a thread: `--thread <id>` for a specific one, bare
        /// `--thread` for the most recent. Omit to start a new thread.
        #[arg(long)]
        thread: Option<Option<String>>,
    },
    /// Manage and run plan documents
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Inspect the tool catalog (MCP, user, plan, and built-in tools)
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// Manage conversation threads
    Threads {
        #[command(subcommand)]
        command: ThreadsCommand,
    },
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect the observed-shape cache
    Shapes {
        #[command(subcommand)]
        command: ShapesCommand,
    },
    /// Show or initialize configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
pub enum PlanCommand {
    /// List available plan documents
    List,
    /// Show a plan document
    Show { name: String },
    /// Validate a plan document by name or file path
    Validate { name_or_path: String },
    /// Run a plan directly (bypasses the agent loop)
    Run {
        name: String,
        /// Inputs as a JSON object: inline ('{"a":1}'), @file.json, or - for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: Option<String>,
        /// Override individual input keys (applied on top of the JSON document)
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ToolsCommand {
    /// List every tool visible to the agent and planner
    List,
    /// Show one tool's description and schemas
    Show { name: String },
    /// Invoke a tool directly
    Test {
        name: String,
        /// Input as a JSON object: inline ('{"a":1}'), @file.json, or - for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: Option<String>,
        /// Override individual input keys (applied on top of the JSON document)
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ThreadsCommand {
    /// List threads
    List,
    /// Show a thread's messages
    Show {
        id: String,
        /// Include the full runtime state
        #[arg(long)]
        state: bool,
    },
    /// Delete a thread
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum McpCommand {
    /// List configured servers and their status
    List,
    /// List tools exposed by servers
    Tools { server: Option<String> },
    /// Connect to a server and verify initialize + tools/list
    Test { server: String },
    /// Pre-warm the observed-shape cache by invoking read-only tools
    Probe { server: Option<String> },
}

#[derive(Subcommand)]
pub enum ShapesCommand {
    /// List cached tool shapes
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one tool's cached schema and example
    Show { tool: String },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the merged effective configuration
    Show,
    /// Write a starter config file
    Init {
        /// Write to the project (./.graph/) instead of the global location
        #[arg(long)]
        project: bool,
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
        #[allow(dead_code)]
        #[arg(long, hide = true)]
        path: Option<PathBuf>,
    },
    /// Print the config file locations and which exist
    Path,
}
