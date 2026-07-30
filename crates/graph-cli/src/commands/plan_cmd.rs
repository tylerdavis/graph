//! `graph plan` — list/show/validate/run plan documents.

use crate::cli::PlanCommand;
use crate::commands::input::resolve_input;
use crate::commands::outcome::{report, Outcome};
use crate::commands::plan_edit;
use crate::output::SilentExit;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use graph_core::pipeline::authoring;
use graph_core::pipeline::catalog;
use graph_core::pipeline::doc::{parse_plan_doc, validate_input, LoadedPlans, PlanDoc};
use std::sync::Arc;

/// Exit code for "the plan needs inputs you didn't provide".
const EXIT_NEEDS_INPUT: i32 = 3;
/// Exit code for "an exit step asserted failure" — distinct from 1
/// (infrastructure failure) so CI can branch on it.
const EXIT_PLAN_ASSERTED: i32 = 4;

pub async fn run(command: PlanCommand) -> Result<()> {
    match command {
        PlanCommand::List { json } => report(list()?, json),
        PlanCommand::Show { name, json } => report(show(&name)?, json),
        PlanCommand::Validate { name_or_path, json } => {
            let outcome = validate(&name_or_path)?;
            if json {
                return report(outcome, true);
            }
            render_validation(&outcome.body)
        }
        PlanCommand::Run {
            name,
            input,
            inputs,
            json,
        } => run_plan(&name, input.as_deref(), &inputs, json).await,
        PlanCommand::New {
            identifier,
            name,
            description,
            output,
            json,
        } => report(
            plan_edit::new_plan(&identifier, name.as_deref(), description.as_deref(), output)?,
            json,
        ),
        PlanCommand::Draft {
            goal,
            from,
            output,
            stdout,
            json,
        } => {
            let outcome = plan_edit::draft(&goal, from.as_deref(), output, stdout).await?;
            // `--stdout` asks for the document itself, so it outranks
            // `--json`: a caller who wanted raw YAML did not want it wrapped.
            report(outcome, json && !stdout)
        }
        PlanCommand::Set {
            target,
            attribute,
            value,
            json,
        } => report(plan_edit::set(&target, attribute, &value)?, json),
        PlanCommand::Unset {
            target,
            attribute,
            json,
        } => report(plan_edit::unset(&target, attribute)?, json),
        PlanCommand::Step { command } => {
            let json = command.json();
            report(plan_edit::step(command)?, json)
        }
    }
}

/// `graph plan list` — the catalog, plus what was skipped and why.
///
/// The text rendering is `identifier\tname` on stdout: unlike the other
/// listings this one *is* a deliverable (it is what you pipe into `xargs`),
/// so it does not move to stderr without `--json`.
fn list() -> Result<Outcome> {
    let runtime = Runtime::init()?;
    let loaded = runtime.plan_docs();
    let body = plan_edit::list_as_json(&loaded);
    if loaded.docs.is_empty() && loaded.skipped.is_empty() {
        eprintln!("no plan documents found — add YAML files under [plans].paths");
        return Ok(Outcome::raw(String::new(), body));
    }
    let text = loaded
        .docs
        .iter()
        .map(|doc| format!("{}\t{}\n", doc.identifier, doc.name))
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

/// `graph plan show` — the document itself, as YAML or as JSON.
fn show(name: &str) -> Result<Outcome> {
    let runtime = Runtime::init()?;
    // Lenient like `validate`: reading a plan the catalog rejects is
    // exactly how you find out why it was rejected.
    let (doc, _loaded) = resolve_target(&runtime, name)?;
    Ok(Outcome::raw(
        authoring::to_yaml(&doc)?,
        plan_edit::doc_as_json(&doc)?,
    ))
}

/// Resolve a plan identifier or YAML file path to a document, for authoring
/// and inspection.
///
/// Deliberately lenient: it uses `parse_plan_doc`, not `load_plan_doc`, so an
/// invalid plan can still be opened. A `graph plan new` scaffold has no steps
/// and therefore never enters the catalog — if this refused to open it, the
/// build-it-up-with-`step add` flow would be impossible, and `plan validate`
/// would report "failed to load" instead of the problem list it exists to
/// print.
///
/// A path wins when it exists on disk. Otherwise the catalog is searched,
/// then every plan file on disk via `Runtime::find_plan_file` — which is what
/// lets a plan the catalog rejects (hidden by `requires_servers`, or invalid)
/// still be opened and repaired. Only then does it give up, explaining why.
pub fn resolve_target(runtime: &Runtime, name_or_path: &str) -> Result<(PlanDoc, LoadedPlans)> {
    let loaded = runtime.plan_docs();
    let path = std::path::Path::new(name_or_path);
    if path.exists() {
        return Ok((parse_plan_doc(path)?, loaded));
    }
    if let Some(doc) = loaded.docs.iter().find(|d| d.identifier == name_or_path) {
        return Ok((doc.clone(), loaded));
    }
    if let Some(found) = runtime.find_plan_file(name_or_path) {
        return Ok((parse_plan_doc(&found)?, loaded));
    }
    bail!(missing_plan(&loaded, name_or_path))
}

/// `graph plan validate` — the full verdict across all three layers.
///
/// `problems` are fatal: the plan cannot run here. `notes` are not — a
/// `requires_servers` entry that this machine doesn't configure means the file
/// is portable but not runnable locally, which is worth saying and not worth
/// failing over.
fn validate(name_or_path: &str) -> Result<Outcome> {
    let runtime = Runtime::init()?;
    let (doc, loaded) = resolve_target(&runtime, name_or_path)?;
    // Structural validation reruns here rather than being assumed from load
    // time: a plan named by path may never have been through the catalog, and
    // a caller asking "is this valid?" deserves every layer's answer.
    let mut problems = authoring::static_problems(&doc);
    let catalog = runtime.tool_catalog(&loaded.docs)?;
    let check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &catalog);
    for problem in check.errors {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    let ok = problems.is_empty();
    let body = serde_json::json!({
        "plan": doc.identifier,
        "steps": doc.steps.len(),
        "ok": ok,
        "problems": problems,
        "notes": check.notes,
    });
    Ok(if ok {
        Outcome::ok(body)
    } else {
        Outcome::rejected(body)
    })
}

/// The human rendering of a validation verdict.
///
/// Deliberately not the generic [`report`] rendering: a verdict is not an
/// edit, and an invalid plan surfaces as a plain error whose message *is* the
/// problem list — the form that reads best at the end of a shell pipeline.
/// Notes print either way, since "portable but not runnable here" is worth
/// saying and not worth failing over.
fn render_validation(body: &serde_json::Value) -> Result<()> {
    for note in body["notes"].as_array().into_iter().flatten() {
        eprintln!("note: {}", note.as_str().unwrap_or_default());
    }
    let plan = body["plan"].as_str().unwrap_or_default();
    let problems: Vec<&str> = body["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .collect();
    if !problems.is_empty() {
        bail!(
            "plan '{}' is not valid:\n  - {}",
            plan,
            problems.join("\n  - ")
        );
    }
    eprintln!(
        "ok: '{}' — {} steps",
        plan,
        body["steps"].as_u64().unwrap_or_default()
    );
    Ok(())
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
        return Err(SilentExit::code(EXIT_NEEDS_INPUT));
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
        return Err(SilentExit::code(EXIT_PLAN_ASSERTED));
    }
    Ok(())
}
