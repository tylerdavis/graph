//! The `map` and `reduce` iteration steps: run a body once per item of a
//! list. Intercepted by the executor — never dispatched to a tool
//! registry. Only `over` (and reduce's `initial`) renders up front; the
//! body renders lazily per item, in a scope layered over the plan's
//! results plus per-item pseudo-roots (`item`, `index`, and for reduce
//! the running `accumulator`). `map` collects per-item results in input
//! order and may run items concurrently; `reduce` is a strict left fold —
//! each iteration depends on the previous, so it is always sequential.

use super::body::{body_schema, parse_branch, validate_body, BodyError, BodyFail, BodyRun};
use super::state::BusKind;
use super::{ExecutionEnd, Pipeline, RunState, Step};
use crate::template::{render_input, RenderError, Roots};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};

/// Reserved step tool names.
pub const MAP_TOOL: &str = "map";
pub const REDUCE_TOOL: &str = "reduce";

/// The map step's input, parsed from the RAW (unrendered) step input: the
/// body stays a plain value so rendering can be deferred until each item's
/// scope exists.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MapSpec {
    /// The list to iterate — usually a template (typed splice keeps it an
    /// array), sometimes a literal array.
    pub over: Value,
    /// Body run once per item: a single tool call or an inline step list.
    #[serde(rename = "do")]
    pub do_: Value,
    /// Maximum items in flight; 1 (the default) runs items sequentially.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_concurrency() -> usize {
    1
}

/// The reduce step's input. No `concurrency`: a fold's every iteration
/// reads the previous accumulator.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReduceSpec {
    pub over: Value,
    #[serde(rename = "do")]
    pub do_: Value,
    /// Starting accumulator; rendered up front, defaults to null.
    #[serde(default)]
    pub initial: Value,
}

/// The map step as described to the planner.
pub fn map_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: MAP_TOOL.to_string(),
        description: "Run the same body once per item of a list. `over` must produce an \
                      array (usually a template like {{E0.issues}}); the body (`do`) runs \
                      once per element with {{item}} (the element) and {{index}} (0-based) \
                      available alongside earlier step results. Per-item results are \
                      collected in input order: later steps reference {{Ex.results}} for \
                      the list and {{Ex.count}} for how many ran. Set `concurrency` above \
                      1 only when the per-item calls are independent. The body may not \
                      contain `exit`, `decide`, `map`, or `reduce` — call a plan (plan__*) \
                      for nested control flow."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["over", "do"],
            "properties": {
                "over": {"description": "The list to iterate — usually a template like {{E0.issues}} that resolves to an array."},
                "do": body_schema(false),
                "concurrency": {"type": "integer", "minimum": 1, "description": "Maximum items in flight; 1 (default) runs items one at a time."}
            }
        }),
        output_schema: None,
        output_example: Some(json!({"count": 3, "results": [{"…": "…"}, {"…": "…"}, {"…": "…"}]})),
        read_only: None, // effect depends entirely on what the body calls
    }
}

/// The reduce step as described to the planner.
pub fn reduce_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: REDUCE_TOOL.to_string(),
        description: "Fold a list into a single value. `over` must produce an array; the \
                      body (`do`) runs once per element in order with {{accumulator}} \
                      (the running value, starting at `initial`), {{item}}, and {{index}} \
                      available, and each run's result becomes the next {{accumulator}}. \
                      Later steps reference {{Ex.result}} for the final value. Always \
                      sequential — each iteration depends on the previous; for \
                      independent per-item work use `map` (optionally concurrent) and \
                      reduce over its results. The body may not contain `exit`, `decide`, \
                      `map`, or `reduce` — call a plan (plan__*) for nested control flow."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["over", "do"],
            "properties": {
                "over": {"description": "The list to fold — usually a template like {{E1.results}} that resolves to an array."},
                "do": body_schema(false),
                "initial": {"description": "Starting accumulator value (any JSON; may use templates). Defaults to null."}
            }
        }),
        output_schema: None,
        output_example: Some(json!({"count": 3, "result": {"…": "…"}})),
        read_only: None, // effect depends entirely on what the body calls
    }
}

/// Static validation of a map step's raw input: spec shape, body shape and
/// tool names, template reference ordering, and pseudo-root placement.
pub fn validate_map_input(
    input: &Map<String, Value>,
    seen: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: MapSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid map input: {e}"));
            return;
        }
    };
    if spec.concurrency == 0 {
        problems.push(format!("step {step_id}: `concurrency` must be at least 1"));
    }
    check_eager_field(&spec.over, seen, step_id, problems);
    // `accumulator` is passed as an available root so the pointed message
    // below is the only one reported for it.
    let pseudo = ["item", "index", "accumulator"];
    validate_body(
        "do",
        &spec.do_,
        seen,
        &pseudo,
        all_plan_ids,
        step_id,
        false,
        problems,
    );
    if template_roots(&spec.do_).iter().any(|r| r == "accumulator") {
        problems.push(format!(
            "step {step_id}: `{{{{accumulator}}}}` is only available inside a reduce body"
        ));
    }
}

/// Static validation of a reduce step's raw input.
pub fn validate_reduce_input(
    input: &Map<String, Value>,
    seen: &[&str],
    all_plan_ids: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: ReduceSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid reduce input: {e}"));
            return;
        }
    };
    check_eager_field(&spec.over, seen, step_id, problems);
    check_eager_field(&spec.initial, seen, step_id, problems);
    let pseudo = ["item", "index", "accumulator"];
    validate_body(
        "do",
        &spec.do_,
        seen,
        &pseudo,
        all_plan_ids,
        step_id,
        false,
        problems,
    );
}

/// `over` and `initial` render before any item exists, so pseudo-roots in
/// them can never resolve — catch that statically. The generic walk runs
/// with the pseudo-roots marked available so the pointed message below is
/// the only one reported for them.
fn check_eager_field(value: &Value, seen: &[&str], step_id: &str, problems: &mut Vec<String>) {
    let mut available: Vec<&str> = seen.to_vec();
    available.extend(["item", "index", "accumulator"]);
    super::check_templates(value, &available, step_id, problems);
    for pseudo in ["item", "index", "accumulator"] {
        if template_roots(value).iter().any(|r| r == pseudo) {
            problems.push(format!(
                "step {step_id}: `{{{{{pseudo}}}}}` is only available inside `do`"
            ));
        }
    }
}

/// Every template root referenced anywhere in a value tree. Parse errors
/// are ignored here — `check_templates`/`validate_body` already report them.
fn template_roots(value: &Value) -> Vec<String> {
    let mut roots = Vec::new();
    collect_roots(value, &mut roots);
    roots
}

fn collect_roots(value: &Value, roots: &mut Vec<String>) {
    match value {
        Value::String(s) if s.contains("{{") => {
            if let Ok(mut found) = crate::template::referenced_roots(s) {
                roots.append(&mut found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_roots(item, roots)),
        Value::Object(map) => map.values().for_each(|child| collect_roots(child, roots)),
        _ => {}
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

impl Pipeline {
    /// Execute a map step: render `over`, then run the body once per item
    /// — up to `concurrency` items in flight, results in input order. Ok
    /// carries the value to store under the map step's id.
    pub(super) async fn run_map(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let failed = |message: String| ExecutionEnd::Failed {
            step: step.id.clone(),
            tool: MAP_TOOL.to_string(),
            message,
        };
        let render_end = |e: RenderError| match e {
            e @ RenderError::EmptyData { .. } => ExecutionEnd::Empty {
                step: step.id.clone(),
                message: e.to_string(),
            },
            e => ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: MAP_TOOL.to_string(),
                message: e.to_string(),
            },
        };

        let spec: MapSpec = serde_json::from_value(Value::Object(step.input.clone()))
            .map_err(|e| failed(format!("invalid map step input: {e}")))?;
        let concurrency = spec.concurrency.max(1);

        // Render only `over`; the body renders per item.
        let over = render_input(&spec.over, &Roots::new(&state.results)).map_err(render_end)?;
        let Value::Array(items) = over else {
            return Err(failed(format!(
                "`over` must produce an array, got {}",
                type_name(&over)
            )));
        };
        let branch = parse_branch("do", &spec.do_).map_err(failed)?;

        self.events.tool_started(
            MAP_TOOL,
            &json!({"over": items.len(), "concurrency": concurrency}),
        );
        let started = std::time::Instant::now();

        // Items run against an immutable snapshot of the plan's results —
        // they never see each other. On failure the stream drains: items
        // already in flight run to completion (cancelling mid-call would
        // orphan MCP requests and lose accounting); items not yet started
        // are skipped.
        let base = state.results.clone();
        let halted = AtomicBool::new(false);
        let branch = &branch;
        let base_ref = &base;
        let halted_ref = &halted;
        let step_id = step.id.as_str();
        // Boxed so the item-future type stays nameable across the
        // recursive dispatch → call_plan cycle.
        let item_futures: Vec<futures::future::BoxFuture<'_, Option<Result<BodyRun, BodyError>>>> =
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let future = async move {
                        if halted_ref.load(Ordering::Relaxed) {
                            return None;
                        }
                        let extras = [("item", item.clone()), ("index", json!(index))];
                        let run = self
                            .run_body(
                                step_id,
                                &format!("do.{index}"),
                                &format!("`do` item {index}"),
                                branch,
                                base_ref,
                                &extras,
                            )
                            .await;
                        if run.is_err() {
                            halted_ref.store(true, Ordering::Relaxed);
                        }
                        Some(run)
                    };
                    Box::pin(future) as futures::future::BoxFuture<'_, _>
                })
                .collect();
        let outcomes: Vec<Option<Result<BodyRun, BodyError>>> = futures::stream::iter(item_futures)
            .buffered(concurrency)
            .collect()
            .await;

        // Merge in input order: `buffered` (unlike `buffer_unordered`)
        // yields outputs in stream order, so `results` needs no sorting and
        // the reported failure is the lowest-index one.
        let mut results = Vec::with_capacity(items.len());
        let mut failure: Option<BodyFail> = None;
        for outcome in outcomes.into_iter().flatten() {
            match outcome {
                Ok(run) => {
                    state.branch_steps_executed += run.steps_executed;
                    state.bus.extend(run.bus);
                    results.push(run.result);
                }
                Err(e) => {
                    state.branch_steps_executed += e.steps_executed;
                    state.bus.extend(e.bus);
                    failure.get_or_insert(e.fail);
                }
            }
        }
        if let Some(fail) = failure {
            self.events.tool_finished(MAP_TOOL, started.elapsed(), true);
            return Err(match fail {
                BodyFail::Render(e) => render_end(e),
                BodyFail::Tool(message) => failed(message),
                BodyFail::Aborted => ExecutionEnd::Aborted {
                    step: step.id.clone(),
                },
                // Validation forbids exit in iteration bodies; defensive.
                BodyFail::Exited(_) => {
                    failed("`exit` fired inside a map body — not supported".to_string())
                }
            });
        }
        self.events
            .tool_finished(MAP_TOOL, started.elapsed(), false);
        state.push_bus(
            &step.id,
            BusKind::Info,
            format!("map: {} items", results.len()),
        );
        Ok(json!({"count": results.len(), "results": results}))
    }

    /// Execute a reduce step: render `over` and `initial`, then fold the
    /// body over the items left to right, threading the accumulator. Ok
    /// carries the value to store under the reduce step's id.
    pub(super) async fn run_reduce(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let failed = |message: String| ExecutionEnd::Failed {
            step: step.id.clone(),
            tool: REDUCE_TOOL.to_string(),
            message,
        };
        let render_end = |e: RenderError| match e {
            e @ RenderError::EmptyData { .. } => ExecutionEnd::Empty {
                step: step.id.clone(),
                message: e.to_string(),
            },
            e => ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: REDUCE_TOOL.to_string(),
                message: e.to_string(),
            },
        };

        let spec: ReduceSpec = serde_json::from_value(Value::Object(step.input.clone()))
            .map_err(|e| failed(format!("invalid reduce step input: {e}")))?;

        // Render `over` and `initial` up front; the body renders per item.
        let roots = Roots::new(&state.results);
        let over = render_input(&spec.over, &roots).map_err(render_end)?;
        let Value::Array(items) = over else {
            return Err(failed(format!(
                "`over` must produce an array, got {}",
                type_name(&over)
            )));
        };
        let mut accumulator = render_input(&spec.initial, &roots).map_err(render_end)?;
        let branch = parse_branch("do", &spec.do_).map_err(failed)?;

        self.events
            .tool_started(REDUCE_TOOL, &json!({"over": items.len()}));
        let started = std::time::Instant::now();

        let base = state.results.clone();
        let count = items.len();
        for (index, item) in items.into_iter().enumerate() {
            let extras = [
                ("accumulator", accumulator.clone()),
                ("item", item),
                ("index", json!(index)),
            ];
            let run = self
                .run_body(
                    &step.id,
                    &format!("do.{index}"),
                    &format!("`do` item {index}"),
                    &branch,
                    &base,
                    &extras,
                )
                .await;
            match run {
                Ok(run) => {
                    state.branch_steps_executed += run.steps_executed;
                    state.bus.extend(run.bus);
                    accumulator = run.result;
                }
                Err(e) => {
                    state.branch_steps_executed += e.steps_executed;
                    state.bus.extend(e.bus);
                    self.events
                        .tool_finished(REDUCE_TOOL, started.elapsed(), true);
                    return Err(match e.fail {
                        BodyFail::Render(e) => render_end(e),
                        BodyFail::Tool(message) => failed(message),
                        BodyFail::Aborted => ExecutionEnd::Aborted {
                            step: step.id.clone(),
                        },
                        // Validation forbids exit in iteration bodies; defensive.
                        BodyFail::Exited(_) => {
                            failed("`exit` fired inside a reduce body — not supported".to_string())
                        }
                    });
                }
            }
        }
        self.events
            .tool_finished(REDUCE_TOOL, started.elapsed(), false);
        state.push_bus(&step.id, BusKind::Info, format!("reduce: {count} items"));
        Ok(json!({"count": count, "result": accumulator}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems_for_map(input: Value) -> Vec<String> {
        let input: Map<String, Value> = serde_json::from_value(input).unwrap();
        let mut problems = Vec::new();
        validate_map_input(&input, &["input", "E0"], &["E0", "E1"], "E1", &mut problems);
        problems
    }

    #[test]
    fn map_spec_rejects_unknown_fields_and_zero_concurrency() {
        let problems = problems_for_map(json!({
            "over": "{{E0.values}}",
            "do": {"toolName": "t__x", "input": {}},
            "parallel": 4,
        }));
        assert!(problems[0].contains("parallel"), "{problems:?}");

        let problems = problems_for_map(json!({
            "over": "{{E0.values}}",
            "do": {"toolName": "t__x", "input": {}},
            "concurrency": 0,
        }));
        assert!(
            problems.iter().any(|p| p.contains("at least 1")),
            "{problems:?}"
        );
    }

    #[test]
    fn pseudo_roots_are_scoped_to_the_body() {
        let problems = problems_for_map(json!({
            "over": "{{item.children}}",
            "do": {"toolName": "t__x", "input": {"q": "{{item.id}}"}},
        }));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("only available inside `do`")),
            "{problems:?}"
        );

        let problems = problems_for_map(json!({
            "over": "{{E0.values}}",
            "do": {"toolName": "t__x", "input": {"q": "{{accumulator}}"}},
        }));
        assert!(
            problems.iter().any(|p| p.contains("reduce body")),
            "{problems:?}"
        );

        // The legitimate pseudo-roots pass.
        let problems = problems_for_map(json!({
            "over": "{{E0.values}}",
            "do": {"toolName": "t__x", "input": {"q": "{{item.id}}", "n": "{{index}}"}},
        }));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn reduce_validation_allows_accumulator_in_body_only() {
        let input: Map<String, Value> = serde_json::from_value(json!({
            "over": "{{E0.values}}",
            "initial": "{{accumulator}}",
            "do": {"toolName": "t__x", "input": {"a": "{{accumulator}}", "b": "{{item}}"}},
        }))
        .unwrap();
        let mut problems = Vec::new();
        validate_reduce_input(&input, &["input", "E0"], &["E0", "E1"], "E1", &mut problems);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("only available inside `do`"),
            "{problems:?}"
        );
    }
}
