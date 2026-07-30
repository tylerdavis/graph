//! Plan authoring: the shared domain rules for creating and editing plan
//! documents, independent of any surface that drives them.
//!
//! Two surfaces consume this module — the plan workbench's `workbench__*`
//! agent tools and the `graph plan` authoring commands — and they must agree
//! down to the error strings, because a plan edited by one is opened by the
//! other.
//!
//! The pieces, in the order an edit flows through them:
//!
//! 1. [`static_problems`] — validation that needs nothing but the document.
//! 2. [`apply_edit`] — the safety choke point. An edit is rejected only when
//!    it introduces a *new* problem; pre-existing problems never block one,
//!    or fixing a broken plan would be chicken-and-egg.
//! 3. The `patch_*` mutators — the actual field writes, each returning a
//!    summary for the caller to report.
//! 4. [`target_path`] + [`write_doc`] — where the document lands and the
//!    guards against clobbering someone else's plan.
//!
//! [`plan_problems`] is the fuller verdict used for *reporting* (it adds
//! catalog-aware tool resolution). Deliberately not the basis of the edit
//! guard: whether an MCP server happens to be configured here says nothing
//! about whether an edit is sound, and a portable plan file must stay
//! editable on a machine that cannot run it.
//!
//! Nothing here needs a [`Pipeline`](super::Pipeline), an LLM, or a network:
//! authoring a plan works without provider credentials. Only drafting a plan
//! from a goal ([`super::Pipeline::draft_plan`]) costs inference.

use super::catalog::{self, ToolCatalog};
use super::doc::{validate_doc, PlanDoc};
use super::plan::{self, Plan, PlannerOutput, SolverData, Step};
use super::{AGENT_TOOL, DECIDE_TOOL, MAP_TOOL, REDUCE_TOOL};
use crate::template;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

// ── Validation ───────────────────────────────────────────────────────────

/// Static validation of a plan's steps: template parse, reference ordering,
/// control-step gates and bodies. No LLM, no registry — tool existence is
/// checked at execution. Returns every problem found, not just the first.
///
/// The body of [`super::Pipeline::validate_plan`], lifted out so authoring
/// does not need a pipeline to validate an edit.
pub fn validate_steps(plan: &Plan) -> Vec<String> {
    let mut problems = Vec::new();
    if plan.is_empty() {
        problems.push("plan has no steps".to_string());
    }
    // Tool existence is checked at execution against the live registry.
    let all_ids: Vec<&str> = plan.iter().map(|s| s.id.as_str()).collect();
    let mut seen: Vec<&str> = vec!["input"];
    for step in plan {
        if let Err(problem) = plan::check_step_id(&step.id) {
            problems.push(problem);
        }
        if seen.contains(&step.id.as_str()) {
            problems.push(format!("duplicate step id '{}'", step.id));
        }
        if let Some(problem) = plan::workbench_tool_problem(&step.tool_name) {
            problems.push(format!("step {}: {problem}", step.id));
        }
        // Control steps are body-aware: body-internal references (same-body
        // ids, per-item pseudo-roots) are legal, so the generic walk below
        // would false-flag them.
        match step.tool_name.as_str() {
            super::AGENT_TOOL => {
                super::agent::validate_agent_input(&step.input, &seen, &step.id, &mut problems)
            }
            super::DECIDE_TOOL => super::decision::validate_decide_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            super::MAP_TOOL => super::iterate::validate_map_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            super::REDUCE_TOOL => super::iterate::validate_reduce_input(
                &step.input,
                &seen,
                &all_ids,
                &step.id,
                &mut problems,
            ),
            _ => {
                // Template parse + reference-ordering check on every string input.
                for value in step.input.values() {
                    super::check_templates(value, &seen, &step.id, &mut problems);
                }
            }
        }
        seen.push(&step.id);
    }
    problems
}

/// Everything wrong with a document that can be known from the document
/// alone: step-level validation plus document-level validation (identifier
/// shape, bare tool names, solver/output exclusion, finish templates).
///
/// This is the basis of the [`apply_edit`] guard — see the module docs for
/// why the catalog is deliberately excluded.
pub fn static_problems(doc: &PlanDoc) -> Vec<String> {
    let mut problems = validate_steps(&doc.steps);
    if let Err(problem) = validate_doc(doc) {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    problems
}

/// The full validation verdict for a document: [`static_problems`] plus
/// catalog-aware tool resolution when a catalog is available.
///
/// The catalog is the runtime-loadable one plans execute against — it
/// deliberately does NOT include the workbench's own `workbench__*` tools,
/// which exist only for that chat agent and would fail at plan run time
/// (they are also rejected statically).
///
/// Catalog *notes* (a `requires_servers` entry that is declared but not
/// configured here) are reported with a `note: ` prefix: the file is
/// portable, but it cannot run on this machine.
pub fn plan_problems(
    doc: &PlanDoc,
    plans: &[PlanDoc],
    catalog: Option<&ToolCatalog>,
) -> Vec<String> {
    let mut problems = static_problems(doc);
    if let Some(catalog) = catalog {
        let check = catalog::resolve_plan_tools_deep(doc, plans, catalog);
        for problem in check.errors {
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
        problems.extend(check.notes.into_iter().map(|note| format!("note: {note}")));
    }
    problems
}

// ── The edit guard ───────────────────────────────────────────────────────

/// An accepted edit: the new document, the mutator's summary, and any
/// problems that were already there before the edit (so a caller can report
/// "applied, but the plan is still invalid for reasons you didn't cause").
#[derive(Debug)]
pub struct EditAccepted {
    pub doc: PlanDoc,
    pub summary: Value,
    pub pre_existing: Vec<String>,
}

/// A rejected edit. `body` is a complete result object ready to report —
/// either the mutator's own structured error, or the guard's
/// "would introduce new problems" verdict.
#[derive(Debug)]
pub struct EditRejected {
    pub body: Value,
    pub introduced: Vec<String>,
    pub pre_existing: Vec<String>,
}

/// Apply `mutate` to a clone of `doc` and accept the result only if it
/// introduces no NEW validation problems (absent before, present after).
/// On rejection the original document is untouched — the caller still holds
/// it.
///
/// Pre-existing problems never block an edit: an already-invalid document
/// (say, straight from the planner) must stay editable, or repairing it
/// becomes impossible. Accepted edits on a still-invalid plan report the
/// remaining pre-existing problems.
///
/// `mutate` errors are full result bodies rather than strings so they can
/// carry structured fields (`availableSteps`, and so on).
pub fn apply_edit(
    doc: &PlanDoc,
    mutate: impl FnOnce(&mut PlanDoc) -> Result<Value, Value>,
) -> Result<EditAccepted, EditRejected> {
    let before = static_problems(doc);
    let mut edited = doc.clone();
    let summary = match mutate(&mut edited) {
        Ok(summary) => summary,
        Err(body) => {
            return Err(EditRejected {
                body,
                introduced: Vec::new(),
                pre_existing: before,
            })
        }
    };
    let after = static_problems(&edited);
    let introduced: Vec<String> = after
        .iter()
        .filter(|p| !before.contains(p))
        .cloned()
        .collect();
    if !introduced.is_empty() {
        let pre_existing: Vec<String> = after
            .iter()
            .filter(|p| before.contains(p))
            .cloned()
            .collect();
        let mut body = json!({
            "error": "edit rejected — it would introduce new validation problems \
                      (the draft is unchanged)",
            "problemsIntroduced": introduced,
        });
        if !pre_existing.is_empty() {
            body["preExistingProblems"] = json!(pre_existing);
        }
        return Err(EditRejected {
            body,
            introduced,
            pre_existing,
        });
    }
    Ok(EditAccepted {
        doc: edited,
        summary,
        pre_existing: after,
    })
}

/// Fold an accepted edit's `pre_existing` problems into its summary — the
/// shared reporting shape for both surfaces.
pub fn summarize_edit(accepted: &EditAccepted) -> Value {
    let mut summary = accepted.summary.clone();
    if !accepted.pre_existing.is_empty() {
        summary["preExistingProblems"] = json!(accepted.pre_existing);
        summary["note"] = json!(
            "edit applied; the plan is still invalid, but only from \
             pre-existing problems (not caused by this edit) — fix them next"
        );
    }
    summary
}

// ── Mutators ─────────────────────────────────────────────────────────────

/// Patch the document's plan-level fields: identifier, name, description,
/// exemplars, requires_servers, input_schema, and/or the finish type
/// (solver ⇄ output).
pub fn patch_metadata(doc: &mut PlanDoc, patch: &Value) -> Result<Value, Value> {
    let mut changed = false;
    if let Some(identifier) = patch.get("identifier").and_then(Value::as_str) {
        if identifier != doc.identifier {
            // A new identifier is a different plan: drop the on-disk identity
            // so the next write creates a new file instead of overwriting the
            // old plan — the document's on-disk identity is kept only while
            // the identifier is unchanged.
            doc.path = None;
        }
        doc.identifier = identifier.to_string();
        changed = true;
    }
    if let Some(name) = patch.get("name").and_then(Value::as_str) {
        doc.name = name.to_string();
        changed = true;
    }
    if let Some(description) = patch.get("description").and_then(Value::as_str) {
        doc.description = description.to_string();
        changed = true;
    }
    if let Some(exemplars) = patch.get("exemplars").and_then(Value::as_array) {
        doc.exemplars = exemplars
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        changed = true;
    }
    if let Some(servers) = patch.get("requires_servers") {
        let list = servers.as_array().ok_or_else(
            || json!({"error": "requires_servers must be an array of server-name strings"}),
        )?;
        doc.requires_servers = list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        changed = true;
    }
    if let Some(schema) = patch.get("input_schema") {
        if schema.is_null() {
            doc.input_schema = None;
        } else if schema.is_object() {
            doc.input_schema = Some(schema.clone());
        } else {
            return Err(
                json!({"error": "input_schema must be a JSON Schema object (or null to clear)"}),
            );
        }
        changed = true;
    }
    if let Some(finish) = patch.get("finish") {
        let has_solver = finish.get("solver").is_some();
        let has_output = finish.get("output").is_some();
        match (has_solver, has_output) {
            (true, true) => {
                return Err(json!({"error": "finish: pass 'solver' OR 'output', not both"}));
            }
            (false, false) => {
                // Empty object or null clears both — a silent side-effect
                // plan. A non-empty object lacking both keys is malformed.
                let is_clear =
                    finish.is_null() || finish.as_object().map(Map::is_empty).unwrap_or(false);
                if !is_clear {
                    return Err(
                        json!({"error": "finish requires 'solver' {queryToAnswer, systemPrompt?} or 'output' {<template map>}, or {} / null to clear to a silent plan"}),
                    );
                }
                doc.solver = None;
                doc.output = None;
                changed = true;
            }
            (true, false) => {
                let solver_val = finish.get("solver").unwrap();
                let solver: SolverData = serde_json::from_value(solver_val.clone())
                    .map_err(|error| json!({"error": format!("invalid solver: {error}")}))?;
                doc.solver = Some(solver);
                doc.output = None;
                changed = true;
            }
            (false, true) => {
                let output_val = finish.get("output").unwrap();
                let output: Map<String, Value> = serde_json::from_value(output_val.clone())
                    .map_err(|error| json!({"error": format!("invalid output: expected a template map — {error}")}))?;
                doc.output = Some(output);
                doc.solver = None;
                changed = true;
            }
        }
    }
    if !changed {
        return Err(json!({
            "error": "update_metadata needs at least one of \
                      identifier, name, description, exemplars, \
                      requires_servers, input_schema, finish"
        }));
    }
    Ok(json!({"ok": true, "identifier": doc.identifier, "name": doc.name}))
}

/// Insert a step: appended, or anchored before/after an existing id.
pub fn patch_add_step(doc: &mut PlanDoc, patch: &Value) -> Result<Value, Value> {
    let Some(step) = patch.get("step") else {
        return Err(json!({
            "error": "add_step requires a 'step' object: \
                      {id, toolName, input, reasoning?}"
        }));
    };
    let step: Step = serde_json::from_value(step.clone())
        .map_err(|error| json!({"error": format!("invalid step: {error}")}))?;
    let before = patch.get("before").and_then(Value::as_str);
    let after = patch.get("after").and_then(Value::as_str);
    let index = match (before, after) {
        (Some(_), Some(_)) => return Err(json!({"error": "pass 'before' or 'after', not both"})),
        (Some(anchor), None) => position_of(anchor, &doc.steps)?,
        (None, Some(anchor)) => position_of(anchor, &doc.steps)? + 1,
        (None, None) => doc.steps.len(),
    };
    let id = step.id.clone();
    doc.steps.insert(index, step);
    Ok(json!({"ok": true, "id": id, "index": index, "steps": doc.steps.len()}))
}

/// Patch one step's fields; `newId` renames it and rewrites downstream
/// `{{id.*}}` references so templates keep working.
pub fn patch_update_step(doc: &mut PlanDoc, patch: &Value) -> Result<Value, Value> {
    let Some(id) = patch.get("id").and_then(Value::as_str) else {
        return Err(json!({"error": "update_step requires an 'id' string"}));
    };
    let index = position_of(id, &doc.steps)?;
    let mut changed = false;
    if let Some(tool_name) = patch.get("toolName").and_then(Value::as_str) {
        doc.steps[index].tool_name = tool_name.to_string();
        changed = true;
    }
    if let Some(new_input) = patch.get("input") {
        let Some(map) = new_input.as_object() else {
            return Err(json!({
                "error": "'input' must be a JSON object — \
                          it replaces the step's whole input"
            }));
        };
        doc.steps[index].input = map.clone();
        changed = true;
    }
    if let Some(reasoning) = patch.get("reasoning").and_then(Value::as_str) {
        doc.steps[index].reasoning = (!reasoning.is_empty()).then(|| reasoning.to_string());
        changed = true;
    }
    let mut final_id = id.to_string();
    if let Some(new_id) = patch.get("newId").and_then(Value::as_str) {
        if new_id != id {
            doc.steps[index].id = new_id.to_string();
            rename_references(doc, index, id, new_id);
            final_id = new_id.to_string();
        }
        changed = true;
    }
    if !changed {
        return Err(json!({
            "error": "update_step needs at least one of \
                      newId, toolName, input, reasoning"
        }));
    }
    Ok(json!({"ok": true, "id": final_id}))
}

/// Remove a step. Validation rejects the edit if later steps still
/// reference it — the problems say which templates dangle.
pub fn patch_delete_step(doc: &mut PlanDoc, patch: &Value) -> Result<Value, Value> {
    let Some(id) = patch.get("id").and_then(Value::as_str) else {
        return Err(json!({"error": "delete_step requires an 'id' string"}));
    };
    let index = position_of(id, &doc.steps)?;
    doc.steps.remove(index);
    Ok(json!({"ok": true, "id": id, "steps": doc.steps.len()}))
}

/// Index of a top-level step by id, or a structured error listing what
/// exists.
pub fn position_of(id: &str, steps: &[Step]) -> Result<usize, Value> {
    steps.iter().position(|step| step.id == id).ok_or_else(|| {
        json!({
            "error": format!("no step with id '{id}'"),
            "availableSteps": steps.iter().map(|step| step.id.as_str()).collect::<Vec<_>>(),
        })
    })
}

/// After renaming a step id, rewrite `{{old.*}}` roots everywhere that can
/// see the step's result: later steps' inputs (which contain any control
/// bodies), the output map, and the solver templates.
pub fn rename_references(doc: &mut PlanDoc, index: usize, old: &str, new: &str) {
    for step in doc.steps.iter_mut().skip(index + 1) {
        for value in step.input.values_mut() {
            rewrite_value_roots(value, old, new);
        }
    }
    if let Some(output) = &mut doc.output {
        for value in output.values_mut() {
            rewrite_value_roots(value, old, new);
        }
    }
    if let Some(solver) = &mut doc.solver {
        solver.query_to_answer = template::rewrite_root(&solver.query_to_answer, old, new);
        if let Some(prompt) = &mut solver.system_prompt {
            *prompt = template::rewrite_root(prompt, old, new);
        }
        for value in solver.data.values_mut() {
            rewrite_value_roots(value, old, new);
        }
    }
}

/// Apply `rewrite_root` to every string in a JSON value.
fn rewrite_value_roots(value: &mut Value, old: &str, new: &str) {
    match value {
        Value::String(text) => {
            if text.contains("{{") {
                *text = template::rewrite_root(text, old, new);
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_value_roots(item, old, new);
            }
        }
        Value::Object(map) => {
            for entry in map.values_mut() {
                rewrite_value_roots(entry, old, new);
            }
        }
        _ => {}
    }
}

// ── Drafting ─────────────────────────────────────────────────────────────

/// Fold the planner's output into a document: a revision keeps the existing
/// document's identity and metadata (identifier, name, description,
/// exemplars, requires_servers) and replaces its steps; a fresh draft
/// derives them from the goal.
pub fn merge_planner_output(
    existing: Option<PlanDoc>,
    goal: &str,
    output: PlannerOutput,
) -> PlanDoc {
    match existing {
        Some(mut doc) => {
            doc.steps = output.plan;
            // Preserve an `output` finish; otherwise refresh the solver.
            if doc.output.is_none() {
                doc.solver = Some(output.solver_data);
            }
            doc
        }
        None => PlanDoc {
            // A name the goal states explicitly ('named "the_goat"') is the
            // plan's identity; raw goal prose is only the fallback.
            identifier: stated_name(goal)
                .map(|name| identifier_from(&name))
                .unwrap_or_else(|| identifier_from(goal)),
            name: stated_name(goal).unwrap_or_else(|| name_from(goal)),
            description: goal.to_string(),
            exemplars: Vec::new(),
            requires_servers: Vec::new(),
            input_schema: None,
            steps: output.plan,
            solver: Some(output.solver_data),
            output: None,
            path: None,
        },
    }
}

/// A plan name the goal states explicitly — "named X", "called X", or
/// "name it X". A quoted string anywhere after the marker wins (so
/// 'named something like "the_goat"' resolves to the_goat); otherwise the
/// next word, unless it's filler that describes rather than names.
pub fn stated_name(goal: &str) -> Option<String> {
    let position = ["named", "called", "name it"]
        .iter()
        .filter_map(|marker| find_ascii_ci(goal, marker).map(|at| at + marker.len()))
        .min()?;
    let mut rest = goal[position..].trim_start();
    loop {
        let first = rest.chars().next()?;
        // A quoted string is the name verbatim.
        if matches!(first, '"' | '\'' | '`') {
            let inner = &rest[first.len_utf8()..];
            let name = inner[..inner.find(first)?].trim();
            return (!name.is_empty() && name.chars().count() <= 60).then(|| name.to_string());
        }
        let (token, remainder) = match rest.split_once(char::is_whitespace) {
            Some((token, remainder)) => (token, remainder.trim_start()),
            None => (rest, ""),
        };
        let word = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        // Skip filler that describes rather than names.
        if matches!(
            word.to_ascii_lowercase().as_str(),
            "" | "something" | "like" | "maybe" | "perhaps" | "it" | "the" | "a" | "an"
        ) {
            rest = remainder;
            continue;
        }
        return Some(word.to_string());
    }
}

/// Byte offset of an ASCII needle, case-insensitively and on word
/// boundaries ("renamed" must not match "named"). Matches are all-ASCII,
/// so the offset is always a char boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let target = needle.as_bytes();
    if bytes.len() < target.len() {
        return None;
    }
    (0..=bytes.len() - target.len()).find(|&at| {
        bytes[at..at + target.len()].eq_ignore_ascii_case(target)
            && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric())
            && bytes
                .get(at + target.len())
                .is_none_or(|next| !next.is_ascii_alphanumeric())
    })
}

/// Tool-name-safe identifier from a free-form goal.
pub fn identifier_from(goal: &str) -> String {
    let mut identifier = String::new();
    for c in goal.chars().take(60) {
        if c.is_ascii_alphanumeric() {
            identifier.push(c.to_ascii_lowercase());
        } else if !identifier.ends_with('_') && !identifier.is_empty() {
            identifier.push('_');
        }
    }
    let identifier = identifier.trim_matches('_').to_string();
    if identifier.is_empty() {
        "draft_plan".to_string()
    } else {
        identifier.chars().take(40).collect::<String>()
    }
}

/// A display name from a free-form goal: its first line, truncated.
pub fn name_from(goal: &str) -> String {
    let first_line = goal.lines().next().unwrap_or_default().trim();
    let mut name: String = first_line.chars().take(60).collect();
    if name.is_empty() {
        name = "Draft plan".to_string();
    }
    name
}

// ── Writing ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The destination holds a *different* plan. Overwriting would silently
    /// destroy it, so this is refused unless the caller forces it.
    #[error("{path} holds plan '{existing}', not '{identifier}' — refusing to overwrite it")]
    IdentityMismatch {
        path: String,
        existing: String,
        identifier: String,
    },
    #[error("{path} already exists — change the identifier or remove the file")]
    Exists { path: String },
    #[error("no plans directory configured ([plans].paths)")]
    NoPlansDir,
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Serialize(String),
}

/// Where a document should be written: back to the file it came from, or
/// `<plans_dir>/<identifier>.yaml` for one that has no on-disk identity yet.
///
/// A new document never lands on an existing file — that path is reserved
/// for the plan already there, and `patch_metadata` clears `path` on an
/// identifier change precisely so a rename cannot overwrite the original.
pub fn target_path(doc: &PlanDoc, plans_dir: Option<&Path>) -> Result<PathBuf, WriteError> {
    match &doc.path {
        Some(path) => Ok(path.clone()),
        None => {
            let dir = plans_dir.ok_or(WriteError::NoPlansDir)?;
            let candidate = dir.join(format!("{}.yaml", doc.identifier));
            if candidate.exists() {
                return Err(WriteError::Exists {
                    path: candidate.display().to_string(),
                });
            }
            Ok(candidate)
        }
    }
}

/// The `agent` step's own input keys, in the spelling the planner emits paired
/// with the spelling a plan file uses. Only these keys are renamed: the *values*
/// are free-form (an `outputSchema` is a whole JSON Schema, whose property names
/// are the tool author's business) and are never descended into.
const AGENT_INPUT_KEYS: [(&str, &str); 3] = [
    ("systemPrompt", "system_prompt"),
    ("maxIterations", "max_iterations"),
    ("outputSchema", "output_schema"),
];

/// Serialize a document as the YAML a human would have written.
///
/// `Step` and `SolverData` *serialize* camelCase, and the control-step specs
/// accept it, because that is *prompt surface* — the planner's JSON schema must
/// keep saying `toolName` and `queryToAnswer`. The plan *file* format is
/// snake_case at every depth, so the two spellings are reconciled here rather
/// than by changing either contract. camelCase stays accepted on the way in
/// (serde aliases), so this round-trips and normalizes an older file in passing.
///
/// See `docs/reference/plan-schema.mdx` — that page is the contract this
/// function has to satisfy.
pub fn to_yaml(doc: &PlanDoc) -> Result<String, WriteError> {
    let value = to_file_shape(doc)?;
    serde_yaml::to_string(&value).map_err(|e| WriteError::Serialize(e.to_string()))
}

/// The document as JSON, in the plan *file*'s shape.
///
/// Shares [`to_yaml`]'s normalization on purpose: `graph plan show --json`
/// describes a file on disk, and a caller that reads the envelope and a human
/// who opens the file must see one spelling. Key order is serde_json's
/// (alphabetical), which is what a machine caller addresses by name anyway.
pub fn to_json(doc: &PlanDoc) -> Result<Value, WriteError> {
    let value = to_file_shape(doc)?;
    serde_json::to_value(value).map_err(|e| WriteError::Serialize(e.to_string()))
}

/// The document normalized into the file format's spelling, still as a tree.
///
/// Deliberately built on `serde_yaml::Value`: its mapping preserves insertion
/// order, so a hand-authored file survives a parse/serialize cycle with its
/// field order intact.
fn to_file_shape(doc: &PlanDoc) -> Result<serde_yaml::Value, WriteError> {
    let mut value = serde_yaml::to_value(doc).map_err(|e| WriteError::Serialize(e.to_string()))?;
    if let Some(steps) = value.get_mut("steps").and_then(|s| s.as_sequence_mut()) {
        for step in steps {
            snake_case_step(step);
        }
    }
    if let Some(solver) = value.get_mut("solver") {
        rename_key(solver, "queryToAnswer", "query_to_answer");
        rename_key(solver, "systemPrompt", "system_prompt");
    }
    Ok(value)
}

/// Rewrite one step into the file's spelling, descending into the bodies a
/// control step carries in its `input`.
///
/// Recursion follows only the bodies of the *known* control steps. An ordinary
/// tool's input is a free-form map that may legitimately hold keys named `do`,
/// `then`, or `input`, and renaming inside one would corrupt a real argument.
fn snake_case_step(step: &mut serde_yaml::Value) {
    rename_key(step, "toolName", "tool_name");
    // `Step::reasoning` can't use `skip_serializing_if` — that would change the
    // planner's schema — so an unset one is dropped here.
    remove_null(step, "reasoning");

    let tool = step
        .get("tool_name")
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_owned);
    let Some(input) = step.get_mut("input") else {
        return;
    };
    match tool.as_deref() {
        Some(AGENT_TOOL) => {
            for (planner, file) in AGENT_INPUT_KEYS {
                rename_key(input, planner, file);
            }
        }
        Some(DECIDE_TOOL) => {
            for side in ["then", "else"] {
                if let Some(branch) = input.get_mut(side) {
                    snake_case_body(branch);
                }
            }
        }
        Some(MAP_TOOL | REDUCE_TOOL) => {
            if let Some(body) = input.get_mut("do") {
                snake_case_body(body);
            }
        }
        _ => {}
    }
}

/// A control step's body, in either of its two shapes: a list of steps, or a
/// single call — a step without an id, which [`snake_case_step`] handles as-is.
fn snake_case_body(body: &mut serde_yaml::Value) {
    match body {
        serde_yaml::Value::Sequence(steps) => steps.iter_mut().for_each(snake_case_step),
        serde_yaml::Value::Mapping(_) => snake_case_step(body),
        _ => {}
    }
}

/// Drop a key from a YAML mapping when its value is null, so a field nobody
/// set doesn't appear as `key: null` in a hand-readable file.
fn remove_null(value: &mut serde_yaml::Value, key: &str) {
    if let Some(mapping) = value.as_mapping_mut() {
        let key = serde_yaml::Value::from(key);
        if mapping.get(&key).is_some_and(serde_yaml::Value::is_null) {
            mapping.remove(&key);
        }
    }
}

/// Rename one key of a YAML mapping, preserving field order by rebuilding it.
fn rename_key(value: &mut serde_yaml::Value, from: &str, to: &str) {
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    if !mapping.contains_key(serde_yaml::Value::from(from)) {
        return;
    }
    let renamed: serde_yaml::Mapping = mapping
        .iter()
        .map(|(key, val)| {
            let key = if key.as_str() == Some(from) {
                serde_yaml::Value::from(to)
            } else {
                key.clone()
            };
            (key, val.clone())
        })
        .collect();
    *value = serde_yaml::Value::Mapping(renamed);
}

/// Write a document to `path` as YAML, atomically (temp file in the same
/// directory, fsync, rename) so a concurrent reader never sees a half-written
/// plan.
///
/// Refuses when the file already there holds a different `identifier`, unless
/// `force`. An unreadable or garbled destination is not a mismatch — the
/// write cannot lose a plan that isn't parseable.
pub fn write_doc(doc: &PlanDoc, path: &Path, force: bool) -> Result<(), WriteError> {
    if !force {
        if let Some(existing) = on_disk_identifier(path) {
            if existing != doc.identifier {
                return Err(WriteError::IdentityMismatch {
                    path: path.display().to_string(),
                    existing,
                    identifier: doc.identifier.clone(),
                });
            }
        }
    }
    let yaml = to_yaml(doc)?;
    let dir = path
        .parent()
        .ok_or_else(|| WriteError::Io(format!("no parent dir for {}", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| WriteError::Io(format!("creating {}: {e}", dir.display())))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| WriteError::Io(format!("creating temp file in {}: {e}", dir.display())))?;
    use std::io::Write;
    tmp.write_all(yaml.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| WriteError::Io(format!("writing {}: {e}", path.display())))?;
    tmp.persist(path)
        .map_err(|e| WriteError::Io(format!("renaming into {}: {e}", path.display())))?;
    Ok(())
}

/// The `identifier` of the plan currently in a file, if it can be read and
/// parsed at all — unreadable/garbled files return None and the write
/// proceeds (it can't lose a plan that isn't there).
pub fn on_disk_identifier(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    Some(value.get("identifier")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> PlanDoc {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// A two-step plan where E2 templates off E1 — the fixture for the
    /// rename and dangling-reference cases.
    fn linked() -> PlanDoc {
        doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: "{{input.q}}" }
  - id: E2
    tool_name: t__summarize
    input: { text: "{{E1.body}}" }
output:
  summary: "{{E2.text}}"
"#)
    }

    // ── the guard ────────────────────────────────────────────────────

    #[test]
    fn accepts_an_edit_that_keeps_the_plan_valid() {
        let before = linked();
        let accepted = apply_edit(&before, |d| patch_metadata(d, &json!({"name": "Renamed"})))
            .expect("edit should be accepted");
        assert_eq!(accepted.doc.name, "Renamed");
        assert!(accepted.pre_existing.is_empty());
        // The caller's copy is untouched — apply_edit works on a clone.
        assert_eq!(before.name, "Demo");
    }

    #[test]
    fn rejects_an_edit_that_introduces_a_problem_and_leaves_the_doc_alone() {
        let before = linked();
        // Deleting E1 dangles E2's {{E1.body}}.
        let rejected = apply_edit(&before, |d| patch_delete_step(d, &json!({"id": "E1"})))
            .expect_err("edit should be rejected");
        assert_eq!(before.steps.len(), 2, "the original must be untouched");
        assert!(
            rejected.introduced.iter().any(|p| p.contains("E1")),
            "the dangling reference should be named: {:?}",
            rejected.introduced
        );
        assert_eq!(
            rejected.body["error"],
            json!(
                "edit rejected — it would introduce new validation problems \
                 (the draft is unchanged)"
            )
        );
    }

    #[test]
    fn pre_existing_problems_never_block_an_edit() {
        // An unknown bare tool name is a pre-existing document problem.
        let mut broken = linked();
        broken.steps[0].tool_name = "not_namespaced".to_string();
        assert!(
            !static_problems(&broken).is_empty(),
            "fixture must be invalid"
        );

        let accepted = apply_edit(&broken, |d| {
            patch_metadata(d, &json!({"description": "still broken, still editable"}))
        })
        .expect("a pre-existing problem must not block an unrelated edit");
        assert_eq!(accepted.doc.description, "still broken, still editable");
        assert!(
            !accepted.pre_existing.is_empty(),
            "the surviving problem should be reported back"
        );
        // And it is surfaced to the caller rather than hidden.
        let summary = summarize_edit(&accepted);
        assert!(summary["preExistingProblems"].is_array());
        assert!(summary["note"].as_str().unwrap().contains("pre-existing"));
    }

    #[test]
    fn a_mutator_error_is_reported_verbatim_with_no_introduced_problems() {
        let rejected = apply_edit(&linked(), |d| {
            patch_update_step(d, &json!({"id": "nope", "toolName": "t__x"}))
        })
        .expect_err("unknown step id should be rejected");
        assert!(rejected.introduced.is_empty());
        assert_eq!(rejected.body["error"], json!("no step with id 'nope'"));
        assert_eq!(rejected.body["availableSteps"], json!(["E1", "E2"]));
    }

    // ── renames ──────────────────────────────────────────────────────

    #[test]
    fn renaming_a_step_rewrites_downstream_inputs_and_the_output_map() {
        let accepted = apply_edit(&linked(), |d| {
            patch_update_step(d, &json!({"id": "E1", "newId": "fetch"}))
        })
        .expect("a rename with reference rewriting stays valid");
        let d = accepted.doc;
        assert_eq!(d.steps[0].id, "fetch");
        assert_eq!(d.steps[1].input["text"], json!("{{fetch.body}}"));
        // E2 was not renamed, so the output map still points at it.
        assert_eq!(d.output.unwrap()["summary"], json!("{{E2.text}}"));
    }

    #[test]
    fn renaming_rewrites_solver_templates() {
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: x }
solver:
  query_to_answer: "what does {{E1.body}} say?"
  system_prompt: "context: {{E1.title}}"
  data:
    body: "{{E1.body}}"
    nested: ["{{E1.a}}", { deep: "{{E1.b}}" }]
"#);
        let accepted = apply_edit(&base, |d| {
            patch_update_step(d, &json!({"id": "E1", "newId": "fetch"}))
        })
        .expect("rename should be accepted");
        let solver = accepted.doc.solver.unwrap();
        assert_eq!(solver.query_to_answer, "what does {{fetch.body}} say?");
        assert_eq!(solver.system_prompt.unwrap(), "context: {{fetch.title}}");
        assert_eq!(solver.data["body"], json!("{{fetch.body}}"));
        assert_eq!(
            solver.data["nested"],
            json!(["{{fetch.a}}", {"deep": "{{fetch.b}}"}]),
            "rewriting must recurse through arrays and objects"
        );
    }

    #[test]
    fn renaming_leaves_earlier_steps_alone() {
        // A rename only rewrites what can *see* the step's result. An
        // earlier step mentioning the same word is coincidence, not a
        // reference, and must not be touched.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: "E2 is not a reference here" }
  - id: E2
    tool_name: t__summarize
    input: { text: "{{E1.body}}" }
"#);
        let accepted = apply_edit(&base, |d| {
            patch_update_step(d, &json!({"id": "E2", "newId": "summarize"}))
        })
        .unwrap();
        assert_eq!(
            accepted.doc.steps[0].input["query"],
            json!("E2 is not a reference here")
        );
    }

    // ── metadata ─────────────────────────────────────────────────────

    #[test]
    fn changing_the_identifier_drops_the_on_disk_identity() {
        let mut base = linked();
        base.path = Some(PathBuf::from("/plans/demo.yaml"));
        let accepted = apply_edit(&base, |d| {
            patch_metadata(d, &json!({"identifier": "renamed"}))
        })
        .unwrap();
        assert_eq!(accepted.doc.identifier, "renamed");
        assert!(
            accepted.doc.path.is_none(),
            "a new identifier must not keep the old file's identity"
        );
    }

    #[test]
    fn setting_one_finish_mode_clears_the_other() {
        // output → solver
        let accepted = apply_edit(&linked(), |d| {
            patch_metadata(
                d,
                &json!({"finish": {"solver": {"query_to_answer": "q?", "data": {}}}}),
            )
        })
        .unwrap();
        assert!(accepted.doc.solver.is_some());
        assert!(
            accepted.doc.output.is_none(),
            "solver and output are mutually exclusive"
        );

        // and back, then cleared to a silent plan
        let back = apply_edit(&accepted.doc, |d| {
            patch_metadata(d, &json!({"finish": {"output": {"x": "{{E2.text}}"}}}))
        })
        .unwrap();
        assert!(back.doc.solver.is_none());
        let silent = apply_edit(&back.doc, |d| patch_metadata(d, &json!({"finish": {}}))).unwrap();
        assert!(silent.doc.solver.is_none() && silent.doc.output.is_none());
    }

    #[test]
    fn an_empty_metadata_patch_is_an_error() {
        let rejected = apply_edit(&linked(), |d| patch_metadata(d, &json!({})))
            .err()
            .unwrap();
        assert!(rejected.body["error"]
            .as_str()
            .unwrap()
            .contains("at least one of"));
    }

    // ── steps ────────────────────────────────────────────────────────

    #[test]
    fn add_step_anchors_before_and_after() {
        let step = |id: &str| json!({"step": {"id": id, "toolName": "t__x", "input": {}}});
        let appended = apply_edit(&linked(), |d| patch_add_step(d, &step("E9"))).unwrap();
        assert_eq!(ids(&appended.doc), vec!["E1", "E2", "E9"]);

        let mut before = step("E0");
        before["before"] = json!("E1");
        let inserted = apply_edit(&linked(), |d| patch_add_step(d, &before)).unwrap();
        assert_eq!(ids(&inserted.doc), vec!["E0", "E1", "E2"]);

        let mut after = step("E15");
        after["after"] = json!("E1");
        let middle = apply_edit(&linked(), |d| patch_add_step(d, &after)).unwrap();
        assert_eq!(ids(&middle.doc), vec!["E1", "E15", "E2"]);
    }

    #[test]
    fn a_step_added_out_of_order_is_rejected_for_referencing_a_later_step() {
        // Inserting before E1 a step that reads {{E1.body}} inverts the
        // dependency — the guard must catch it.
        let patch = json!({
            "step": {"id": "E0", "toolName": "t__x", "input": {"v": "{{E1.body}}"}},
            "before": "E1",
        });
        let rejected = apply_edit(&linked(), |d| patch_add_step(d, &patch))
            .err()
            .unwrap();
        assert!(
            rejected.introduced.iter().any(|p| p.contains("E0")),
            "{:?}",
            rejected.introduced
        );
    }

    #[test]
    fn deleting_an_unreferenced_step_is_fine() {
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: x }
  - id: E2
    tool_name: t__log
    input: { msg: hello }
"#);
        let accepted = apply_edit(&base, |d| patch_delete_step(d, &json!({"id": "E2"}))).unwrap();
        assert_eq!(ids(&accepted.doc), vec!["E1"]);
    }

    #[test]
    fn update_step_replaces_the_whole_input_and_clears_reasoning_on_empty() {
        let accepted = apply_edit(&linked(), |d| {
            patch_update_step(
                d,
                &json!({"id": "E2", "input": {"text": "literal"}, "reasoning": "why"}),
            )
        })
        .unwrap();
        assert_eq!(accepted.doc.steps[1].input, {
            let mut m = Map::new();
            m.insert("text".into(), json!("literal"));
            m
        });
        assert_eq!(accepted.doc.steps[1].reasoning.as_deref(), Some("why"));

        let cleared = apply_edit(&accepted.doc, |d| {
            patch_update_step(d, &json!({"id": "E2", "reasoning": ""}))
        })
        .unwrap();
        assert_eq!(cleared.doc.steps[1].reasoning, None);
    }

    #[test]
    fn update_step_rejects_a_non_object_input() {
        let rejected = apply_edit(&linked(), |d| {
            patch_update_step(d, &json!({"id": "E2", "input": "not an object"}))
        })
        .err()
        .unwrap();
        assert!(rejected.body["error"]
            .as_str()
            .unwrap()
            .contains("must be a JSON object"));
    }

    #[test]
    fn a_workbench_tool_step_is_rejected() {
        let patch = json!({
            "step": {"id": "E9", "toolName": "workbench__run_plan", "input": {}},
        });
        let rejected = apply_edit(&linked(), |d| patch_add_step(d, &patch))
            .err()
            .unwrap();
        assert!(
            !rejected.introduced.is_empty(),
            "workbench__ steps must never enter a plan"
        );
    }

    fn ids(doc: &PlanDoc) -> Vec<&str> {
        doc.steps.iter().map(|s| s.id.as_str()).collect()
    }

    // ── writing ──────────────────────────────────────────────────────

    #[test]
    fn write_doc_round_trips_and_is_readable_as_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        write_doc(&linked(), &path, false).unwrap();
        let reloaded = super::super::doc::load_plan_doc(&path).unwrap();
        assert_eq!(reloaded.identifier, "demo");
        assert_eq!(reloaded.steps.len(), 2);
    }

    #[test]
    fn written_yaml_uses_the_documented_snake_case_spelling() {
        // `Step` serializes camelCase for the planner's schema; the plan file
        // format is snake_case. A file an agent writes must look like one a
        // human wrote, or `graph plan show` and the docs disagree.
        let yaml = to_yaml(&linked()).unwrap();
        assert!(yaml.contains("tool_name: t__search"), "{yaml}");
        assert!(!yaml.contains("toolName"), "{yaml}");
    }

    #[test]
    fn written_yaml_omits_empty_defaults() {
        // Every one of these would otherwise appear as `exemplars: []` /
        // `reasoning: null` noise in a plan nobody set them on.
        let yaml = to_yaml(&linked()).unwrap();
        for absent in ["exemplars", "requires_servers", "input_schema", "reasoning"] {
            assert!(
                !yaml.contains(absent),
                "{absent} should be omitted:\n{yaml}"
            );
        }
    }

    #[test]
    fn written_yaml_snake_cases_solver_keys() {
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input: { query: x }
solver:
  query_to_answer: "what does {{E1.body}} say?"
  system_prompt: "be terse"
  data: { body: "{{E1.body}}" }
"#);
        let yaml = to_yaml(&base).unwrap();
        assert!(yaml.contains("query_to_answer:"), "{yaml}");
        assert!(yaml.contains("system_prompt:"), "{yaml}");
        assert!(!yaml.contains("queryToAnswer"), "{yaml}");
        assert!(!yaml.contains("systemPrompt"), "{yaml}");
    }

    #[test]
    fn written_yaml_snake_cases_an_agent_steps_input_keys() {
        // The `agent` spec's own keys are part of the plan file format, so they
        // are written in the file's spelling like every other field.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: agent
    input:
      prompt: do the thing
      maxIterations: 3
      systemPrompt: be terse
      outputSchema: { type: object, properties: { ok: { type: boolean } } }
"#);
        let yaml = to_yaml(&base).unwrap();
        assert!(yaml.contains("max_iterations: 3"), "{yaml}");
        assert!(yaml.contains("system_prompt: be terse"), "{yaml}");
        assert!(yaml.contains("output_schema:"), "{yaml}");
        assert!(yaml.contains("tool_name: agent"), "{yaml}");
        assert!(!yaml.contains("maxIterations"), "{yaml}");
        assert!(!yaml.contains("systemPrompt"), "{yaml}");
        assert!(!yaml.contains("outputSchema"), "{yaml}");
    }

    #[test]
    fn an_output_schemas_own_property_names_are_left_alone() {
        // The key is the plan's; the *value* is a JSON Schema whose property
        // names belong to whoever consumes the agent's output. Descending into
        // it would silently rewrite the contract the agent is held to.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: agent
    input:
      prompt: do the thing
      outputSchema:
        type: object
        properties:
          reviewStatus: { type: string }
          maxIterations: { type: integer }
"#);
        let yaml = to_yaml(&base).unwrap();
        assert!(yaml.contains("output_schema:"), "{yaml}");
        assert!(yaml.contains("reviewStatus:"), "{yaml}");
        // The nested property merely *named* like a spec key stays untouched.
        assert!(yaml.contains("maxIterations:"), "{yaml}");
    }

    #[test]
    fn an_ordinary_tools_input_keeps_its_own_camel_case_keys() {
        // `input` is free-form for every non-control tool: a real MCP argument
        // may be spelled camelCase, or named like a control-step body key.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: t__search
    input:
      teamId: ENG
      outputSchema: passthrough
      do: something
      then: later
"#);
        let yaml = to_yaml(&base).unwrap();
        for untouched in ["teamId:", "outputSchema: passthrough", "do:", "then:"] {
            assert!(yaml.contains(untouched), "{untouched} rewritten:\n{yaml}");
        }
    }

    #[test]
    fn written_yaml_snake_cases_inside_control_step_bodies() {
        // A body is a step list or a single call, and either can hold an
        // `agent`. Renaming only top-level steps would leak the planner's
        // spelling into every branch.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: decide
    input:
      if: { value: "{{E0.count}}", op: gt, to: 0 }
      then:
        toolName: t__search
        input: { query: x }
      else:
        - id: B1
          toolName: agent
          input:
            prompt: summarize
            maxIterations: 2
            outputSchema: { type: object }
  - id: E2
    tool_name: map
    input:
      over: "{{E1.results}}"
      do:
        toolName: agent
        input:
          prompt: "check {{item}}"
          outputSchema: { type: object }
"#);
        let yaml = to_yaml(&base).unwrap();
        assert!(!yaml.contains("toolName"), "{yaml}");
        assert!(!yaml.contains("maxIterations"), "{yaml}");
        assert!(!yaml.contains("outputSchema"), "{yaml}");
        assert_eq!(yaml.matches("tool_name: agent").count(), 2, "{yaml}");
        assert!(yaml.contains("tool_name: t__search"), "{yaml}");
        assert!(yaml.contains("max_iterations: 2"), "{yaml}");
    }

    #[test]
    fn the_json_shape_spells_fields_like_the_file_does() {
        // `plan show --json` describes a file on disk; a caller parsing the
        // envelope and a human reading the file must not see two spellings.
        let base = doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E1
    tool_name: agent
    input:
      prompt: do the thing
      maxIterations: 3
      outputSchema: { type: object }
solver:
  query_to_answer: what happened?
  data: {}
"#);
        let json = serde_json::to_string(&to_json(&base).unwrap()).unwrap();
        assert!(json.contains("\"tool_name\""), "{json}");
        assert!(json.contains("\"max_iterations\""), "{json}");
        assert!(json.contains("\"output_schema\""), "{json}");
        assert!(json.contains("\"query_to_answer\""), "{json}");
        assert!(!json.contains("toolName"), "{json}");
        assert!(!json.contains("maxIterations"), "{json}");
        assert!(!json.contains("queryToAnswer"), "{json}");
    }

    #[test]
    fn a_camel_case_plan_file_still_loads() {
        // The planner's spelling stays accepted on the way in, so a plan
        // authored before the file format settled keeps working.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.yaml");
        std::fs::write(
            &path,
            r#"
identifier: legacy
name: Legacy
description: authored in the planner's spelling
steps:
  - id: E1
    toolName: agent
    input:
      prompt: do the thing
      maxIterations: 3
      outputSchema: { type: object }
solver:
  queryToAnswer: what happened?
  systemPrompt: be terse
  data: {}
"#,
        )
        .unwrap();
        let loaded = super::super::doc::load_plan_doc(&path).unwrap();
        assert_eq!(loaded.steps[0].tool_name, "agent");
        assert_eq!(
            loaded.solver.as_ref().unwrap().query_to_answer,
            "what happened?"
        );
        // …and rewriting it normalizes the file to the documented spelling.
        let yaml = to_yaml(&loaded).unwrap();
        assert!(!yaml.contains("toolName"), "{yaml}");
        assert!(!yaml.contains("queryToAnswer"), "{yaml}");
        assert!(!yaml.contains("maxIterations"), "{yaml}");
    }

    #[test]
    fn a_canonical_file_round_trips_byte_for_byte() {
        // Editing one field must not churn the rest of a hand-authored file.
        let original = "identifier: demo\nname: Demo\ndescription: demo plan\nsteps:\n\
                        - id: E1\n  tool_name: t__search\n  input:\n    query: x\n";
        let parsed = doc(original);
        assert_eq!(
            to_yaml(&parsed).unwrap(),
            original,
            "a canonical plan file should survive a parse/serialize cycle unchanged"
        );
    }

    #[test]
    fn write_doc_refuses_to_overwrite_a_different_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        let mut other = linked();
        other.identifier = "someone_else".to_string();
        write_doc(&other, &path, false).unwrap();

        let error = write_doc(&linked(), &path, false).unwrap_err();
        assert!(
            matches!(&error, WriteError::IdentityMismatch { existing, .. } if existing == "someone_else"),
            "got {error:?}"
        );
        assert_eq!(
            on_disk_identifier(&path).unwrap(),
            "someone_else",
            "the refused write must not have touched the file"
        );
        // …and force overrides it.
        write_doc(&linked(), &path, true).unwrap();
        assert_eq!(on_disk_identifier(&path).unwrap(), "demo");
    }

    #[test]
    fn write_doc_overwrites_an_unparseable_file() {
        // Garbage on disk is not a plan, so the write can't lose one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        std::fs::write(&path, "\t: not: valid: yaml: [").unwrap();
        write_doc(&linked(), &path, false).unwrap();
        assert_eq!(on_disk_identifier(&path).unwrap(), "demo");
    }

    #[test]
    fn target_path_prefers_the_source_file() {
        let mut d = linked();
        d.path = Some(PathBuf::from("/elsewhere/custom-name.yaml"));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            target_path(&d, Some(dir.path())).unwrap(),
            PathBuf::from("/elsewhere/custom-name.yaml"),
            "a loaded plan is written back where it came from, not renamed"
        );
    }

    #[test]
    fn target_path_derives_from_the_identifier_and_will_not_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let d = linked();
        let expected = dir.path().join("demo.yaml");
        assert_eq!(target_path(&d, Some(dir.path())).unwrap(), expected);

        std::fs::write(&expected, "identifier: demo\n").unwrap();
        assert!(matches!(
            target_path(&d, Some(dir.path())),
            Err(WriteError::Exists { .. })
        ));
        assert!(matches!(target_path(&d, None), Err(WriteError::NoPlansDir)));
    }

    // ── planner merge ────────────────────────────────────────────────

    #[test]
    fn merging_into_an_existing_doc_keeps_its_identity() {
        let mut existing = linked();
        existing.exemplars = vec!["an example".to_string()];
        let output = PlannerOutput {
            plan: vec![Step {
                id: "E1".to_string(),
                tool_name: "t__new".to_string(),
                input: Map::new(),
                reasoning: None,
            }],
            solver_data: SolverData::default(),
        };
        let merged = merge_planner_output(Some(existing), "a totally different goal", output);
        assert_eq!(merged.identifier, "demo", "identity survives a revision");
        assert_eq!(merged.exemplars, vec!["an example".to_string()]);
        assert_eq!(merged.steps.len(), 1);
        assert!(
            merged.solver.is_none() && merged.output.is_some(),
            "an existing `output` finish is preserved, not replaced by a solver"
        );
    }

    #[test]
    fn a_fresh_draft_takes_a_stated_name_over_the_goal_prose() {
        let output = PlannerOutput {
            plan: Vec::new(),
            solver_data: SolverData::default(),
        };
        let fresh = merge_planner_output(
            None,
            "summarize the sprint, named \"sprint_digest\"",
            output,
        );
        assert_eq!(fresh.identifier, "sprint_digest");
        assert_eq!(fresh.name, "sprint_digest");
        assert_eq!(
            fresh.description, "summarize the sprint, named \"sprint_digest\"",
            "the goal is kept verbatim as the description — it is the routing signal"
        );
    }

    #[test]
    fn a_fresh_draft_falls_back_to_a_slug_of_the_goal() {
        let output = PlannerOutput {
            plan: Vec::new(),
            solver_data: SolverData::default(),
        };
        let fresh = merge_planner_output(None, "Summarize the sprint!", output);
        assert_eq!(fresh.identifier, "summarize_the_sprint");
        assert_eq!(fresh.name, "Summarize the sprint!");
    }

    #[test]
    fn identifiers_are_tool_name_safe() {
        assert_eq!(
            identifier_from("Summarize this sprint's progress!"),
            "summarize_this_sprint_s_progress"
        );
        assert_eq!(identifier_from("!!!"), "draft_plan");
    }

    #[test]
    fn stated_names_in_goals_set_the_draft_identity() {
        let output = || PlannerOutput {
            plan: Vec::new(),
            solver_data: SolverData::default(),
        };

        // The incident goal: the name is stated, quoted, after filler.
        let goal = r#"Build a Linear-workbench plan named something like "the_goat" that pulls sprint data"#;
        let fresh = merge_planner_output(None, goal, output());
        assert_eq!(fresh.identifier, "the_goat");
        assert_eq!(fresh.name, "the_goat");

        // Unquoted single-token names work too.
        let fresh = merge_planner_output(
            None,
            "make a plan called sprint_report, for the team",
            output(),
        );
        assert_eq!(fresh.identifier, "sprint_report");
        assert_eq!(fresh.name, "sprint_report");

        // "renamed" is not a naming marker: prose fallback.
        let fresh = merge_planner_output(None, "List files renamed last week", output());
        assert_eq!(fresh.identifier, "list_files_renamed_last_week");
    }
}
