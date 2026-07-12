//! Workbench-local agent tools: the chat agent builds and edits the draft
//! plan through these, so the plan pane is always the agent's source of
//! truth. Registered under `workbench__` alongside the normal catalog.

use super::app::Msg;
use async_trait::async_trait;
use graph_core::pipeline::doc::{load_plan_doc, validate_doc, PlanDoc};
use graph_core::pipeline::{Pipeline, PlannerOutput};
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub const DRAFT_PLAN: &str = "workbench__draft_plan";
pub const GET_PLAN: &str = "workbench__get_plan";
pub const SET_PLAN: &str = "workbench__set_plan";
pub const LOAD_PLAN: &str = "workbench__load_plan";

pub struct WorkbenchTools {
    draft: Arc<Mutex<Option<PlanDoc>>>,
    pipeline: Arc<Pipeline>,
    tx: UnboundedSender<Msg>,
}

impl WorkbenchTools {
    pub fn new(
        draft: Arc<Mutex<Option<PlanDoc>>>,
        pipeline: Arc<Pipeline>,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        Self {
            draft,
            pipeline,
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
        // Keep the on-disk identity of the draft being edited.
        doc.path = self.current().and_then(|prior| prior.path);
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
        match name {
            DRAFT_PLAN => Ok(self.draft_plan(&input).await),
            GET_PLAN => Ok(self.get_plan()),
            SET_PLAN => Ok(self.set_plan(&input)),
            LOAD_PLAN => Ok(self.load_plan(&input)),
            other => Err(ToolError::Unknown(other.to_string())),
        }
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
        let tools = WorkbenchTools::new(draft.clone(), test_pipeline(vec![demo_doc()]), tx);

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
            tx,
        );
        let outcome = tools.load_plan(&json!({"name_or_path": "nope"}));
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availablePlans"], json!(["demo"]));
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
