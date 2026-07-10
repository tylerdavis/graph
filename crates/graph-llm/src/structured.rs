//! Typed structured output with a single repair pass.

use crate::roles::ModelRouter;
use crate::types::{ChatMessage, ChatRequest, ResponseSchema};
use crate::LlmError;
use graph_config::Role;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

impl ModelRouter {
    /// Ask `role`'s model for a `T`, enforcing the schema provider-natively.
    /// On deserialization failure, one repair attempt runs through the
    /// `Repair` role before the error propagates (the caller's replan loop
    /// handles anything beyond that).
    pub async fn get_structured<T>(
        &self,
        role: Role,
        system: impl Into<String>,
        messages: Vec<ChatMessage>,
        schema_name: &str,
    ) -> Result<T, LlmError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        let schema = serde_json::to_value(schemars::schema_for!(T))
            .map_err(|e| LlmError::Parse(e.to_string()))?;
        let raw = self
            .raw_structured(role, system.into(), messages, schema_name, schema.clone())
            .await?;

        match serde_json::from_value::<T>(raw.clone()) {
            Ok(value) => Ok(value),
            Err(original) => {
                let repaired = self.repair(&raw, &schema, &original.to_string()).await?;
                serde_json::from_value::<T>(repaired)
                    .map_err(|e| LlmError::SchemaMismatch(format!("{e} (after repair)")))
            }
        }
    }

    async fn raw_structured(
        &self,
        role: Role,
        system: String,
        messages: Vec<ChatMessage>,
        schema_name: &str,
        schema: Value,
    ) -> Result<Value, LlmError> {
        let (provider, choice) = self.resolve(role)?;
        let response = provider
            .chat(ChatRequest {
                model: choice.model.clone(),
                system,
                messages,
                temperature: choice.temperature,
                response_schema: Some(ResponseSchema {
                    name: schema_name.to_string(),
                    schema,
                }),
                ..Default::default()
            })
            .await?;
        response
            .structured
            .ok_or_else(|| LlmError::SchemaMismatch("model produced no structured output".into()))
    }

    /// One repair pass for a value-level schema (runtime schemas from user
    /// tool docs, as opposed to `get_structured`'s Rust types). The caller
    /// validates; this only produces the corrected document.
    pub async fn repair_structured(
        &self,
        broken: &Value,
        schema: &Value,
        error: &str,
    ) -> Result<Value, LlmError> {
        self.repair(broken, schema, error).await
    }

    async fn repair(&self, broken: &Value, schema: &Value, error: &str) -> Result<Value, LlmError> {
        let system = "You fix JSON documents. Given a JSON document, the JSON Schema it must \
                      conform to, and the validation error, produce a corrected document. \
                      Preserve the original content and intent; change only what is needed \
                      to satisfy the schema.";
        let message = format!(
            "JSON document:\n{broken}\n\nJSON Schema:\n{schema}\n\nValidation error:\n{error}"
        );
        self.raw_structured(
            Role::Repair,
            system.to_string(),
            vec![ChatMessage::User { content: message }],
            "repaired",
            schema.clone(),
        )
        .await
    }
}
