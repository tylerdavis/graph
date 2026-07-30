//! `graph plan new/draft/set/unset/step …` — the authoring half of
//! `graph plan`, and the surface a plan-managing agent drives.
//!
//! Every command here is stateless: resolve a plan identifier-or-path, apply
//! one edit, write the YAML back. There is no draft session and no `--undo`
//! (that is what version control is for), which makes each command
//! idempotent and safe to call concurrently.
//!
//! The domain rules — what an edit may do, what counts as a regression, where
//! the file lands — all live in `graph_core::pipeline::authoring`, shared with
//! the plan workbench so both surfaces behave identically.
//!
//! Two kinds of failure, kept deliberately separate:
//! - **Argument errors** (wrong value count, malformed JSON, unknown
//!   attribute) surface as ordinary `anyhow` errors — the caller mis-typed.
//! - **Domain rejections** (an edit that would break the plan, an unknown
//!   step id) come back as a structured body from the edit guard, carried in
//!   an [`Outcome`] so `--json` callers get the problem list.
//!
//! Nothing here prints. Every command returns an [`Outcome`]; rendering it is
//! the caller's job (see [`crate::commands::outcome`]).

use crate::cli::{PlanAttribute, StepAttribute, StepCommand};
use crate::commands::outcome::Outcome;
use crate::commands::plan_cmd::resolve_target;
use crate::runtime::Runtime;
use anyhow::{bail, Context, Result};
use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::{PipelineError, PlannerOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ── The shared edit pipeline ─────────────────────────────────────────────

/// Resolve → apply the guard → write → report. Every mutating command in
/// this module is a `mutate` closure handed to this function.
fn edit(
    target: &str,
    mutate: impl FnOnce(&mut PlanDoc) -> Result<Value, Value>,
) -> Result<Outcome> {
    let runtime = Runtime::init()?;
    let (doc, _loaded) = resolve_target(&runtime, target)?;
    let source = doc.path.clone();
    match authoring::apply_edit(&doc, mutate) {
        Ok(accepted) => {
            let mut summary = authoring::summarize_edit(&accepted);
            let path = write_edited(&accepted.doc, &runtime, source.as_deref(), &mut summary)?;
            summary["savedTo"] = json!(path.display().to_string());
            Ok(Outcome::ok(summary))
        }
        Err(rejected) => Ok(Outcome::rejected(rejected.body)),
    }
}

/// Write an edited document, and explain the one case where the destination
/// is not the file it came from.
///
/// `patch_metadata` clears `path` on an identifier change, because a renamed
/// plan is a different plan. Rather than silently deleting the original, the
/// new identifier lands in a new file and the note says the old one is still
/// there — removing it is the caller's call, not ours.
fn write_edited(
    doc: &PlanDoc,
    runtime: &Runtime,
    source: Option<&Path>,
    summary: &mut Value,
) -> Result<PathBuf> {
    let renamed = doc.path.is_none() && source.is_some();
    let plans_dir = runtime.plans_dir();
    let path = authoring::target_path(doc, plans_dir.as_deref())?;
    authoring::write_doc(doc, &path, false)?;
    if renamed {
        let original = source.unwrap().display();
        summary["renamedFrom"] = json!(original.to_string());
        summary["note"] = json!(format!(
            "'{}' was written to a new file — the original is still at {original}; \
             remove it if this was a rename",
            doc.identifier
        ));
    }
    Ok(path)
}

// ── Commands ─────────────────────────────────────────────────────────────

/// `graph plan new` — scaffold a plan file with no steps.
///
/// Deliberately invalid on creation ("plan has no steps"): the edit guard
/// only blocks *new* problems, so a scaffold stays editable and an agent can
/// build the plan up with `step add` without ever calling the planner.
pub fn new_plan(
    identifier: &str,
    name: Option<&str>,
    description: Option<&str>,
    output: Option<PathBuf>,
) -> Result<Outcome> {
    let runtime = Runtime::init()?;
    // The identifier is the file name and the `plan__<id>` tool name, so a
    // bad one is an argument error, not something to write out and report.
    if identifier.is_empty()
        || !identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("identifier '{identifier}' must be non-empty and use only [a-zA-Z0-9_-]");
    }
    let doc = PlanDoc {
        identifier: identifier.to_string(),
        name: name.unwrap_or(identifier).to_string(),
        description: description.unwrap_or_default().to_string(),
        exemplars: Vec::new(),
        requires_servers: Vec::new(),
        input_schema: None,
        steps: Vec::new(),
        solver: None,
        output: None,
        path: output,
    };
    let plans_dir = runtime.plans_dir();
    let path = authoring::target_path(&doc, plans_dir.as_deref())?;
    authoring::write_doc(&doc, &path, false)?;

    let mut body = json!({
        "ok": true,
        "identifier": doc.identifier,
        "savedTo": path.display().to_string(),
        "problems": authoring::static_problems(&doc),
    });
    if doc.description.is_empty() {
        body["note"] = json!(
            "no description set — it is the plan's routing signal, so set it with \
             `graph plan set <id> description '<what it does>'`"
        );
    }
    Ok(Outcome::ok(body))
}

/// `graph plan draft <goal>` — author a plan with the planner model.
///
/// The only command here that costs inference. Mirrors the workbench's
/// `workbench__draft_plan` exactly: drafting validates each step as it is
/// generated, and on exhaustion the valid prefix is salvaged rather than
/// thrown away — a partial plan finished with `step add` beats redrafting
/// from scratch.
///
/// `from` supplies the identity to draft into (identifier, name,
/// description, input schema). It is not a revision mechanism: drafting
/// replaces every step, so pointing it at a plan whose steps have been
/// hand-tuned discards that work. Corrections go through `set` and
/// `step update`, which apply one intent and are validated atomically.
pub async fn draft(
    goal: &str,
    from: Option<&str>,
    output: Option<PathBuf>,
    stdout: bool,
) -> Result<Outcome> {
    let runtime = Runtime::init()?;
    let existing = match from {
        Some(target) => Some(resolve_target(&runtime, target)?.0),
        None => None,
    };
    let existing_output = existing.as_ref().map(|doc| PlannerOutput {
        plan: doc.steps.clone(),
        solver_data: doc.solver.clone().unwrap_or_default(),
    });

    let store = runtime.store()?;
    // Drafting's deliverable is the plan, not prose: keep progress on stderr
    // so `--stdout` can hand clean YAML to a pipe.
    let events = crate::output::make_sink(true, false);
    let pipeline = runtime.pipeline(&store, events).await?;

    let drafted = pipeline.draft_plan(goal, existing_output.as_ref()).await;
    let mut salvaged = None;
    let planner_output = match drafted {
        Ok(output) => output,
        Err(PipelineError::DraftStepExhausted {
            step_id,
            problems,
            partial,
            ..
        }) => {
            salvaged = Some((step_id, problems));
            *partial
        }
        Err(error) => {
            runtime.shutdown().await;
            bail!("planner failed: {error}");
        }
    };

    runtime.shutdown().await;

    let mut doc = authoring::merge_planner_output(existing, goal, planner_output);
    if output.is_some() {
        doc.path = output;
    }

    if stdout {
        let yaml = authoring::to_yaml(&doc)?;
        return Ok(Outcome::raw(yaml, doc_as_json(&doc)?));
    }

    let loaded = runtime.plan_docs();
    let catalog = runtime.tool_catalog(&loaded.docs)?;
    let problems = authoring::plan_problems(&doc, &loaded.docs, Some(&catalog));
    let plans_dir = runtime.plans_dir();
    let path = authoring::target_path(&doc, plans_dir.as_deref())?;
    authoring::write_doc(&doc, &path, false)?;

    let mut body = json!({
        "ok": true,
        "identifier": doc.identifier,
        "steps": doc.steps.len(),
        "savedTo": path.display().to_string(),
        "problems": problems,
    });
    if let Some((step_id, step_problems)) = salvaged {
        body["salvaged"] = json!(true);
        body["failedStep"] = json!(step_id);
        body["stepProblems"] = json!(step_problems);
        body["note"] = json!(format!(
            "drafting could not produce a valid step {step_id}; the valid \
             partial draft ({} steps) was saved — finish it with \
             `graph plan step add` rather than redrafting",
            doc.steps.len()
        ));
    } else if !problems.is_empty() {
        body["note"] = json!(
            "the draft is not valid yet — fix the problems with \
             `graph plan set` / `graph plan step update`"
        );
    }
    Ok(Outcome::ok(body))
}

/// `graph plan set <target> <attribute> <value>...`
pub fn set(target: &str, attribute: PlanAttribute, values: &[String]) -> Result<Outcome> {
    let patch = metadata_patch(attribute, values)?;
    edit(target, |doc| authoring::patch_metadata(doc, &patch))
}

/// `graph plan unset <target> <attribute>`
pub fn unset(target: &str, attribute: PlanAttribute) -> Result<Outcome> {
    match attribute {
        PlanAttribute::Name | PlanAttribute::Description | PlanAttribute::Identifier => {
            bail!(
                "{} is required and cannot be cleared — change it with \
                 `graph plan set <target> {} <value>`",
                attribute.as_str(),
                attribute.as_str()
            )
        }
        PlanAttribute::Exemplars => edit(target, |doc| {
            authoring::patch_metadata(doc, &json!({"exemplars": []}))
        }),
        PlanAttribute::RequiresServers => edit(target, |doc| {
            authoring::patch_metadata(doc, &json!({"requires_servers": []}))
        }),
        PlanAttribute::InputSchema => edit(target, |doc| {
            authoring::patch_metadata(doc, &json!({"input_schema": null}))
        }),
        // `finish: {}` clears solver *and* output, so refuse unless the named
        // mode is the active one — otherwise `unset solver` on an
        // output-rendering plan would quietly delete its output map.
        PlanAttribute::Solver => edit(target, |doc| {
            if doc.solver.is_none() {
                return Err(json!({
                    "error": format!(
                        "plan '{}' has no solver to clear (it finishes with {})",
                        doc.identifier,
                        if doc.output.is_some() { "`output`" } else { "neither — it is already silent" }
                    ),
                }));
            }
            authoring::patch_metadata(doc, &json!({"finish": {}}))
        }),
        PlanAttribute::Output => edit(target, |doc| {
            if doc.output.is_none() {
                return Err(json!({
                    "error": format!(
                        "plan '{}' has no output map to clear (it finishes with {})",
                        doc.identifier,
                        if doc.solver.is_some() { "`solver`" } else { "neither — it is already silent" }
                    ),
                }));
            }
            authoring::patch_metadata(doc, &json!({"finish": {}}))
        }),
    }
}

/// `graph plan step …`
pub fn step(command: StepCommand) -> Result<Outcome> {
    match command {
        StepCommand::Add {
            target,
            id,
            tool,
            input,
            reasoning,
            before,
            after,
            json: _,
        } => {
            let input = json_document(&input, "input")?;
            let mut step = json!({"id": id, "toolName": tool, "input": input});
            if let Some(reasoning) = &reasoning {
                step["reasoning"] = json!(reasoning);
            }
            let mut patch = json!({"step": step});
            if let Some(anchor) = &before {
                patch["before"] = json!(anchor);
            }
            if let Some(anchor) = &after {
                patch["after"] = json!(anchor);
            }
            edit(&target, |doc| authoring::patch_add_step(doc, &patch))
        }
        StepCommand::Update {
            target,
            id,
            attribute,
            value,
            json: _,
        } => {
            let mut patch = json!({"id": id});
            match attribute {
                StepAttribute::Tool => patch["toolName"] = json!(value),
                StepAttribute::Input => patch["input"] = json_document(&value, "input")?,
                StepAttribute::Reasoning => {
                    if value.is_empty() {
                        bail!(
                            "reasoning cannot be set to an empty string — \
                             clear it with `graph plan step unset {target} {id} reasoning`"
                        );
                    }
                    patch["reasoning"] = json!(value);
                }
            }
            edit(&target, |doc| authoring::patch_update_step(doc, &patch))
        }
        StepCommand::Rename {
            target,
            id,
            new_id,
            json: _,
        } => {
            let patch = json!({"id": id, "newId": new_id});
            edit(&target, |doc| authoring::patch_update_step(doc, &patch))
        }
        StepCommand::Unset {
            target,
            id,
            attribute,
            json: _,
        } => {
            match attribute {
                // `patch_update_step` treats an empty reasoning as "clear".
                StepAttribute::Reasoning => {
                    let patch = json!({"id": id, "reasoning": ""});
                    edit(&target, |doc| authoring::patch_update_step(doc, &patch))
                }
                StepAttribute::Tool | StepAttribute::Input => bail!(
                    "a step's {} is required and cannot be cleared — change it with \
                     `graph plan step update <target> <id> {} <value>`",
                    attribute.as_str(),
                    attribute.as_str()
                ),
            }
        }
        StepCommand::Rm {
            target,
            id,
            json: _,
        } => {
            let patch = json!({"id": id});
            edit(&target, |doc| authoring::patch_delete_step(doc, &patch))
        }
    }
}

// ── Argument → patch mapping ─────────────────────────────────────────────

/// Turn one `<attribute> <value>...` pair into the JSON patch object that
/// `authoring::patch_metadata` consumes.
///
/// Arity lives here rather than in clap because it varies per attribute:
/// scalars take exactly one value, `exemplars` and `requires_servers` take a
/// list, and the three structured fields take one JSON document.
fn metadata_patch(attribute: PlanAttribute, values: &[String]) -> Result<Value> {
    Ok(match attribute {
        PlanAttribute::Name => json!({"name": one(attribute, values)?}),
        PlanAttribute::Description => json!({"description": one(attribute, values)?}),
        PlanAttribute::Identifier => json!({"identifier": one(attribute, values)?}),
        PlanAttribute::Exemplars => json!({"exemplars": values}),
        PlanAttribute::RequiresServers => json!({"requires_servers": values}),
        PlanAttribute::InputSchema => {
            json!({"input_schema": json_document(one(attribute, values)?, "input_schema")?})
        }
        // Both finish modes go through the one `finish` discriminator, which
        // is what makes setting either clear the other.
        PlanAttribute::Solver => {
            json!({"finish": {"solver": json_document(one(attribute, values)?, "solver")?}})
        }
        PlanAttribute::Output => {
            json!({"finish": {"output": json_document(one(attribute, values)?, "output")?}})
        }
    })
}

/// The single value a scalar attribute takes, or an error naming the
/// attribute — `graph plan set demo name a b` should say which one is wrong.
fn one(attribute: PlanAttribute, values: &[String]) -> Result<&String> {
    match values {
        [only] => Ok(only),
        [] => bail!("{} needs a value", attribute.as_str()),
        many => bail!(
            "{} takes one value, got {} — quote it if it contains spaces",
            attribute.as_str(),
            many.len()
        ),
    }
}

/// Resolve a `JSON|@FILE|-` argument to a JSON object.
///
/// Shares the convention (and the stdin/`@file` handling) with
/// `commands::input::resolve_input`, which every other input-taking command
/// uses; the difference is that a plan field is a bare document with no
/// `--input key=value` overrides layered on top.
fn json_document(raw: &str, field: &str) -> Result<Value> {
    let value = crate::commands::input::resolve_input(Some(raw), &[])
        .with_context(|| format!("reading {field}"))?;
    // resolve_input already guarantees an object; keep the type explicit so a
    // future change there can't silently let a scalar through.
    if !value.is_object() {
        bail!("{field} must be a JSON object");
    }
    Ok(value)
}

/// A plan document as a JSON object — the `--json` shape of `plan show`.
///
/// Goes through `authoring::to_json` rather than serializing the struct, so the
/// envelope spells fields the way the file does (`tool_name`, not `toolName`).
pub fn doc_as_json(doc: &PlanDoc) -> Result<Value> {
    let mut value = authoring::to_json(doc)?;
    // `path` is #[serde(skip)] because it isn't part of the file format, but a
    // machine caller needs to know which file it would be editing.
    if let (Some(map), Some(path)) = (value.as_object_mut(), &doc.path) {
        map.insert("path".to_string(), json!(path.display().to_string()));
    }
    Ok(value)
}

/// The `--json` shape of `plan list`: the catalog, plus the plans that are
/// present on disk but unusable here. Both matter to a caller deciding what
/// it can edit — `skipped` files are broken, `hidden` ones just need MCP
/// servers this machine lacks.
pub fn list_as_json(loaded: &graph_core::pipeline::doc::LoadedPlans) -> Value {
    json!({
        "plans": loaded.docs.iter().map(|doc| json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "description": doc.description,
            "steps": doc.steps.len(),
            "path": doc.path.as_ref().map(|p| p.display().to_string()),
        })).collect::<Vec<_>>(),
        "skipped": loaded.skipped.iter().map(|error| json!({
            "path": error.path(),
            "reason": error.to_string(),
        })).collect::<Vec<_>>(),
        "hidden": loaded.hidden.iter().map(|hidden| json!({
            "identifier": hidden.identifier,
            "missingServers": hidden.missing_servers,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_attributes_take_exactly_one_value() {
        let two = vec!["a".to_string(), "b".to_string()];
        let error = metadata_patch(PlanAttribute::Name, &two).unwrap_err();
        assert!(
            error.to_string().contains("name takes one value, got 2"),
            "{error}"
        );
        assert!(metadata_patch(PlanAttribute::Name, &[])
            .unwrap_err()
            .to_string()
            .contains("name needs a value"));
        let one = vec!["Urgent issues".to_string()];
        assert_eq!(
            metadata_patch(PlanAttribute::Name, &one).unwrap(),
            json!({"name": "Urgent issues"})
        );
    }

    #[test]
    fn list_attributes_take_several_values() {
        let values = vec!["what is urgent".to_string(), "show blockers".to_string()];
        assert_eq!(
            metadata_patch(PlanAttribute::Exemplars, &values).unwrap(),
            json!({"exemplars": ["what is urgent", "show blockers"]})
        );
        assert_eq!(
            metadata_patch(PlanAttribute::RequiresServers, &values).unwrap(),
            json!({"requires_servers": ["what is urgent", "show blockers"]})
        );
    }

    #[test]
    fn both_finish_modes_route_through_the_one_discriminator() {
        // This is why setting either clears the other: they are one field to
        // `patch_metadata`, not two independent ones.
        let solver = vec![r#"{"query_to_answer":"q?","data":{}}"#.to_string()];
        assert_eq!(
            metadata_patch(PlanAttribute::Solver, &solver).unwrap(),
            json!({"finish": {"solver": {"query_to_answer": "q?", "data": {}}}})
        );
        let output = vec![r#"{"body":"{{E1.text}}"}"#.to_string()];
        assert_eq!(
            metadata_patch(PlanAttribute::Output, &output).unwrap(),
            json!({"finish": {"output": {"body": "{{E1.text}}"}}})
        );
    }

    #[test]
    fn structured_attributes_reject_malformed_json() {
        let bad = vec!["not json".to_string()];
        let error = metadata_patch(PlanAttribute::InputSchema, &bad).unwrap_err();
        assert!(
            format!("{error:#}").contains("input_schema"),
            "the error should name the field: {error:#}"
        );
    }

    #[test]
    fn attribute_names_match_the_plan_yaml() {
        // An agent reads `requires_servers` in a plan file; it must be able
        // to write the same word on the command line.
        assert_eq!(PlanAttribute::RequiresServers.as_str(), "requires_servers");
        assert_eq!(PlanAttribute::InputSchema.as_str(), "input_schema");
        assert_eq!(StepAttribute::Tool.as_str(), "tool");
    }

    #[test]
    fn list_json_reports_broken_and_hidden_plans_separately() {
        use graph_core::pipeline::doc::{DocError, HiddenPlan, LoadedPlans};
        let loaded = LoadedPlans {
            docs: Vec::new(),
            skipped: vec![DocError::Invalid {
                path: "/plans/broken.yaml".to_string(),
                message: "plan has no steps".to_string(),
            }],
            hidden: vec![HiddenPlan {
                identifier: "needs_linear".to_string(),
                missing_servers: vec!["linear".to_string()],
            }],
        };
        let value = list_as_json(&loaded);
        assert_eq!(value["skipped"][0]["path"], json!("/plans/broken.yaml"));
        assert_eq!(value["hidden"][0]["identifier"], json!("needs_linear"));
        assert_eq!(value["hidden"][0]["missingServers"], json!(["linear"]));
    }

    #[test]
    fn doc_json_carries_the_source_path() {
        let mut doc: PlanDoc = serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap();
        doc.path = Some(PathBuf::from("/plans/demo.yaml"));
        let value = doc_as_json(&doc).unwrap();
        assert_eq!(value["identifier"], json!("demo"));
        assert_eq!(
            value["path"],
            json!("/plans/demo.yaml"),
            "a machine caller needs to know which file it would edit"
        );
    }
}
