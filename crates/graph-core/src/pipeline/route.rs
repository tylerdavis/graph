//! The `route` fork: a step that routes execution into one of two
//! branches (`then`/`else`), gated like an `exit` step by a logical
//! condition (`if`) or an inferred verdict (`infer`). Intercepted by
//! the executor — never dispatched to a tool registry. Only the chosen
//! branch is rendered and run: the other side's templates are never
//! evaluated, so each branch may safely reference data that only exists
//! when that branch is the right one to take.

use super::body::{body_schema, parse_branch, validate_body, BodyError, BodyFail, BodyRun};
use super::condition::{
    check_gate, decide_choice, select_gate, with_probability, ChoiceOutcome, Condition, DecideGate,
    GateOutcome,
};
use super::gate::StepPath;
use super::state::{BusEntry, BusKind};
use super::{ExecutionEnd, Pipeline, RunState, Step};
use crate::template::{render_input, render_str, RenderError, Roots};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// Reserved step tool name.
pub const ROUTE_TOOL: &str = "route";

/// The route step's input, parsed from the RAW (unrendered) step input:
/// the condition and branches stay as plain values so rendering can be
/// deferred until the gate has picked a side.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
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
    #[serde(default)]
    pub decide: Option<Value>,
    /// Branch taken when the gate holds.
    #[serde(default)]
    pub then: Option<Value>,
    #[serde(default)]
    pub cases: Option<Map<String, Value>>,
    /// Branch taken otherwise; absent means the plan just continues.
    #[serde(rename = "else", default)]
    pub else_: Option<Value>,
}

/// The route step as described to the planner.
pub fn route_tool_def() -> crate::tools::ToolDef {
    let branch_schema = body_schema();
    crate::tools::ToolDef {
        name: ROUTE_TOOL.to_string(),
        description: "Fork the plan on a condition: run `then` when it holds, otherwise \
                      `else` (or continue if `else` is omitted). Use it when the correct \
                      next call depends on a prior result — e.g. update an existing record \
                      vs. create a new one. Gate it with exactly one of `if` (a logical \
                      comparison), `infer` (a yes/no question judged against prior \
                      results), or `decide` (that question asked of a decision model, when \
                      one is configured). A branch is a single tool call or a list of steps; \
                      branches may contain `exit`, `agent`, `ask`, `filter`, and \
                      `route` steps (a fired exit ends the WHOLE plan) but never `map` \
                      or `reduce` — call a plan (plan__*) for nested iteration. Later \
                      steps reference this step's id: {{Ex.result}} is the chosen branch's output, \
                      {{Ex.branch}} which side ran. With a decision model configured, \
                      `decide.options` turns the fork N-way: `cases` holds one branch per \
                      option key, the model picks one, and `else` runs when its \
                      confidence is below `decide.min_confidence`."
            .to_string(),
        input_schema: json!({
            "type": "object",
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
                "decide": route_decide_schema(),
                "model": {"type": "string", "description": "Model role for the `infer` verdict (any configured role, standard or custom); defaults to the judge role."},
                "then": branch_schema.clone(),
                "cases": {
                    "type": "object",
                    "description": "One branch per `decide.options` key, used instead of `then`.",
                    "additionalProperties": branch_schema.clone()
                },
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

fn route_decide_schema() -> Value {
    let mut schema = super::condition::decide_gate_schema(
        "A question answered by a decision model (only when one is configured). Without `options` it is yes/no and runs `then` when its probability reaches min_confidence; with `options` the model picks one and the matching entry in `cases` runs.",
        "The data to judge, usually a template like {{E2.text}}",
    );
    schema["properties"]["options"] = json!({
        "type": "object",
        "description": "Case name → what that case means; 2 to 255 entries. Each name needs a branch under `cases`.",
        "additionalProperties": {"type": "string"}
    });
    schema
}

/// Static validation of a route step's raw input: gate arity, branch
/// shape, branch tool names, branch-step ids, and template reference
/// ordering. `seen` is the ids available before this step (including
/// `input`); `all_plan_ids` is every top-level id, for collision checks.
pub fn validate_route_input(
    input: &Map<String, Value>,
    seen: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: RouteSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid route input: {e}"));
            return;
        }
    };
    match [spec.if_.is_some(), spec.infer.is_some(), spec.decide.is_some()]
        .iter()
        .filter(|set| **set)
        .count()
    {
        0 => problems.push(format!(
            "step {step_id}: route needs `if`, `infer`, or `decide` — an unconditional route is just steps"
        )),
        1 => {}
        _ => problems.push(format!(
            "step {step_id}: `if`, `infer`, and `decide` are mutually exclusive"
        )),
    }
    if let Some(gate) = &spec.decide {
        super::check_decide_gate_shape(gate, true, step_id, problems);
        super::check_templates(gate, seen, step_id, problems);
        if spec.model.is_some() {
            problems.push(format!(
                "step {step_id}: `model` sits inside the `decide` object; move it there"
            ));
        }
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
    let options = spec
        .decide
        .as_ref()
        .and_then(|gate| gate.get("options"))
        .and_then(Value::as_object);
    match (options, &spec.cases) {
        (Some(options), Some(cases)) => {
            if spec.then.is_some() {
                problems.push(format!(
                    "step {step_id}: a route with `cases` takes no `then`; each case is its own branch"
                ));
            }
            let missing: Vec<&str> = options
                .keys()
                .filter(|key| !cases.contains_key(*key))
                .map(String::as_str)
                .collect();
            let extra: Vec<&str> = cases
                .keys()
                .filter(|key| !options.contains_key(*key))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                problems.push(format!(
                    "step {step_id}: `cases` is missing a branch for {}",
                    missing.join(", ")
                ));
            }
            if !extra.is_empty() {
                problems.push(format!(
                    "step {step_id}: `cases` has a branch for {}, which is not in `decide.options`",
                    extra.join(", ")
                ));
            }
            let has_min_confidence = spec
                .decide
                .as_ref()
                .is_some_and(|gate| gate.get("min_confidence").is_some());
            if spec.else_.is_some() && !has_min_confidence {
                problems.push(format!(
                    "step {step_id}: `else` can never run under `cases` without `decide.min_confidence`"
                ));
            }
            for (key, body) in cases {
                validate_body(key, body, seen, &[], all_plan_ids, step_id, problems);
            }
        }
        (Some(_), None) => problems.push(format!(
            "step {step_id}: `decide.options` needs `cases`, one branch per option"
        )),
        (None, Some(_)) => problems.push(format!(
            "step {step_id}: `cases` needs `decide.options` to choose between them"
        )),
        (None, None) => match &spec.then {
            Some(then) => validate_body("then", then, seen, &[], all_plan_ids, step_id, problems),
            None => problems.push(format!("step {step_id}: route needs `then`")),
        },
    }
    if let Some(else_) = &spec.else_ {
        validate_body("else", else_, seen, &[], all_plan_ids, step_id, problems);
    }
}

impl Pipeline {
    /// Execute a route step: render only the condition, evaluate the
    /// gate, then render and run just the chosen branch. Ok carries the
    /// value to store under the route step's id.
    pub(super) async fn run_route(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let run = self
            .eval_route(&StepPath::top(&step.id), &step.input, &state.results)
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
                        tool: ROUTE_TOOL.to_string(),
                        message: e.to_string(),
                    },
                    BodyFail::Tool(message) => ExecutionEnd::Failed {
                        step: step.id.clone(),
                        tool: ROUTE_TOOL.to_string(),
                        message,
                    },
                    BodyFail::Aborted(error) => ExecutionEnd::Aborted {
                        step: step.id.clone(),
                        error,
                    },
                    // Not a failure: an exit in the branch ends the plan.
                    BodyFail::Exited(exit) => ExecutionEnd::Exited(exit),
                })
            }
        }
    }

    pub(super) async fn eval_route(
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
        let failed = |message: String| fail(BodyFail::Tool(message));
        let render_end = |e: RenderError| fail(BodyFail::Render(e));
        let path_text = path.to_string();

        let spec: RouteSpec = serde_json::from_value(Value::Object(input.clone()))
            .map_err(|e| failed(format!("invalid route step input: {e}")))?;

        // Render only the condition; the branches wait until the gate has
        // picked a side.
        let roots = Roots::new(scope);
        let mut gate_payload = Map::new();
        let condition = match &spec.if_ {
            Some(raw) => {
                let rendered = render_input(raw, &roots).map_err(render_end)?;
                gate_payload.insert("if".to_string(), rendered.clone());
                Some(
                    serde_json::from_value::<Condition>(rendered)
                        .map_err(|e| failed(format!("invalid route condition: {e}")))?,
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
        let model = match &spec.model {
            Some(model) => {
                let rendered = render_str(model, &roots).map_err(render_end)?;
                gate_payload.insert("model".to_string(), json!(rendered));
                Some(rendered)
            }
            None => None,
        };
        let decide = match &spec.decide {
            Some(raw) => {
                let rendered = render_input(raw, &roots).map_err(render_end)?;
                gate_payload.insert("decide".to_string(), rendered.clone());
                Some(
                    serde_json::from_value::<DecideGate>(rendered)
                        .map_err(|e| failed(format!("invalid decide gate: {e}")))?,
                )
            }
            None => None,
        };

        self.events
            .tool_started(ROUTE_TOOL, &Value::Object(gate_payload));
        let started = std::time::Instant::now();
        let eval = crate::usage::CallSite::role("judge")
            .at(&path_text)
            .in_plans(&self.call_stack)
            .scope(async {
                if let Some(gate) = decide.as_ref().filter(|gate| gate.options.is_some()) {
                    if model.is_some() {
                        return Err(
                            "`model` sits inside the `decide` object; move it there".to_string()
                        );
                    }
                    return decide_choice(gate, &self.router)
                        .await
                        .map(RouteOutcome::Choice);
                }
                let gate = select_gate(
                    condition.as_ref(),
                    infer.as_deref(),
                    decide.as_ref(),
                    model.as_deref(),
                    "if",
                )?
                .ok_or_else(|| "a route step needs `if`, `infer`, or `decide`".to_string())?;
                check_gate(gate, &self.router).await.map(RouteOutcome::Gate)
            })
            .await;
        self.events
            .tool_finished(ROUTE_TOOL, started.elapsed(), eval.is_err());
        let outcome = eval.map_err(|e| failed(format!("route step: {e}")))?;
        let decision = match outcome {
            RouteOutcome::Gate(outcome) => {
                let raw = if outcome.triggered {
                    spec.then.as_ref()
                } else {
                    spec.else_.as_ref()
                };
                let branch = if outcome.triggered { "then" } else { "else" };
                Decision {
                    branch: branch.to_string(),
                    raw,
                    fields: with_probability(
                        json!({"verdict": outcome.triggered, "reason": outcome.reason}),
                        outcome.probability,
                    ),
                }
            }
            RouteOutcome::Choice(choice) => {
                let fields = json!({
                    "choice": choice.choice,
                    "confidence": choice.confidence,
                    "probabilities": choice.probabilities,
                });
                if choice.committed {
                    let raw = spec
                        .cases
                        .as_ref()
                        .and_then(|cases| cases.get(&choice.choice));
                    if raw.is_none() {
                        return Err(failed(format!(
                            "route step: no case for the chosen option '{}'",
                            choice.choice
                        )));
                    }
                    Decision {
                        branch: choice.choice,
                        raw,
                        fields,
                    }
                } else {
                    Decision {
                        branch: "else".to_string(),
                        raw: spec.else_.as_ref(),
                        fields,
                    }
                }
            }
        };
        let info = |content: String| BusEntry {
            source: path_text.clone(),
            kind: BusKind::Info,
            content,
        };
        let branch_name = decision.branch.as_str();
        let Some(raw_branch) = decision.raw else {
            return Ok(BodyRun {
                result: route_result(Value::Null, decision.fields, Value::Null),
                steps_executed: 0,
                bus: vec![info("gate not met, no else — continuing".to_string())],
            });
        };

        let branch = parse_branch(branch_name, raw_branch).map_err(failed)?;
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
        run.bus.push(info(format!("route → {branch_name}")));
        run.result = route_result(json!(branch_name), decision.fields, run.result);
        Ok(run)
    }
}

enum RouteOutcome {
    Gate(GateOutcome),
    Choice(ChoiceOutcome),
}

struct Decision<'a> {
    branch: String,
    raw: Option<&'a Value>,
    fields: Value,
}

fn route_result(branch: Value, fields: Value, result: Value) -> Value {
    let mut out = json!({"branch": branch, "result": result});
    if let (Some(out), Some(fields)) = (out.as_object_mut(), fields.as_object()) {
        for (key, value) in fields {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_rejects_unknown_fields() {
        let err = serde_json::from_value::<RouteSpec>(json!({
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
        validate_route_input(&input, &["input"], &["E0"], "E0", &mut problems);
        assert!(problems
            .iter()
            .any(|p| p.contains("`if`, `infer`, or `decide`")));
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
        validate_route_input(&input, &["input"], &["E0"], "E0", &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }
}
