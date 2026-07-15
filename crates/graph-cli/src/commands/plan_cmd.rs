//! `graph plan` — list/show/validate/run plan documents.

use crate::cli::PlanCommand;
use crate::commands::input::resolve_input;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::pipeline::catalog;
use graph_core::pipeline::doc::{load_plan_doc, validate_input, LoadedPlans};
use std::sync::Arc;

/// Exit code for "the plan needs inputs you didn't provide".
const EXIT_NEEDS_INPUT: i32 = 3;
/// Exit code for "an exit step asserted failure" — distinct from 1
/// (infrastructure failure) so CI can branch on it.
const EXIT_PLAN_ASSERTED: i32 = 4;

pub async fn run(command: PlanCommand) -> Result<()> {
    match command {
        PlanCommand::List => {
            let runtime = Runtime::init()?;
            let loaded = runtime.plan_docs();
            if loaded.docs.is_empty() && loaded.skipped.is_empty() {
                println!("no plan documents found — add YAML files under [plans].paths");
                return Ok(());
            }
            for doc in &loaded.docs {
                println!("{}\t{}", doc.identifier, doc.name);
            }
            Ok(())
        }
        PlanCommand::Show { name } => {
            let runtime = Runtime::init()?;
            let loaded = runtime.plan_docs();
            let Some(doc) = loaded.docs.iter().find(|d| d.identifier == name) else {
                bail!(missing_plan(&loaded, &name));
            };
            print!("{}", serde_yaml::to_string(doc)?);
            Ok(())
        }
        PlanCommand::Validate { name_or_path } => {
            let runtime = Runtime::init()?;
            let loaded = runtime.plan_docs();
            let path = std::path::Path::new(&name_or_path);
            let doc = if path.exists() {
                load_plan_doc(path)?
            } else {
                match loaded.docs.iter().find(|d| d.identifier == name_or_path) {
                    Some(doc) => doc.clone(),
                    None => bail!(missing_plan(&loaded, &name_or_path)),
                }
            };
            // Second layer: resolve every step tool against what this
            // config can actually load (structural validation already ran
            // in load_plan_doc / plan_docs).
            let catalog = runtime.tool_catalog(&loaded.docs)?;
            let check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &catalog);
            for note in &check.notes {
                eprintln!("note: {note}");
            }
            if !check.is_ok() {
                bail!(
                    "plan '{}' has unresolvable tools:\n  - {}",
                    doc.identifier,
                    check.errors.join("\n  - ")
                );
            }
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

/// Why a named plan isn't in the catalog: it requires MCP servers this
/// config doesn't have, its file failed to load (say why), or it simply
/// doesn't exist.
fn missing_plan(loaded: &LoadedPlans, name: &str) -> String {
    if let Some(reason) = loaded.hidden_reason(name) {
        return reason;
    }
    match loaded.skip_reason(name) {
        Some(reason) => format!("plan '{name}' failed to load — {reason}"),
        None => format!("no plan named '{name}' (see `graph plan list`)"),
    }
}

async fn run_plan(name: &str, document: Option<&str>, inputs: &[String], json: bool) -> Result<()> {
    // `--json` promises machine-parseable stdout, so it suppresses CI
    // annotations (which are stdout workflow commands) even when a mode
    // like GRAPH_EVENTS=github is active.
    let annotate = |message: &str| {
        if !json {
            crate::output::annotate_failure(message);
        }
    };
    let runtime = Runtime::init()?;
    let loaded = runtime.plan_docs();
    let Some(doc) = loaded.docs.iter().find(|d| d.identifier == name).cloned() else {
        let message = missing_plan(&loaded, name);
        annotate(&message);
        bail!(message);
    };
    // Fail fast: resolve every step tool (sub-plans included) against the
    // loadable catalog before anything runs or connects. At run time a
    // declared-but-unconfigured server is as fatal as an undeclared one.
    let catalog = runtime.tool_catalog(&loaded.docs)?;
    let mut check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &catalog);
    check.errors.append(&mut check.notes);
    if !check.errors.is_empty() {
        let message = format!(
            "plan '{name}' has unresolvable tools:\n  - {}",
            check.errors.join("\n  - ")
        );
        annotate(&message);
        bail!(message);
    }
    let mut input = resolve_input(document, inputs)?;
    if let Some(schema) = &doc.input_schema {
        graph_core::pipeline::doc::apply_schema_defaults(schema, &mut input);
    }

    if let Err(problems) = validate_input(&doc, &input) {
        eprintln!("plan '{name}' needs inputs:");
        for problem in &problems {
            eprintln!("  - {problem}");
        }
        if let Some(schema) = &doc.input_schema {
            eprintln!("input schema:\n{}", serde_json::to_string_pretty(schema)?);
        }
        annotate(&format!(
            "plan '{name}' needs inputs: {}",
            problems.join("; ")
        ));
        runtime.shutdown().await;
        std::process::exit(EXIT_NEEDS_INPUT);
    }

    let store = runtime.store()?;
    // Non-JSON runs stream the solver's answer to stdout as it generates;
    // --json buffers and emits the envelope instead.
    let events: Arc<dyn graph_core::EventSink> = crate::output::make_sink(json, !json);
    let pipeline = runtime.pipeline(&store, events).await?;
    let query = format!("Run the '{}' plan", doc.name);
    let finish = doc.finish();
    let result = pipeline
        .run_explicit(&query, doc.steps.clone(), finish, Some(input))
        .await;
    runtime.shutdown().await;

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            annotate(&format!("plan '{name}' failed: {err:#}"));
            return Err(err.into());
        }
    };
    let exited_error = matches!(
        &outcome.exit,
        Some(e) if e.status == graph_core::pipeline::ExitStatus::Error
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "answer": (!outcome.answer.is_empty()).then_some(&outcome.answer),
                "output": outcome.structured,
                "plan": doc.identifier,
                "steps_executed": outcome.state.steps_executed(),
                "exit": outcome.exit,
            }))?
        );
    } else if let Some(exit) = &outcome.exit {
        // Exit-step endings: message to the human, output (if any) to stdout.
        if let Some(structured) = &outcome.structured {
            println!("{}", serde_json::to_string_pretty(structured)?);
        }
        if exited_error {
            eprintln!("✗ {}", exit.message);
        } else {
            eprintln!("✓ {}", exit.message);
        }
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
    if exited_error {
        if let Some(exit) = &outcome.exit {
            annotate(&exit.message);
        }
        std::process::exit(EXIT_PLAN_ASSERTED);
    }
    Ok(())
}
