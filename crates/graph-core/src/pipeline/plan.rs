//! Plan data structures — the planner's output format. Field names are
//! camelCase because they are prompt surface: the planner is taught this
//! schema and examples of it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Unique identifier templates reference the step by: "E0", "E1", …
    /// or a descriptive name like "fetch_pr_meta". See [`check_step_id`].
    pub id: String,
    /// Exact tool name from the tool list, e.g. "linear__search_issues".
    #[serde(alias = "tool_name")]
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
    #[serde(alias = "query_to_answer", default)]
    pub query_to_answer: String,
    /// Extra system-prompt guidance for the solver.
    #[serde(default, alias = "system_prompt")]
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

/// Parse the numeric part of an E-sequence step id ("E2" → 2). The
/// E-sequence is the planner's convention, not a requirement — see
/// [`check_step_id`]; this exists so replans can mint fresh E ids past
/// the executed ones.
pub fn step_number(id: &str) -> Option<u32> {
    id.strip_prefix('E')?.parse().ok()
}

/// Roots with fixed meanings in templates (`input`, the map/reduce
/// pseudo-roots) plus `length`, which the path grammar claims as an
/// operator. Step ids may not shadow them.
pub const RESERVED_ROOTS: [&str; 5] = ["input", "item", "index", "accumulator", "length"];

/// A legal step id: any identifier templates can reference as a root —
/// letters, digits, and `_`, not starting with a digit — that doesn't
/// shadow a reserved root. `E0`-style ids and descriptive names
/// (`fetch_pr_meta`) are equally valid; uniqueness is checked per plan.
pub fn check_step_id(id: &str) -> Result<(), String> {
    let starts_well = id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_well || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "step id '{id}' must be an identifier — letters, digits, or _, \
             not starting with a digit"
        ));
    }
    if RESERVED_ROOTS.contains(&id) {
        return Err(format!(
            "step id '{id}' shadows a reserved template root ({})",
            RESERVED_ROOTS.join(", ")
        ));
    }
    Ok(())
}

/// The tool namespace of the workbench TUI's own draft-editing tools.
/// Those tools exist only inside the workbench chat agent — they are never
/// registered in the plan runtime's `ToolRegistry` — so a plan step naming
/// one is always a defect, regardless of context.
pub const WORKBENCH_TOOL_PREFIX: &str = "workbench__";

/// Why a step's tool name can never resolve in the plan runtime, if so —
/// callers prepend their own step/body context. This is the static,
/// context-free tier of tool-name checking; catalog resolution against
/// what is actually loadable lives in [`super::catalog`].
pub fn workbench_tool_problem(tool: &str) -> Option<String> {
    tool.starts_with(WORKBENCH_TOOL_PREFIX).then(|| {
        format!(
            "tool '{tool}': workbench__* tools belong to the workbench TUI \
             agent and are not available in the plan runtime — plans cannot \
             call them"
        )
    })
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
