//! The `exit` escape hatch: a step that ends the plan early with a
//! success or error state, gated by a logical condition (`when`) or an
//! inferred verdict (`infer`). Intercepted by the executor — never
//! dispatched to a tool registry, and skips the solver entirely (an
//! escape hatch is a zero-inference exit, except the verdict call itself).

use graph_config::Role;
use graph_llm::types::ChatMessage;
use graph_llm::ModelRouter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Reserved step tool name.
pub const EXIT_TOOL: &str = "exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Success,
    Error,
}

/// A triggered exit, carried through the pipeline outcome.
#[derive(Debug, Clone, Serialize)]
pub struct PlanExit {
    pub status: ExitStatus,
    pub message: String,
    /// The model's reasoning, for inferred exits.
    pub reason: Option<String>,
    /// Rendered output map, when the step declared one.
    pub output: Option<Map<String, Value>>,
    /// The step id that exited.
    pub step: String,
}

/// The exit step's input, parsed *after* template rendering.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitSpec {
    /// Logical gate. Mutually exclusive with `infer`; omit both for an
    /// unconditional exit.
    pub when: Option<Condition>,
    /// Inferred gate: a yes/no question answered by the `judge` model
    /// role with a structured verdict.
    pub infer: Option<String>,
    pub status: ExitStatus,
    #[serde(default)]
    pub message: Option<String>,
    /// Emitted as the plan's structured output when the exit triggers.
    #[serde(default)]
    pub output: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub value: Value,
    pub op: Op,
    #[serde(default)]
    pub to: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
    Empty,
    NotEmpty,
    Contains,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Verdict {
    /// True when the answer to the question is yes.
    pub verdict: bool,
    /// One or two sentences explaining the verdict.
    pub reason: String,
}

/// The exit step as described to the planner.
pub fn exit_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: EXIT_TOOL.to_string(),
        description: "End the plan early with a success or error state. Use it instead of \
                      fabricating results: exit success when there is legitimately nothing to \
                      do (e.g. a search returned nothing actionable), exit error to assert a \
                      failure condition. Gate it with `when` (a logical comparison) or `infer` \
                      (a yes/no question judged against prior results); omit both to exit \
                      unconditionally. When the gate does not fire, the plan continues."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["status"],
            "properties": {
                "status": {"type": "string", "enum": ["success", "error"]},
                "message": {"type": "string", "description": "Shown as the plan's answer / error message. May use templates."},
                "when": {
                    "type": "object",
                    "required": ["value", "op"],
                    "properties": {
                        "value": {"description": "Usually a template like {{E0.issues.length}}"},
                        "op": {"type": "string", "enum": ["eq","ne","gt","lt","gte","lte","empty","not_empty","contains"]},
                        "to": {"description": "Comparison operand (omit for empty/not_empty)"}
                    }
                },
                "infer": {"type": "string", "description": "A yes/no question about prior results; exits when the answer is yes."}
            }
        }),
        output_schema: None,
        output_example: Some(json!({"passed": true, "verdict": false, "reason": "…"})),
        read_only: Some(true),
    }
}

/// How an evaluated exit step resolved.
pub enum ExitEval {
    /// Gate did not fire; the plan continues. The value becomes the step's
    /// result (so later steps can reference e.g. `{{Ex.reason}}`).
    Passed(Value),
    /// Gate fired; the plan ends.
    Exited(PlanExit),
}

pub async fn evaluate(
    step_id: &str,
    rendered_input: &Value,
    router: &ModelRouter,
) -> Result<ExitEval, String> {
    let spec: ExitSpec = serde_json::from_value(rendered_input.clone())
        .map_err(|e| format!("invalid exit step input: {e}"))?;
    if spec.when.is_some() && spec.infer.is_some() {
        return Err("exit step: `when` and `infer` are mutually exclusive".to_string());
    }

    let (triggered, reason) = match (&spec.when, &spec.infer) {
        (Some(condition), None) => (eval_condition(condition)?, None),
        (None, Some(question)) => {
            let verdict: Verdict = router
                .get_structured(
                    Role::Judge,
                    "You answer a single yes/no question about the provided data, \
                     honestly and conservatively. Answer yes only when the data \
                     clearly supports it.",
                    vec![ChatMessage::User {
                        content: question.clone(),
                    }],
                    "verdict",
                )
                .await
                .map_err(|e| format!("exit step verdict failed: {e}"))?;
            (verdict.verdict, Some(verdict.reason))
        }
        (None, None) => (true, None), // unconditional
        (Some(_), Some(_)) => unreachable!(),
    };

    if !triggered {
        return Ok(ExitEval::Passed(json!({
            "passed": true,
            "verdict": false,
            "reason": reason,
        })));
    }

    let mut message = spec.message.unwrap_or_else(|| match spec.status {
        ExitStatus::Success => "plan exited early".to_string(),
        ExitStatus::Error => "plan asserted failure".to_string(),
    });
    if let Some(reason) = &reason {
        message = format!("{message} ({reason})");
    }
    Ok(ExitEval::Exited(PlanExit {
        status: spec.status,
        message,
        reason,
        output: spec.output,
        step: step_id.to_string(),
    }))
}

fn eval_condition(condition: &Condition) -> Result<bool, String> {
    let value = &condition.value;
    let to = &condition.to;
    let result = match condition.op {
        Op::Eq => value == to,
        Op::Ne => value != to,
        Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
            let (a, b) = match (value.as_f64(), to.as_f64()) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    return Err(format!(
                        "exit condition: ordering ops need numbers, got {value} vs {to}"
                    ))
                }
            };
            match condition.op {
                Op::Gt => a > b,
                Op::Lt => a < b,
                Op::Gte => a >= b,
                Op::Lte => a <= b,
                _ => unreachable!(),
            }
        }
        Op::Empty | Op::NotEmpty => {
            let empty = match value {
                Value::Null => true,
                Value::Array(items) => items.is_empty(),
                Value::String(s) => s.is_empty(),
                Value::Object(map) => map.is_empty(),
                _ => false,
            };
            (condition.op == Op::Empty) == empty
        }
        Op::Contains => match (value, to) {
            (Value::String(haystack), Value::String(needle)) => haystack.contains(needle.as_str()),
            (Value::Array(items), needle) => items.contains(needle),
            _ => {
                return Err(format!(
                "exit condition: contains needs string/string or array/value, got {value} vs {to}"
            ))
            }
        },
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(value: Value, op: Op, to: Value) -> Condition {
        Condition { value, op, to }
    }

    #[test]
    fn conditions_evaluate() {
        assert!(eval_condition(&cond(json!(0), Op::Eq, json!(0))).unwrap());
        assert!(eval_condition(&cond(json!(3), Op::Gt, json!(2))).unwrap());
        assert!(!eval_condition(&cond(json!(1), Op::Gte, json!(2))).unwrap());
        assert!(eval_condition(&cond(json!([]), Op::Empty, Value::Null)).unwrap());
        assert!(eval_condition(&cond(json!(["a"]), Op::NotEmpty, Value::Null)).unwrap());
        assert!(eval_condition(&cond(json!("hello world"), Op::Contains, json!("world"))).unwrap());
        assert!(eval_condition(&cond(json!([1, 2]), Op::Contains, json!(2))).unwrap());
        assert!(eval_condition(&cond(json!("a"), Op::Gt, json!("b"))).is_err());
        // typed splice means numbers arrive as numbers; strings compare as strings
        assert!(eval_condition(&cond(json!("open"), Op::Eq, json!("open"))).unwrap());
    }
}
