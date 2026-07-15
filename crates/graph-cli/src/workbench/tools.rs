//! Workbench-local agent tools: the chat agent builds and edits the draft
//! plan through these, so the plan pane is always the agent's source of
//! truth. Registered under `workbench__` alongside the normal catalog.

use super::app::Msg;
use super::runner::{DebugControls, UiGate};
use async_trait::async_trait;
use graph_core::pipeline::doc::{
    apply_schema_defaults, load_plan_doc, validate_doc, validate_input, PlanDoc,
};
use graph_core::pipeline::{Pipeline, PlannerOutput};
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub const DRAFT_PLAN: &str = "workbench__draft_plan";
pub const GET_PLAN: &str = "workbench__get_plan";
pub const SET_PLAN: &str = "workbench__set_plan";
pub const LOAD_PLAN: &str = "workbench__load_plan";
pub const LIST_PLANS: &str = "workbench__list_plans";
pub const VALIDATE_PLAN: &str = "workbench__validate_plan";
pub const RUN_PLAN: &str = "workbench__run_plan";
pub const SAVE_PLAN: &str = "workbench__save_plan";

pub struct WorkbenchTools {
    draft: Arc<Mutex<Option<PlanDoc>>>,
    pipeline: Arc<Pipeline>,
    plans_dir: Option<PathBuf>,
    debug: Arc<DebugControls>,
    tx: UnboundedSender<Msg>,
}

impl WorkbenchTools {
    pub fn new(
        draft: Arc<Mutex<Option<PlanDoc>>>,
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
        self.draft.lock().unwrap().clone()
    }

    fn publish(&self, doc: PlanDoc, dirty: bool) {
        *self.draft.lock().unwrap() = Some(doc.clone());
        let _ = self.tx.send(Msg::DraftReplaced {
            doc: Box::new(doc),
            dirty,
        });
    }

    /// Load an existing plan into the workbench: an identifier from the
    /// configured plan catalog, or a YAML file path.
    fn load_plan(&self, input: &Value) -> ToolOutcome {
        let Some(name_or_path) = input.get("name_or_path").and_then(Value::as_str) else {
            return error_outcome("load_plan requires a 'name_or_path' string");
        };
        let doc = if let Some(doc) = self
            .pipeline
            .plans
            .iter()
            .find(|d| d.identifier == name_or_path)
        {
            doc.clone()
        } else {
            let path = std::path::Path::new(name_or_path);
            if !path.exists() {
                let available: Vec<&str> = self
                    .pipeline
                    .plans
                    .iter()
                    .map(|d| d.identifier.as_str())
                    .collect();
                return ToolOutcome {
                    result: json!({
                        "error": format!(
                            "'{name_or_path}' is neither a known plan identifier nor a file"
                        ),
                        "availablePlans": available,
                    }),
                    is_error: true,
                };
            }
            match load_plan_doc(path) {
                Ok(doc) => doc,
                Err(error) => return error_outcome(&format!("failed to load plan: {error}")),
            }
        };
        let problems = self
            .pipeline
            .validate_plan(&doc.steps)
            .err()
            .unwrap_or_default();
        let summary = json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        self.publish(doc, false);
        ToolOutcome {
            result: summary,
            is_error: false,
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
        let mut problems = self
            .pipeline
            .validate_plan(&doc.steps)
            .err()
            .unwrap_or_default();
        if let Err(problem) = validate_doc(&doc) {
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
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
        let mut pipeline = (*self.pipeline).clone();
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
        let feedback = input.get("feedback").and_then(Value::as_str);
        let existing = self.current();
        let existing_output = existing.as_ref().map(|doc| PlannerOutput {
            plan: doc.steps.clone(),
            solver_data: doc.solver.clone().unwrap_or_default(),
        });
        let output = match self
            .pipeline
            .draft_plan(goal, existing_output.as_ref(), feedback)
            .await
        {
            Ok(output) => output,
            Err(error) => return error_outcome(&format!("planner failed: {error}")),
        };

        let doc = match existing {
            Some(mut doc) => {
                doc.steps = output.plan;
                // Preserve an `output` finish; otherwise refresh the solver.
                if doc.output.is_none() {
                    doc.solver = Some(output.solver_data);
                }
                doc
            }
            None => PlanDoc {
                identifier: identifier_from(goal),
                name: name_from(goal),
                description: goal.to_string(),
                exemplars: Vec::new(),
                requires_servers: Vec::new(),
                input_schema: None,
                steps: output.plan,
                solver: Some(output.solver_data),
                output: None,
                path: None,
            },
        };
        let problems = self
            .pipeline
            .validate_plan(&doc.steps)
            .err()
            .unwrap_or_default();
        let summary = json!({
            "identifier": doc.identifier,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        self.publish(doc, true);
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    fn get_plan(&self) -> ToolOutcome {
        match self.current() {
            Some(doc) => match serde_yaml::to_string(&doc) {
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

    fn set_plan(&self, input: &Value) -> ToolOutcome {
        let Some(yaml) = input.get("yaml").and_then(Value::as_str) else {
            return error_outcome(
                "set_plan requires a 'yaml' string with a complete plan document",
            );
        };
        let mut doc: PlanDoc = match serde_yaml::from_str(yaml) {
            Ok(doc) => doc,
            Err(error) => return error_outcome(&format!("invalid plan YAML: {error}")),
        };
        if let Err(problem) = validate_doc(&doc) {
            return error_outcome(&format!("invalid plan: {problem}"));
        }
        if let Err(problems) = self.pipeline.validate_plan(&doc.steps) {
            return error_outcome(&format!("invalid plan: {}", problems.join("; ")));
        }
        // Keep the on-disk identity of the draft being edited — but only
        // while it IS the same plan. If the yaml changes the identifier,
        // this is a different plan now; carrying the old file's path
        // forward would make the next save overwrite that file.
        doc.path = self
            .current()
            .filter(|prior| prior.identifier == doc.identifier)
            .and_then(|prior| prior.path);
        let summary = json!({"ok": true, "identifier": doc.identifier, "steps": doc.steps.len()});
        self.publish(doc, true);
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }
}

fn error_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        result: json!({"error": message}),
        is_error: true,
    }
}

/// Tool-name-safe identifier from a free-form goal.
fn identifier_from(goal: &str) -> String {
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

fn name_from(goal: &str) -> String {
    let first_line = goal.lines().next().unwrap_or_default().trim();
    let mut name: String = first_line.chars().take(60).collect();
    if name.is_empty() {
        name = "Draft plan".to_string();
    }
    name
}

#[async_trait]
impl ToolRegistry for WorkbenchTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(vec![
            ToolDef {
                name: DRAFT_PLAN.to_string(),
                description: "Create or revise the workbench's draft plan from a goal. The \
                              planner sees the full tool catalog and the current draft; pass \
                              the user's request as a self-contained `goal`, and `feedback` \
                              when revising after validation problems or user corrections."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["goal"],
                    "properties": {
                        "goal": {"type": "string", "description": "What the plan should accomplish, self-contained."},
                        "feedback": {"type": "string", "description": "What to change about the current draft, or validation errors to fix."}
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
                description: "Load an existing plan into the workbench as the draft — by \
                              identifier from the plan catalog, or by YAML file path. \
                              Replaces the current draft: if there are unsaved changes, \
                              confirm with the user before loading over them."
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
                    json!({"identifier": "sprint_analysis", "name": "Sprint analysis", "steps": 4, "validation": "ok"}),
                ),
                read_only: Some(true),
            },
            ToolDef {
                name: GET_PLAN.to_string(),
                description: "Read the current draft plan as YAML. Call this before making \
                              targeted edits with workbench__set_plan."
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
                output_example: Some(
                    json!({"savedTo": "~/.config/graph/plans/sprint_report.yaml"}),
                ),
                read_only: None,
            },
            ToolDef {
                name: SET_PLAN.to_string(),
                description: "Replace the draft plan with a complete YAML plan document \
                              (identifier, name, description, steps, and solver/output). \
                              Invalid documents are rejected with the problems to fix."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["yaml"],
                    "properties": {
                        "yaml": {"type": "string", "description": "The complete plan document as YAML."}
                    }
                }),
                output_schema: None,
                output_example: None,
                read_only: None,
            },
        ])
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        tracing::debug!(
            target: "workbench",
            "agent invoked {name}: {}",
            super::runner::truncate(&input.to_string(), 300)
        );
        let started = std::time::Instant::now();
        let outcome = match name {
            DRAFT_PLAN => Ok(self.draft_plan(&input).await),
            GET_PLAN => Ok(self.get_plan()),
            SET_PLAN => Ok(self.set_plan(&input)),
            LOAD_PLAN => Ok(self.load_plan(&input)),
            LIST_PLANS => Ok(self.list_plans()),
            VALIDATE_PLAN => Ok(self.validate_plan()),
            RUN_PLAN => Ok(self.run_plan(&input).await),
            SAVE_PLAN => Ok(self.save_plan()),
            other => Err(ToolError::Unknown(other.to_string())),
        };
        if let Ok(outcome) = &outcome {
            tracing::debug!(
                target: "workbench",
                "{name} finished in {:.1}s (is_error={}): {}",
                started.elapsed().as_secs_f64(),
                outcome.is_error,
                super::runner::truncate(&outcome.result.to_string(), 300)
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
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
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
        let draft = Arc::new(Mutex::new(None));
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
        assert!(draft.lock().unwrap().is_some());
        match rx.try_recv().unwrap() {
            Msg::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.identifier, "demo");
                assert!(!dirty, "a load is not an unsaved edit");
            }
            _ => panic!("expected DraftReplaced"),
        }
    }

    #[test]
    fn load_plan_unknown_name_lists_available_plans() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(None)),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = tools.load_plan(&json!({"name_or_path": "nope"}));
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availablePlans"], json!(["demo"]));
    }

    #[test]
    fn list_plans_enumerates_the_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(None)),
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
            Arc::new(Mutex::new(Some(doc))),
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
            Arc::new(Mutex::new(Some(demo_doc()))),
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
    fn set_plan_keeps_the_loaded_path_only_for_the_same_plan() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut loaded = demo_doc();
        loaded.path = Some(PathBuf::from("/plans/demo.yaml"));
        let draft = Arc::new(Mutex::new(Some(loaded)));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        // Same identifier: a surgical edit keeps the on-disk identity.
        let same = serde_yaml::to_string(&demo_doc()).unwrap();
        assert!(!tools.set_plan(&json!({ "yaml": same })).is_error);
        assert_eq!(
            draft.lock().unwrap().as_ref().unwrap().path,
            Some(PathBuf::from("/plans/demo.yaml"))
        );

        // Different identifier: this is a different plan now — carrying
        // the path forward would make the next save overwrite demo.yaml.
        let mut other = demo_doc();
        other.identifier = "other_plan".to_string();
        let other = serde_yaml::to_string(&other).unwrap();
        assert!(!tools.set_plan(&json!({ "yaml": other })).is_error);
        assert_eq!(draft.lock().unwrap().as_ref().unwrap().path, None);
    }

    #[test]
    fn save_refuses_to_overwrite_a_file_holding_another_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        std::fs::write(&path, serde_yaml::to_string(&demo_doc()).unwrap()).unwrap();

        // A draft whose identity drifted from the file its path points at
        // (the pre-fix bug state) must not clobber that file on save.
        let mut drifted = demo_doc();
        drifted.identifier = "other_plan".to_string();
        drifted.path = Some(path.clone());
        let draft = Mutex::new(Some(drifted));
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
            Arc::new(Mutex::new(Some(doc))),
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

    #[test]
    fn identifiers_are_tool_name_safe() {
        assert_eq!(
            identifier_from("Summarize this sprint's progress!"),
            "summarize_this_sprint_s_progress"
        );
        assert_eq!(identifier_from("!!!"), "draft_plan");
    }
}
