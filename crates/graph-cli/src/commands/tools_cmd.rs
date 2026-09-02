//! `graph tools` — inspect the tool catalog.

use crate::cli::ToolsCommand;
use crate::commands::outcome::{report, Outcome};
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::ToolRegistry;
use serde_json::json;

pub async fn run(command: ToolsCommand) -> Result<()> {
    if let ToolsCommand::Migrate { path, json } = command {
        return report(migrate(&path)?, json);
    }
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
        ToolsCommand::Migrate { .. } => unreachable!("handled before the runtime starts"),
    }
}

/// `graph tools migrate <path>` — rewrite one tool file to the current tool
/// format. Takes a path, not a catalog name: a file the catalog refuses is
/// the one most likely to need it.
fn migrate(path: &std::path::Path) -> Result<Outcome> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let check = |value: &serde_yaml::Value| -> Result<(), String> {
        let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
        graph_core::user_tools::parse_tool_source(&yaml).map(|_| ())
    };
    let migrated = graph_core::format::migrate_file(graph_core::format::Kind::Tool, path, &check)
        .map_err(anyhow::Error::msg)?;
    Ok(crate::commands::plan_cmd::migrated_outcome(migrated))
}

/// The catalog as an authoring caller needs to see it: invokable tools
/// plus the control-step vocabulary.
///
/// Control steps are appended here rather than added to a registry because
/// they are not invokable — the executor evaluates them. But an agent
/// writing a plan needs their schemas exactly as much as a tool's, and a
/// catalog that omits them sends it to read graph's source instead.
pub(crate) async fn catalog(
    registry: &(dyn ToolRegistry + Send + Sync),
) -> Result<Vec<graph_core::ToolDef>> {
    let mut defs = registry.tools().await?;
    defs.extend(graph_core::pipeline::control_step_defs());
    Ok(defs)
}

pub(crate) async fn list(registry: &(dyn ToolRegistry + Send + Sync)) -> Result<Outcome> {
    let defs = catalog(registry).await?;
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

pub(crate) async fn show(
    registry: &(dyn ToolRegistry + Send + Sync),
    name: &str,
) -> Result<Outcome> {
    let defs = catalog(registry).await?;
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

pub(crate) async fn test(
    registry: &(dyn ToolRegistry + Send + Sync),
    name: &str,
    input: Option<&str>,
    inputs: &[String],
) -> Result<Outcome> {
    // A control step has a description and a schema, so it looks testable
    // in a listing. It is not: the executor evaluates it, and dispatching
    // the bare name would fail deep in the registry with an unhelpful
    // "unknown tool". Say what it actually is instead.
    if graph_core::pipeline::is_control_step(name) {
        bail!(
            "'{name}' is a control step, not an invokable tool — it is evaluated \
             by the plan executor. Add it to a plan (graph plan step add) and run \
             the plan; `graph tools show {name}` describes its input."
        );
    }
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
