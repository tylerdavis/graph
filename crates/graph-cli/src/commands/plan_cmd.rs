//! `graph plan` — list/show/validate/run plan documents.

use crate::cli::PlanCommand;
use crate::commands::input::resolve_input;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::pipeline::doc::{load_plan_doc, validate_input};
use std::sync::Arc;

/// Exit code for "the plan needs inputs you didn't provide".
const EXIT_NEEDS_INPUT: i32 = 3;

pub async fn run(command: PlanCommand) -> Result<()> {
    match command {
        PlanCommand::List => {
            let runtime = Runtime::init()?;
            let docs = runtime.plan_docs()?;
            if docs.is_empty() {
                println!("no plan documents found — add YAML files under [plans].paths");
                return Ok(());
            }
            for doc in docs {
                println!("{}\t{}", doc.identifier, doc.name);
            }
            Ok(())
        }
        PlanCommand::Show { name } => {
            let runtime = Runtime::init()?;
            let docs = runtime.plan_docs()?;
            let Some(doc) = docs.iter().find(|d| d.identifier == name) else {
                bail!("no plan named '{name}' (see `graph plan list`)");
            };
            print!("{}", serde_yaml::to_string(doc)?);
            Ok(())
        }
        PlanCommand::Validate { name_or_path } => {
            let path = std::path::Path::new(&name_or_path);
            if path.exists() {
                let doc = load_plan_doc(path)?;
                println!("ok: '{}' — {} steps", doc.identifier, doc.steps.len());
                return Ok(());
            }
            let runtime = Runtime::init()?;
            let docs = runtime.plan_docs()?;
            let Some(doc) = docs.iter().find(|d| d.identifier == name_or_path) else {
                bail!("'{name_or_path}' is neither a file nor a known plan identifier");
            };
            println!("ok: '{}' — {} steps", doc.identifier, doc.steps.len());
            Ok(())
        }
        PlanCommand::Run {
            name,
            input,
            inputs,
            json,
        } => run_plan(&name, input.as_deref(), &inputs, json).await,
    }
}

async fn run_plan(name: &str, document: Option<&str>, inputs: &[String], json: bool) -> Result<()> {
    let runtime = Runtime::init()?;
    let docs = runtime.plan_docs()?;
    let Some(doc) = docs.into_iter().find(|d| d.identifier == name) else {
        bail!("no plan named '{name}' (see `graph plan list`)");
    };
    let mut input = resolve_input(document, inputs)?;
    if let Some(schema) = &doc.input_schema {
        graph_core::pipeline::doc::apply_schema_defaults(schema, &mut input);
    }

    if let Err(problems) = validate_input(&doc, &input) {
        eprintln!("plan '{name}' needs inputs:");
        for problem in problems {
            eprintln!("  - {problem}");
        }
        if let Some(schema) = &doc.input_schema {
            eprintln!("input schema:\n{}", serde_json::to_string_pretty(schema)?);
        }
        runtime.shutdown().await;
        std::process::exit(EXIT_NEEDS_INPUT);
    }

    let handles = runtime.store_handles()?;
    // Non-JSON runs stream the solver's answer to stdout as it generates;
    // --json buffers and emits the envelope instead.
    let events: Arc<dyn graph_core::EventSink> = crate::output::make_sink(json, !json);
    let pipeline = runtime.pipeline(&handles, events).await?;
    let query = format!("Run the '{}' plan", doc.name);
    let finish = doc.finish();
    let result = pipeline
        .run_explicit(&query, doc.steps.clone(), finish, Some(input))
        .await;
    runtime.shutdown().await;

    let outcome = result?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "answer": (!outcome.answer.is_empty()).then_some(&outcome.answer),
                "output": outcome.structured,
                "plan": doc.identifier,
                "steps_executed": outcome.state.steps_executed(),
            }))?
        );
    } else if let Some(structured) = &outcome.structured {
        println!("{}", serde_json::to_string_pretty(structured)?);
    } else if outcome.answer.is_empty() {
        eprintln!(
            "✓ plan '{}' completed ({} steps)",
            doc.identifier,
            outcome.state.steps_executed()
        );
    } else {
        // Solver output already streamed; just terminate the line.
        println!();
    }
    Ok(())
}
