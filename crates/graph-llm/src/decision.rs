use crate::types::Usage;
use crate::LlmError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    Likelihood {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<LikelihoodCriteria>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LikelihoodCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Likelihood {
        probability: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, LlmError>;
}

pub fn request_content(req: &DecisionRequest) -> Value {
    serde_json::json!({
        "state": req.state,
        "questions": req.questions,
    })
}

pub fn response_content(response: &DecisionResponse) -> Value {
    serde_json::json!({ "answers": response.answers })
}

pub fn estimate_state_tokens(state: &Value) -> u64 {
    let bytes = match state {
        Value::String(text) => text.len(),
        other => other.to_string().len(),
    };
    (bytes as u64).div_ceil(4)
}

pub fn check_context_window(
    role: &str,
    model: &str,
    limit: Option<u32>,
    req: &DecisionRequest,
) -> Result<(), LlmError> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let estimate = estimate_state_tokens(&req.state);
    if estimate > u64::from(limit) {
        return Err(LlmError::ContextWindowExceeded {
            role: role.to_string(),
            model: model.to_string(),
            limit,
            estimate,
        });
    }
    Ok(())
}
