//! Workbench-local agent tools: the chat agent builds and edits the draft
//! plan through these, so the plan pane is always the agent's source of
//! truth. Registered under `workbench__` alongside the normal catalog.

use super::app::Msg;
use super::runner::{DebugControls, UiGate, UiInterlocutor};
use async_trait::async_trait;
use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::{apply_schema_defaults, load_plan_doc, validate_input, PlanDoc};
use graph_core::pipeline::{Pipeline, PlannerOutput};
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub const DRAFT_PLAN: &str = "workbench__draft_plan";
pub const GET_PLAN: &str = "workbench__get_plan";
pub const LOAD_PLAN: &str = "workbench__load_plan";
pub const LIST_PLANS: &str = "workbench__list_plans";
pub const VALIDATE_PLAN: &str = "workbench__validate_plan";
pub const RUN_PLAN: &str = "workbench__run_plan";
pub const SAVE_PLAN: &str = "workbench__save_plan";
pub const UPDATE_METADATA: &str = "workbench__update_metadata";
pub const ADD_STEP: &str = "workbench__add_step";
pub const UPDATE_STEP: &str = "workbench__update_step";
pub const DELETE_STEP: &str = "workbench__delete_step";
pub const RESTORE_DRAFT: &str = "workbench__restore_draft";
pub const SHOW_PLAN: &str = "workbench__show_plan";

/// The mutating edit tools — a successful call to one is genuine forward
/// progress on the draft. The agent loop resets its iteration budget on these
/// (see `Agent::progress_tools`) so a long fix-forward session (edit, validate,
/// run, repeat) isn't starved mid-repair.
pub fn progress_tools() -> Vec<String> {
    [
        DRAFT_PLAN,
        UPDATE_METADATA,
        ADD_STEP,
        UPDATE_STEP,
        DELETE_STEP,
        RESTORE_DRAFT,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Appended to every workbench__ tool description surfaced to the chat
/// agent: these are agent-side tools, not runtime tools, so a plan step
/// referencing one only fails at run time. Descriptions are routing
/// signals — this keeps them out of drafted steps.
pub(crate) const WORKBENCH_ONLY_NOTE: &str =
    " Workbench-only: not available to plan steps at runtime.";

/// The shared draft: the doc, its unsaved-changes flag, and a one-deep undo
/// snapshot. One mutex holds all three so a tool can check `dirty`
/// atomically with replacing the doc — the agent runs a batch's tool calls
/// concurrently, so a check across two locks would race.
pub struct DraftState {
    pub doc: Option<PlanDoc>,
    pub dirty: bool,
    /// The (doc, dirty) displaced by the last replacement. `restore` swaps
    /// it with the current draft, so calling it twice is redo.
    pub undo: Option<(PlanDoc, bool)>,
}

impl DraftState {
    pub fn new(doc: Option<PlanDoc>) -> Self {
        Self {
            doc,
            dirty: false,
            undo: None,
        }
    }

    /// Swap the current draft with the undo snapshot; returns the restored
    /// (doc, dirty), or None when nothing has been replaced yet.
    pub fn restore(&mut self) -> Option<(PlanDoc, bool)> {
        let (doc, dirty) = self.undo.take()?;
        self.undo = self.doc.take().map(|old| (old, self.dirty));
        self.doc = Some(doc.clone());
        self.dirty = dirty;
        Some((doc, dirty))
    }
}

pub type SharedDraft = Arc<Mutex<DraftState>>;

pub struct WorkbenchTools {
    draft: SharedDraft,
    pipeline: Arc<Pipeline>,
    plans_dir: Option<PathBuf>,
    debug: Arc<DebugControls>,
    tx: UnboundedSender<Msg>,
}

impl WorkbenchTools {
    pub fn new(
        draft: SharedDraft,
        pipeline: Arc<Pipeline>,
        plans_dir: Option<PathBuf>,
        debug: Arc<DebugControls>,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        Self {
            draft,
            pipeline,
            plans_dir,
            debug,
            tx,
        }
    }

    fn current(&self) -> Option<PlanDoc> {
        self.draft.lock().unwrap().doc.clone()
    }

    /// Replace the draft, stashing the displaced doc as the undo snapshot.
    /// Deliberately unguarded: the edit path (draft_plan, the
    /// precise tools) preserves unsaved work by setting dirty=true, and any
    /// bad replacement is one restore away. Only load_plan needs the dirty
    /// guard, and it does its check-and-replace under the same lock.
    fn publish(&self, doc: PlanDoc, dirty: bool) {
        {
            let mut state = self.draft.lock().unwrap();
            state.undo = state.doc.take().map(|old| (old, state.dirty));
            state.doc = Some(doc.clone());
            state.dirty = dirty;
        }
        let _ = self.tx.send(Msg::DraftReplaced {
            doc: Box::new(doc),
            dirty,
        });
    }

    /// Resolve a catalog identifier or YAML file path to a plan document.
    fn resolve_plan(&self, name_or_path: &str) -> Result<PlanDoc, ToolOutcome> {
        if let Some(doc) = self
            .pipeline
            .plans
            .iter()
            .find(|d| d.identifier == name_or_path)
        {
            return Ok(doc.clone());
        }
        let path = std::path::Path::new(name_or_path);
        if !path.exists() {
            let available: Vec<&str> = self
                .pipeline
                .plans
                .iter()
                .map(|d| d.identifier.as_str())
                .collect();
            return Err(ToolOutcome {
                result: json!({
                    "error": format!(
                        "'{name_or_path}' is neither a known plan identifier nor a file"
                    ),
                    "availablePlans": available,
                }),
                is_error: true,
            });
        }
        load_plan_doc(path).map_err(|error| {
            let mut message = format!("failed to load plan: {error}");
            if error.to_string().contains("unknown field") {
                message.push_str(
                    "\nhint: control flow is not a field — it is a step whose \
                     toolName is one of the bare control steps exit, agent, \
                     ask, decide, filter, map, or reduce (there is no \
                     gate/assert tool); a \
                     plan finishes with `solver` OR `output`, never both",
                );
            }
            error_outcome(&message)
        })
    }

    /// Read a plan's YAML without touching the draft — the inspection
    /// counterpart to load_plan, so studying a plan never replaces work.
    fn show_plan(&self, input: &Value) -> ToolOutcome {
        let Some(name_or_path) = input.get("name_or_path").and_then(Value::as_str) else {
            return error_outcome("show_plan requires a 'name_or_path' string");
        };
        let doc = match self.resolve_plan(name_or_path) {
            Ok(doc) => doc,
            Err(outcome) => return outcome,
        };
        match authoring::to_yaml(&doc) {
            Ok(yaml) => ToolOutcome {
                result: json!({"identifier": doc.identifier, "name": doc.name, "yaml": yaml}),
                is_error: false,
            },
            Err(error) => error_outcome(&error.to_string()),
        }
    }

    /// Load an existing plan into the workbench: an identifier from the
    /// configured plan catalog, or a YAML file path.
    fn load_plan(&self, input: &Value) -> ToolOutcome {
        let Some(name_or_path) = input.get("name_or_path").and_then(Value::as_str) else {
            return error_outcome("load_plan requires a 'name_or_path' string");
        };
        let doc = match self.resolve_plan(name_or_path) {
            Ok(doc) => doc,
            Err(outcome) => return outcome,
        };
        let problems = plan_problems(&self.pipeline, &doc);
        let summary = json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        // Check-and-replace under one lock: concurrent same-batch loads
        // each see the true dirty state, so unsaved work is never lost
        // without an explicit overwrite.
        let overwrite = input
            .get("overwrite_draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        {
            let mut state = self.draft.lock().unwrap();
            if state.dirty && !overwrite {
                return ToolOutcome {
                    result: json!({
                        "error": "the draft has unsaved changes — save them with \
                                  workbench__save_plan, or pass overwrite_draft: true \
                                  only after the user confirms discarding them",
                        "dirtyDraft": state.doc.as_ref().map(|d| d.identifier.clone()),
                    }),
                    is_error: true,
                };
            }
            state.undo = state.doc.take().map(|old| (old, state.dirty));
            state.doc = Some(doc.clone());
            state.dirty = false;
        }
        let _ = self.tx.send(Msg::DraftReplaced {
            doc: Box::new(doc),
            dirty: false,
        });
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    /// One-level undo of the last draft replacement; calling it again redoes.
    fn restore_draft(&self) -> ToolOutcome {
        let restored = self.draft.lock().unwrap().restore();
        match restored {
            Some((doc, dirty)) => {
                let result = json!({"restored": doc.identifier, "dirty": dirty});
                let _ = self.tx.send(Msg::DraftReplaced {
                    doc: Box::new(doc),
                    dirty,
                });
                ToolOutcome {
                    result,
                    is_error: false,
                }
            }
            None => error_outcome("nothing to restore — the draft has not been replaced yet"),
        }
    }

    /// The plan catalog, as the workspace context tab sees it.
    fn list_plans(&self) -> ToolOutcome {
        let plans: Vec<Value> = self
            .pipeline
            .plans
            .iter()
            .map(|doc| {
                json!({
                    "identifier": doc.identifier,
                    "name": doc.name,
                    "description": doc.description,
                    "steps": doc.steps.len(),
                })
            })
            .collect();
        ToolOutcome {
            result: json!({"count": plans.len(), "plans": plans}),
            is_error: false,
        }
    }

    /// Validate the draft and surface the verdict in the plan pane.
    fn validate_plan(&self) -> ToolOutcome {
        let Some(doc) = self.current() else {
            return error_outcome("no draft to validate");
        };
        let problems = plan_problems(&self.pipeline, &doc);
        let _ = self.tx.send(Msg::Validated(problems.clone()));
        ToolOutcome {
            result: json!({"valid": problems.is_empty(), "problems": problems}),
            is_error: false,
        }
    }

    /// Run the draft inside this agent turn. Step events stream to the
    /// workspace pane; gated runs pause on the USER's y/s/a decisions.
    async fn run_plan(&self, input: &Value) -> ToolOutcome {
        let Some(doc) = self.current() else {
            return error_outcome("no draft to run — load or draft one first");
        };
        let breakpoints: Vec<String> = input
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Breakpoints imply debugging.
        let gated =
            input.get("gated").and_then(Value::as_bool).unwrap_or(false) || !breakpoints.is_empty();
        let unknown_breakpoints: Vec<&String> = breakpoints
            .iter()
            .filter(|id| !doc.steps.iter().any(|s| &s.id == *id))
            .collect();
        let mut run_input = input
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        if !run_input.is_object() {
            return error_outcome("'input' must be a JSON object of plan inputs");
        }
        if let Some(schema) = &doc.input_schema {
            apply_schema_defaults(schema, &mut run_input);
            if let Err(problems) = validate_input(&doc, &run_input) {
                return ToolOutcome {
                    result: json!({
                        "error": "invalid or missing plan inputs",
                        "problems": problems,
                        "inputSchema": schema,
                    }),
                    is_error: true,
                };
            }
        }
        let provided = !breakpoints.is_empty();
        if gated {
            if provided {
                self.debug
                    .set_breakpoints(breakpoints.iter().cloned().collect());
            }
            self.debug.arm();
        }
        let _ = self.tx.send(Msg::RunStarted {
            gated,
            breakpoints: provided.then(|| breakpoints.clone()),
        });
        // The interlocutor rides every run, gated or not: an `ask` step's
        // question is the plan asking for input, not a debugger pause.
        let mut pipeline = (*self.pipeline)
            .clone()
            .with_interlocutor(Arc::new(UiInterlocutor::new(self.tx.clone())));
        if gated {
            pipeline =
                pipeline.with_gate(Arc::new(UiGate::new(self.tx.clone(), self.debug.clone())));
        }
        let query = format!("Run the '{}' plan", doc.name);
        let result = pipeline
            .run_explicit(&query, doc.steps.clone(), doc.finish(), Some(run_input))
            .await;
        let report = super::runner::report(result);
        let is_error = report.is_error;
        let _ = self.tx.send(report.finished_msg());
        let mut summary = report.summary;
        if !unknown_breakpoints.is_empty() {
            summary["unknownBreakpoints"] = json!(unknown_breakpoints);
        }
        ToolOutcome {
            result: summary,
            is_error,
        }
    }

    /// Save the draft to disk and surface the result in the status bar.
    fn save_plan(&self) -> ToolOutcome {
        let result = super::effects::save_draft(&self.draft, self.plans_dir.as_deref());
        let _ = self.tx.send(Msg::Saved(result.clone()));
        match result {
            Ok(path) => ToolOutcome {
                result: json!({"savedTo": path}),
                is_error: false,
            },
            Err(error) => error_outcome(&error),
        }
    }

    async fn draft_plan(&self, input: &Value) -> ToolOutcome {
        let Some(goal) = input.get("goal").and_then(Value::as_str) else {
            return error_outcome("draft_plan requires a 'goal' string");
        };
        // fresh: the goal describes a NEW plan — ignore the current draft
        // entirely, so an unrelated loaded plan isn't treated as the plan
        // being drafted into (which would keep its identifier and metadata).
        let fresh = input.get("fresh").and_then(Value::as_bool).unwrap_or(false);
        let existing = if fresh { None } else { self.current() };
        let existing_output = existing.as_ref().map(|doc| PlannerOutput {
            plan: doc.steps.clone(),
            solver_data: doc.solver.clone(),
        });
        // Drafting validates every step before accepting it, so a draft
        // that comes back at all is statically valid; only catalog
        // problems (reported below) can remain.
        let output = match self
            .pipeline
            .draft_plan(goal, existing_output.as_ref())
            .await
        {
            Ok(output) => output,
            // Drafting exhausted its retries: salvage the valid prefix so
            // the agent finishes it with the edit tools instead of
            // redrafting from scratch.
            Err(graph_core::pipeline::PipelineError::DraftStepExhausted {
                step_id,
                problems,
                partial,
                ..
            }) => {
                let doc = authoring::merge_planner_output(existing, goal, *partial);
                let steps = doc.steps.len();
                self.publish(doc, true);
                return ToolOutcome {
                    result: json!({
                        "error": format!(
                            "drafting could not produce a valid step \
                             {step_id}; the valid partial draft \
                             ({steps} steps) has been published"
                        ),
                        "failedStep": step_id,
                        "problems": problems,
                        "note": "finish the plan with the editing tools \
                                 (workbench__add_step, workbench__update_step) \
                                 instead of redrafting",
                    }),
                    is_error: true,
                };
            }
            Err(error) => return error_outcome(&format!("planner failed: {error}")),
        };

        let doc = authoring::merge_planner_output(existing, goal, output);
        let problems = plan_problems(&self.pipeline, &doc);
        let mut summary = json!({
            "identifier": doc.identifier,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        if !problems.is_empty() {
            summary["note"] = json!(
                "the draft has catalog problems — fix them with the \
                 editing tools"
            );
        }
        self.publish(doc, true);
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    fn get_plan(&self) -> ToolOutcome {
        match self.current() {
            Some(doc) => match authoring::to_yaml(&doc) {
                Ok(yaml) => ToolOutcome {
                    result: json!({"yaml": yaml}),
                    is_error: false,
                },
                Err(error) => error_outcome(&error.to_string()),
            },
            None => ToolOutcome {
                result: json!({"yaml": null, "note": "no draft yet — use workbench__draft_plan"}),
                is_error: false,
            },
        }
    }

    /// The choke point for the precise editing tools. The edit rules
    /// (reject only what introduces NEW problems) live in
    /// [`authoring::apply_edit`]; this wrapper adds the workbench's own
    /// concerns — there must *be* a draft, and an accepted edit publishes
    /// to the UI and marks the draft dirty.
    fn edit_draft(&self, mutate: impl FnOnce(&mut PlanDoc) -> Result<Value, Value>) -> ToolOutcome {
        let Some(doc) = self.current() else {
            return error_outcome("no draft — load or draft one first");
        };
        match authoring::apply_edit(&doc, mutate) {
            Ok(accepted) => {
                let summary = authoring::summarize_edit(&accepted);
                self.publish(accepted.doc, true);
                ToolOutcome {
                    result: summary,
                    is_error: false,
                }
            }
            Err(rejected) => ToolOutcome {
                result: rejected.body,
                is_error: true,
            },
        }
    }

    /// Patch the draft's plan-level fields: identifier, name, description,
    /// exemplars, and/or the finish type (solver ⇄ output).
    fn update_metadata(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_metadata(doc, input))
    }

    /// Insert a step: appended, or anchored before/after an existing id.
    fn add_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_add_step(doc, input))
    }

    /// Patch one step's fields; `newId` renames it and rewrites downstream
    /// `{{id.*}}` references so templates keep working.
    fn update_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_update_step(doc, input))
    }

    /// Remove a step. Validation rejects the edit if later steps still
    /// reference it — the problems say which templates dangle.
    fn delete_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_delete_step(doc, input))
    }
}

/// The workbench's full validation verdict for a draft: the document's own
/// problems plus catalog-aware tool resolution against the pipeline's
/// catalog. See [`authoring::plan_problems`] for why the catalog is
/// reporting-only and never gates an edit.
pub(super) fn plan_problems(pipeline: &Pipeline, doc: &PlanDoc) -> Vec<String> {
    authoring::plan_problems(doc, &pipeline.plans, pipeline.catalog.as_deref())
}

fn error_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        result: json!({"error": message}),
        is_error: true,
    }
}

#[async_trait]
impl ToolRegistry for WorkbenchTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut defs = vec![
            ToolDef {
                name: DRAFT_PLAN.to_string(),
                description: "Draft the workbench's plan from a goal, when there is no draft \
                              yet or the user asks to start over. The planner sees the full \
                              tool catalog; pass the user's request as a self-contained \
                              `goal`. Pass fresh: true when the goal describes a NEW plan — \
                              otherwise the current draft's identifier and metadata are \
                              kept and its steps are drafted again. This ALWAYS replaces \
                              every step, so never call it to change a draft that already \
                              has steps worth keeping: use workbench__update_step / \
                              add_step / update_metadata, which apply one change each and \
                              are rejected if they would break the plan. Each step is \
                              validated as it is drafted; any remaining problems are \
                              returned in `validation`."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["goal"],
                    "properties": {
                        "goal": {"type": "string", "description": "What the plan should accomplish, self-contained."},
                        "fresh": {"type": "boolean", "description": "Draft a NEW plan from scratch, ignoring the current draft (which otherwise keeps its identifier and metadata). Default false."}
                    }
                }),
                output_schema: None,
                output_example: Some(
                    json!({"identifier": "sprint_report", "steps": 3, "validation": "ok"}),
                ),
                read_only: None,
            },
            ToolDef {
                name: LOAD_PLAN.to_string(),
                description: "Open a DIFFERENT plan the user explicitly names — by \
                              identifier from the plan catalog, or by YAML file path. \
                              Never use it to edit, fix, or continue the current draft; \
                              use the editing tools for that. Replaces the current \
                              draft, and FAILS if the draft has unsaved changes unless \
                              overwrite_draft is true."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name_or_path"],
                    "properties": {
                        "name_or_path": {"type": "string", "description": "A plan identifier (e.g. sprint_analysis) or a path to a plan YAML file."},
                        "overwrite_draft": {"type": "boolean", "description": "Required (true) to load over a draft with unsaved changes. Only pass it after the user explicitly confirms discarding them. Default false."}
                    }
                }),
                output_schema: None,
                output_example: Some(
                    json!({"identifier": "sprint_analysis", "name": "Sprint analysis", "steps": 4, "validation": "ok"}),
                ),
                read_only: Some(true),
            },
            ToolDef {
                name: GET_PLAN.to_string(),
                description: "Re-read the current draft plan as YAML. The workbench \
                              includes the current draft in the system prompt every \
                              turn, so call this only to re-check the draft after your \
                              own edits within the same turn."
                    .to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            },
            ToolDef {
                name: LIST_PLANS.to_string(),
                description: "List the plans available in the catalog: identifier, name, \
                              description, and step count. Use this when the user asks what \
                              plans exist or which one to load."
                    .to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: None,
                output_example: Some(
                    json!({"count": 1, "plans": [{"identifier": "sprint_analysis", "name": "Sprint analysis", "description": "…", "steps": 4}]}),
                ),
                read_only: Some(true),
            },
            ToolDef {
                name: VALIDATE_PLAN.to_string(),
                description: "Validate the current draft (templates, reference ordering, \
                              control-step bodies, document structure). Returns every \
                              problem and updates the plan pane's verdict."
                    .to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: None,
                output_example: Some(
                    json!({"valid": false, "problems": ["step E1 references E5, which is not an earlier step"]}),
                ),
                read_only: Some(true),
            },
            ToolDef {
                name: RUN_PLAN.to_string(),
                description: "Execute the current draft. Step activity streams to the \
                              workspace pane. Set gated=true for a debug run — it pauses \
                              for the USER's decision (step / continue / skip / abort) and \
                              breaks on any failing call; pass `breakpoints` (top-level \
                              step ids, implies gated) to run freely to those steps. \
                              Prefer debugging for plans with side effects, and only run \
                              when the user asks. Pass `input` when the plan declares an \
                              input_schema."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "gated": {"type": "boolean", "description": "Debug run: pause for the user's decisions and break on errors. Default false."},
                        "breakpoints": {"type": "array", "items": {"type": "string"}, "description": "Top-level step ids to pause at. Implies gated; the run auto-proceeds until a breakpoint (or a failing call) and pauses for the USER."},
                        "input": {"type": "object", "description": "The plan's input object, validated against its input_schema."}
                    }
                }),
                output_schema: None,
                output_example: Some(
                    json!({"status": "completed", "stepsExecuted": 3, "output": {"…": "…"}}),
                ),
                read_only: None,
            },
            ToolDef {
                name: SAVE_PLAN.to_string(),
                description: "Save the current draft to disk as YAML — back to the file it \
                              was loaded from, or into the plans directory for new drafts."
                    .to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: None,
                output_example: Some(json!({"savedTo": "./.graph/plans/sprint_report.yaml"})),
                read_only: None,
            },
            ToolDef {
                name: UPDATE_METADATA.to_string(),
                description: "Update the draft's plan-level fields: identifier, name, \
                              description, exemplars, input_schema, requires_servers, \
                              and/or the finish type. `input_schema` declares the plan's \
                              inputs; `requires_servers` lists the MCP servers it needs. \
                              `finish` sets how the plan produces its result — {solver: \
                              {queryToAnswer, systemPrompt?}} for LLM synthesis of the \
                              step results, or {output: {<template map>}} for a structured \
                              templated result — solver and output are mutually exclusive, \
                              and {} / null clears both for a silent side-effect plan. \
                              Changing the identifier makes it a new plan."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "identifier": {"type": "string", "description": "Tool-name-safe identifier (letters, digits, _, -). Changing it detaches the draft from its loaded file."},
                        "name": {"type": "string", "description": "Human-readable display name."},
                        "description": {"type": "string", "description": "What the plan does — shown in the catalog and used for routing."},
                        "exemplars": {"type": "array", "items": {"type": "string"}, "description": "Example requests the plan should handle; replaces the current list."},
                        "input_schema": {"type": "object", "description": "Replace the plan's input schema (a JSON Schema object describing the plan's inputs, e.g. {\"type\":\"object\",\"required\":[\"pr\"],\"properties\":{\"pr\":{\"type\":\"integer\"}}}). Pass null to clear it."},
                        "requires_servers": {"type": "array", "items": {"type": "string"}, "description": "Replace the list of MCP server names the plan requires to be configured (gates catalog visibility). Empty array clears it."},
                        "finish": {
                            "type": "object",
                            "description": "Set the plan's finish type. Provide EITHER 'solver' {queryToAnswer: string, systemPrompt?: string} for LLM synthesis, OR 'output' {a map of key -> template string} for a structured result. Mutually exclusive.",
                            "properties": {
                                "solver": {"type": "object", "properties": {"queryToAnswer": {"type": "string"}, "systemPrompt": {"type": "string"}}},
                                "output": {"type": "object", "description": "Map of output key to template string, e.g. {\"summary\": \"{{E3.text}}\"}."}
                            }
                        }
                    }
                }),
                output_schema: None,
                output_example: Some(
                    json!({"ok": true, "identifier": "sprint_report", "name": "Sprint report"}),
                ),
                read_only: None,
            },
            ToolDef {
                name: ADD_STEP.to_string(),
                description: "Insert one top-level step into the draft — appended, or \
                              anchored with `before`/`after` an existing step id. The \
                              edit is rejected (draft unchanged) only if it introduces \
                              NEW validation problems, e.g. a duplicate id or a \
                              reference to a later step; pre-existing problems never \
                              block it and are reported in the result. Steps inside \
                              decide/map/reduce bodies live in the control step's \
                              input — use update_step on that step instead."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["step"],
                    "properties": {
                        "step": {
                            "type": "object",
                            "required": ["id", "toolName", "input"],
                            "properties": {
                                "id": {"type": "string", "description": "Unique step id templates reference: E-style (E4) or descriptive (fetch_issues)."},
                                "toolName": {"type": "string", "description": "Exact tool name from the catalog."},
                                "input": {"type": "object", "description": "Tool input; string values may reference earlier steps with {{id.path}} templates."},
                                "reasoning": {"type": "string", "description": "Why this step exists and what it should produce."}
                            }
                        },
                        "before": {"type": "string", "description": "Insert before this existing step id."},
                        "after": {"type": "string", "description": "Insert after this existing step id. Omit both anchors to append."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({"ok": true, "id": "E4", "index": 2, "steps": 5})),
                read_only: None,
            },
            ToolDef {
                name: UPDATE_STEP.to_string(),
                description: "Update fields of one top-level step: toolName, input \
                              (replaced whole, not merged), reasoning (empty string \
                              clears it), and/or newId to rename the step — renaming \
                              rewrites {{id.*}} references in later steps, the solver, \
                              and the output so templates keep working. Rejected only \
                              when the edit introduces NEW validation problems; \
                              pre-existing ones are reported, not blocking. For steps \
                              inside decide/map/reduce bodies, update the owning \
                              control step's input."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": {"type": "string", "description": "The step to update."},
                        "newId": {"type": "string", "description": "Rename the step; downstream template references are rewritten."},
                        "toolName": {"type": "string", "description": "New tool name from the catalog."},
                        "input": {"type": "object", "description": "New tool input — replaces the step's entire input object."},
                        "reasoning": {"type": "string", "description": "New reasoning; an empty string clears it."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({"ok": true, "id": "fetch_issues"})),
                read_only: None,
            },
            ToolDef {
                name: DELETE_STEP.to_string(),
                description: "Delete one top-level step from the draft. Rejected \
                              (draft unchanged) if later steps still reference it — \
                              update those steps first — or if it is the plan's only \
                              step."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": {"type": "string", "description": "The step to delete."}
                    }
                }),
                output_schema: None,
                output_example: Some(json!({"ok": true, "id": "E2", "steps": 3})),
                read_only: None,
            },
            ToolDef {
                name: SHOW_PLAN.to_string(),
                description: "Read a catalog plan's YAML without touching the draft — by \
                              identifier or YAML file path. Use this to inspect or \
                              reference existing plans; use load_plan only to switch the \
                              draft to a plan the user names."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name_or_path"],
                    "properties": {
                        "name_or_path": {"type": "string", "description": "A plan identifier (e.g. sprint_analysis) or a path to a plan YAML file."}
                    }
                }),
                output_schema: None,
                output_example: Some(
                    json!({"identifier": "sprint_analysis", "name": "Sprint analysis", "yaml": "identifier: sprint_analysis\n…"}),
                ),
                read_only: Some(true),
            },
            ToolDef {
                name: RESTORE_DRAFT.to_string(),
                description: "One-level undo: put the draft back to what it was before \
                              the last replacement (load, draft, or edit) — use it \
                              when you or the user replaced the draft by mistake. \
                              Calling it again redoes."
                    .to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: None,
                output_example: Some(json!({"restored": "sprint_report", "dirty": true})),
                read_only: None,
            },
        ];
        for def in &mut defs {
            def.description.push_str(WORKBENCH_ONLY_NOTE);
        }
        Ok(defs)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        match name {
            DRAFT_PLAN | GET_PLAN | LOAD_PLAN | LIST_PLANS | VALIDATE_PLAN | RUN_PLAN
            | SAVE_PLAN | UPDATE_METADATA | ADD_STEP | UPDATE_STEP | DELETE_STEP
            | RESTORE_DRAFT | SHOW_PLAN => {}
            // Not ours: stay silent, or the composite registry's fallthrough
            // (the fs tools are also workbench__*) double-logs the call.
            other => return Err(ToolError::Unknown(other.to_string())),
        }
        tracing::debug!(
            target: "workbench",
            "agent invoked {name}: {}",
            input.to_string()
        );
        let started = std::time::Instant::now();
        let outcome = match name {
            DRAFT_PLAN => Ok(self.draft_plan(&input).await),
            GET_PLAN => Ok(self.get_plan()),
            LOAD_PLAN => Ok(self.load_plan(&input)),
            LIST_PLANS => Ok(self.list_plans()),
            VALIDATE_PLAN => Ok(self.validate_plan()),
            RUN_PLAN => Ok(self.run_plan(&input).await),
            SAVE_PLAN => Ok(self.save_plan()),
            UPDATE_METADATA => Ok(self.update_metadata(&input)),
            ADD_STEP => Ok(self.add_step(&input)),
            UPDATE_STEP => Ok(self.update_step(&input)),
            DELETE_STEP => Ok(self.delete_step(&input)),
            RESTORE_DRAFT => Ok(self.restore_draft()),
            SHOW_PLAN => Ok(self.show_plan(&input)),
            other => Err(ToolError::Unknown(other.to_string())),
        };
        if let Ok(outcome) = &outcome {
            tracing::debug!(
                target: "workbench",
                "{name} finished in {:.1}s (is_error={}): {}",
                started.elapsed().as_secs_f64(),
                outcome.is_error,
                outcome.result.to_string()
            );
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pipeline(plans: Vec<PlanDoc>) -> Arc<Pipeline> {
        Arc::new(Pipeline {
            router: Arc::new(graph_llm::ModelRouter::with_providers(
                Default::default(),
                Default::default(),
            )),
            registry: Arc::new(graph_core::CompositeRegistry::new(vec![])),
            events: Arc::new(graph_core::NullSink),
            plans: Arc::new(plans),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
            usage: std::sync::Arc::new(graph_core::usage::UsageLedger::unpriced()),
        })
    }

    fn demo_doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap()
    }

    #[test]
    fn load_plan_by_identifier_publishes_a_clean_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let draft: SharedDraft = Arc::new(Mutex::new(DraftState::new(None)));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = tools.load_plan(&json!({"name_or_path": "demo"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("demo"));
        assert_eq!(outcome.result["validation"], json!("ok"));
        assert!(draft.lock().unwrap().doc.is_some());
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.identifier, "demo");
                assert!(!dirty, "a load is not an unsaved edit");
            }
            _ => panic!("expected DraftReplaced"),
        }
    }

    #[test]
    fn validate_plan_resolves_tools_against_the_runtime_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut doc = demo_doc();
        doc.steps[0].tool_name = "user__nope".to_string();
        // The catalog is the runtime-loadable one — empty here, so the
        // user tool cannot resolve. workbench__* never appears in it.
        let mut pipeline = (*test_pipeline(vec![])).clone();
        pipeline.catalog = Some(Arc::new(graph_core::pipeline::ToolCatalog::default()));
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(doc)))),
            Arc::new(pipeline),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.validate_plan();
        assert_eq!(outcome.result["valid"], json!(false));
        assert!(
            outcome.result["problems"]
                .to_string()
                .contains("user__nope"),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn load_plan_unknown_name_lists_available_plans() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.load_plan(&json!({"name_or_path": "nope"}));
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availablePlans"], json!(["demo"]));
    }

    fn other_doc() -> PlanDoc {
        let mut other = demo_doc();
        other.identifier = "other_plan".to_string();
        other
    }

    #[test]
    fn show_plan_reads_yaml_without_touching_the_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = tools.show_plan(&json!({"name_or_path": "other_plan"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("other_plan"));
        assert!(outcome.result["yaml"]
            .as_str()
            .unwrap()
            .contains("identifier: other_plan"));
        assert!(rx.try_recv().is_err(), "a peek must not publish");
        let state = draft.lock().unwrap();
        assert_eq!(
            state.doc.as_ref().unwrap().identifier,
            "demo",
            "the draft is untouched"
        );
        assert!(state.dirty, "the dirty flag is untouched");

        let unknown = tools.show_plan(&json!({"name_or_path": "nope"}));
        assert!(unknown.is_error);
        assert_eq!(unknown.result["availablePlans"], json!(["other_plan"]));
    }

    #[test]
    fn load_plan_refuses_to_replace_a_dirty_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = tools.load_plan(&json!({"name_or_path": "other_plan"}));
        assert!(outcome.is_error);
        assert!(
            outcome.result["error"]
                .as_str()
                .unwrap()
                .contains("unsaved changes"),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.result["dirtyDraft"], json!("demo"));
        assert!(rx.try_recv().is_err(), "a refused load must not publish");
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().identifier,
            "demo",
            "the dirty draft survives"
        );
    }

    #[test]
    fn load_plan_overwrite_flag_replaces_a_dirty_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome =
            tools.load_plan(&json!({"name_or_path": "other_plan", "overwrite_draft": true}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.identifier, "other_plan");
                assert!(!dirty);
            }
            _ => panic!("expected DraftReplaced"),
        }
        let state = draft.lock().unwrap();
        assert_eq!(state.doc.as_ref().unwrap().identifier, "other_plan");
        let (undone, was_dirty) = state.undo.as_ref().unwrap();
        assert_eq!(
            undone.identifier, "demo",
            "the overwritten draft is one undo away"
        );
        assert!(*was_dirty);
    }

    #[test]
    fn list_plans_enumerates_the_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.list_plans();
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["count"], json!(1));
        assert_eq!(outcome.result["plans"][0]["identifier"], json!("demo"));
        assert_eq!(outcome.result["plans"][0]["steps"], json!(1));
    }

    #[test]
    fn validate_plan_reports_problems_and_updates_the_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut doc = demo_doc();
        doc.steps[0].input.insert(
            "bad".to_string(),
            Value::String("{{E5.values}}".to_string()),
        );
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(doc)))),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.validate_plan();
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["valid"], json!(false));
        assert!(outcome.result["problems"][0]
            .as_str()
            .unwrap()
            .contains("E5"));
        assert!(matches!(rx.try_recv().unwrap(), Msg::Validated(p) if p.len() == 1));
    }

    #[test]
    fn save_plan_writes_yaml_and_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(demo_doc())))),
            test_pipeline(vec![]),
            Some(dir.path().to_path_buf()),
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.save_plan();
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(dir.path().join("demo.yaml").exists());
        assert!(matches!(rx.try_recv().unwrap(), Msg::Saved(Ok(_))));
    }

    #[test]
    fn save_refuses_to_overwrite_a_file_holding_another_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        std::fs::write(&path, authoring::to_yaml(&demo_doc()).unwrap()).unwrap();

        // A draft whose identity drifted from the file its path points at
        // (the pre-fix bug state) must not clobber that file on save.
        let mut drifted = demo_doc();
        drifted.identifier = "other_plan".to_string();
        drifted.path = Some(path.clone());
        let draft = Mutex::new(DraftState::new(Some(drifted)));
        let error = super::super::effects::save_draft(&draft, Some(dir.path())).unwrap_err();
        assert!(error.contains("refusing to overwrite"), "{error}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("identifier: demo"), "file was clobbered");
    }

    #[test]
    fn run_plan_missing_required_input_errors_with_schema() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut doc = demo_doc();
        doc.input_schema = Some(json!({
            "type": "object",
            "required": ["team"],
            "properties": {"team": {"type": "string"}}
        }));
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(doc)))),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = futures::executor::block_on(tools.run_plan(&json!({})));
        assert!(outcome.is_error);
        assert!(outcome.result["inputSchema"].is_object());
        assert!(rx.try_recv().is_err(), "no run should have started");
    }

    /// Three steps with cross-references plus solver templates — the
    /// fixture for the precise editing tools.
    fn referencing_doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E1
    tool_name: t__fetch
    input: { id: "{{E0.values.0.id}}" }
  - id: E2
    tool_name: t__report
    input: { rows: "{{#E1.values}}x{{/E1.values}} of {{E1.values.length}}" }
solver:
  queryToAnswer: "Summarize {{E1.values.length}} items"
"#,
        )
        .unwrap()
    }

    fn editing_tools(
        doc: Option<PlanDoc>,
    ) -> (
        WorkbenchTools,
        SharedDraft,
        tokio::sync::mpsc::UnboundedReceiver<Msg>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let draft = Arc::new(Mutex::new(DraftState::new(doc)));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        (tools, draft, rx)
    }

    fn assert_dirty_publish(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>) {
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { dirty, .. } => assert!(dirty, "edits are unsaved changes"),
            _ => panic!("expected DraftReplaced"),
        }
    }

    #[test]
    fn update_metadata_patches_fields_and_publishes_dirty() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome =
            tools.update_metadata(&json!({"name": "Better name", "description": "clearer"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["name"], json!("Better name"));
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.name, "Better name");
        assert_eq!(doc.description, "clearer");
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn update_metadata_identifier_change_drops_the_loaded_path() {
        let mut loaded = referencing_doc();
        loaded.path = Some(PathBuf::from("/plans/demo.yaml"));
        let (tools, draft, _rx) = editing_tools(Some(loaded));

        // Renaming (display name) keeps the on-disk identity…
        assert!(!tools.update_metadata(&json!({"name": "Renamed"})).is_error);
        assert!(draft.lock().unwrap().doc.as_ref().unwrap().path.is_some());

        // …but a new identifier is a different plan: the path is dropped
        // so the next save cannot overwrite the old file.
        assert!(
            !tools
                .update_metadata(&json!({"identifier": "other_plan"}))
                .is_error
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().path, None);
    }

    #[test]
    fn update_metadata_with_nothing_to_change_errors() {
        let (tools, _draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({}));
        assert!(outcome.is_error);
        assert!(rx.try_recv().is_err(), "nothing should have been published");
    }

    #[test]
    fn update_metadata_switches_finish_type_both_directions() {
        // referencing_doc finishes with solver; switch it to output.
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome =
            tools.update_metadata(&json!({"finish": {"output": {"summary": "{{E0.text}}"}}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver.is_none(), "solver should be cleared");
        let output = doc.output.expect("output should be set");
        assert!(output.contains_key("summary"));
        assert_dirty_publish(&mut rx);

        // Reverse: from an output finish back to a solver.
        let outcome =
            tools.update_metadata(&json!({"finish": {"solver": {"queryToAnswer": "answer it"}}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.output.is_none(), "output should be cleared");
        let solver = doc.solver.expect("solver should be set");
        assert_eq!(solver.query_to_answer, "answer it");
    }

    #[test]
    fn update_metadata_finish_rejects_both_and_neither() {
        let (tools, _draft, _rx) = editing_tools(Some(referencing_doc()));

        let both = tools.update_metadata(&json!({
            "finish": {"solver": {"queryToAnswer": "x"}, "output": {"k": "v"}}
        }));
        assert!(both.is_error);
        assert!(
            both.result["error"].as_str().unwrap().contains("not both"),
            "{:?}",
            both.result
        );

        // A non-empty finish object lacking both keys is malformed.
        let malformed = tools.update_metadata(&json!({"finish": {"bogus": 1}}));
        assert!(malformed.is_error);
        let message = malformed.result["error"].as_str().unwrap();
        assert!(
            message.contains("solver") && message.contains("output"),
            "{message}"
        );
    }

    #[test]
    fn update_metadata_clears_finish_to_silent() {
        // referencing_doc finishes with solver; clear it to a silent plan.
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({"finish": {}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver.is_none(), "solver should be cleared");
        assert!(doc.output.is_none(), "output should be cleared");
        assert_dirty_publish(&mut rx);

        // null clears too — reset a solver first, then clear via null.
        tools.update_metadata(&json!({"finish": {"solver": {"queryToAnswer": "answer it"}}}));
        let outcome = tools.update_metadata(&json!({"finish": null}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver.is_none() && doc.output.is_none());
    }

    #[test]
    fn update_metadata_sets_and_clears_input_schema() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let schema = json!({
            "type": "object",
            "required": ["pr"],
            "properties": {"pr": {"type": "integer"}}
        });
        let outcome = tools.update_metadata(&json!({"input_schema": schema.clone()}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().input_schema,
            Some(schema)
        );

        let outcome = tools.update_metadata(&json!({"input_schema": null}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().input_schema,
            None
        );

        let bad = tools.update_metadata(&json!({"input_schema": "foo"}));
        assert!(bad.is_error);
    }

    #[test]
    fn update_metadata_edits_requires_servers() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({"requires_servers": ["linear", "github"]}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().requires_servers,
            vec!["linear".to_string(), "github".to_string()]
        );

        let outcome = tools.update_metadata(&json!({"requires_servers": []}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(draft
            .lock()
            .unwrap()
            .doc
            .as_ref()
            .unwrap()
            .requires_servers
            .is_empty());

        let bad = tools.update_metadata(&json!({"requires_servers": "linear"}));
        assert!(bad.is_error);
    }

    #[test]
    fn add_step_appends_by_default_and_anchors_on_request() {
        let step = json!({"id": "E3", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}});

        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.add_step(&json!({ "step": step }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["index"], json!(3));
        assert_eq!(outcome.result["steps"], json!(4));
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[3].id,
            "E3"
        );
        assert_dirty_publish(&mut rx);

        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.add_step(&json!({"step": step, "after": "E0"}));
        assert_eq!(outcome.result["index"], json!(1), "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[1].id,
            "E3"
        );

        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        // Anchored before E0 the step may not reference E0 anymore.
        let independent = json!({"id": "E3", "toolName": "t__extra", "input": {"q": "fixed"}});
        let outcome = tools.add_step(&json!({"step": independent, "before": "E0"}));
        assert_eq!(outcome.result["index"], json!(0), "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].id,
            "E3"
        );
    }

    #[test]
    fn add_step_anchor_and_shape_errors() {
        let (tools, _draft, _rx) = editing_tools(Some(referencing_doc()));
        let step = json!({"id": "E3", "toolName": "t__extra", "input": {}});

        let outcome = tools.add_step(&json!({"step": step, "after": "E9"}));
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availableSteps"], json!(["E0", "E1", "E2"]));

        let outcome = tools.add_step(&json!({"step": step, "before": "E0", "after": "E1"}));
        assert!(outcome.is_error);

        let outcome = tools.add_step(&json!({"step": {"id": "E3"}}));
        assert!(outcome.is_error, "missing toolName/input must not parse");
    }

    #[test]
    fn add_step_rejects_an_invalid_result_and_keeps_the_draft() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        // Inserted at the front, this step references E1 — a forward
        // reference the validator must reject.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E3", "toolName": "t__extra", "input": {"q": "{{E1.values}}"}},
            "before": "E0",
        }));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"].is_array(),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");

        let duplicate = json!({"id": "E0", "toolName": "t__extra", "input": {}});
        assert!(tools.add_step(&json!({ "step": duplicate })).is_error);
    }

    #[test]
    fn update_step_patches_fields() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({
            "id": "E0",
            "toolName": "t__better_search",
            "input": {"query": "y", "limit": 5},
            "reasoning": "narrower query",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.steps[0].tool_name, "t__better_search");
        assert_eq!(doc.steps[0].input["limit"], json!(5));
        assert_eq!(doc.steps[0].reasoning.as_deref(), Some("narrower query"));
        assert_dirty_publish(&mut rx);

        // An empty reasoning clears it; an empty patch is an error.
        assert!(
            !tools
                .update_step(&json!({"id": "E0", "reasoning": ""}))
                .is_error
        );
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].reasoning,
            None
        );
        assert!(tools.update_step(&json!({"id": "E0"})).is_error);
        assert!(
            tools
                .update_step(&json!({"id": "E9", "toolName": "t__x"}))
                .is_error
        );
    }

    #[test]
    fn update_step_rename_rewrites_downstream_references() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({"id": "E1", "newId": "issues"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["id"], json!("issues"));
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.steps[1].id, "issues");
        assert_eq!(
            doc.steps[2].input["rows"],
            json!("{{#issues.values}}x{{/issues.values}} of {{issues.values.length}}")
        );
        assert_eq!(
            doc.solver.unwrap().query_to_answer,
            "Summarize {{issues.values.length}} items"
        );
    }

    #[test]
    fn update_step_rename_collision_is_rejected() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({"id": "E0", "newId": "E1"}));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"].is_array(),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].id,
            "E0"
        );
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");
    }

    #[test]
    fn delete_step_removes_and_publishes() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.delete_step(&json!({"id": "E2"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["steps"], json!(2));
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 2);
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn delete_step_rejects_dangling_references_and_empty_plans() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        // E1 still reads {{E0.values.0.id}} — deleting E0 must be refused.
        let outcome = tools.delete_step(&json!({"id": "E0"}));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"]
                .to_string()
                .contains("E0"),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");

        // The last remaining step cannot be deleted either.
        let (tools, _draft, _rx) = editing_tools(Some(demo_doc()));
        assert!(tools.delete_step(&json!({"id": "E0"})).is_error);
    }

    /// A draft with a pre-existing validation problem: E10's input has a
    /// template parse error — the state draft_plan handed over in the
    /// 2026-07-15 incident.
    fn invalid_doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E10
    tool_name: t__report
    input: { rows: "{{.}}" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn edits_on_an_already_invalid_plan_are_accepted_when_they_break_nothing() {
        let (tools, draft, mut rx) = editing_tools(Some(invalid_doc()));
        // The new step is valid; the plan stays invalid, but only from the
        // pre-existing E10 problem — the edit must land.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E5", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}},
            "after": "E0",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let pre_existing = outcome.result["preExistingProblems"].to_string();
        assert!(pre_existing.contains("E10"), "{:?}", outcome.result);
        assert!(
            outcome.result["note"]
                .as_str()
                .unwrap()
                .contains("not caused by this edit"),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn edit_introducing_a_new_problem_is_rejected_citing_only_that_problem() {
        let (tools, draft, mut rx) = editing_tools(Some(invalid_doc()));
        // Forward reference to E99: a NEW problem on top of E10's.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E5", "toolName": "t__extra", "input": {"q": "{{E99.values}}"}},
        }));
        assert!(outcome.is_error);
        let introduced = outcome.result["problemsIntroduced"].as_array().unwrap();
        assert_eq!(introduced.len(), 1, "{:?}", outcome.result);
        assert!(introduced[0].as_str().unwrap().contains("E99"));
        let pre_existing = outcome.result["preExistingProblems"].to_string();
        assert!(
            pre_existing.contains("E10") && !introduced[0].as_str().unwrap().contains("E10"),
            "pre-existing problems must be reported separately, not as the cause: {:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 2);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");
    }

    /// The 2026-07-15 incident replayed: draft_plan handed over a draft
    /// whose E10 had a template parse error. The fix was add_step E9b then
    /// update_step E10 to read from it — both edits must land in order,
    /// without the delete/re-add workaround.
    #[test]
    fn incident_e9b_then_e10_fix_sequence_is_accepted_in_order() {
        let (tools, draft, _rx) = editing_tools(Some(invalid_doc()));

        // 1. Add the new valid step E9b (previously rejected with E10's
        //    pre-existing error).
        let outcome = tools.add_step(&json!({
            "step": {"id": "E9b", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}},
            "after": "E0",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);

        // 2. Point E10 at E9b (previously rejected because E9b was never
        //    admitted).
        let outcome = tools.update_step(&json!({
            "id": "E10",
            "input": {"rows": "{{E9b.values}}"},
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(
            outcome.result.get("preExistingProblems").is_none(),
            "the plan is now fully valid: {:?}",
            outcome.result
        );

        let doc = draft.lock().unwrap().doc.clone().unwrap();
        let ids: Vec<&str> = doc.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["E0", "E9b", "E10"]);
        assert!(tools.validate_plan().result["valid"].as_bool().unwrap());
    }

    #[test]
    fn load_plan_unknown_field_error_includes_a_control_flow_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        let yaml = "identifier: demo\nname: Demo\ndescription: d\nsteps:\n\
                    - id: E0\n  tool_name: t__x\n  input: {}\ngate:\n  condition: true\n";
        std::fs::write(&path, yaml).unwrap();
        let (tools, _draft, _rx) = editing_tools(None);
        let outcome = tools.load_plan(&json!({ "name_or_path": path.to_str().unwrap() }));
        assert!(outcome.is_error);
        let message = outcome.result["error"].as_str().unwrap();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("hint:"), "{message}");
        assert!(
            message.contains("exit, agent, ask, decide, filter, map"),
            "{message}"
        );
    }

    #[test]
    fn edits_capture_an_undo_snapshot_and_restore_swaps_back() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        assert!(!tools.update_metadata(&json!({"name": "Renamed"})).is_error);
        assert_dirty_publish(&mut rx);
        {
            let state = draft.lock().unwrap();
            let (undone, was_dirty) = state.undo.as_ref().unwrap();
            assert_eq!(undone.name, "Demo");
            assert!(!*was_dirty, "the displaced draft was clean");
        }

        // Restore puts the original back with its clean flag…
        let outcome = tools.restore_draft();
        assert!(!outcome.is_error, "{:?}", outcome.result);
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.name, "Demo");
                assert!(!dirty);
            }
            _ => panic!("expected DraftReplaced"),
        }
        assert!(!draft.lock().unwrap().dirty);

        // …and restoring again redoes the edit.
        assert!(!tools.restore_draft().is_error);
        let state = draft.lock().unwrap();
        assert_eq!(state.doc.as_ref().unwrap().name, "Renamed");
        assert!(state.dirty);
    }

    #[test]
    fn restore_with_no_snapshot_errors() {
        let (tools, _draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.restore_draft();
        assert!(outcome.is_error);
        assert!(rx.try_recv().is_err(), "nothing should have been published");
    }

    #[test]
    fn editing_tools_require_a_draft() {
        let (tools, _draft, _rx) = editing_tools(None);
        assert!(tools.update_metadata(&json!({"name": "x"})).is_error);
        assert!(
            tools
                .add_step(&json!({"step": {"id": "E0", "toolName": "t__x", "input": {}}}))
                .is_error
        );
        assert!(
            tools
                .update_step(&json!({"id": "E0", "toolName": "t__x"}))
                .is_error
        );
        assert!(tools.delete_step(&json!({"id": "E0"})).is_error);
    }

    #[tokio::test]
    async fn workbench_tool_descriptions_carry_the_not_plan_legal_note() {
        let (tools, _draft, _rx) = editing_tools(None);
        for def in ToolRegistry::tools(&tools).await.unwrap() {
            assert!(
                def.description.ends_with(WORKBENCH_ONLY_NOTE),
                "{} is missing the workbench-only note",
                def.name
            );
        }
    }

    // ── draft_plan (scripted LLM) ──────────────────────────────────────

    struct ScriptedProvider {
        responses: Mutex<Vec<graph_llm::types::ChatResponse>>,
        requests: Mutex<Vec<graph_llm::types::ChatRequest>>,
    }

    #[async_trait]
    impl graph_llm::ChatProvider for ScriptedProvider {
        async fn chat(
            &self,
            req: graph_llm::types::ChatRequest,
        ) -> Result<graph_llm::types::ChatResponse, graph_llm::LlmError> {
            self.requests.lock().unwrap().push(req);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(graph_llm::LlmError::Parse("script exhausted".into()));
            }
            Ok(responses.remove(0))
        }

        async fn chat_stream(
            &self,
            req: graph_llm::types::ChatRequest,
        ) -> Result<graph_llm::types::EventStream, graph_llm::LlmError> {
            use futures::StreamExt;
            let response = self.chat(req).await?;
            Ok(
                futures::stream::iter(vec![Ok(graph_llm::types::StreamEvent::Completed(response))])
                    .boxed(),
            )
        }
    }

    /// A pipeline whose planner answers from a script of structured
    /// planner outputs — the same pattern as graph-core's pipeline tests.
    fn scripted_pipeline(outputs: Vec<Value>) -> (Arc<Pipeline>, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(
                outputs
                    .into_iter()
                    .map(|value| graph_llm::types::ChatResponse {
                        content: None,
                        tool_calls: vec![],
                        thinking: Vec::new(),
                        structured: Some(value),
                        stop_reason: graph_llm::types::StopReason::EndTurn,
                        usage: graph_llm::types::Usage::default(),
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        });
        let mut providers: std::collections::HashMap<String, Arc<dyn graph_llm::ChatProvider>> =
            std::collections::HashMap::new();
        providers.insert("mock".to_string(), provider.clone());
        let roles = graph_config::ModelRoles {
            default: Some(graph_config::ModelChoice {
                provider: "mock".to_string(),
                model: "test".to_string(),
                temperature: None,
                dimensions: None,
                description: None,
                fallbacks: Vec::new(),
            }),
            ..Default::default()
        };
        let router = Arc::new(graph_llm::ModelRouter::with_providers(providers, roles));
        let pipeline = Arc::new(Pipeline {
            router,
            registry: Arc::new(graph_core::CompositeRegistry::new(vec![])),
            events: Arc::new(graph_core::NullSink),
            plans: Arc::new(Vec::new()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
            usage: std::sync::Arc::new(graph_core::usage::UsageLedger::unpriced()),
        });
        (pipeline, provider)
    }

    fn draft_tools(
        pipeline: Arc<Pipeline>,
    ) -> (WorkbenchTools, tokio::sync::mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            pipeline,
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        (tools, rx)
    }

    fn outline_output() -> Value {
        json!({
            "items": [
                {"summary": "search for x", "expectedTool": "t__search"},
                {"summary": "report on it", "expectedTool": "t__report"},
            ],
            "queryToAnswer": "report on x",
        })
    }

    #[tokio::test]
    async fn draft_publishes_once_when_complete() {
        let (pipeline, provider) = scripted_pipeline(vec![
            outline_output(),
            json!({"step": {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                   "planComplete": false}),
            json!({"step": {"id": "E1", "toolName": "t__report",
                            "input": {"rows": "{{E0.values}}"}},
                   "planComplete": true}),
        ]);
        let (tools, mut rx) = draft_tools(pipeline);

        let outcome = tools.draft_plan(&json!({"goal": "report on x"})).await;
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["validation"], json!("ok"));
        assert_eq!(outcome.result["steps"], json!(2));
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            3,
            "outline + one call per step"
        );
        // Exactly one publish — partial plans never hit the shared doc.
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert!(dirty);
                assert_eq!(doc.steps.len(), 2);
                assert_eq!(doc.solver.as_ref().unwrap().query_to_answer, "report on x");
            }
            _ => panic!("expected DraftReplaced"),
        }
        assert!(rx.try_recv().is_err(), "one publish only");
    }

    #[tokio::test]
    async fn draft_exhaustion_salvages_the_valid_prefix() {
        let invalid_step = || {
            json!({"step": {"id": "E1", "toolName": "t__report",
                            "input": {"rows": "{{E9.values}}"}},
                   "planComplete": false})
        };
        let (pipeline, _) = scripted_pipeline(vec![
            outline_output(),
            json!({"step": {"id": "E0", "toolName": "t__search", "input": {"query": "x"}},
                   "planComplete": false}),
            invalid_step(),
            invalid_step(),
            invalid_step(),
        ]);
        let (tools, mut rx) = draft_tools(pipeline);

        let outcome = tools.draft_plan(&json!({"goal": "report on x"})).await;
        assert!(outcome.is_error);
        assert_eq!(outcome.result["failedStep"], json!("E1"));
        assert!(
            outcome.result["problems"].to_string().contains("E9"),
            "{:?}",
            outcome.result
        );
        assert!(
            outcome.result["note"]
                .as_str()
                .unwrap()
                .contains("editing tools"),
            "{:?}",
            outcome.result
        );
        // The valid prefix was published (dirty) for the agent to finish.
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert!(dirty);
                assert_eq!(doc.steps.len(), 1);
                assert_eq!(doc.steps[0].id, "E0");
            }
            _ => panic!("expected DraftReplaced"),
        }
    }
}
