//! The MCP binding for `ask` steps: `elicitation/create`.
//!
//! When a plan running behind `graph mcp serve` reaches an `ask` step, the
//! server turns the question around and asks the *client* — a server→client
//! request issued from inside a `tools/call`. That is legal under the
//! 2025-06-18 spec and is what lets one plan be interactive from Claude
//! Code, the workbench, and a terminal without knowing which it is in.
//!
//! It is also the least uniformly implemented corner of MCP, so nothing
//! here assumes it works:
//!
//! - **Capability-gated.** No `elicitation` in the client's `initialize`
//!   capabilities means no request is ever sent; the ask resolves as
//!   [`AskOutcome::Unavailable`] and the step's `whenUnanswered` decides.
//! - **Transport failures degrade.** A client that advertises support and
//!   then errors is unavailable, not a pipeline failure.
//! - **Bounded.** A question with nobody watching the client would
//!   otherwise hold a `tools/call` open forever.
//!
//! The schema constraint that shapes [`ask`](graph_core::pipeline::ask) —
//! a flat object of primitives — is this protocol's constraint, enforced
//! at plan-validation time so the failure is a review comment rather than
//! a runtime surprise on one host.

use async_trait::async_trait;
use graph_core::pipeline::{AskOutcome, AskRequest, Interlocutor};
use rmcp::model::{ElicitRequestParams, ElicitationAction, ElicitationSchema};
use rmcp::service::Peer;
use rmcp::RoleServer;
use serde_json::{Map, Value};
use std::time::Duration;

/// How long a question may stay open before the server stops waiting.
/// Generous — a person may be away from the keyboard — but finite, so a
/// client that silently drops elicitations cannot pin a plan run forever.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Answers `ask` steps by eliciting from the connected MCP client.
pub struct ElicitationInterlocutor {
    peer: Peer<RoleServer>,
    /// One question at a time. Concurrent `map` items would otherwise
    /// stack modal prompts on the user.
    lock: tokio::sync::Mutex<()>,
}

impl ElicitationInterlocutor {
    /// An interlocutor for this connection, or `None` when the client did
    /// not advertise `elicitation` at initialize time.
    ///
    /// Checking the capability rather than trying and catching the error
    /// matters: an unsupported request is a protocol error on some clients
    /// and a silent hang on others, and neither is a good way to discover
    /// that a plan's `whenUnanswered` path should have run.
    pub fn detect(peer: &Peer<RoleServer>) -> Option<Self> {
        let supported = peer
            .peer_info()
            .is_some_and(|info| info.capabilities.elicitation.is_some());
        supported.then(|| Self {
            peer: peer.clone(),
            lock: tokio::sync::Mutex::new(()),
        })
    }
}

#[async_trait]
impl Interlocutor for ElicitationInterlocutor {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        let _guard = self.lock.lock().await;

        let requested_schema = match to_elicitation_schema(&request.schema) {
            Ok(schema) => schema,
            Err(problem) => {
                return AskOutcome::Unavailable(format!(
                    "the question's schema is not elicitable: {problem}"
                ))
            }
        };

        let params = ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: request.prompt.clone(),
            requested_schema,
        };

        match self
            .peer
            .create_elicitation_with_timeout(params, Some(ANSWER_TIMEOUT))
            .await
        {
            Ok(result) => match result.action {
                ElicitationAction::Accept => match result.content {
                    Some(content) => AskOutcome::Answered(content),
                    // Accept without content is a client bug; treating it
                    // as an empty answer would fail schema validation with
                    // a confusing message.
                    None => AskOutcome::Unavailable(
                        "the client accepted the question but sent no answer".to_string(),
                    ),
                },
                // Both are "a person saw this and did not answer".
                ElicitationAction::Decline | ElicitationAction::Cancel => AskOutcome::Declined,
                // ElicitationAction is #[non_exhaustive]: a future action
                // must not be silently read as an answer.
                _ => AskOutcome::Declined,
            },
            Err(error) => {
                AskOutcome::Unavailable(format!("the client could not be asked: {error}"))
            }
        }
    }
}

/// Convert a plan's `outputSchema` into MCP's typed elicitation schema.
///
/// Plan authors write ordinary JSON Schema; MCP wants a narrower, typed
/// shape. Two normalizations bridge them, both of which only ever *add*
/// what the protocol requires:
///
/// - `type: object` is implied by an `ask` schema and may be omitted.
/// - an enum property may omit `type`, which JSON Schema allows and the
///   protocol's `EnumSchema` does not.
fn to_elicitation_schema(schema: &Value) -> Result<ElicitationSchema, String> {
    let mut normalized = schema.as_object().cloned().unwrap_or_default();
    normalized.insert("type".to_string(), Value::String("object".to_string()));

    if let Some(Value::Object(properties)) = normalized.get("properties").cloned() {
        let mut fixed = Map::new();
        for (name, property) in properties {
            let mut property = property.as_object().cloned().unwrap_or_default();
            if property.contains_key("enum") && !property.contains_key("type") {
                property.insert("type".to_string(), Value::String("string".to_string()));
            }
            fixed.insert(name, Value::Object(property));
        }
        normalized.insert("properties".to_string(), Value::Object(fixed));
    }

    serde_json::from_value(Value::Object(normalized)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plain_ask_schema_becomes_an_elicitation_form() {
        let schema = to_elicitation_schema(&json!({
            "type": "object",
            "required": ["repo"],
            "properties": {
                "repo": {"type": "string", "description": "Target repo"},
                "count": {"type": "integer"},
                "confirm": {"type": "boolean"}
            }
        }))
        .expect("convertible");
        assert_eq!(schema.properties.len(), 3);
        assert_eq!(schema.required.as_deref(), Some(&["repo".to_string()][..]));
    }

    #[test]
    fn an_implied_object_type_is_supplied() {
        // `ask` validation accepts a schema that only declares properties;
        // the protocol's schema requires the discriminator.
        let schema = to_elicitation_schema(&json!({
            "properties": {"repo": {"type": "string"}}
        }))
        .expect("convertible");
        assert_eq!(schema.properties.len(), 1);
    }

    #[test]
    fn a_bare_enum_property_is_typed_as_a_string() {
        // JSON Schema lets an enum stand alone; EnumSchema does not.
        let schema = to_elicitation_schema(&json!({
            "type": "object",
            "properties": {"verdict": {"enum": ["ship", "hold"]}}
        }))
        .expect("convertible");
        assert!(schema.properties.contains_key("verdict"));
    }

    #[test]
    fn a_nested_property_is_refused_rather_than_flattened() {
        // `ask`'s validator rejects this at load time; if one reaches here
        // anyway the answer must not be silently mis-shaped.
        let problem = to_elicitation_schema(&json!({
            "type": "object",
            "properties": {"repo": {"type": "object", "properties": {}}}
        }))
        .unwrap_err();
        assert!(!problem.is_empty());
    }
}
