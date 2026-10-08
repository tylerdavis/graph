//! Workbench-local agent tools: the chat agent builds and edits the draft
//! plan through these, so the plan pane is always the agent's source of
//! truth. Registered under `workbench__` alongside the normal catalog.

use super::app::Msg;
use super::runner::{DebugControls, UiGate, UiInterlocutor};
use async_trait::async_trait;
use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::{apply_schema_defaults, load_plan_doc, validate_input, PlanDoc};
use graph_core::pipeline::Pipeline;
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub const DESCRIBE_TOOL: &str = "workbench__describe_tool";
pub const UPDATE_METADATA: &str = "workbench__update_metadata";
pub const ADD_STEP: &str = "workbench__add_step";
pub const UPDATE_STEP: &str = "workbench__update_step";
pub const DELETE_STEP: &str = "workbench__delete_step";
pub const RESTORE_DRAFT: &str = "workbench__restore_draft";

/// The mutating edit tools — a successful call to one is genuine forward
/// progress on the draft. The agent loop resets its iteration budget on these
/// (see `Agent::progress_tools`) so a long fix-forward session (edit, validate,
/// run, repeat) isn't starved mid-repair.
pub fn progress_tools() -> Vec<String> {
    [
        super::artifact::SET_ARTIFACT,
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
    pub composed: Option<Value>,
    pub artifact: Option<super::artifact::Artifact>,
    pub parked: Option<super::artifact::Artifact>,
}

impl DraftState {
    pub fn new(doc: Option<PlanDoc>) -> Self {
        Self {
            doc,
            dirty: false,
            undo: None,
            composed: None,
            artifact: None,
            parked: None,
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

pub fn publish_draft(
    draft: &Mutex<DraftState>,
    tx: &UnboundedSender<Msg>,
    doc: PlanDoc,
    dirty: bool,
) {
    {
        let mut state = draft.lock().unwrap();
        state.undo = state.doc.take().map(|old| (old, state.dirty));
        state.doc = Some(doc.clone());
        state.dirty = dirty;
    }
    let _ = tx.send(Msg::DraftReplaced {
        doc: Box::new(doc),
        dirty,
    });
}

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
        publish_draft(&self.draft, &self.tx, doc, dirty);
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
                     ask, route, filter, map, or reduce (there is no \
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
        let Some(name_or_path) = input.get("name").and_then(Value::as_str) else {
            return error_outcome(
                "show_artifact requires a 'name': a plan identifier or file path",
            );
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
        let Some(name_or_path) = input.get("name").and_then(Value::as_str) else {
            return error_outcome(
                "load_artifact requires a 'name': a plan identifier or file path",
            );
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
                                  workbench__save_artifact, or pass overwrite_draft: true \
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

    fn set_plan(&self, input: &Value) -> ToolOutcome {
        let plan = match input["yaml"]
            .as_str()
            .filter(|yaml| !yaml.trim().is_empty())
        {
            Some(yaml) => match serde_yaml::from_str::<Value>(yaml) {
                Ok(plan) => plan,
                Err(error) => return error_outcome(&format!("the plan isn't valid YAML: {error}")),
            },
            None => match self.draft.lock().unwrap().composed.clone() {
                Some(plan) => plan,
                None => {
                    return error_outcome(
                        "there is no composed plan to publish: call plan__compose_plan first",
                    )
                }
            },
        };
        let doc = match graph_core::pipeline::plan_doc(&plan) {
            Ok(doc) => doc,
            Err(error) => return error_outcome(&format!("not a plan document: {error}")),
        };
        let overwrite = input
            .get("overwrite_draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let problems = plan_problems(&self.pipeline, &doc);
        let summary = json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        {
            let mut state = self.draft.lock().unwrap();
            if state.dirty && !overwrite {
                return ToolOutcome {
                    result: json!({
                        "error": "the draft has unsaved changes — save them with \
                                  workbench__save_artifact, or pass overwrite_draft: true \
                                  only after the user confirms discarding them",
                        "dirtyDraft": state.doc.as_ref().map(|d| d.identifier.clone()),
                    }),
                    is_error: true,
                };
            }
            state.undo = state.doc.take().map(|old| (old, state.dirty));
            state.doc = Some(doc.clone());
            state.dirty = true;
        }
        let _ = self.tx.send(Msg::DraftReplaced {
            doc: Box::new(doc),
            dirty: true,
        });
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    async fn describe_tool(&self, input: &Value) -> ToolOutcome {
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return error_outcome("describe_tool requires a 'name' string");
        };
        if let Some(identifier) = name.strip_prefix("plan__") {
            if let Some(doc) = self
                .pipeline
                .plans
                .iter()
                .find(|d| d.identifier == identifier)
            {
                return ToolOutcome {
                    result: json!({
                        "name": name,
                        "description": doc.tool_description(),
                        "inputSchema": doc.tool_input_schema(),
                        "readOnly": null,
                    }),
                    is_error: false,
                };
            }
        }
        let defs = self.pipeline.planner_tool_defs().await;
        let Some(def) = defs.iter().find(|def| def.name == name).cloned() else {
            let servers = self.pipeline.registry.servers().await;
            return error_outcome(&unknown_tool_message(name, &defs, &servers));
        };
        let observed = match &self.pipeline.store {
            Some(store) => store
                .tool_shapes()
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|shape| shape.tool == name)
                .map(|shape| json!({"schema": shape.schema, "example": shape.example})),
            None => None,
        };
        ToolOutcome {
            result: json!({
                "name": def.name,
                "description": def.description,
                "inputSchema": def.input_schema,
                "outputSchema": def.output_schema,
                "outputExample": def.output_example,
                "observedOutput": observed,
                "readOnly": def.read_only,
            }),
            is_error: false,
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

    async fn artifact_tool(&self, name: &str, target: Target, input: &Value) -> ToolOutcome {
        use super::artifact as file;
        match (name, target) {
            (file::SET_ARTIFACT, Target::Plan) => self.set_plan(input),
            (file::SET_ARTIFACT, Target::File(_)) => {
                file::set_artifact(&self.draft, &self.pipeline, &self.tx, input).await
            }
            (file::LOAD_ARTIFACT, Target::Plan) => self.load_plan(input),
            (file::LOAD_ARTIFACT, Target::File(kind)) => match file::open_existing(
                &self.draft,
                &self.pipeline,
                &self.tx,
                kind,
                input["name"].as_str().unwrap_or_default(),
            )
            .await
            {
                Ok(name) => ToolOutcome {
                    result: json!({ "opened": name }),
                    is_error: false,
                },
                Err(error) => error_outcome(&error),
            },
            (file::TRY_ARTIFACT, Target::Plan) => self.run_plan(input).await,
            (file::TRY_ARTIFACT, Target::File(kind)) => match self.open_mismatch(kind) {
                Some(outcome) => outcome,
                None => file::try_artifact(&self.draft, &self.pipeline, &self.tx, input).await,
            },
            (file::SAVE_ARTIFACT, Target::Plan) => self.save_plan(),
            (file::SAVE_ARTIFACT, Target::File(kind)) => {
                if let Some(outcome) = self.open_mismatch(kind) {
                    return outcome;
                }
                match file::save_artifact(
                    &self.draft,
                    &self.pipeline,
                    &self.tx,
                    input["overwrite"].as_bool().unwrap_or(false),
                    input["untested"].as_bool().unwrap_or(false),
                    true,
                )
                .await
                {
                    Ok(message) => ToolOutcome {
                        result: json!({ "saved": message }),
                        is_error: false,
                    },
                    Err(error) => error_outcome(&error),
                }
            }
            (file::DISCARD_ARTIFACT, Target::Plan) => error_outcome(
                "a plan draft can't be discarded; it stays in the pane until another plan replaces it",
            ),
            (file::DISCARD_ARTIFACT, Target::File(kind)) => {
                if let Some(outcome) = self.open_mismatch(kind) {
                    return outcome;
                }
                match file::discard_artifact(&self.draft, &self.pipeline, &self.tx) {
                    Some(message) => ToolOutcome {
                        result: json!({ "discarded": message }),
                        is_error: false,
                    },
                    None => error_outcome("there is no draft to discard"),
                }
            }
            (file::LIST_ARTIFACTS, Target::Plan) => self.list_plans(),
            (file::LIST_ARTIFACTS, Target::File(file::ArtifactKind::Agent)) => {
                file::list_agents(&self.pipeline)
            }
            (file::LIST_ARTIFACTS, Target::File(file::ArtifactKind::Tool)) => {
                file::list_tools(&self.pipeline).await
            }
            (file::SHOW_ARTIFACT, Target::Plan) => self.show_plan(input),
            (file::SHOW_ARTIFACT, Target::File(kind)) => {
                let name = input["name"].as_str().unwrap_or_default();
                match file::existing(kind, name, &self.pipeline.drafted.tool_dirs) {
                    Ok(artifact) => ToolOutcome {
                        result: json!({"name": artifact.name(), "yaml": artifact.yaml}),
                        is_error: false,
                    },
                    Err(error) => error_outcome(&error),
                }
            }
            (other, _) => error_outcome(&format!("unknown artifact tool '{other}'")),
        }
    }

    fn open_mismatch(&self, kind: super::artifact::ArtifactKind) -> Option<ToolOutcome> {
        let state = self.draft.lock().unwrap();
        let open = state.artifact.as_ref()?;
        (open.kind != kind).then(|| {
            error_outcome(&format!(
                "the pane holds a {} draft, not a {}",
                open.kind.label(),
                kind.label()
            ))
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Plan,
    File(super::artifact::ArtifactKind),
}

impl Target {
    fn of(input: &Value) -> Result<Self, ToolOutcome> {
        match input["kind"].as_str() {
            Some("plan") => Ok(Self::Plan),
            Some(kind) => super::artifact::ArtifactKind::parse(kind)
                .map(Self::File)
                .ok_or(()),
            None => Err(()),
        }
        .map_err(|()| error_outcome("kind must be \"plan\", \"agent\" or \"tool\""))
    }
}

/// The workbench's full validation verdict for a draft: the document's own
/// problems plus catalog-aware tool resolution against the pipeline's
/// catalog. See [`authoring::plan_problems`] for why the catalog is
/// reporting-only and never gates an edit.
pub(super) fn plan_problems(pipeline: &Pipeline, doc: &PlanDoc) -> Vec<String> {
    authoring::plan_problems(doc, &pipeline.plans, pipeline.live_catalog().as_ref())
}

fn unknown_tool_message(
    name: &str,
    defs: &[graph_core::tools::ToolDef],
    servers: &[graph_core::tools::ToolServer],
) -> String {
    let mut message = format!("no tool named '{name}' in the plan catalog.");
    if let Some((prefix, _)) = name.split_once("__") {
        let known = ["builtin", "user", "plan", "agent"].contains(&prefix)
            || servers.iter().any(|server| server.name == prefix);
        if !known {
            let configured: Vec<&str> = servers.iter().map(|server| server.name.as_str()).collect();
            message.push_str(&format!(
                " No MCP server '{prefix}' is configured (configured: {}), so no {prefix}__ tool exists.",
                if configured.is_empty() { "none".to_string() } else { configured.join(", ") }
            ));
        }
    }
    let words: Vec<&str> = name
        .split(['_', '-'])
        .filter(|word| word.len() > 2)
        .collect();
    let mut close: Vec<(usize, &str)> = defs
        .iter()
        .map(|def| {
            let shared = words
                .iter()
                .filter(|word| {
                    def.name.contains(*word) || def.description.to_lowercase().contains(*word)
                })
                .count();
            (shared, def.name.as_str())
        })
        .filter(|(shared, _)| *shared > 0)
        .collect();
    close.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let close: Vec<&str> = close.into_iter().take(8).map(|(_, name)| name).collect();
    if close.is_empty() {
        message.push_str(" Nothing in the catalog resembles it; do not guess further names.");
    } else {
        message.push_str(&format!(
            " Closest tools that exist: {}. If none fits, the capability is missing from the catalog; tell the user instead of guessing further names.",
            close.join(", ")
        ));
    }
    message
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
                name: DESCRIBE_TOOL.to_string(),
                description: "Describe one tool from the plan catalog without calling it: its \
                              description, input schema, declared output schema, the output \
                              shape observed in earlier runs, and whether it's read-only. Use it \
                              to write a step's input and the templates that read its result."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": {"type": "string", "description": "The tool's full name, e.g. linear__list_issues or plan__project_status"}
                    }
                }),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
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
                              route/map/reduce bodies live in the control step's \
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
                              inside route/map/reduce bodies, update the owning \
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
        defs.extend(super::artifact::tool_defs());
        for def in &mut defs {
            def.description.push_str(WORKBENCH_ONLY_NOTE);
        }
        Ok(defs)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        match name {
            DESCRIBE_TOOL | UPDATE_METADATA | ADD_STEP | UPDATE_STEP | DELETE_STEP
            | RESTORE_DRAFT => {}
            name if super::artifact::ARTIFACT_TOOLS.contains(&name) => {}
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
            DESCRIBE_TOOL => Ok(self.describe_tool(&input).await),
            UPDATE_METADATA => Ok(self.update_metadata(&input)),
            ADD_STEP => Ok(self.add_step(&input)),
            UPDATE_STEP => Ok(self.update_step(&input)),
            DELETE_STEP => Ok(self.delete_step(&input)),
            RESTORE_DRAFT => Ok(self.restore_draft()),
            artifact_tool => Ok(match Target::of(&input) {
                Ok(target) => self.artifact_tool(artifact_tool, target, &input).await,
                Err(outcome) => outcome,
            }),
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

pub struct ComposedCapture {
    inner: Arc<dyn ToolRegistry>,
    draft: SharedDraft,
}

impl ComposedCapture {
    pub fn new(inner: Arc<dyn ToolRegistry>, draft: SharedDraft) -> Self {
        Self { inner, draft }
    }
}

#[async_trait]
impl ToolRegistry for ComposedCapture {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        self.inner.tools().await
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let outcome = self.inner.invoke(name, input).await?;
        if name == COMPOSE_PLAN_TOOL && !outcome.is_error {
            let plan = outcome.result.get("plan").filter(|plan| plan.is_object());
            self.draft.lock().unwrap().composed = plan.cloned();
        }
        Ok(outcome)
    }

    async fn servers(&self) -> Vec<graph_core::ToolServer> {
        self.inner.servers().await
    }
}

const COMPOSE_PLAN_TOOL: &str = "plan__compose_plan";

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::workbench::artifact::{
        DISCARD_ARTIFACT, LIST_ARTIFACTS, LOAD_ARTIFACT, SAVE_ARTIFACT, SET_ARTIFACT,
        SHOW_ARTIFACT, TRY_ARTIFACT,
    };

    fn call(tools: &WorkbenchTools, name: &str, input: Value) -> ToolOutcome {
        futures::executor::block_on(tools.invoke(name, input)).unwrap()
    }

    fn report_yaml() -> String {
        serde_yaml::to_string(&report_plan()).unwrap()
    }

    pub(in crate::workbench) fn test_pipeline(plans: Vec<PlanDoc>) -> Arc<Pipeline> {
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
            max_agent_iterations: 15,
            usage: std::sync::Arc::new(graph_core::usage::UsageLedger::unpriced()),
            agents: Arc::new(graph_core::agent::doc::AgentSet::default()),
            agent_depth: 0,
            always_loaded: Default::default(),
            drafted: Default::default(),
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

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "demo"}),
        );
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
        let doc = tools.current().unwrap();
        let problems = plan_problems(&tools.pipeline, &doc);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("user__nope")),
            "{problems:?}"
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
        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "nope"}),
        );
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

        let outcome = call(
            &tools,
            SHOW_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan"}),
        );
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

        let unknown = call(
            &tools,
            SHOW_ARTIFACT,
            json!({"kind": "plan", "name": "nope"}),
        );
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

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan"}),
        );
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

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan", "overwrite_draft": true}),
        );
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
        let outcome = call(&tools, LIST_ARTIFACTS, json!({"kind": "plan"}));
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["count"], json!(1));
        assert_eq!(outcome.result["plans"][0]["identifier"], json!("demo"));
        assert_eq!(outcome.result["plans"][0]["steps"], json!(1));
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
        let outcome = call(&tools, SAVE_ARTIFACT, json!({"kind": "plan"}));
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
        let outcome = call(&tools, TRY_ARTIFACT, json!({"kind": "plan"}));
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
        assert!(plan_problems(&tools.pipeline, &doc).is_empty());
    }

    #[test]
    fn load_plan_unknown_field_error_includes_a_control_flow_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        let yaml = "identifier: demo\nname: Demo\ndescription: d\nsteps:\n\
                    - id: E0\n  tool_name: t__x\n  input: {}\ngate:\n  condition: true\n";
        std::fs::write(&path, yaml).unwrap();
        let (tools, _draft, _rx) = editing_tools(None);
        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": path.to_str().unwrap() }),
        );
        assert!(outcome.is_error);
        let message = outcome.result["error"].as_str().unwrap();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("hint:"), "{message}");
        assert!(
            message.contains("exit, agent, ask, route, filter, map"),
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
        let roles = graph_config::ModelRoles::from([(
            "default",
            graph_config::ModelChoice {
                provider: "mock".to_string(),
                model: "test".to_string(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
            },
        )]);
        let router = Arc::new(graph_llm::ModelRouter::with_providers(providers, roles));
        let llm = graph_core::user_tools::load_pack_tools(&["llm".to_string(), "data".to_string()])
            .unwrap();
        let pipeline = Arc::new(Pipeline {
            registry: Arc::new(graph_core::user_tools::UserToolRegistry::builtins(
                llm,
                router.clone(),
            )),
            router,
            events: Arc::new(graph_core::NullSink),
            plans: Arc::new(graph_core::pipeline::doc::builtin_plan_docs()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
            max_agent_iterations: 15,
            usage: std::sync::Arc::new(graph_core::usage::UsageLedger::unpriced()),
            agents: Arc::new(graph_core::agent::doc::AgentSet::default()),
            agent_depth: 0,
            always_loaded: Default::default(),
            drafted: Default::default(),
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

    fn report_plan() -> Value {
        json!({
            "identifier": "report_on_x",
            "name": "Report on x",
            "description": "report on x",
            "steps": [
                {"id": "E0", "tool_name": "t__search", "input": {"query": "x"}},
            ],
        })
    }

    #[tokio::test]
    async fn describe_tool_knows_control_steps() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let outcome = tools.describe_tool(&json!({"name": "route"})).await;
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["name"], json!("route"));
    }

    #[tokio::test]
    async fn describe_tool_says_when_a_guessed_server_does_not_exist() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let outcome = tools
            .describe_tool(&json!({"name": "github__list_pull_requests"}))
            .await;
        assert!(outcome.is_error);
        let error = outcome.result["error"].as_str().unwrap();
        assert!(
            error.contains("No MCP server 'github' is configured"),
            "{error}"
        );
        assert!(error.contains("instead of guessing"), "{error}");
    }

    #[tokio::test]
    async fn set_draft_publishes_the_plan_once_as_unsaved() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, mut rx) = draft_tools(pipeline);
        let outcome = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml()}),
        );
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("report_on_x"));
        assert_eq!(outcome.result["steps"], json!(1));
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert!(dirty);
                assert_eq!(doc.steps[0].id, "E0");
            }
            _ => panic!("expected DraftReplaced"),
        }
        assert!(rx.try_recv().is_err(), "one publish only");
    }

    #[tokio::test]
    async fn set_draft_without_a_plan_publishes_the_last_composed_one_untouched() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        assert!(
            call(&tools, SET_ARTIFACT, json!({"kind": "plan"})).is_error,
            "nothing composed yet"
        );

        struct Composer;
        #[async_trait]
        impl ToolRegistry for Composer {
            async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
                Ok(Vec::new())
            }
            async fn invoke(&self, _name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
                Ok(ToolOutcome {
                    result: json!({"plan": report_plan(), "valid": true}),
                    is_error: false,
                })
            }
        }
        let capture = ComposedCapture::new(Arc::new(Composer), tools.draft.clone());
        capture
            .invoke(COMPOSE_PLAN_TOOL, json!({"goal": "x"}))
            .await
            .unwrap();

        let outcome = call(&tools, SET_ARTIFACT, json!({"kind": "plan"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("report_on_x"));
    }

    #[tokio::test]
    async fn set_draft_refuses_to_discard_unsaved_changes_without_confirmation() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        assert!(
            !call(
                &tools,
                SET_ARTIFACT,
                json!({"kind": "plan", "yaml": report_yaml()})
            )
            .is_error
        );

        let refused = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml()}),
        );
        assert!(refused.is_error);
        assert!(
            refused.result["error"]
                .as_str()
                .unwrap()
                .contains("unsaved changes"),
            "{:?}",
            refused.result
        );
        let confirmed = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml(), "overwrite_draft": true}),
        );
        assert!(!confirmed.is_error, "{:?}", confirmed.result);
    }

    const WEATHER_TOOL: &str = "name: weather\ndescription: Current weather for a city\nkind: exec\ncommand: printf\nargs: ['{\"city\":\"%s\"}', '{{input.city}}']\ninput_schema:\n  type: object\n  required: [city]\n  properties:\n    city: {type: string, description: The city}\n";

    fn drafting_tools() -> (
        WorkbenchTools,
        tokio::sync::mpsc::UnboundedReceiver<Msg>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(graph_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        let (tools, rx) = draft_tools(Arc::new(pipeline));
        (tools, rx, dir)
    }

    fn shown(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>) -> Option<bool> {
        let mut last = None;
        while let Ok(msg) = rx.try_recv() {
            if let Msg::ArtifactChanged(view) = msg {
                last = Some(view.is_some());
            }
        }
        last
    }

    #[tokio::test]
    async fn a_tool_draft_lives_in_the_pane_until_it_is_saved() {
        let (tools, mut rx, dir) = drafting_tools();
        let set = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": WEATHER_TOOL}),
            )
            .await
            .unwrap();
        assert!(!set.is_error, "{}", set.result);
        assert_eq!(set.result["problems"], json!([]));
        assert_eq!(shown(&mut rx), Some(true));

        let refused = tools
            .invoke(
                crate::workbench::artifact::SAVE_ARTIFACT,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(refused.is_error);
        assert!(
            refused.result.to_string().contains("test run"),
            "{}",
            refused.result
        );
        assert!(!dir.path().join("tools/weather.yaml").exists());

        let saving = tools.invoke(
            crate::workbench::artifact::SAVE_ARTIFACT,
            json!({"kind": "tool", "untested": true}),
        );
        let answering = async {
            while let Some(msg) = rx.recv().await {
                if let Msg::GateAsk { reply, input, .. } = msg {
                    assert!(input["prompt"]
                        .as_str()
                        .unwrap()
                        .starts_with("Save user__weather"));
                    reply
                        .send(crate::workbench::runner::UiDecision::Skip {
                            result: json!({"save": true}),
                        })
                        .unwrap();
                    break;
                }
            }
        };
        let (saved, ()) = tokio::join!(saving, answering);
        let saved = saved.unwrap();
        assert!(!saved.is_error, "{}", saved.result);
        assert!(dir.path().join("tools/weather.yaml").exists());
        assert!(tools.draft.lock().unwrap().artifact.is_none());
        assert_eq!(shown(&mut rx), Some(false));
    }

    #[tokio::test]
    async fn saving_an_opened_agent_replaces_that_file_and_a_renamed_one_is_saved_beside_it() {
        use crate::workbench::artifact::{save_artifact, Artifact, ArtifactKind};
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(graph_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        let provider: Arc<dyn graph_llm::ChatProvider> = Arc::new(ScriptedProvider {
            responses: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        });
        pipeline.router = Arc::new(graph_llm::ModelRouter::with_providers(
            std::collections::HashMap::from([("mock".to_string(), provider)]),
            graph_config::ModelRoles::from_iter([(
                "default".to_string(),
                graph_config::ModelChoice {
                    provider: "mock".into(),
                    model: "m".into(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                },
            )]),
        ));
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        let global = dir.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        let opened = global.join("triager.yaml");
        std::fs::write(&opened, "name: triager\n").unwrap();
        let agent = |name: &str| {
            Artifact {
            kind: ArtifactKind::Agent,
            yaml: format!(
                "name: {name}\ndescription: Triages issues\nmodel: default\ntools: []\nsystem_prompt: You triage.\n"
            ),
            problems: Vec::new(),
            last_run: None,
            editing: Some(opened.clone()),
            original: Some("triager".to_string()),
        }
        };
        let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();

        tools.draft.lock().unwrap().artifact = Some(agent("triager"));
        save_artifact(&tools.draft, &tools.pipeline, &tx, false, false, false)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(&opened)
            .unwrap()
            .contains("You triage."));
        assert!(!dir.path().join("agents/triager.yaml").exists());

        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/other.yaml"), "name: other\n").unwrap();
        tools.draft.lock().unwrap().artifact = Some(agent("other"));
        let refused = save_artifact(&tools.draft, &tools.pipeline, &tx, false, false, false)
            .await
            .unwrap_err();
        assert!(refused.contains("already exists"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("agents/other.yaml")).unwrap(),
            "name: other\n",
            "a renamed draft never replaces another agent's file"
        );
    }

    #[tokio::test]
    async fn an_open_draft_is_never_swapped_for_a_different_one() {
        let (tools, _rx, _dir) = drafting_tools();
        tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": WEATHER_TOOL}),
            )
            .await
            .unwrap();
        let other = WEATHER_TOOL.replace("name: weather", "name: forecast");
        let refused = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": other}),
            )
            .await
            .unwrap();
        assert!(refused.is_error);
        assert!(refused.result["error"]
            .as_str()
            .unwrap()
            .contains("'weather' is still open"));
        let edited = WEATHER_TOOL.replace("Current weather", "Weather now");
        let kept = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": edited}),
            )
            .await
            .unwrap();
        assert!(!kept.is_error, "editing the same draft is fine");
    }

    #[tokio::test]
    async fn every_artifact_tool_routes_on_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(
            graph_core::pipeline::Drafted::new(Some(dir.path().join("tools")), None)
                .with_tool_dirs(vec![dir.path().join("tools")]),
        );
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/weather.yaml"), WEATHER_TOOL).unwrap();
        let invoke = |name: &'static str, input: Value| {
            let tools = &tools;
            async move { tools.invoke(name, input).await.unwrap() }
        };

        let missing = invoke(SET_ARTIFACT, json!({"yaml": WEATHER_TOOL})).await;
        assert!(missing.is_error);
        assert!(missing.result.to_string().contains("kind must be"));

        let shown = invoke(
            SHOW_ARTIFACT,
            json!({"kind": "tool", "name": "user__weather"}),
        )
        .await;
        assert!(!shown.is_error, "{}", shown.result);
        assert_eq!(shown.result["yaml"], json!(WEATHER_TOOL));

        let listed = invoke(LIST_ARTIFACTS, json!({"kind": "tool"})).await;
        assert_eq!(
            listed.result["user_tools"][0]["name"],
            json!("user__weather")
        );
        let agents = invoke(LIST_ARTIFACTS, json!({"kind": "agent"})).await;
        assert!(agents.result["agents"].is_array());

        let plan_discard = invoke(DISCARD_ARTIFACT, json!({"kind": "plan"})).await;
        assert!(plan_discard.is_error);

        let set = invoke(SET_ARTIFACT, json!({"kind": "tool", "yaml": WEATHER_TOOL})).await;
        assert!(!set.is_error, "{}", set.result);
        let wrong = invoke(DISCARD_ARTIFACT, json!({"kind": "agent"})).await;
        assert!(wrong.is_error);
        assert!(wrong.result.to_string().contains("holds a tool draft"));
        assert!(tools.draft.lock().unwrap().artifact.is_some());
        let tried = invoke(TRY_ARTIFACT, json!({"kind": "agent"})).await;
        assert!(tried.is_error);
    }

    #[tokio::test]
    async fn an_invalid_draft_shows_its_problems_and_can_be_discarded() {
        let (tools, mut rx, _dir) = drafting_tools();
        let set = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": "name: weather\ndescription: x\nkind: exec\ncommand: printf\nbogus: 1\n"}),
            )
            .await
            .unwrap();
        assert!(set.result["problems"].to_string().contains("bogus"));
        let discarded = tools
            .invoke(
                crate::workbench::artifact::DISCARD_ARTIFACT,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(!discarded.is_error);
        assert_eq!(shown(&mut rx), Some(false));
    }

    #[test]
    fn the_guard_parks_an_agent_draft_for_the_tool_drafter_and_restores_it() {
        use crate::workbench::artifact::{handoff_guard, Artifact, ArtifactKind};
        let (tools, _rx) = draft_tools(test_pipeline(Vec::new()));
        let draft = tools.draft.clone();
        let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();
        let guard = handoff_guard(draft.clone(), test_pipeline(Vec::new()), tx);
        let agent = Artifact {
            kind: ArtifactKind::Agent,
            yaml: "name: triager\n".to_string(),
            problems: Vec::new(),
            last_run: None,
            editing: None,
            original: None,
        };
        draft.lock().unwrap().artifact = Some(agent);

        assert!(
            guard("agent_drafter", "plan_drafter").is_some(),
            "an open draft blocks leaving"
        );
        assert!(guard("agent_drafter", "tool_drafter").is_none());
        assert!(draft.lock().unwrap().artifact.is_none());
        assert_eq!(
            draft.lock().unwrap().parked.as_ref().unwrap().name(),
            "triager"
        );

        draft.lock().unwrap().artifact = Some(Artifact {
            kind: ArtifactKind::Tool,
            yaml: "name: lookup\n".to_string(),
            problems: Vec::new(),
            last_run: None,
            editing: None,
            original: None,
        });
        assert!(
            guard("tool_drafter", "agent_drafter").is_some(),
            "the tool draft has to be saved or discarded first"
        );

        draft.lock().unwrap().artifact = None;
        assert!(guard("tool_drafter", "agent_drafter").is_none());
        let state = draft.lock().unwrap();
        assert_eq!(state.artifact.as_ref().unwrap().name(), "triager");
        assert!(state.parked.is_none());
    }
}
