//! The `filter` selection step: partition a list with a per-item gate —
//! a logical condition (`where`) or an inferred verdict (`infer`), the
//! same gate grammar `exit` and `decide` use, evaluated once per item
//! with `item`/`index` pseudo-roots in scope. Intercepted by the
//! executor — never dispatched to a tool registry, and (like every
//! control step's evaluation) never consulted with the execution gate:
//! selection makes no tool call. That is also why, unlike its siblings,
//! `filter` may nest inside `decide`/`map`/`reduce` bodies — inside a
//! body its `where`/`infer` see their own `item`/`index`, shadowing the
//! enclosing body's. Both halves of the partition are returned (`items`
//! and `dropped`): selection narrows what runs next, never what is
//! known.

use super::condition::{evaluate_gate, Condition};
use super::iterate::{template_roots, type_name};
use super::state::BusKind;
use super::{ExecutionEnd, Pipeline, RunState, Step};
use crate::template::{render_input, render_str, RenderError, Roots};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};

/// Reserved step tool name.
pub const FILTER_TOOL: &str = "filter";

/// The filter step's input, parsed from the RAW (unrendered) step input:
/// the gate stays a plain value so it can render per item, against a
/// scope that carries that item.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilterSpec {
    /// The list to partition — usually a template (typed splice keeps it
    /// an array), sometimes a literal array.
    pub over: Value,
    /// Logical per-item gate. Exactly one of `where`/`infer` is required.
    #[serde(rename = "where", default)]
    pub where_: Option<Value>,
    /// Inferred per-item gate: a yes/no question about `{{item}}`,
    /// answered by the `judge` model role — one verdict per item.
    #[serde(default)]
    pub infer: Option<String>,
    /// Model for `infer` verdicts: a role name, `default`, or a
    /// `[models.named]` entry. Defaults to the `judge` role. Ignored
    /// without `infer`.
    #[serde(default)]
    pub model: Option<String>,
    /// Maximum `infer` verdicts in flight; 1 (the default) evaluates
    /// sequentially. Irrelevant for `where` — pure computation.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_concurrency() -> usize {
    1
}

/// The filter step as described to the planner.
pub fn filter_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: FILTER_TOOL.to_string(),
        description: "Partition a list with a per-item gate. `over` must produce an array \
                      (usually a template like {{E0.issues}}); the gate — exactly one of \
                      `where` (a logical condition) or `infer` (a yes/no question judged \
                      per item) — is evaluated once per element with {{item}} and \
                      {{index}} available. Later steps reference {{Ex.items}} (elements \
                      that passed, input order) with {{Ex.count}}, and {{Ex.dropped}} \
                      (elements that did not) with {{Ex.dropped_count}}. Use it to select \
                      before iterating — e.g. filter a changed-file list to non-deleted \
                      entries, then map over {{Ex.items}} — instead of mapping over \
                      entries a later call cannot handle. `infer` costs one judge call \
                      per item (set `concurrency` to run them in parallel); prefer \
                      `where` whenever a field comparison can decide. Unlike other \
                      control steps, `filter` may appear inside `decide`/`map`/`reduce` \
                      bodies; its {{item}}/{{index}} shadow the enclosing body's inside \
                      the gate."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["over"],
            "properties": {
                "over": {"description": "The list to partition — usually a template like {{E0.issues}} that resolves to an array."},
                "where": {
                    "type": "object",
                    "required": ["value", "op"],
                    "properties": {
                        "value": {"description": "Usually a template about the current element, like {{item.status}}"},
                        "op": {"type": "string", "enum": ["eq","ne","gt","lt","gte","lte","empty","not_empty","contains"]},
                        "to": {"description": "Comparison operand (omit for empty/not_empty)"}
                    }
                },
                "infer": {"type": "string", "description": "A yes/no question about {{item}}; the element is kept on yes. One judge call per item."},
                "model": {"type": "string", "description": "Model for `infer` verdicts (a named model or role); defaults to the judge role."},
                "concurrency": {"type": "integer", "minimum": 1, "description": "Maximum `infer` verdicts in flight; 1 (default) evaluates one at a time."}
            }
        }),
        output_schema: None,
        output_example: Some(json!({
            "count": 2,
            "items": [{"…": "…"}, {"…": "…"}],
            "dropped": [{"…": "…"}],
            "dropped_count": 1
        })),
        read_only: Some(true), // evaluates conditions; never dispatches
    }
}

/// Static validation of a filter step's raw input: spec shape, gate
/// arity, template reference ordering, and pseudo-root placement. `seen`
/// is the ids available where the step sits — for a filter nested in a
/// `map`/`reduce` body that already includes the body's own pseudo-roots,
/// which is what legitimizes `over: {{item.children}}` there.
pub fn validate_filter_input(
    input: &Map<String, Value>,
    seen: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: FilterSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid filter input: {e}"));
            return;
        }
    };
    match (&spec.where_, &spec.infer) {
        (Some(_), Some(_)) => problems.push(format!(
            "step {step_id}: `where` and `infer` are mutually exclusive"
        )),
        (None, None) => problems.push(format!(
            "step {step_id}: filter needs `where` or `infer` — an ungated filter keeps everything"
        )),
        _ => {}
    }
    if spec.concurrency == 0 {
        problems.push(format!("step {step_id}: `concurrency` must be at least 1"));
    }

    // The gate sees this step's own `item`/`index` on top of the ambient
    // scope (shadowing an enclosing body's, when nested).
    let mut gate_avail: Vec<&str> = seen.to_vec();
    for pseudo in ["item", "index"] {
        if !gate_avail.contains(&pseudo) {
            gate_avail.push(pseudo);
        }
    }
    if let Some(where_) = &spec.where_ {
        super::check_templates(where_, &gate_avail, step_id, problems);
    }
    if let Some(infer) = &spec.infer {
        super::check_templates(
            &Value::String(infer.clone()),
            &gate_avail,
            step_id,
            problems,
        );
    }
    if let Some(model) = &spec.model {
        super::check_templates(&Value::String(model.clone()), seen, step_id, problems);
    }

    // `over` renders before any item exists. Walk it with the gate scope
    // so the pointed message below is the only one reported for this
    // step's own pseudo-roots — but only pointed when the ambient scope
    // doesn't already provide them (a nested filter's `over` may
    // legitimately read the enclosing body's `{{item}}`).
    super::check_templates(&spec.over, &gate_avail, step_id, problems);
    for pseudo in ["item", "index"] {
        if !seen.contains(&pseudo) && template_roots(&spec.over).iter().any(|r| r == pseudo) {
            problems.push(format!(
                "step {step_id}: `{{{{{pseudo}}}}}` is only available inside `where`/`infer`"
            ));
        }
    }
}

/// How a filter evaluation failed. Mirrors `AskFail`: `Empty` keeps the
/// `RenderError` so data running out degrades instead of failing, at any
/// depth; there is no abort channel — a filter never dispatches.
pub(super) enum FilterFail {
    Empty(RenderError),
    Failed(String),
}

impl Pipeline {
    /// Top-level `filter` step: evaluates against the plan's results map.
    pub(super) async fn run_filter(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        match self.run_filter_scoped(&step.input, &state.results).await {
            Ok(result) => {
                let kept = result.get("count").and_then(Value::as_u64).unwrap_or(0);
                let dropped = result
                    .get("dropped_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                state.push_bus(
                    &step.id,
                    BusKind::Info,
                    format!("filter: kept {kept} of {}", kept + dropped),
                );
                Ok(result)
            }
            Err(FilterFail::Empty(error)) => Err(ExecutionEnd::Empty {
                step: step.id.clone(),
                message: error.to_string(),
            }),
            Err(FilterFail::Failed(message)) => Err(ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: FILTER_TOOL.to_string(),
                message,
            }),
        }
    }

    /// Filter, evaluated against an arbitrary scope so it works
    /// identically at the top level and inside a `decide`/`map`/`reduce`
    /// body. Per-item evaluation layers `item`/`index` over `scope`,
    /// shadowing an enclosing body's.
    pub(super) async fn run_filter_scoped(
        &self,
        raw_input: &Map<String, Value>,
        scope: &Map<String, Value>,
    ) -> Result<Value, FilterFail> {
        let spec: FilterSpec = serde_json::from_value(Value::Object(raw_input.clone()))
            .map_err(|e| FilterFail::Failed(format!("invalid filter input: {e}")))?;
        // Validation catches these, but an unvalidated plan can reach the
        // executor (a gate-injected draft, a direct API caller).
        match (&spec.where_, &spec.infer) {
            (Some(_), Some(_)) => {
                return Err(FilterFail::Failed(
                    "`where` and `infer` are mutually exclusive".to_string(),
                ))
            }
            (None, None) => {
                return Err(FilterFail::Failed(
                    "filter needs `where` or `infer`".to_string(),
                ))
            }
            _ => {}
        }
        let concurrency = spec.concurrency.max(1);

        let classify = |e: RenderError| match e {
            e @ RenderError::EmptyData { .. } => FilterFail::Empty(e),
            e => FilterFail::Failed(e.to_string()),
        };

        // Render only `over` (and `model`); the gate renders per item.
        let roots = Roots::new(scope);
        let over = render_input(&spec.over, &roots).map_err(classify)?;
        let Value::Array(items) = over else {
            return Err(FilterFail::Failed(format!(
                "`over` must produce an array, got {}",
                type_name(&over)
            )));
        };
        let model = match &spec.model {
            Some(model) => Some(render_str(model, &roots).map_err(classify)?),
            None => None,
        };

        self.events.tool_started(
            FILTER_TOOL,
            &json!({
                "over": items.len(),
                "mode": if spec.where_.is_some() { "where" } else { "infer" },
                "concurrency": concurrency,
            }),
        );
        let started = std::time::Instant::now();

        // One verdict per item, in input order (`buffered`, like map). On
        // failure the stream drains: verdicts already in flight complete,
        // ones not yet started are skipped.
        let halted = AtomicBool::new(false);
        let halted_ref = &halted;
        let where_ref = &spec.where_;
        let infer_ref = &spec.infer;
        let model_ref = model.as_deref();
        let verdict_futures: Vec<futures::future::BoxFuture<'_, Option<Result<bool, FilterFail>>>> =
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let future = async move {
                        if halted_ref.load(Ordering::Relaxed) {
                            return None;
                        }
                        let mut layered = scope.clone();
                        layered.insert("item".to_string(), item.clone());
                        layered.insert("index".to_string(), json!(index));
                        let verdict = self
                            .item_verdict(where_ref, infer_ref, model_ref, &layered, index)
                            .await;
                        if verdict.is_err() {
                            halted_ref.store(true, Ordering::Relaxed);
                        }
                        Some(verdict)
                    };
                    Box::pin(future) as futures::future::BoxFuture<'_, _>
                })
                .collect();
        let outcomes: Vec<Option<Result<bool, FilterFail>>> =
            futures::stream::iter(verdict_futures)
                .buffered(concurrency)
                .collect()
                .await;

        let mut kept = Vec::new();
        let mut dropped = Vec::new();
        let mut failure: Option<FilterFail> = None;
        for (item, outcome) in items.into_iter().zip(outcomes) {
            match outcome {
                Some(Ok(true)) => kept.push(item),
                Some(Ok(false)) => dropped.push(item),
                Some(Err(e)) => {
                    // `buffered` yields in order: the kept failure is the
                    // lowest-index one.
                    failure.get_or_insert(e);
                }
                None => {} // skipped after a halt
            }
        }
        if let Some(fail) = failure {
            self.events
                .tool_finished(FILTER_TOOL, started.elapsed(), true);
            return Err(fail);
        }
        self.events
            .tool_finished(FILTER_TOOL, started.elapsed(), false);
        Ok(json!({
            "items": kept,
            "count": kept.len(),
            "dropped": dropped,
            "dropped_count": dropped.len(),
        }))
    }

    /// One item's verdict: render the gate against the item's scope and
    /// evaluate it — locally for `where`, via the judge for `infer`.
    async fn item_verdict(
        &self,
        where_: &Option<Value>,
        infer: &Option<String>,
        model: Option<&str>,
        layered: &Map<String, Value>,
        index: usize,
    ) -> Result<bool, FilterFail> {
        let classify = |e: RenderError| match e {
            e @ RenderError::EmptyData { .. } => FilterFail::Empty(e),
            e => FilterFail::Failed(format!("`where` item {index}: {e}")),
        };
        let roots = Roots::new(layered);
        match (where_, infer) {
            (Some(raw), None) => {
                let rendered = render_input(raw, &roots).map_err(classify)?;
                let condition: Condition = serde_json::from_value(rendered).map_err(|e| {
                    FilterFail::Failed(format!("item {index}: invalid filter condition: {e}"))
                })?;
                super::condition::eval_condition(&condition)
                    .map_err(|e| FilterFail::Failed(format!("item {index}: {e}")))
            }
            (None, Some(question)) => {
                let rendered = render_str(question, &roots).map_err(|e| match e {
                    e @ RenderError::EmptyData { .. } => FilterFail::Empty(e),
                    e => FilterFail::Failed(format!("`infer` item {index}: {e}")),
                })?;
                let (verdict, _reason) = evaluate_gate(None, Some(&rendered), model, &self.router)
                    .await
                    .map_err(|e| FilterFail::Failed(format!("item {index}: {e}")))?;
                Ok(verdict)
            }
            // Arity is checked before any item runs.
            _ => unreachable!("filter gate arity checked by run_filter_scoped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems_for(input: Value) -> Vec<String> {
        let input: Map<String, Value> = serde_json::from_value(input).unwrap();
        let mut problems = Vec::new();
        validate_filter_input(&input, &["input", "E0"], "E1", &mut problems);
        problems
    }

    #[test]
    fn spec_rejects_unknown_fields_and_bad_arity() {
        let problems = problems_for(json!({
            "over": "{{E0.values}}",
            "when": {"value": "{{item}}", "op": "eq", "to": 1},
        }));
        assert!(problems[0].contains("when"), "{problems:?}");

        let problems = problems_for(json!({"over": "{{E0.values}}"}));
        assert!(
            problems.iter().any(|p| p.contains("`where` or `infer`")),
            "{problems:?}"
        );

        let problems = problems_for(json!({
            "over": "{{E0.values}}",
            "where": {"value": "{{item}}", "op": "not_empty"},
            "infer": "is {{item}} relevant?",
        }));
        assert!(
            problems.iter().any(|p| p.contains("mutually exclusive")),
            "{problems:?}"
        );

        let problems = problems_for(json!({
            "over": "{{E0.values}}",
            "infer": "keep {{item}}?",
            "concurrency": 0,
        }));
        assert!(
            problems.iter().any(|p| p.contains("at least 1")),
            "{problems:?}"
        );
    }

    #[test]
    fn pseudo_roots_are_scoped_to_the_gate() {
        // {{item}} in `over` at the top level is pointed at, once.
        let problems = problems_for(json!({
            "over": "{{item.children}}",
            "where": {"value": "{{item.status}}", "op": "ne", "to": "deleted"},
        }));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("only available inside `where`/`infer`"),
            "{problems:?}"
        );

        // The legitimate shape passes.
        let problems = problems_for(json!({
            "over": "{{E0.changes}}",
            "where": {"value": "{{item.status}}", "op": "ne", "to": "deleted"},
        }));
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// Nested in a map body, the ambient scope provides `item`/`index` —
    /// `over: {{item.children}}` is then legitimate, not an error.
    #[test]
    fn nested_filter_may_read_the_enclosing_body_scope() {
        let input: Map<String, Value> = serde_json::from_value(json!({
            "over": "{{item.children}}",
            "where": {"value": "{{item.size}}", "op": "gt", "to": 0},
        }))
        .unwrap();
        let mut problems = Vec::new();
        validate_filter_input(
            &input,
            &["input", "E0", "item", "index"],
            "inner",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn forward_references_are_still_caught() {
        let problems = problems_for(json!({
            "over": "{{E9.values}}",
            "where": {"value": "{{item}}", "op": "not_empty"},
        }));
        assert!(problems.iter().any(|p| p.contains("E9")), "{problems:?}");
    }
}
