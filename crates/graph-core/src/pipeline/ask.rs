//! The `ask` control step: put a question to a human and bind the answer
//! to a step id. Intercepted by the executor — never dispatched to a tool
//! registry.
//!
//! `ask` is the in-band half of interactivity (the `ExecutionGate` is the
//! out-of-band half): the plan itself declares that it needs a value only
//! a person can supply. The answer is an ordinary step result, so
//! templates, `exit` gates, and `decide` branches compose over it exactly
//! as they do over a tool's output.
//!
//! The load-bearing field is `whenUnanswered`. A plan that only runs with
//! a human at a keyboard is not portable, and portability across the CLI,
//! the workbench, an MCP client, and CI is the reason plans beat prose. So
//! the headless behaviour is declared in the plan, statically visible, and
//! reviewable — never inferred from the environment.

use super::gate::StepPath;
use super::interlocutor::{AskOutcome, AskRequest};
use super::{ExecutionEnd, Pipeline, RunState, Step};
use crate::template::{render_input, render_str, RenderError, Roots};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Reserved step tool name.
pub const ASK_TOOL: &str = "ask";

/// What the step does when no answer comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum WhenUnanswered {
    /// The step fails, and with it the run. The safe default: a plan that
    /// needs a human and cannot reach one has not done its job.
    #[default]
    Fail,
    /// Fall back to the declared `default` value and carry on. The answer
    /// envelope records `answered: false`, so a later `decide` can still
    /// branch on whether a human was involved.
    Default,
}

/// The ask step's input, parsed from the RAW (unrendered) step input:
/// `prompt` and `default` render against the step's scope when the step
/// runs, not at parse time.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskSpec {
    pub prompt: String,
    #[serde(alias = "output_schema")]
    pub output_schema: Value,
    #[serde(default, alias = "when_unanswered")]
    pub when_unanswered: WhenUnanswered,
    #[serde(default)]
    pub default: Option<Value>,
}

/// The ask step's result envelope.
#[derive(Debug, Serialize)]
pub struct AskResult {
    /// The schema-conforming answer, or the rendered `default`.
    pub answer: Value,
    /// Did a human actually answer?
    pub answered: bool,
    /// Why not, when `answered` is false: "declined" or "unavailable".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The ask step as described to the planner.
pub fn ask_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: ASK_TOOL.to_string(),
        description: "Ask the human running the plan a question and bind their \
                      answer to this step. Use ONLY for a value the plan cannot \
                      compute or look up — a choice between options only the \
                      user can make, a confirmation before an irreversible \
                      action, a missing detail. Never use it for something a \
                      tool could fetch or an `infer` gate could judge. The \
                      answer conforms to outputSchema, which must be a flat \
                      object of primitive fields. Declare whenUnanswered so the \
                      plan still behaves when it runs somewhere with no human \
                      (CI, a headless client)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["prompt", "outputSchema"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The question, as the user will see it. May use templates like {{E0.candidates}}."
                },
                "outputSchema": {
                    "description": "JSON Schema for the answer. Must be type object whose properties are all primitives (string, number, integer, boolean) or string enums — no nested objects or arrays. Give every property a description: it is the field label the user sees."
                },
                "whenUnanswered": {
                    "type": "string",
                    "enum": ["fail", "default"],
                    "description": "What to do when nobody can answer (headless run, client without elicitation support, user declined). 'fail' (the default) fails the step; 'default' falls back to the `default` value."
                },
                "default": {
                    "description": "The fallback answer used when whenUnanswered is 'default'. Must conform to outputSchema. May use templates."
                }
            }
        }),
        output_schema: None,
        output_example: Some(json!({
            "answer": {"repo": "tylerdavis/graph"},
            "answered": true
        })),
        read_only: None,
    }
}

// ── Static validation ──────────────────────────────────────────────────────

/// Why `schema` cannot be put to a human as a form, or `None` when it can.
///
/// The constraint comes from MCP's `elicitation/create`, whose
/// `requestedSchema` is a flat object of primitives. It is enforced for
/// *every* host, not just MCP: a schema that only a raw-JSON prompt can
/// satisfy would make the plan silently unusable from an MCP client, which
/// is precisely the portability trap `whenUnanswered` exists to close.
/// Nested data belongs in a tool result, not in an answer typed by a human.
pub fn elicitation_schema_problem(schema: &Value) -> Option<String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Some("`outputSchema` must declare `properties`".to_string());
    };
    if properties.is_empty() {
        return Some("`outputSchema` must declare at least one property".to_string());
    }
    for (name, property) in properties {
        // An enum pins the value set; its type annotation is optional.
        if property.get("enum").is_some() {
            continue;
        }
        match property.get("type").and_then(Value::as_str) {
            Some("string" | "number" | "integer" | "boolean") => {}
            Some(other) => {
                return Some(format!(
                    "`outputSchema` property `{name}` has type `{other}` — an \
                     answer's fields must be primitives (string, number, \
                     integer, boolean) or enums, because a human fills them in \
                     one form field at a time"
                ))
            }
            None => {
                return Some(format!(
                    "`outputSchema` property `{name}` declares no `type` — an \
                     answer's fields must each be a primitive type or an enum"
                ))
            }
        }
    }
    None
}

/// True when `value` contains a template tag anywhere. A `default` with
/// templates cannot be schema-checked statically (a tag renders to a
/// string here and an integer there), so its check is deferred to the run.
fn has_templates(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("{{"),
        Value::Array(items) => items.iter().any(has_templates),
        Value::Object(map) => map.values().any(has_templates),
        _ => false,
    }
}

/// Validate an ask step's raw input at plan load time.
pub fn validate_ask_input(
    input: &Map<String, Value>,
    seen: &[&str],
    step_id: &str,
    problems: &mut Vec<String>,
) {
    let spec: AskSpec = match serde_json::from_value(Value::Object(input.clone())) {
        Ok(spec) => spec,
        Err(e) => {
            problems.push(format!("step {step_id}: invalid ask input: {e}"));
            return;
        }
    };

    let schema_ok = match jsonschema::validator_for(&spec.output_schema) {
        Ok(_) => {
            if spec
                .output_schema
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "object")
            {
                problems.push(format!(
                    "step {step_id}: `outputSchema` must have type \"object\""
                ));
                false
            } else if let Some(problem) = elicitation_schema_problem(&spec.output_schema) {
                problems.push(format!("step {step_id}: {problem}"));
                false
            } else {
                true
            }
        }
        Err(e) => {
            problems.push(format!(
                "step {step_id}: `outputSchema` is not valid JSON Schema: {e}"
            ));
            false
        }
    };

    match (&spec.when_unanswered, &spec.default) {
        (WhenUnanswered::Default, None) => problems.push(format!(
            "step {step_id}: `whenUnanswered` is \"default\" but no `default` \
             value is declared"
        )),
        (WhenUnanswered::Fail, Some(_)) => problems.push(format!(
            "step {step_id}: a `default` is declared but `whenUnanswered` is \
             \"fail\", so it can never be used — set `whenUnanswered` to \
             \"default\" or drop the `default`"
        )),
        _ => {}
    }

    // A template-free default is checkable now; one with tags is checked
    // after rendering, at the point where its real types exist.
    if let Some(default) = &spec.default {
        if schema_ok && !has_templates(default) {
            if let Some(problem) = schema_mismatch(default, &spec.output_schema) {
                problems.push(format!(
                    "step {step_id}: `default` does not conform to `outputSchema`: {problem}"
                ));
            }
        }
        super::check_templates(default, seen, step_id, problems);
    }

    super::check_templates(&Value::String(spec.prompt), seen, step_id, problems);
}

/// Schema-check a value, returning the joined errors when it fails.
fn schema_mismatch(value: &Value, schema: &Value) -> Option<String> {
    let validator = jsonschema::validator_for(schema).ok()?;
    let errors: Vec<String> = validator.iter_errors(value).map(|e| e.to_string()).collect();
    (!errors.is_empty()).then(|| errors.join("; "))
}

// ── Execution ──────────────────────────────────────────────────────────────

/// How an ask step ended badly.
pub(super) enum AskFail {
    Failed(String),
    /// Data ran out while rendering the prompt or default — degrades,
    /// never replans.
    Empty(RenderError),
}

impl Pipeline {
    /// Top-level `ask` step: renders against the plan's results map.
    pub(super) async fn run_ask(
        &self,
        step: &Step,
        state: &mut RunState,
    ) -> Result<Value, ExecutionEnd> {
        let path = StepPath::top(&step.id);
        let scope = state.results.clone();
        match self.run_ask_scoped(&path, &step.input, &scope).await {
            Ok(result) => {
                let note = match (
                    result.get("answered").and_then(Value::as_bool),
                    result.get("reason").and_then(Value::as_str),
                ) {
                    (Some(true), _) => "ask: answered".to_string(),
                    (_, Some(reason)) => format!("ask: unanswered ({reason}) — used default"),
                    _ => "ask: unanswered — used default".to_string(),
                };
                state.push_bus(&step.id, super::state::BusKind::Info, note);
                Ok(result)
            }
            Err(AskFail::Empty(error)) => Err(ExecutionEnd::Empty {
                step: step.id.clone(),
                message: error.to_string(),
            }),
            Err(AskFail::Failed(message)) => Err(ExecutionEnd::Failed {
                step: step.id.clone(),
                tool: ASK_TOOL.to_string(),
                message,
            }),
        }
    }

    /// Ask, rendered against an arbitrary scope so it works identically at
    /// the top level and inside a `decide`/`map`/`reduce` body (where the
    /// scope carries `item`/`index`/`accumulator`).
    ///
    /// The step is never gated: like every control step, it makes no tool
    /// call. Its side effect is a question, and the gate's contract is
    /// about dispatches.
    pub(super) async fn run_ask_scoped(
        &self,
        path: &StepPath,
        raw_input: &Map<String, Value>,
        scope: &Map<String, Value>,
    ) -> Result<Value, AskFail> {
        let spec: AskSpec = serde_json::from_value(Value::Object(raw_input.clone()))
            .map_err(|e| AskFail::Failed(format!("invalid ask input: {e}")))?;

        let roots = Roots::new(scope);
        let prompt = match render_str(&spec.prompt, &roots) {
            Ok(text) => text,
            Err(e @ RenderError::EmptyData { .. }) => return Err(AskFail::Empty(e)),
            Err(e) => return Err(AskFail::Failed(e.to_string())),
        };

        // Validation rejects a schema a human cannot fill in, but a plan
        // can reach the executor unvalidated (a gate-injected draft, a
        // direct API caller). Refuse rather than hand a host a form it
        // cannot render.
        if let Some(problem) = elicitation_schema_problem(&spec.output_schema) {
            return Err(AskFail::Failed(problem));
        }

        let outcome = match &self.interlocutor {
            Some(interlocutor) => {
                interlocutor
                    .ask(AskRequest {
                        path: path.clone(),
                        call_stack: self.call_stack.clone(),
                        prompt: prompt.clone(),
                        schema: spec.output_schema.clone(),
                    })
                    .await
            }
            None => AskOutcome::Unavailable("this run has no way to reach a human".to_string()),
        };

        let reason = match outcome {
            AskOutcome::Answered(answer) => {
                // The host is not trusted to have validated: an MCP client
                // may return a partial form, a TTY answer is hand-typed.
                if let Some(problem) = schema_mismatch(&answer, &spec.output_schema) {
                    return Err(AskFail::Failed(format!(
                        "the answer to \"{}\" does not conform to `outputSchema`: {problem}",
                        truncate(&prompt, 80)
                    )));
                }
                return Ok(envelope(answer, true, None));
            }
            AskOutcome::Declined => "declined".to_string(),
            AskOutcome::Unavailable(why) => {
                tracing::debug!(target: "ask", "{path}: no answer — {why}");
                "unavailable".to_string()
            }
        };

        match spec.when_unanswered {
            WhenUnanswered::Fail => Err(AskFail::Failed(format!(
                "no answer to \"{}\" ({reason}), and `whenUnanswered` is \
                 \"fail\" — declare a `default` with whenUnanswered: default \
                 to make this plan runnable without a human",
                truncate(&prompt, 80)
            ))),
            WhenUnanswered::Default => {
                let raw = spec.default.clone().unwrap_or(Value::Null);
                let rendered = match render_input(&raw, &roots) {
                    Ok(value) => value,
                    Err(e @ RenderError::EmptyData { .. }) => return Err(AskFail::Empty(e)),
                    Err(e) => return Err(AskFail::Failed(e.to_string())),
                };
                if let Some(problem) = schema_mismatch(&rendered, &spec.output_schema) {
                    return Err(AskFail::Failed(format!(
                        "the rendered `default` does not conform to `outputSchema`: {problem}"
                    )));
                }
                Ok(envelope(rendered, false, Some(reason)))
            }
        }
    }
}

fn envelope(answer: Value, answered: bool, reason: Option<String>) -> Value {
    serde_json::to_value(AskResult {
        answer,
        answered,
        reason,
    })
    .unwrap_or_else(|e| json!({ "error": format!("ask serialization failed: {e}") }))
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    format!("{}…", trimmed.chars().take(max).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["repo"],
            "properties": {"repo": {"type": "string", "description": "Target repo"}}
        })
    }

    #[test]
    fn a_flat_primitive_schema_is_accepted() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({"prompt": "Which repo?", "outputSchema": schema()})),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_nested_object_property_is_rejected_for_every_host() {
        // MCP elicitation cannot render it; allowing it on the TTY would
        // make the plan silently host-specific.
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which repo?",
                "outputSchema": {
                    "type": "object",
                    "properties": {"repo": {"type": "object", "properties": {}}}
                }
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("must be primitives")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_array_property_is_rejected() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which?",
                "outputSchema": {
                    "type": "object",
                    "properties": {"tags": {"type": "array", "items": {"type": "string"}}}
                }
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("type `array`")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_string_enum_property_is_accepted() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Ship it?",
                "outputSchema": {
                    "type": "object",
                    "properties": {"verdict": {"enum": ["ship", "hold"]}}
                }
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn default_without_when_unanswered_default_is_dead_config() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which repo?",
                "outputSchema": schema(),
                "default": {"repo": "x"}
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("can never be used")),
            "{problems:?}"
        );
    }

    #[test]
    fn when_unanswered_default_without_a_default_is_rejected() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which repo?",
                "outputSchema": schema(),
                "whenUnanswered": "default"
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("no `default` value")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_template_free_default_is_schema_checked_statically() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which repo?",
                "outputSchema": schema(),
                "whenUnanswered": "default",
                "default": {"repo": 12}
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("does not conform")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_templated_default_defers_its_schema_check_to_the_run() {
        // "{{input.repo}}" is a string here but may render to any type;
        // flagging it statically would ban the useful case.
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "Which repo?",
                "outputSchema": schema(),
                "whenUnanswered": "default",
                "default": {"repo": "{{input.repo}}"}
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn forward_references_are_caught_in_the_prompt_and_the_default() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({"prompt": "{{E7.x}}", "outputSchema": schema()})),
            &["input", "E0"],
            "E1",
            &mut problems,
        );
        assert!(problems.iter().any(|p| p.contains("E7")), "{problems:?}");

        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "fine",
                "outputSchema": schema(),
                "whenUnanswered": "default",
                "default": {"repo": "{{E9.name}}"}
            })),
            &["input", "E0"],
            "E1",
            &mut problems,
        );
        assert!(problems.iter().any(|p| p.contains("E9")), "{problems:?}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut problems = Vec::new();
        validate_ask_input(
            &input(json!({
                "prompt": "x",
                "outputSchema": schema(),
                "maxIterations": 3
            })),
            &["input"],
            "E1",
            &mut problems,
        );
        assert!(
            problems.iter().any(|p| p.contains("invalid ask input")),
            "{problems:?}"
        );
    }

    #[test]
    fn snake_case_aliases_are_accepted_for_yaml_authors() {
        let spec: AskSpec = serde_json::from_value(json!({
            "prompt": "x",
            "output_schema": schema(),
            "when_unanswered": "default",
            "default": {"repo": "y"}
        }))
        .expect("snake_case aliases must parse");
        assert_eq!(spec.when_unanswered, WhenUnanswered::Default);
    }

    /// The planner writes plans by copying the tool def's schema. If the
    /// schema and the serde contract disagree, every plan the planner
    /// writes is unparseable.
    #[test]
    fn planner_schema_and_serde_contract_cannot_drift() {
        let def = ask_tool_def();
        let mut doc = Map::new();
        doc.insert("prompt".into(), json!("q"));
        doc.insert("outputSchema".into(), schema());
        serde_json::from_value::<AskSpec>(Value::Object(doc.clone()))
            .expect("required fields must deserialize");

        for name in def.input_schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
        {
            let mut doc = doc.clone();
            let value = match name.as_str() {
                "prompt" => json!("q"),
                "outputSchema" => schema(),
                "whenUnanswered" => json!("default"),
                "default" => json!({"repo": "x"}),
                other => panic!("undocumented property `{other}`"),
            };
            doc.insert(name.clone(), value);
            serde_json::from_value::<AskSpec>(Value::Object(doc))
                .unwrap_or_else(|e| panic!("advertised property `{name}` rejected by serde: {e}"));
        }
    }

    #[test]
    fn the_prompt_surface_stays_camel_case() {
        let def = ask_tool_def();
        assert_eq!(def.name, "ask");
        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("outputSchema"));
        assert!(props.contains_key("whenUnanswered"));
        for leaked in ["output_schema", "when_unanswered"] {
            assert!(!props.contains_key(leaked), "snake_case `{leaked}` leaked");
        }
    }
}
