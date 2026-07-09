//! Plan data structures — the planner's output format. Field names are
//! camelCase because they are prompt surface: the planner is taught this
//! schema and examples of it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Sequential id: "E0", "E1", …
    pub id: String,
    /// Exact tool name from the tool list, e.g. "linear__search_issues".
    pub tool_name: String,
    /// Tool input. String values may reference earlier steps with
    /// templates like {{E0.values.0.id}}.
    pub input: Map<String, Value>,
    /// Why this step exists and what it should produce.
    #[serde(default)]
    pub reasoning: Option<String>,
}

pub type Plan = Vec<Step>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SolverData {
    /// The question the solver must answer; always includes the user's
    /// original task.
    pub query_to_answer: String,
    /// Extra system-prompt guidance for the solver.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Data collected from steps, as templates: {"issues": "{{E1}}"}.
    #[serde(default)]
    pub data: Map<String, Value>,
}

/// What the planner produces per attempt.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlannerOutput {
    pub plan: Plan,
    pub solver_data: SolverData,
}

/// Parse the numeric part of a step id ("E2" → 2).
pub fn step_number(id: &str) -> Option<u32> {
    id.strip_prefix('E')?.parse().ok()
}

pub fn sort_plan(plan: &mut Plan) {
    plan.sort_by_key(|step| step_number(&step.id).unwrap_or(u32::MAX));
}

/// Default solver data mapping every step result in, used when the planner
/// leaves `data` empty.
pub fn default_solver_data(plan: &Plan, data: &mut Map<String, Value>) {
    if !data.is_empty() {
        return;
    }
    for step in plan {
        let label = match &step.reasoning {
            Some(reason) => format!("{} {}", step.id, reason),
            None => step.id.clone(),
        };
        data.insert(label, Value::String(format!("{{{{{}}}}}", step.id)));
    }
}
