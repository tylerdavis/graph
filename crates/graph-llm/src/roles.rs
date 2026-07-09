//! Role → provider/model resolution from configuration.

use crate::providers::{AnthropicProvider, OpenAiCompatProvider};
use crate::types::{ChatRequest, ChatResponse, EventStream};
use crate::{ChatProvider, LlmError};
use graph_config::{Config, ModelChoice, ProviderKind, Role};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ModelRouter {
    providers: HashMap<String, Arc<dyn ChatProvider>>,
    roles: graph_config::ModelRoles,
}

impl ModelRouter {
    pub fn from_config(config: &Config) -> Result<Self, LlmError> {
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        for (name, provider) in &config.providers {
            let instance: Arc<dyn ChatProvider> = match provider.kind {
                ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(
                    provider.api_key.clone().unwrap_or_default(),
                    provider.base_url.clone(),
                )),
                ProviderKind::Openai => Arc::new(OpenAiCompatProvider::new(
                    provider
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                    provider.api_key.clone(),
                )),
                ProviderKind::OpenaiCompat => {
                    let base_url = provider.base_url.clone().ok_or_else(|| {
                        LlmError::Unsupported(format!(
                            "provider '{name}': openai_compat requires base_url"
                        ))
                    })?;
                    Arc::new(OpenAiCompatProvider::new(
                        base_url,
                        provider.api_key.clone(),
                    ))
                }
                ProviderKind::Bedrock => {
                    return Err(LlmError::Unsupported(format!(
                        "provider '{name}': bedrock support lands in a later phase"
                    )))
                }
            };
            providers.insert(name.clone(), instance);
        }
        Ok(Self {
            providers,
            roles: config.models.clone(),
        })
    }

    pub fn resolve(&self, role: Role) -> Result<(Arc<dyn ChatProvider>, &ModelChoice), LlmError> {
        let choice = self
            .roles
            .resolve(role)
            .ok_or_else(|| LlmError::NoModelForRole(format!("{role:?}")))?;
        let provider = self
            .providers
            .get(&choice.provider)
            .ok_or_else(|| LlmError::UnknownProvider(choice.provider.clone()))?;
        Ok((Arc::clone(provider), choice))
    }

    /// Convenience: run a chat for a role with its configured model and
    /// temperature applied (request model/temperature are overwritten).
    pub async fn chat(&self, role: Role, mut req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let (provider, choice) = self.resolve(role)?;
        req.model = choice.model.clone();
        req.temperature = req.temperature.or(choice.temperature);
        provider.chat(req).await
    }

    pub async fn chat_stream(
        &self,
        role: Role,
        mut req: ChatRequest,
    ) -> Result<EventStream, LlmError> {
        let (provider, choice) = self.resolve(role)?;
        req.model = choice.model.clone();
        req.temperature = req.temperature.or(choice.temperature);
        provider.chat_stream(req).await
    }
}
