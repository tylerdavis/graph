//! Shared gate machinery for control steps (`exit`, `decide`): a logical
//! condition (`when`) or an inferred verdict (`infer`) answered by the
//! `judge` model role.

use graph_config::Role;
use graph_llm::types::ChatMessage;
use graph_llm::ModelRouter;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

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

/// Evaluate a gate: `(triggered, reason)`, the reason present for inferred
/// gates. Callers enforce their own arity rules on top — `exit` treats
/// neither gate as unconditional, `decide` requires exactly one. `model`
/// overrides the model used for an inferred verdict (a role name, `default`,
/// or a `[models.named]` entry); `None` uses the `judge` role.
pub async fn evaluate_gate(
    when: Option<&Condition>,
    infer: Option<&str>,
    model: Option<&str>,
    router: &ModelRouter,
) -> Result<(bool, Option<String>), String> {
    match (when, infer) {
        (Some(condition), None) => Ok((eval_condition(condition)?, None)),
        (None, Some(question)) => {
            let verdict: Verdict = router
                .get_structured_named(
                    model,
                    Role::Judge,
                    "You answer a single yes/no question about the provided data, \
                     honestly and conservatively. Answer yes only when the data \
                     clearly supports it.",
                    vec![ChatMessage::User {
                        content: question.to_string(),
                    }],
                    "verdict",
                )
                .await
                .map_err(|e| format!("verdict failed: {e}"))?;
            Ok((verdict.verdict, Some(verdict.reason)))
        }
        (Some(_), Some(_)) => Err("`when` and `infer` are mutually exclusive".to_string()),
        (None, None) => Err("a gate needs `when` or `infer`".to_string()),
    }
}

pub fn eval_condition(condition: &Condition) -> Result<bool, String> {
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
                        "condition: ordering ops need numbers, got {value} vs {to}"
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
                    "condition: contains needs string/string or array/value, got {value} vs {to}"
                ))
            }
        },
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
