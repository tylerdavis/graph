//! `graph plan` — list/show/validate/run plan documents.

use crate::cli::PlanCommand;
use crate::commands::input::resolve_input;
use crate::output::TtySink;
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
    let input = resolve_input(document, inputs)?;

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

    let store = runtime.store()?;
    let events: Arc<dyn graph_core::EventSink> = Arc::new(TtySink::new(true));
    let pipeline = runtime.pipeline(store, events).await?;
    let query = format!("Run the '{}' plan", doc.name);
    let result = pipeline
        .run_explicit(&query, doc.steps.clone(), doc.solver.clone(), Some(input))
        .await;
    runtime.shutdown().await;

    let outcome = result?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "answer": outcome.answer,
                "plan": doc.identifier,
                "steps_executed": outcome.state.results.len(),
            }))?
        );
    } else {
        println!("{}", outcome.answer);
    }
    Ok(())
}
