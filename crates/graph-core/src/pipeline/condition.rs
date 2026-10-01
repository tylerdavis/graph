//! Shared gate machinery for control steps (`exit`, `decide`): a logical
//! condition (`when`) or an inferred verdict (`infer`) answered by the
//! `judge` model role.

use crate::usage::CallSite;
use graph_config::{ModelKind, Role};
use graph_llm::decision::{Answer, DecisionRequest, LikelihoodCriteria, Question};
use graph_llm::types::ChatMessage;
use graph_llm::ModelRouter;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideGate {
    pub question: String,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub criteria: Option<LikelihoodCriteria>,
    #[serde(default)]
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub options: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceOutcome {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub committed: bool,
}

pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.5;

pub fn decide_gate_schema(description: &str, state: &str) -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["question"],
        "description": description,
        "properties": {
            "question": {"type": "string"},
            "state": {"description": state},
            "criteria": {"type": "object", "properties": {"true": {"type": "string"}, "false": {"type": "string"}}},
            "min_confidence": {"type": "number", "description": "0 to 1; default 0.5"},
            "model": {"type": "string", "description": "A decision model role; defaults to decider"}
        }
    })
}

pub enum Gate<'a> {
    Logic(&'a Condition),
    Infer {
        question: &'a str,
        model: Option<&'a str>,
    },
    Decide(&'a DecideGate),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateOutcome {
    pub triggered: bool,
    pub reason: Option<String>,
    pub probability: Option<f64>,
}

pub fn select_gate<'a>(
    logic: Option<&'a Condition>,
    infer: Option<&'a str>,
    decide: Option<&'a DecideGate>,
    model: Option<&'a str>,
    logic_key: &str,
) -> Result<Option<Gate<'a>>, String> {
    match (logic, infer, decide) {
        (None, None, None) => Ok(None),
        (Some(condition), None, None) => Ok(Some(Gate::Logic(condition))),
        (None, Some(question), None) => Ok(Some(Gate::Infer { question, model })),
        (None, None, Some(gate)) => {
            if model.is_some() {
                return Err("`model` sits inside the `decide` object; move it there".to_string());
            }
            Ok(Some(Gate::Decide(gate)))
        }
        _ => Err(format!(
            "`{logic_key}`, `infer`, and `decide` are mutually exclusive"
        )),
    }
}

pub async fn check_gate(gate: Gate<'_>, router: &ModelRouter) -> Result<GateOutcome, String> {
    match gate {
        Gate::Logic(condition) => Ok(GateOutcome {
            triggered: eval_condition(condition)?,
            reason: None,
            probability: None,
        }),
        Gate::Infer { question, model } => infer_verdict(question, model, router).await,
        Gate::Decide(gate) => decide_verdict(gate, router).await,
    }
}

async fn infer_verdict(
    question: &str,
    model: Option<&str>,
    router: &ModelRouter,
) -> Result<GateOutcome, String> {
    let role = model.unwrap_or(Role::Judge.as_str());
    if router.kind_of_role(role) == Some(ModelKind::Decision) {
        return Err(format!(
            "`infer` asks a chat model, but model role '{role}' is a decision model; use a `decide` gate for decision models"
        ));
    }
    let verdict: Verdict = CallSite::as_role(
        role,
        router.get_structured_named(
            model,
            Role::Judge,
            "You answer a single yes/no question about the provided data, \
             honestly and conservatively. Answer yes only when the data \
             clearly supports it.",
            vec![ChatMessage::User {
                content: question.to_string(),
            }],
            "verdict",
        ),
    )
    .await
    .map_err(|e| format!("verdict failed: {e}"))?;
    Ok(GateOutcome {
        triggered: verdict.verdict,
        reason: Some(verdict.reason),
        probability: None,
    })
}

async fn decide_verdict(gate: &DecideGate, router: &ModelRouter) -> Result<GateOutcome, String> {
    if gate.options.is_some() {
        return Err(
            "`options` (named cases) only works on a `route` step, which runs one case per option"
                .to_string(),
        );
    }
    let min_confidence = min_confidence(gate, DEFAULT_MIN_CONFIDENCE)?;
    let question = Question::Likelihood {
        instructions: gate.question.clone(),
        criteria: gate.criteria.clone(),
    };
    let probability = match ask_decision_model(gate, question, router).await? {
        Answer::Likelihood { probability } => probability,
        other => {
            return Err(format!(
                "decision failed: expected a likelihood answer, got {other:?}"
            ))
        }
    };
    Ok(GateOutcome {
        triggered: probability >= min_confidence,
        reason: None,
        probability: Some(probability),
    })
}

pub async fn decide_choice(
    gate: &DecideGate,
    router: &ModelRouter,
) -> Result<ChoiceOutcome, String> {
    let Some(options) = &gate.options else {
        return Err("a choice needs `options`".to_string());
    };
    let min_confidence = min_confidence(gate, 0.0)?;
    let question = Question::Choice {
        instructions: gate.question.clone(),
        criteria: options.clone(),
    };
    match ask_decision_model(gate, question, router).await? {
        Answer::Choice {
            choice,
            confidence,
            probabilities,
        } => {
            if !options.contains_key(&choice) {
                return Err(format!(
                    "decision failed: the model chose '{choice}', which is not one of the options"
                ));
            }
            Ok(ChoiceOutcome {
                committed: confidence >= min_confidence,
                choice,
                confidence,
                probabilities,
            })
        }
        other => Err(format!(
            "decision failed: expected a choice answer, got {other:?}"
        )),
    }
}

fn min_confidence(gate: &DecideGate, default: f64) -> Result<f64, String> {
    let min_confidence = gate.min_confidence.unwrap_or(default);
    if !(0.0..=1.0).contains(&min_confidence) {
        return Err(format!(
            "`min_confidence` must be between 0 and 1, got {min_confidence}"
        ));
    }
    Ok(min_confidence)
}

async fn ask_decision_model(
    gate: &DecideGate,
    question: Question,
    router: &ModelRouter,
) -> Result<Answer, String> {
    let role = gate.model.as_deref().unwrap_or(Role::Decider.as_str());
    if router.kind_of_role(role) == Some(ModelKind::Chat) {
        return Err(format!(
            "`decide` asks a decision model, but model role '{role}' is a chat model; use an `infer` gate for chat models"
        ));
    }
    let request = DecisionRequest {
        model: String::new(),
        state: gate.state.clone().unwrap_or(Value::Null),
        questions: BTreeMap::from([(GATE_QUESTION.to_string(), question)]),
    };
    let response = CallSite::as_role(role, router.decide_named(gate.model.as_deref(), request))
        .await
        .map_err(|e| format!("decision failed: {e}"))?;
    response
        .answers
        .get(GATE_QUESTION)
        .cloned()
        .ok_or_else(|| "decision failed: the model returned no answer".to_string())
}

const GATE_QUESTION: &str = "gate";

pub fn with_probability(mut result: Value, probability: Option<f64>) -> Value {
    if let Some(probability) = probability {
        result["probability"] = serde_json::json!(probability);
    }
    result
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
