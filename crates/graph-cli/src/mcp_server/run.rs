//! The command calls behind the MCP tools.
//!
//! Everything here reuses the same functions the CLI drives, so a plan edited
//! over MCP goes through the identical validation guard as one edited from a
//! shell. The exception is [`run_plan`]: `plan run` on the CLI streams to a
//! terminal and exits with a code, neither of which exists here, so the
//! pipeline is driven directly and the result becomes a value.

use super::runtime;
use crate::cli::StepCommand;
use crate::commands::outcome::Outcome;
use crate::commands::{listing, plan_cmd, plan_edit};
use anyhow::{bail, Result};
use graph_core::pipeline::{catalog, doc::validate_input, ExitStatus};
use serde_json::{json, Map, Value};

pub fn plan_list() -> Result<Outcome> {
    let runtime = runtime()?;
    Ok(Outcome::ok(plan_edit::list_as_json(&runtime.plan_docs())))
}

pub fn plan_show(target: &str) -> Result<Outcome> {
    let runtime = runtime()?;
    let (doc, _) = plan_cmd::resolve_target(&runtime, target)?;
    Ok(Outcome::ok(plan_edit::doc_as_json(&doc)?))
}

pub fn plan_validate(target: &str) -> Result<Outcome> {
    plan_cmd::validate(target)
}

fn target_of(args: &Map<String, Value>) -> Result<String> {
    match args.get("target").and_then(Value::as_str) {
        Some(target) => Ok(target.to_string()),
        None => bail!("`target` is required and must be a string"),
    }
}

fn string_of(args: &Map<String, Value>, key: &str) -> Result<String> {
    match args.get(key).and_then(Value::as_str) {
        Some(value) => Ok(value.to_string()),
        None => bail!("`{key}` is required and must be a string"),
    }
}

/// Step inputs arrive as real JSON objects here; the CLI takes them as a
/// string it parses. Re-serializing keeps one parser rather than two.
fn json_arg(args: &Map<String, Value>, key: &str) -> Result<String> {
    match args.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(value) => Ok(value.to_string()),
        None => bail!("`{key}` is required"),
    }
}

pub fn step_add(args: &Map<String, Value>) -> Result<Outcome> {
    plan_edit::step(StepCommand::Add {
        target: target_of(args)?,
        id: string_of(args, "id")?,
        tool: string_of(args, "tool")?,
        input: json_arg(args, "input")?,
        reasoning: args
            .get("reasoning")
            .and_then(Value::as_str)
            .map(str::to_string),
        before: args
            .get("before")
            .and_then(Value::as_str)
            .map(str::to_string),
        after: args
            .get("after")
            .and_then(Value::as_str)
            .map(str::to_string),
        json: false,
    })
}

pub fn step_update(args: &Map<String, Value>) -> Result<Outcome> {
    let attribute = string_of(args, "attribute")?;
    let value = match attribute.as_str() {
        "input" => json_arg(args, "value")?,
        _ => string_of(args, "value")?,
    };
    plan_edit::step(StepCommand::Update {
        target: target_of(args)?,
        id: string_of(args, "id")?,
        attribute: super::step_attribute(&attribute)
            .map_err(|error| anyhow::anyhow!("{}", error.message))?,
        value,
        json: false,
    })
}

pub fn step_rename(args: &Map<String, Value>) -> Result<Outcome> {
    plan_edit::step(StepCommand::Rename {
        target: target_of(args)?,
        id: string_of(args, "id")?,
        new_id: string_of(args, "new_id")?,
        json: false,
    })
}

pub fn step_rm(args: &Map<String, Value>) -> Result<Outcome> {
    plan_edit::step(StepCommand::Rm {
        target: target_of(args)?,
        id: string_of(args, "id")?,
        json: false,
    })
}

pub async fn tools_list() -> Result<Outcome> {
    let runtime = runtime()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(&store, std::sync::Arc::new(graph_core::NullSink))
        .await?;
    let defs = {
        use graph_core::ToolRegistry;
        toolbox.tools().await?
    };
    runtime.shutdown().await;
    Ok(Outcome::ok(listing::tool_listing_as_json(&defs)))
}

pub async fn tools_show(name: &str) -> Result<Outcome> {
    let runtime = runtime()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(&store, std::sync::Arc::new(graph_core::NullSink))
        .await?;
    let defs = {
        use graph_core::ToolRegistry;
        toolbox.tools().await?
    };
    runtime.shutdown().await;
    let Some(def) = defs.into_iter().find(|d| d.name == name) else {
        bail!("unknown tool: {name}");
    };
    Ok(Outcome::ok(listing::tool_as_json(&def)))
}

pub async fn tools_test(name: &str, input: Value) -> Result<Outcome> {
    let runtime = runtime()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(&store, std::sync::Arc::new(graph_core::NullSink))
        .await?;
    let invoked = {
        use graph_core::ToolRegistry;
        toolbox.invoke(name, input).await
    };
    runtime.shutdown().await;
    let invoked = invoked?;
    // A tool reporting failure is a result, not a call failure — the payload
    // is what the caller needs in order to fix the step.
    Ok(Outcome::ok(json!({
        "tool": name,
        "isError": invoked.is_error,
        "result": invoked.result,
    })))
}

/// Run a plan and return its result.
///
/// The CLI's `plan run` streams to a terminal and signals with an exit code;
/// neither exists over MCP. The distinctions those codes carried have to
/// survive as data instead:
///
/// - **needs input** (CLI exit 3) → a rejection whose body carries the input
///   schema, so the agent can retry with the missing argument rather than
///   treating the plan as broken.
/// - **exit-gate assertion** (CLI exit 4) → a rejection carrying the gate's
///   message and step. The plan worked; its condition fired.
pub async fn run_plan(
    identifier: &str,
    input: Value,
    events: std::sync::Arc<dyn graph_core::EventSink>,
    gate: Option<std::sync::Arc<dyn graph_core::pipeline::ExecutionGate>>,
    interlocutor: Option<std::sync::Arc<dyn graph_core::pipeline::Interlocutor>>,
) -> Result<Outcome> {
    let runtime = runtime()?;
    let loaded = runtime.plan_docs();
    let Some(doc) = loaded
        .docs
        .iter()
        .find(|d| d.identifier == identifier)
        .cloned()
    else {
        runtime.shutdown().await;
        bail!("no plan named '{identifier}'");
    };

    // Fail fast, before anything runs or connects.
    let tool_catalog = runtime.tool_catalog(&loaded.docs)?;
    let mut check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &tool_catalog);
    check.errors.append(&mut check.notes);
    if !check.errors.is_empty() {
        runtime.shutdown().await;
        bail!(
            "plan '{identifier}' has unresolvable tools:\n  - {}",
            check.errors.join("\n  - ")
        );
    }

    let mut input = input;
    if let Some(schema) = &doc.input_schema {
        graph_core::pipeline::doc::apply_schema_defaults(schema, &mut input);
    }
    if let Err(problems) = validate_input(&doc, &input) {
        runtime.shutdown().await;
        return Ok(Outcome::rejected(json!({
            "error": format!("plan '{identifier}' needs inputs"),
            "problems": problems,
            "inputSchema": doc.input_schema,
        })));
    }

    let store = runtime.store()?;
    // Never a terminal sink: anything written to stdout would corrupt the
    // protocol. `events` either forwards MCP progress notifications or
    // discards.
    let pipeline = runtime
        .pipeline_with(
            &store,
            events,
            crate::runtime::PipelineHooks { gate, interlocutor },
        )
        .await?;
    let query = format!("Run the '{}' plan", doc.name);
    let finish = doc.finish();
    let result = pipeline
        .run_explicit(&query, doc.steps.clone(), finish, Some(input))
        .await;
    runtime.shutdown().await;

    // A cancelled run is not a failure to report as one: the client asked
    // for it and has already stopped listening for the result.
    let outcome = match result {
        Err(graph_core::pipeline::PipelineError::Aborted { state, .. }) => {
            return Ok(Outcome::rejected(json!({
                "error": format!("plan '{identifier}' was cancelled"),
                "plan": identifier,
                "steps_executed": state.steps_executed(),
            })));
        }
        other => other?,
    };
    let body = json!({
        "answer": (!outcome.answer.is_empty()).then_some(&outcome.answer),
        "output": outcome.structured,
        "plan": doc.identifier,
        "steps_executed": outcome.state.steps_executed(),
        "exit": outcome.exit,
    });
    let asserted = matches!(&outcome.exit, Some(exit) if exit.status == ExitStatus::Error);
    Ok(if asserted {
        Outcome::rejected(body)
    } else {
        Outcome::ok(body)
    })
}
