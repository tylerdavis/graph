//! `graph tools` — inspect the tool catalog.

use crate::cli::ToolsCommand;
use crate::commands::outcome::{report, Outcome};
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::ToolRegistry;
use serde_json::json;

pub async fn run(command: ToolsCommand) -> Result<()> {
    let runtime = Runtime::init()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(&store, std::sync::Arc::new(graph_core::NullSink))
        .await?;
    let result = dispatch(toolbox.as_ref(), command).await;
    // MCP children must be shut down before the runtime drops, on every
    // path — including the one where `report` returns a SilentExit.
    runtime.shutdown().await;
    let (outcome, json) = result?;
    report(outcome, json)
}

async fn dispatch(
    registry: &(dyn ToolRegistry + Send + Sync),
    command: ToolsCommand,
) -> Result<(Outcome, bool)> {
    match command {
        ToolsCommand::List { json } => Ok((list(registry).await?, json)),
        ToolsCommand::Show { name, json } => Ok((show(registry, &name).await?, json)),
        ToolsCommand::Test {
            name,
            input,
            inputs,
            json,
        } => Ok((
            test(registry, &name, input.as_deref(), &inputs).await?,
            json,
        )),
    }
}

async fn list(registry: &(dyn ToolRegistry + Send + Sync)) -> Result<Outcome> {
    let defs = registry.tools().await?;
    // An empty catalog is a valid answer, not an error: the envelope still
    // parses, so a caller branches on `count`.
    let body = super::listing::tool_listing_as_json(&defs);
    if defs.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no tools available — configure [mcp.*] servers"));
    }
    let text = super::listing::render_tool_listing(
        &defs,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
    );
    Ok(Outcome::raw(text, body))
}

async fn show(registry: &(dyn ToolRegistry + Send + Sync), name: &str) -> Result<Outcome> {
    let defs = registry.tools().await?;
    let Some(def) = defs.into_iter().find(|d| d.name == name) else {
        // A bad argument, not a domain rejection — so no envelope, even
        // under `--json`.
        bail!("unknown tool: {name}");
    };
    let body = super::listing::tool_as_json(&def);
    let mut text = format!("{}\n\n{}\n\n", def.name, def.description);
    text.push_str(&format!(
        "input schema:\n{}\n",
        serde_json::to_string_pretty(&def.input_schema)?
    ));
    if let Some(schema) = &def.output_schema {
        text.push_str(&format!(
            "\noutput schema:\n{}\n",
            serde_json::to_string_pretty(schema)?
        ));
    }
    Ok(Outcome::raw(text, body))
}

async fn test(
    registry: &(dyn ToolRegistry + Send + Sync),
    name: &str,
    input: Option<&str>,
    inputs: &[String],
) -> Result<Outcome> {
    let input = crate::commands::input::resolve_input(input, inputs)?;
    let invoked = registry.invoke(name, input).await?;
    // A tool that reports failure is not a CLI failure: the call completed
    // and its error payload is the result worth reading. `isError` carries
    // that distinction to a machine caller, which the exit code cannot.
    if invoked.is_error {
        eprintln!("tool returned an error:");
    }
    let body = json!({
        "tool": name,
        "isError": invoked.is_error,
        "result": invoked.result,
    });
    let text = format!("{}\n", serde_json::to_string_pretty(&invoked.result)?);
    Ok(Outcome::raw(text, body))
}
