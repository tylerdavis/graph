//! The `decide` fork: a step that routes execution into one of two
//! branches (`then`/`else`), gated like an `exit` step by a logical
//! condition (`when`) or an inferred verdict (`infer`). Intercepted by
//! the executor — never dispatched to a tool registry. Only the chosen
//! branch is rendered and run: the other side's templates are never
//! evaluated, so each branch may safely reference data that only exists
//! when that branch is the right one to take.

use super::condition::{evaluate_gate, Condition};
use super::plan::{step_number, Step};
use super::state::BusKind;
use super::{ExecutionEnd, Pipeline, RunState};
use crate::template::{render_input, render_str, RenderError, Roots};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// Reserved step tool name.
pub const DECIDE_TOOL: &str = "decide";

/// The decide step's input, parsed from the RAW (unrendered) step input:
/// the condition and branches stay as plain values so rendering can be
/// deferred until the gate has picked a side.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideSpec {
    /// Logical gate. Exactly one of `when`/`infer` is required.
    #[serde(default)]
    pub when: Option<Value>,
    /// Inferred gate: a yes/no question answered by the `judge` model role.
    #[serde(default)]
    pub infer: Option<String>,
    /// Branch taken when the gate holds.
    pub then: Value,
    /// Branch taken otherwise; absent means the plan just continues.
    #[serde(rename = "else", default)]
    pub else_: Option<Value>,
}

/// One side of the fork.
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

/// Parse a branch value: an array is an inline step list, an object is a
/// single call. Explicit rather than serde-untagged so authors get a
/// pointed error instead of "did not match any variant".
pub fn parse_branch(name: &str, raw: &Value) -> Result<Branch, String> {
    match raw {
        Value::Array(_) => {
            let steps: Vec<Step> = serde_json::from_value(raw.clone())
                .map_err(|e| format!("`{name}` branch steps: {e}"))?;
            if steps.is_empty() {
                return Err(format!("`{name}` branch has no steps"));
            }
            Ok(Branch::Steps(steps))
        }
        Value::Object(_) => {
            let call: BranchCall = serde_json::from_value(raw.clone())
                .map_err(|e| format!("`{name}` branch call: {e}"))?;
            Ok(Branch::Call(call))
        }
        _ => Err(format!(
            "`{name}` branch must be a tool call object or a list of steps"
        )),
    }
}

/// The decide step as described to the planner.
pub fn decide_tool_def() -> crate::tools::ToolDef {
    let branch_schema = json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["toolName", "input"],
                "properties": {
                    "toolName": {"type": "string", "description": "Exact tool name; may be plan__* for a multi-step branch. Never exit or decide."},
                    "input": {"type": "object"}
                }
            },
            {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["id", "toolName", "input"],
                    "properties": {
                        "id": {"type": "string", "description": "E-shaped id unique across the whole plan; visible only inside this branch."},
                        "toolName": {"type": "string"},
                        "input": {"type": "object"}
                    }
                }
            }
        ]
    });
    crate::tools::ToolDef {
        name: DECIDE_TOOL.to_string(),
        description: "Fork the plan on a condition: run `then` when it holds, otherwise \
                      `else` (or continue if `else` is omitted). Use it when the correct \
                      next call depends on a prior result — e.g. update an existing record \
                      vs. create a new one. Gate it with exactly one of `when` (a logical \
                      comparison) or `infer` (a yes/no question judged against prior \
                      results). A branch is a single tool call or a list of steps; \
                      branches may not contain `exit` or `decide` — call a plan (plan__*) \
                      for nested control flow. Later steps reference this step's id: \
                      {{Ex.result}} is the chosen branch's output, {{Ex.branch}} which \
                      side ran."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["then"],
            "properties": {
                "when": {
                    "type": "object",
                    "required": ["value", "op"],
                    "properties": {
                        "value": {"description": "Usually a template like {{E0.issues.length}}"},
                        "op": {"type": "string", "enum": ["eq","ne","gt","lt","gte","lte","empty","not_empty","contains"]},
                        "to": {"description": "Comparison operand (omit for empty/not_empty)"}
                    }
                },
                "infer": {"type": "string", "description": "A yes/no question about prior results; runs `then` on yes."},
                "then": branch_schema,
                "else": branch_schema
            }
        }),
        output_schema: None,
        output_example: Some(json!({
            "branch": "then",
            "verdict": true,
            "reason": "…",
            "result": {"…": "…"}
        })),
        read_only: None, // effect depends entirely on what the branch calls
    }
}

/// Static validation of a decide step's raw input: gate arity, branch
/// shape, branch tool names, branch-step ids, and template reference
/// ordering. `seen` is the ids available before this step (including
/// `input`); `all_plan_ids` is every top-level id, for collision checks.
pub fn validate_decide_input(
    input: &Map<String, Value>,
    seen: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: DecideSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid decide input: {e}"));
            return;
        }
    };
    match (&spec.when, &spec.infer) {
        (Some(_), Some(_)) => problems.push(format!(
            "step {step_id}: `when` and `infer` are mutually exclusive"
        )),
        (None, None) => problems.push(format!(
            "step {step_id}: decide needs `when` or `infer` — an unconditional decide is just steps"
        )),
        _ => {}
    }
    if let Some(when) = &spec.when {
        super::check_templates(when, seen, step_id, problems);
    }
    if let Some(infer) = &spec.infer {
        super::check_templates(&Value::String(infer.clone()), seen, step_id, problems);
    }
    validate_branch("then", &spec.then, seen, all_plan_ids, step_id, problems);
    if let Some(else_) = &spec.else_ {
        validate_branch("else", else_, seen, all_plan_ids, step_id, problems);
    }
}

fn validate_branch(
    name: &str,
    raw: &Value,
    seen: &[&str],
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
    match branch {
        Branch::Call(call) => {
            check_branch_tool(name, &call.tool_name, step_id, problems);
            for value in call.input.values() {
                super::check_templates(value, seen, step_id, problems);
            }
        }
        Branch::Steps(steps) => {
            // Branch steps may reference earlier top-level steps and
            // earlier same-branch steps — never the other branch or
            // anything later.
            let mut branch_seen: Vec<&str> = seen.to_vec();
            for (index, step) in steps.iter().enumerate() {
                if step_number(&step.id).is_none() {
                    problems.push(format!(
                        "step {step_id}: `{name}` branch step id '{}' must look like E0, E1, …",
                        step.id
                    ));
                }
                if all_plan_ids.contains(&step.id.as_str()) {
                    problems.push(format!(
                        "step {step_id}: `{name}` branch step id {} collides with a plan step id",
                        step.id
                    ));
                }
                if steps[..index].iter().any(|s| s.id == step.id) {
                    problems.push(format!(
                        "step {step_id}: `{name}` branch has duplicate step id {}",
                        step.id
                    ));
                }
                check_branch_tool(name, &step.tool_name, step_id, problems);
                for value in step.input.values() {
                    super::check_templates(value, &branch_seen, &step.id, problems);
                }
                branch_seen.push(step.id.as_str());
            }
        }
    }
}

fn check_branch_tool(name: &str, tool: &str, step_id: &str, problems: &mut Vec<String>) {
    if tool == super::EXIT_TOOL || tool == DECIDE_TOOL {
        problems.push(format!(
            "step {step_id}: `{name}` branch uses '{tool}' — control steps cannot nest \
             inside a branch; call a plan (plan__*) instead"
        ));
    } else if !tool.contains("__") && tool != "plan_and_execute" {
        problems.push(format!(
            "step {step_id}: `{name}` branch tool '{tool}' is not a namespaced tool name"
        ));
    }
}

impl Pipeline {
    /// Execute a decide step: render only the condition, evaluate the
    /// gate, then render and run just the chosen branch. Ok carries the
    /// value to store under the decide step's id.
    pub(super) async fn run_decide(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let failed = |message: String| ExecutionEnd::Failed {
            step: step.id.clone(),
            tool: DECIDE_TOOL.to_string(),
            message,
        };
        let render_end = |e: RenderError| match e {
            e @ RenderError::EmptyData { .. } => ExecutionEnd::Empty {
                step: step.id.clone(),
                message: e.to_string(),
            },
            e => ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: DECIDE_TOOL.to_string(),
                message: e.to_string(),
            },
        };

        let spec: DecideSpec = serde_json::from_value(Value::Object(step.input.clone()))
            .map_err(|e| failed(format!("invalid decide step input: {e}")))?;

        // Render only the condition; the branches wait until the gate has
        // picked a side.
        let roots = Roots::new(&state.results);
        let mut gate_payload = Map::new();
        let when = match &spec.when {
            Some(raw) => {
                let rendered = render_input(raw, &roots).map_err(render_end)?;
                gate_payload.insert("when".to_string(), rendered.clone());
                Some(
                    serde_json::from_value::<Condition>(rendered)
                        .map_err(|e| failed(format!("invalid decide condition: {e}")))?,
                )
            }
            None => None,
        };
        let infer = match &spec.infer {
            Some(question) => {
                let rendered = render_str(question, &roots).map_err(render_end)?;
                gate_payload.insert("infer".to_string(), json!(rendered));
                Some(rendered)
            }
            None => None,
        };

        self.events
            .tool_started(DECIDE_TOOL, &Value::Object(gate_payload));
        let started = std::time::Instant::now();
        let eval = evaluate_gate(when.as_ref(), infer.as_deref(), &self.router).await;
        self.events
            .tool_finished(DECIDE_TOOL, started.elapsed(), eval.is_err());
        let (triggered, reason) = eval.map_err(|e| failed(format!("decide step: {e}")))?;

        let (branch_name, raw_branch) = if triggered {
            ("then", Some(&spec.then))
        } else {
            ("else", spec.else_.as_ref())
        };
        let Some(raw_branch) = raw_branch else {
            state.push_bus(
                &step.id,
                BusKind::Info,
                "gate not met, no else — continuing",
            );
            return Ok(json!({
                "branch": null,
                "verdict": false,
                "reason": reason,
                "result": null,
            }));
        };

        let result = match parse_branch(branch_name, raw_branch).map_err(failed)? {
            Branch::Call(call) => {
                let rendered =
                    render_input(&Value::Object(call.input), &Roots::new(&state.results))
                        .map_err(render_end)?;
                let value = self
                    .dispatch(&call.tool_name, rendered)
                    .await
                    .map_err(|m| {
                        failed(format!("`{branch_name}` branch ({}): {m}", call.tool_name))
                    })?;
                state.branch_steps_executed += 1;
                value
            }
            Branch::Steps(steps) => {
                // Branch steps run against a scope layered over the plan's
                // results: they see earlier top-level and same-branch
                // results, but their ids never enter `state.results` (the
                // step cursor and replan merge are keyed on it).
                let mut scope = state.results.clone();
                let mut last = Value::Null;
                for branch_step in &steps {
                    let rendered = render_input(
                        &Value::Object(branch_step.input.clone()),
                        &Roots::new(&scope),
                    )
                    .map_err(render_end)?;
                    let value = self
                        .dispatch(&branch_step.tool_name, rendered)
                        .await
                        .map_err(|m| {
                            failed(format!(
                                "`{branch_name}` branch step {} ({}): {m}",
                                branch_step.id, branch_step.tool_name
                            ))
                        })?;
                    state.branch_steps_executed += 1;
                    state.push_bus(
                        &format!("{}/{branch_name}/{}", step.id, branch_step.id),
                        BusKind::Info,
                        "ok",
                    );
                    scope.insert(branch_step.id.clone(), value.clone());
                    last = value;
                }
                last
            }
        };
        state.push_bus(&step.id, BusKind::Info, format!("decide → {branch_name}"));
        Ok(json!({
            "branch": branch_name,
            "verdict": triggered,
            "reason": reason,
            "result": result,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_parse_errors_are_pointed() {
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
    fn spec_rejects_unknown_fields() {
        let err = serde_json::from_value::<DecideSpec>(json!({
            "then": {"toolName": "t__x", "input": {}},
            "otherwise": {"toolName": "t__y", "input": {}},
        }))
        .unwrap_err();
        assert!(err.to_string().contains("otherwise"), "{err}");
    }

    #[test]
    fn validation_catches_gate_arity_and_nested_control() {
        let input: Map<String, Value> = serde_json::from_value(json!({
            "then": {"toolName": "exit", "input": {}},
        }))
        .unwrap();
        let mut problems = Vec::new();
        validate_decide_input(&input, &["input"], &["E0"], "E0", &mut problems);
        assert!(problems.iter().any(|p| p.contains("`when` or `infer`")));
        assert!(problems.iter().any(|p| p.contains("cannot nest")));
    }
}
