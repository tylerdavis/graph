//! The shared body grammar for control steps: `decide` branches and
//! `map`/`reduce` bodies are each either a single tool call or an inline
//! step list, parsed lazily from raw values so rendering can be deferred
//! until the executor knows the body will actually run (and against which
//! scope). Bodies run in a scope layered over the plan's results — their
//! step ids never enter `RunState::results` (the step cursor and replan
//! merge are keyed on it), but their work counts in `steps_executed`.

use super::exit::PlanExit;
use super::plan::{check_step_id, Step};
use super::state::{BusEntry, BusKind};
use super::{Pipeline, EXIT_TOOL};
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
/// `allow_exit` mirrors the validator: decide branches may hold `exit`
/// steps, iteration bodies may not.
pub fn body_schema(allow_exit: bool) -> Value {
    let tool_name_doc = if allow_exit {
        "Exact tool name; may be plan__* for a multi-step body, or exit \
         (ends the WHOLE plan from inside the branch). Never decide, map, \
         or reduce."
    } else {
        "Exact tool name; may be plan__* for a multi-step body. Never \
         exit, decide, map, or reduce."
    };
    json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["toolName", "input"],
                "properties": {
                    "toolName": {"type": "string", "description": tool_name_doc},
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
/// `allow_exit` is true for decide branches — an `exit` there ends the
/// whole plan — and false for map/reduce bodies, where a per-item exit
/// has no coherent meaning.
#[allow(clippy::too_many_arguments)]
pub fn validate_body(
    name: &str,
    raw: &Value,
    seen: &[&str],
    pseudo: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    allow_exit: bool,
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
            check_body_tool(name, &call.tool_name, step_id, allow_exit, problems);
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
                check_body_tool(name, &step.tool_name, step_id, allow_exit, problems);
                for value in step.input.values() {
                    super::check_templates(value, &body_seen, &step.id, problems);
                }
                body_seen.push(step.id.as_str());
            }
        }
    }
}

fn check_body_tool(
    name: &str,
    tool: &str,
    step_id: &str,
    allow_exit: bool,
    problems: &mut Vec<String>,
) {
    if tool == EXIT_TOOL {
        if !allow_exit {
            problems.push(format!(
                "step {step_id}: `{name}` uses 'exit' — an exit inside an \
                 iteration body has no single-plan meaning; it is only \
                 allowed in decide branches"
            ));
        }
        return;
    }
    let control = [super::DECIDE_TOOL, super::MAP_TOOL, super::REDUCE_TOOL];
    if control.contains(&tool) {
        problems.push(format!(
            "step {step_id}: `{name}` uses '{tool}' — control steps cannot nest \
             inside a body; call a plan (plan__*) instead"
        ));
    } else if !tool.contains("__") && tool != "plan_and_execute" {
        problems.push(format!(
            "step {step_id}: `{name}` tool '{tool}' is not a namespaced tool \
             name like server__tool"
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
    /// An `exit` step in the body fired. Not a failure — it rides the
    /// error channel so the accounting (steps run, bus entries) travels
    /// with it; decide maps it to `ExecutionEnd::Exited`, ending the
    /// whole plan.
    Exited(PlanExit),
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
                if call.tool_name == EXIT_TOOL {
                    return match self.eval_body_exit(&path, &rendered, label).await {
                        Ok(super::exit::ExitEval::Passed(value)) => Ok(BodyRun {
                            result: value,
                            steps_executed: 1,
                            bus: Vec::new(),
                        }),
                        Ok(super::exit::ExitEval::Exited(exit)) => {
                            Err(BodyError::fail(BodyFail::Exited(exit), 1, Vec::new()))
                        }
                        Err(fail) => Err(BodyError::fail(fail, 0, Vec::new())),
                    };
                }
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
                    let value = if body_step.tool_name == EXIT_TOOL {
                        match self.eval_body_exit(&path, &rendered, label).await {
                            Ok(super::exit::ExitEval::Passed(value)) => value,
                            Ok(super::exit::ExitEval::Exited(exit)) => {
                                return Err(BodyError::fail(
                                    BodyFail::Exited(exit),
                                    steps_executed + 1,
                                    bus,
                                ));
                            }
                            Err(fail) => {
                                return Err(BodyError::fail(fail, steps_executed, bus));
                            }
                        }
                    } else {
                        self.dispatch(&path, &body_step.tool_name, rendered, &scope)
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
                            })?
                    };
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

impl Pipeline {
    /// Evaluate an `exit` step inside a body, with the same event envelope
    /// the top-level exit path emits. The exit's `step` is the full body
    /// path (e.g. `E4/then/bail`).
    async fn eval_body_exit(
        &self,
        path: &super::StepPath,
        rendered: &Value,
        label: &str,
    ) -> Result<super::exit::ExitEval, BodyFail> {
        let path_text = path.to_string();
        self.events
            .step_started(&self.call_stack, &path_text, EXIT_TOOL, rendered);
        self.events.tool_started(EXIT_TOOL, rendered);
        let started = std::time::Instant::now();
        let eval = super::exit::evaluate(&path_text, rendered, &self.router).await;
        match &eval {
            Ok(super::exit::ExitEval::Passed(value)) => {
                self.events
                    .tool_finished(EXIT_TOOL, started.elapsed(), false);
                self.events.step_finished(
                    &self.call_stack,
                    &path_text,
                    EXIT_TOOL,
                    value,
                    false,
                    started.elapsed(),
                );
            }
            Ok(super::exit::ExitEval::Exited(exit)) => {
                self.events
                    .tool_finished(EXIT_TOOL, started.elapsed(), false);
                self.events.step_finished(
                    &self.call_stack,
                    &path_text,
                    EXIT_TOOL,
                    &serde_json::to_value(exit).unwrap_or_default(),
                    false,
                    started.elapsed(),
                );
            }
            Err(message) => {
                self.events
                    .tool_finished(EXIT_TOOL, started.elapsed(), true);
                self.events.step_finished(
                    &self.call_stack,
                    &path_text,
                    EXIT_TOOL,
                    &json!({"error": message}),
                    true,
                    started.elapsed(),
                );
            }
        }
        eval.map_err(|message| BodyFail::Tool(format!("{label} (exit): {message}")))
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
        for tool in ["decide", "map", "reduce"] {
            for allow_exit in [false, true] {
                let mut problems = Vec::new();
                check_body_tool("do", tool, "E1", allow_exit, &mut problems);
                assert!(
                    problems.iter().any(|p| p.contains("cannot nest")),
                    "{tool}: {problems:?}"
                );
            }
        }
        let mut problems = Vec::new();
        check_body_tool("do", "plan__inner", "E1", false, &mut problems);
        check_body_tool("do", "plan_and_execute", "E1", false, &mut problems);
        check_body_tool("do", "t__search", "E1", false, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn exit_is_allowed_only_where_the_body_says_so() {
        // Decide branches allow it…
        let mut problems = Vec::new();
        check_body_tool("then", "exit", "E1", true, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");

        // …iteration bodies don't.
        let mut problems = Vec::new();
        check_body_tool("do", "exit", "E1", false, &mut problems);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("only") && p.contains("decide branches")),
            "{problems:?}"
        );
    }
}
