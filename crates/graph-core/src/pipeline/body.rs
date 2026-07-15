//! The shared body grammar for control steps: `decide` branches and
//! `map`/`reduce` bodies are each either a single tool call or an inline
//! step list, parsed lazily from raw values so rendering can be deferred
//! until the executor knows the body will actually run (and against which
//! scope). Bodies run in a scope layered over the plan's results — their
//! step ids never enter `RunState::results` (the step cursor and replan
//! merge are keyed on it), but their work counts in `steps_executed`.

use super::plan::{check_step_id, Step};
use super::state::{BusEntry, BusKind};
use super::Pipeline;
use crate::template::{render_input, RenderError, Roots};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// A control step's body: one side of a `decide` fork, or the `do` of a
/// `map`/`reduce`.
#[derive(Debug)]
pub enum Branch {
    /// A single tool call — a `Step` without an id.
    Call(BranchCall),
    /// An inline list of steps, run in authored order within a scope
    /// layered over the plan's results.
    Steps(Vec<Step>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BranchCall {
    #[serde(alias = "tool_name")]
    pub tool_name: String,
    pub input: Map<String, Value>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

/// Parse a body value: an array is an inline step list, an object is a
/// single call. Explicit rather than serde-untagged so authors get a
/// pointed error instead of "did not match any variant".
pub fn parse_branch(name: &str, raw: &Value) -> Result<Branch, String> {
    match raw {
        Value::Array(_) => {
            let steps: Vec<Step> =
                serde_json::from_value(raw.clone()).map_err(|e| format!("`{name}` steps: {e}"))?;
            if steps.is_empty() {
                return Err(format!("`{name}` has no steps"));
            }
            Ok(Branch::Steps(steps))
        }
        Value::Object(_) => {
            let call: BranchCall =
                serde_json::from_value(raw.clone()).map_err(|e| format!("`{name}` call: {e}"))?;
            Ok(Branch::Call(call))
        }
        _ => Err(format!(
            "`{name}` must be a tool call object or a list of steps"
        )),
    }
}

/// The body schema shared by the decide/map/reduce planner tool defs.
pub fn body_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["toolName", "input"],
                "properties": {
                    "toolName": {"type": "string", "description": "Exact tool name; may be plan__* for a multi-step body. Never exit, decide, map, or reduce."},
                    "input": {"type": "object"}
                }
            },
            {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["id", "toolName", "input"],
                    "properties": {
                        "id": {"type": "string", "description": "Identifier (letters, digits, _) unique across the whole plan; visible only inside this body."},
                        "toolName": {"type": "string"},
                        "input": {"type": "object"}
                    }
                }
            }
        ]
    })
}

/// Static validation of a body: shape, tool names, step ids, and template
/// reference ordering. `seen` is the ids available before the owning step
/// (including `input`); `pseudo` is the pseudo-roots this body's scope
/// adds (`item`/`index` for map, plus `accumulator` for reduce, none for
/// decide); `all_plan_ids` is every top-level id, for collision checks.
pub fn validate_body(
    name: &str,
    raw: &Value,
    seen: &[&str],
    pseudo: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let branch = match parse_branch(name, raw) {
        Ok(branch) => branch,
        Err(message) => {
            problems.push(format!("step {step_id}: {message}"));
            return;
        }
    };
    let mut body_seen: Vec<&str> = seen.to_vec();
    body_seen.extend_from_slice(pseudo);
    match branch {
        Branch::Call(call) => {
            check_body_tool(name, &call.tool_name, step_id, problems);
            for value in call.input.values() {
                super::check_templates(value, &body_seen, step_id, problems);
            }
        }
        Branch::Steps(steps) => {
            // Body steps may reference earlier top-level steps and earlier
            // same-body steps — never a sibling body or anything later.
            for (index, step) in steps.iter().enumerate() {
                if let Err(problem) = check_step_id(&step.id) {
                    problems.push(format!("step {step_id}: `{name}` {problem}"));
                }
                if all_plan_ids.contains(&step.id.as_str()) {
                    problems.push(format!(
                        "step {step_id}: `{name}` step id {} collides with a plan step id",
                        step.id
                    ));
                }
                if steps[..index].iter().any(|s| s.id == step.id) {
                    problems.push(format!(
                        "step {step_id}: `{name}` has duplicate step id {}",
                        step.id
                    ));
                }
                check_body_tool(name, &step.tool_name, step_id, problems);
                for value in step.input.values() {
                    super::check_templates(value, &body_seen, &step.id, problems);
                }
                body_seen.push(step.id.as_str());
            }
        }
    }
}

fn check_body_tool(name: &str, tool: &str, step_id: &str, problems: &mut Vec<String>) {
    let control = [
        super::EXIT_TOOL,
        super::DECIDE_TOOL,
        super::MAP_TOOL,
        super::REDUCE_TOOL,
    ];
    if control.contains(&tool) {
        problems.push(format!(
            "step {step_id}: `{name}` uses '{tool}' — control steps cannot nest \
             inside a body; call a plan (plan__*) instead"
        ));
    } else if !tool.contains("__") && tool != "plan_and_execute" {
        problems.push(format!(
            "step {step_id}: `{name}` tool '{tool}' is not a namespaced tool name"
        ));
    }
}

/// A completed body run.
pub(super) struct BodyRun {
    /// The single call's result, or the last step's value for step lists.
    pub result: Value,
    /// Body steps executed — merged into `RunState::branch_steps_executed`.
    pub steps_executed: usize,
    /// Bus entries recorded during the run — merged into `RunState::bus`.
    /// Deferred rather than pushed live so bodies can run concurrently
    /// against a shared immutable scope (map items).
    pub bus: Vec<BusEntry>,
}

/// A failed body run, carrying the work done before the failure so callers
/// can keep the accounting honest.
pub(super) struct BodyError {
    pub fail: BodyFail,
    pub steps_executed: usize,
    pub bus: Vec<BusEntry>,
}

pub(super) enum BodyFail {
    /// Classified by the caller: `EmptyData` degrades, the rest fail.
    Render(RenderError),
    /// Pre-formatted message naming the body, inner step, and tool.
    Tool(String),
    /// The execution gate aborted the run — hard stop, never replans.
    Aborted,
}

impl BodyError {
    fn fail(fail: BodyFail, steps_executed: usize, bus: Vec<BusEntry>) -> Self {
        Self {
            fail,
            steps_executed,
            bus,
        }
    }
}

impl Pipeline {
    /// Render and run one body in a scope layered over `base_scope` plus
    /// the given pseudo-roots (`item`/`index` for map, `accumulator` too
    /// for reduce, none for decide). Takes the scope immutably and returns
    /// state deltas instead of mutating, so map can run bodies for many
    /// items concurrently. `label` prefixes error messages (e.g.
    /// "`then` branch", "`do` item 3"); `bus_path` names bus sources
    /// (e.g. "then", "do.3").
    pub(super) async fn run_body(
        &self,
        step_id: &str,
        bus_path: &str,
        label: &str,
        branch: &Branch,
        base_scope: &Map<String, Value>,
        extras: &[(&str, Value)],
    ) -> Result<BodyRun, BodyError> {
        let mut scope = base_scope.clone();
        for (name, value) in extras {
            scope.insert((*name).to_string(), value.clone());
        }
        match branch {
            Branch::Call(call) => {
                let rendered =
                    render_input(&Value::Object(call.input.clone()), &Roots::new(&scope))
                        .map_err(|e| BodyError::fail(BodyFail::Render(e), 0, Vec::new()))?;
                let path = super::StepPath::in_body(step_id, bus_path, None);
                let value = self
                    .dispatch(&path, &call.tool_name, rendered, &scope)
                    .await
                    .map_err(|e| {
                        let fail = match e {
                            super::DispatchError::Aborted => BodyFail::Aborted,
                            super::DispatchError::Failed(m) => {
                                BodyFail::Tool(format!("{label} ({}): {m}", call.tool_name))
                            }
                        };
                        BodyError::fail(fail, 0, Vec::new())
                    })?;
                Ok(BodyRun {
                    result: value,
                    steps_executed: 1,
                    bus: Vec::new(),
                })
            }
            Branch::Steps(steps) => {
                let mut bus = Vec::new();
                let mut steps_executed = 0;
                let mut last = Value::Null;
                for body_step in steps {
                    let rendered =
                        render_input(&Value::Object(body_step.input.clone()), &Roots::new(&scope))
                            .map_err(|e| {
                                BodyError::fail(BodyFail::Render(e), steps_executed, bus.clone())
                            })?;
                    let path = super::StepPath::in_body(step_id, bus_path, Some(&body_step.id));
                    let value = self
                        .dispatch(&path, &body_step.tool_name, rendered, &scope)
                        .await
                        .map_err(|e| {
                            let fail = match e {
                                super::DispatchError::Aborted => BodyFail::Aborted,
                                super::DispatchError::Failed(m) => BodyFail::Tool(format!(
                                    "{label} step {} ({}): {m}",
                                    body_step.id, body_step.tool_name
                                )),
                            };
                            BodyError::fail(fail, steps_executed, bus.clone())
                        })?;
                    steps_executed += 1;
                    bus.push(BusEntry {
                        source: format!("{step_id}/{bus_path}/{}", body_step.id),
                        kind: BusKind::Info,
                        content: "ok".to_string(),
                    });
                    scope.insert(body_step.id.clone(), value.clone());
                    last = value;
                }
                Ok(BodyRun {
                    result: last,
                    steps_executed,
                    bus,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_parse_errors_are_pointed() {
        let err = parse_branch("then", &json!("t__search")).unwrap_err();
        assert!(err.contains("tool call object or a list of steps"), "{err}");
        let err = parse_branch("else", &json!([])).unwrap_err();
        assert!(err.contains("no steps"), "{err}");
        let err = parse_branch("then", &json!({"input": {}})).unwrap_err();
        assert!(
            err.contains("toolName") || err.contains("tool_name"),
            "{err}"
        );
    }

    #[test]
    fn nested_control_tools_are_rejected() {
        for tool in ["exit", "decide", "map", "reduce"] {
            let mut problems = Vec::new();
            check_body_tool("do", tool, "E1", &mut problems);
            assert!(
                problems.iter().any(|p| p.contains("cannot nest")),
                "{tool}: {problems:?}"
            );
        }
        let mut problems = Vec::new();
        check_body_tool("do", "plan__inner", "E1", &mut problems);
        check_body_tool("do", "plan_and_execute", "E1", &mut problems);
        check_body_tool("do", "t__search", "E1", &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }
}
