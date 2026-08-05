//! Role → provider/model resolution from configuration.

use crate::failover::{Candidate, FailoverProvider};
use crate::providers::{AnthropicProvider, OpenAiCompatProvider};
use crate::types::{ChatRequest, ChatResponse, EventStream};
use crate::{ChatProvider, LlmError};
use graph_config::{Config, ModelChoice, ProviderKind, Role};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ModelRouter {
    providers: HashMap<String, Arc<dyn ChatProvider>>,
    /// Providers that are configured but cannot be built as configured —
    /// an unset `${VAR}` behind a secret, an unsupported kind — keyed to
    /// the reason. Kept out of `providers` so resolving one errors with
    /// that reason at the moment of use, instead of the whole config
    /// failing to load over an entry the command may never touch.
    unavailable: HashMap<String, String>,
    roles: graph_config::ModelRoles,
}

impl ModelRouter {
    /// Build from explicit provider instances — custom providers, tests.
    pub fn with_providers(
        providers: HashMap<String, Arc<dyn ChatProvider>>,
        roles: graph_config::ModelRoles,
    ) -> Self {
        Self {
            providers,
            unavailable: HashMap::new(),
            roles,
        }
    }

    pub fn from_config(config: &Config) -> Result<Self, LlmError> {
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        let mut unavailable: HashMap<String, String> = HashMap::new();
        for (name, provider) in &config.providers {
            if !provider.missing_env.is_empty() {
                unavailable.insert(
                    name.clone(),
                    graph_config::describe_missing_env(
                        &format!("providers.{name}"),
                        &provider.missing_env,
                    ),
                );
                continue;
            }
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
                ProviderKind::OpenaiCompat => match provider.base_url.clone() {
                    Some(base_url) => Arc::new(OpenAiCompatProvider::new(
                        base_url,
                        provider.api_key.clone(),
                    )),
                    None => {
                        unavailable
                            .insert(name.clone(), "openai_compat requires base_url".to_string());
                        continue;
                    }
                },
                ProviderKind::Bedrock => {
                    unavailable.insert(
                        name.clone(),
                        "bedrock support lands in a later phase".to_string(),
                    );
                    continue;
                }
            };
            providers.insert(name.clone(), instance);
        }
        let router = Self {
            providers,
            unavailable,
            roles: config.models.clone(),
        };
        // A typo'd fallback provider would otherwise surface only at the
        // moment of an outage — exactly when the fallback was supposed to
        // save the run. Fail at startup instead. Unavailable providers are
        // configured, not typo'd: they pass this check and report their own
        // reason if the fallback is ever resolved.
        for choice in router.roles.all_choices() {
            for fallback in &choice.fallbacks {
                if !router.providers.contains_key(&fallback.provider)
                    && !router.unavailable.contains_key(&fallback.provider)
                {
                    return Err(LlmError::UnknownProvider(fallback.provider.clone()));
                }
            }
        }
        Ok(router)
    }

    /// The provider under this name, or why there isn't one: unavailable
    /// beats unknown, because "your key is unset" is actionable where
    /// "not configured" sends someone off to check spelling.
    fn provider(&self, name: &str) -> Result<&Arc<dyn ChatProvider>, LlmError> {
        if let Some(provider) = self.providers.get(name) {
            return Ok(provider);
        }
        match self.unavailable.get(name) {
            Some(reason) => Err(LlmError::ProviderUnavailable {
                provider: name.to_string(),
                reason: reason.clone(),
            }),
            None => Err(LlmError::UnknownProvider(name.to_string())),
        }
    }

    pub fn resolve(&self, role: Role) -> Result<(Arc<dyn ChatProvider>, &ModelChoice), LlmError> {
        let choice = self
            .roles
            .resolve(role)
            .ok_or_else(|| LlmError::NoModelForRole(format!("{role:?}")))?;
        let provider = self.provider(&choice.provider)?;
        Ok((self.with_failover(Arc::clone(provider), choice)?, choice))
    }

    /// Wrap `primary` with the choice's failover chain, if it has one. The
    /// returned provider is a drop-in `ChatProvider`: callers keep applying
    /// the primary's model/temperature to requests, and the wrapper rewrites
    /// them per fallback only when it actually fails over.
    fn with_failover(
        &self,
        primary: Arc<dyn ChatProvider>,
        choice: &ModelChoice,
    ) -> Result<Arc<dyn ChatProvider>, LlmError> {
        if choice.fallbacks.is_empty() {
            return Ok(primary);
        }
        let fallbacks = choice
            .fallbacks
            .iter()
            .map(|fallback| {
                let provider = self.provider(&fallback.provider)?;
                Ok(Candidate {
                    provider: Arc::clone(provider),
                    provider_name: fallback.provider.clone(),
                    model: fallback.model.clone(),
                    temperature: fallback.temperature,
                })
            })
            .collect::<Result<Vec<_>, LlmError>>()?;
        Ok(Arc::new(FailoverProvider {
            primary,
            primary_name: choice.provider.clone(),
            fallbacks,
        }))
    }

    /// Resolve a model *name*: a role name (with its `default` fallback)
    /// or a `[models.named]` entry.
    pub fn resolve_named(
        &self,
        name: &str,
    ) -> Result<(Arc<dyn ChatProvider>, &ModelChoice), LlmError> {
        let choice = self
            .roles
            .resolve_name(name)
            .ok_or_else(|| LlmError::UnknownModelName {
                name: name.to_string(),
                available: {
                    let mut names: Vec<&str> =
                        self.roles.named.keys().map(String::as_str).collect();
                    names.extend_from_slice(graph_config::RESERVED_MODEL_NAMES);
                    names.join(", ")
                },
            })?;
        let provider = self.provider(&choice.provider)?;
        Ok((self.with_failover(Arc::clone(provider), choice)?, choice))
    }

    /// The configured `[models.named]` entries, for catalog surfaces that
    /// advertise selectable models.
    pub fn named_models(&self) -> &std::collections::BTreeMap<String, ModelChoice> {
        &self.roles.named
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

    /// Like [`ModelRouter::chat`], but selecting the model by name.
    pub async fn chat_named(
        &self,
        name: &str,
        mut req: ChatRequest,
    ) -> Result<ChatResponse, LlmError> {
        let (provider, choice) = self.resolve_named(name)?;
        req.model = choice.model.clone();
        req.temperature = req.temperature.or(choice.temperature);
        provider.chat(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatResponse, StopReason, StreamEvent, Usage};
    use async_trait::async_trait;
    use futures::StreamExt;
    use graph_config::FallbackChoice;

    /// Always answers with its own tag so tests can see who served the call.
    struct TaggedProvider {
        tag: &'static str,
        healthy: bool,
    }

    #[async_trait]
    impl ChatProvider for TaggedProvider {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            if !self.healthy {
                return Err(LlmError::Api {
                    status: 503,
                    body: "down".into(),
                    retry_after: None,
                });
            }
            Ok(ChatResponse {
                content: Some(format!("{}:{}", self.tag, req.model)),
                tool_calls: Vec::new(),
                structured: None,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = self.chat(req).await?;
            Ok(futures::stream::once(async move { Ok(StreamEvent::Completed(response)) }).boxed())
        }
    }

    fn choice(provider: &str, model: &str, fallbacks: Vec<FallbackChoice>) -> ModelChoice {
        ModelChoice {
            provider: provider.into(),
            model: model.into(),
            temperature: None,
            dimensions: None,
            description: None,
            fallbacks,
        }
    }

    #[tokio::test]
    async fn router_fails_over_to_the_configured_fallback() {
        let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
        providers.insert(
            "down".into(),
            Arc::new(TaggedProvider {
                tag: "down",
                healthy: false,
            }),
        );
        providers.insert(
            "up".into(),
            Arc::new(TaggedProvider {
                tag: "up",
                healthy: true,
            }),
        );
        let roles = graph_config::ModelRoles {
            default: Some(choice(
                "down",
                "primary-model",
                vec![FallbackChoice {
                    provider: "up".into(),
                    model: "backup-model".into(),
                    temperature: None,
                }],
            )),
            ..Default::default()
        };
        let router = ModelRouter::with_providers(providers, roles);

        let response = router
            .chat(Role::Chat, ChatRequest::default())
            .await
            .unwrap();
        assert_eq!(response.content.as_deref(), Some("up:backup-model"));
    }

    #[test]
    fn a_provider_with_an_unset_env_var_errors_at_resolve_naming_the_var() {
        // The config loads and the router builds — a missing key must not
        // take down commands that never call a model. The command that does
        // gets the variable name and the config path that wants it.
        let mut config = Config::default();
        config.providers.insert(
            "anthropic".into(),
            graph_config::ProviderConfig {
                kind: ProviderKind::Anthropic,
                api_key: Some("${ANTHROPIC_API_KEY}".into()),
                base_url: None,
                region: None,
                profile: None,
                missing_env: vec![graph_config::MissingEnv {
                    field: "api_key".into(),
                    var: "ANTHROPIC_API_KEY".into(),
                }],
            },
        );
        config.models.default = Some(choice("anthropic", "m", vec![]));

        let router = ModelRouter::from_config(&config).expect("router still builds");
        let error = match router.resolve(Role::Chat) {
            Ok(_) => panic!("resolve must fail"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("ANTHROPIC_API_KEY")
                && message.contains("providers.anthropic.api_key"),
            "{message}"
        );
    }

    #[test]
    fn from_config_rejects_unknown_fallback_providers_at_startup() {
        let mut config = Config::default();
        config.providers.insert(
            "anthropic".into(),
            graph_config::ProviderConfig {
                kind: ProviderKind::Anthropic,
                api_key: Some("k".into()),
                base_url: None,
                region: None,
                profile: None,
                missing_env: Vec::new(),
            },
        );
        config.models.default = Some(choice(
            "anthropic",
            "m",
            vec![FallbackChoice {
                provider: "typo".into(),
                model: "m2".into(),
                temperature: None,
            }],
        ));

        let error = match ModelRouter::from_config(&config) {
            Ok(_) => panic!("expected startup validation to fail"),
            Err(error) => error,
        };
        assert!(matches!(error, LlmError::UnknownProvider(name) if name == "typo"));
    }
}
