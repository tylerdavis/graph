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
    /// Interactive workbench: a dual-pane TUI for building and testing plans
    #[command(visible_alias = "wb")]
    Workbench {
        #[command(subcommand)]
        command: WorkbenchCommand,
    },
}

#[derive(Subcommand)]
pub enum WorkbenchCommand {
    /// Open the plan workbench: draft plans with the chat agent, inspect
    /// steps, and run them — fully, or gated with per-tool-call confirmation
    Plan {
        /// A plan identifier or YAML file path to open; omit for a blank draft
        name_or_path: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PlanCommand {
    /// List available plan documents
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a plan document
    Show {
        name: String,
        /// Emit the document as JSON instead of YAML
        #[arg(long)]
        json: bool,
    },
    /// Validate a plan document by name or file path
    Validate {
        name_or_path: String,
        #[arg(long)]
        json: bool,
    },
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
    /// Scaffold an empty plan file to build up with `set` and `step add`
    New {
        /// Tool-name-safe identifier; also the file name
        identifier: String,
        /// Display name (defaults to the identifier)
        #[arg(long)]
        name: Option<String>,
        /// What the plan does — the agent's routing signal
        #[arg(long)]
        description: Option<String>,
        /// Write here instead of <plans dir>/<identifier>.yaml
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Draft a plan from a goal using the planner model (costs inference)
    Draft {
        /// What the plan should do, as a self-contained instruction
        goal: String,
        /// Revise this plan instead of drafting a new one
        #[arg(long, value_name = "NAME|PATH")]
        from: Option<String>,
        /// Guidance for the revision (validation problems, corrections)
        #[arg(long)]
        feedback: Option<String>,
        /// Write here instead of <plans dir>/<identifier>.yaml
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        /// Print the drafted YAML to stdout instead of writing a file
        #[arg(long)]
        stdout: bool,
        #[arg(long)]
        json: bool,
    },
    /// Set one plan attribute
    Set {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        attribute: PlanAttribute,
        /// One value; `exemplars` and `requires_servers` accept several.
        /// `input_schema`, `solver`, and `output` take JSON, @file, or -
        #[arg(required = true, value_name = "VALUE")]
        value: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Clear one optional plan attribute
    Unset {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        attribute: PlanAttribute,
        #[arg(long)]
        json: bool,
    },
    /// Add, edit, or remove a plan's steps
    Step {
        #[command(subcommand)]
        command: StepCommand,
    },
}

/// Plan-level attributes addressable by `graph plan set` / `unset`. Named
/// exactly as they appear in the plan YAML, so what an agent reads in a plan
/// file is what it writes on the command line.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PlanAttribute {
    Name,
    Description,
    Identifier,
    Exemplars,
    #[value(name = "requires_servers", alias = "requires-servers")]
    RequiresServers,
    #[value(name = "input_schema", alias = "input-schema")]
    InputSchema,
    Solver,
    Output,
}

impl PlanAttribute {
    /// The attribute's name as spelled on the command line and in the YAML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
            Self::Identifier => "identifier",
            Self::Exemplars => "exemplars",
            Self::RequiresServers => "requires_servers",
            Self::InputSchema => "input_schema",
            Self::Solver => "solver",
            Self::Output => "output",
        }
    }
}

/// Step attributes addressable by `graph plan step update` / `unset`.
/// A step's `id` is deliberately absent: renaming rewrites downstream
/// `{{id.*}}` references, so it gets its own verb (`step rename`) rather
/// than looking like a plain field write.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StepAttribute {
    #[value(name = "tool", alias = "tool_name")]
    Tool,
    Input,
    Reasoning,
}

impl StepAttribute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Input => "input",
            Self::Reasoning => "reasoning",
        }
    }
}

#[derive(Subcommand)]
pub enum StepCommand {
    /// Append a step, or anchor it before/after an existing one
    Add {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        /// Step id — how later steps reference it as {{<id>.field}}
        id: String,
        /// Tool name, or a control step: exit, agent, decide, map, reduce
        #[arg(value_name = "TOOL")]
        tool: String,
        /// The step's input object: inline JSON, @file.json, or - for stdin
        #[arg(value_name = "JSON|@FILE|-")]
        input: String,
        /// Why this step exists (carried into the plan for readers)
        #[arg(long)]
        reasoning: Option<String>,
        /// Insert before this step id instead of appending
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        before: Option<String>,
        /// Insert after this step id instead of appending
        #[arg(long, value_name = "ID")]
        after: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set one attribute of a step
    Update {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        id: String,
        attribute: StepAttribute,
        /// `input` takes JSON, @file, or -; the others take a string
        #[arg(value_name = "VALUE")]
        value: String,
        #[arg(long)]
        json: bool,
    },
    /// Rename a step, rewriting every downstream {{id.*}} reference
    Rename {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        id: String,
        new_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Clear one optional attribute of a step
    Unset {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        id: String,
        attribute: StepAttribute,
        #[arg(long)]
        json: bool,
    },
    /// Remove a step
    Rm {
        #[arg(value_name = "NAME|PATH")]
        target: String,
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ToolsCommand {
    /// List every tool visible to the agent and planner
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one tool's description and schemas
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
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
        /// Write to the global location (~/.config/graph/) instead of the project (./.graph/)
        #[arg(long)]
        global: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        // clap's own consistency check: duplicate arg ids, conflicting short
        // flags, `required` on a positional after an optional one. All of it
        // otherwise panics at runtime, on the user's terminal, and only for
        // the one subcommand that happens to be broken.
        Cli::command().debug_assert();
    }
}
