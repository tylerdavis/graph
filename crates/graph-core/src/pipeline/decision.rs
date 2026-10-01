//! The `decide` fork: a step that routes execution into one of two
//! branches (`then`/`else`), gated like an `exit` step by a logical
//! condition (`if`) or an inferred verdict (`infer`). Intercepted by
//! the executor — never dispatched to a tool registry. Only the chosen
//! branch is rendered and run: the other side's templates are never
//! evaluated, so each branch may safely reference data that only exists
//! when that branch is the right one to take.

use super::body::{body_schema, parse_branch, validate_body, BodyError, BodyFail, BodyRun};
use super::condition::{evaluate_gate, Condition};
use super::gate::StepPath;
use super::state::{BusEntry, BusKind};
use super::{ExecutionEnd, Pipeline, RunState, Step};
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
    /// Logical gate. Exactly one of `if`/`infer` is required.
    #[serde(rename = "if", default)]
    pub if_: Option<Value>,
    /// Inferred gate: a yes/no question answered by the `judge` model role.
    #[serde(default)]
    pub infer: Option<String>,
    /// Model role for the `infer` verdict: any configured `[models.<role>]`
    /// or `default`. Defaults to the `judge` role. Ignored without `infer`.
    #[serde(default)]
    pub model: Option<String>,
    /// Branch taken when the gate holds.
    pub then: Value,
    /// Branch taken otherwise; absent means the plan just continues.
    #[serde(rename = "else", default)]
    pub else_: Option<Value>,
}

/// The decide step as described to the planner.
pub fn decide_tool_def() -> crate::tools::ToolDef {
    let branch_schema = body_schema();
    crate::tools::ToolDef {
        name: DECIDE_TOOL.to_string(),
        description: "Fork the plan on a condition: run `then` when it holds, otherwise \
                      `else` (or continue if `else` is omitted). Use it when the correct \
                      next call depends on a prior result — e.g. update an existing record \
                      vs. create a new one. Gate it with exactly one of `if` (a logical \
                      comparison) or `infer` (a yes/no question judged against prior \
                      results). A branch is a single tool call or a list of steps; \
                      branches may contain `exit`, `agent`, `ask`, `filter`, and \
                      `decide` steps (a fired exit ends the WHOLE plan) but never `map` \
                      or `reduce` — call a plan (plan__*) for nested iteration. Later \
                      steps reference this step's id: {{Ex.result}} is the chosen branch's output, \
                      {{Ex.branch}} which side ran."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["then"],
            "properties": {
                "if": {
                    "type": "object",
                    "required": ["value", "op"],
                    "properties": {
                        "value": {"description": "Usually a template like {{E0.issues.length}}"},
                        "op": {"type": "string", "enum": ["eq","ne","gt","lt","gte","lte","empty","not_empty","contains"]},
                        "to": {"description": "Comparison operand (omit for empty/not_empty)"}
                    }
                },
                "infer": {"type": "string", "description": "A yes/no question about prior results; runs `then` on yes."},
                "model": {"type": "string", "description": "Model role for the `infer` verdict (any configured role, standard or custom); defaults to the judge role."},
                "then": branch_schema.clone(),
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
    match (&spec.if_, &spec.infer) {
        (Some(_), Some(_)) => problems.push(format!(
            "step {step_id}: `if` and `infer` are mutually exclusive"
        )),
        (None, None) => problems.push(format!(
            "step {step_id}: decide needs `if` or `infer` — an unconditional decide is just steps"
        )),
        _ => {}
    }
    if let Some(condition) = &spec.if_ {
        super::check_templates(condition, seen, step_id, problems);
    }
    if let Some(infer) = &spec.infer {
        super::check_templates(&Value::String(infer.clone()), seen, step_id, problems);
    }
    if let Some(model) = &spec.model {
        super::check_templates(&Value::String(model.clone()), seen, step_id, problems);
    }
    validate_body(
        "then",
        &spec.then,
        seen,
        &[],
        all_plan_ids,
        step_id,
        problems,
    );
    if let Some(else_) = &spec.else_ {
        validate_body("else", else_, seen, &[], all_plan_ids, step_id, problems);
    }
}

impl Pipeline {
    pub(super) async fn run_decide(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let run = self
            .eval_decide(&StepPath::top(&step.id), &step.input, &state.results)
            .await;
        match run {
            Ok(run) => {
                state.branch_steps_executed += run.steps_executed;
                state.bus.extend(run.bus);
                Ok(run.result)
            }
            Err(e) => {
                state.branch_steps_executed += e.steps_executed;
                state.bus.extend(e.bus);
                Err(match e.fail {
                    BodyFail::Render(e @ RenderError::EmptyData { .. }) => ExecutionEnd::Empty {
                        step: step.id.clone(),
                        message: e.to_string(),
                    },
                    BodyFail::Render(e) => ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: DECIDE_TOOL.to_string(),
                        message: e.to_string(),
                    },
                    BodyFail::Tool(message) => ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: DECIDE_TOOL.to_string(),
                        message,
                    },
                    BodyFail::Aborted(error) => ExecutionEnd::Aborted {
                        step: step.id.clone(),
                        error,
                    },
                    BodyFail::Exited(exit) => ExecutionEnd::Exited(exit),
                })
            }
        }
    }

    pub(super) async fn eval_decide(
        &self,
        path: &StepPath,
        input: &Map<String, Value>,
        scope: &Map<String, Value>,
    ) -> Result<BodyRun, BodyError> {
        let fail = |fail: BodyFail| BodyError {
            fail,
            steps_executed: 0,
            bus: Vec::new(),
        };
        let path_text = path.to_string();
        let spec: DecideSpec = serde_json::from_value(Value::Object(input.clone()))
            .map_err(|e| fail(BodyFail::Tool(format!("invalid decide step input: {e}"))))?;

        let roots = Roots::new(scope);
        let mut gate_payload = Map::new();
        let condition = match &spec.if_ {
            Some(raw) => {
                let rendered = render_input(raw, &roots).map_err(|e| fail(BodyFail::Render(e)))?;
                gate_payload.insert("if".to_string(), rendered.clone());
                Some(
                    serde_json::from_value::<Condition>(rendered).map_err(|e| {
                        fail(BodyFail::Tool(format!("invalid decide condition: {e}")))
                    })?,
                )
            }
            None => None,
        };
        let infer = match &spec.infer {
            Some(question) => {
                let rendered =
                    render_str(question, &roots).map_err(|e| fail(BodyFail::Render(e)))?;
                gate_payload.insert("infer".to_string(), json!(rendered));
                Some(rendered)
            }
            None => None,
        };
        let model = match &spec.model {
            Some(model) => {
                let rendered = render_str(model, &roots).map_err(|e| fail(BodyFail::Render(e)))?;
                gate_payload.insert("model".to_string(), json!(rendered));
                Some(rendered)
            }
            None => None,
        };

        self.events
            .tool_started(DECIDE_TOOL, &Value::Object(gate_payload));
        let started = std::time::Instant::now();
        let eval = crate::usage::CallSite::role("judge")
            .at(&path_text)
            .in_plans(&self.call_stack)
            .scope(evaluate_gate(
                condition.as_ref(),
                infer.as_deref(),
                model.as_deref(),
                &self.router,
            ))
            .await;
        self.events
            .tool_finished(DECIDE_TOOL, started.elapsed(), eval.is_err());
        let (triggered, reason) =
            eval.map_err(|e| fail(BodyFail::Tool(format!("decide step: {e}"))))?;

        let (branch_name, raw_branch) = if triggered {
            ("then", Some(&spec.then))
        } else {
            ("else", spec.else_.as_ref())
        };
        let info = |content: String| BusEntry {
            source: path_text.clone(),
            kind: BusKind::Info,
            content,
        };
        let Some(raw_branch) = raw_branch else {
            return Ok(BodyRun {
                result: json!({
                    "branch": null,
                    "verdict": false,
                    "reason": reason,
                    "result": null,
                }),
                steps_executed: 0,
                bus: vec![info("gate not met, no else — continuing".to_string())],
            });
        };

        let branch = parse_branch(branch_name, raw_branch).map_err(|e| fail(BodyFail::Tool(e)))?;
        let bus_path = path
            .nested(branch_name, None)
            .body
            .unwrap_or_else(|| branch_name.to_string());
        let mut run = self
            .run_body(
                &path.step,
                &bus_path,
                &format!("`{branch_name}` branch"),
                &branch,
                scope,
                &[],
            )
            .await?;
        run.bus.push(info(format!("decide → {branch_name}")));
        run.result = json!({
            "branch": branch_name,
            "verdict": triggered,
            "reason": reason,
            "result": run.result,
        });
        Ok(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "then": {"toolName": "map", "input": {}},
        }))
        .unwrap();
        let mut problems = Vec::new();
        validate_decide_input(&input, &["input"], &["E0"], "E0", &mut problems);
        assert!(problems.iter().any(|p| p.contains("`if` or `infer`")));
        assert!(problems.iter().any(|p| p.contains("cannot nest")));
    }

    #[test]
    fn branches_may_contain_exit() {
        let input: Map<String, Value> = serde_json::from_value(json!({
            "if": {"value": "{{input.n}}", "op": "gt", "to": 0},
            "then": {"toolName": "exit", "input": {"status": "success", "message": "done"}},
            "else": [
                {"id": "bail", "toolName": "exit", "input": {"status": "error", "message": "no"}},
            ],
        }))
        .unwrap();
        let mut problems = Vec::new();
        validate_decide_input(&input, &["input"], &["E0"], "E0", &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }
}
