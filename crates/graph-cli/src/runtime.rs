//! Shared wiring: config → providers → MCP registry → agent.

use anyhow::{Context, Result};
use graph_core::{Agent, EventSink};
use graph_llm::ModelRouter;
use graph_mcp::McpManager;
use std::sync::Arc;

pub struct Runtime {
    pub config: graph_config::Config,
    pub registry: Arc<McpManager>,
    router: Arc<ModelRouter>,
}

impl Runtime {
    pub fn init() -> Result<Self> {
        let loaded = graph_config::load()?;
        let router = ModelRouter::from_config(&loaded.config)?;
        let registry = Arc::new(McpManager::new(loaded.config.mcp.clone()));
        Ok(Self {
            config: loaded.config,
            registry,
            router: Arc::new(router),
        })
    }

    /// Gracefully close MCP connections. Call before returning from a
    /// command; skipping it orphans stdio MCP servers (their async-Drop
    /// cleanup never runs once the tokio runtime starts shutting down).
    pub async fn shutdown(&self) {
        self.registry.shutdown().await;
    }

    pub fn agent(&self, events: Arc<dyn EventSink>) -> Result<Agent> {
        let (provider, choice) = self
            .router
            .resolve(graph_config::Role::Chat)
            .context("configure [models] chat or default in your config")?;
        let now = chrono::Local::now()
            .format("%A, %B %e %Y, %H:%M %Z")
            .to_string();
        Ok(Agent {
            provider,
            registry: self.registry.clone(),
            events,
            model: choice.model.clone(),
            temperature: choice.temperature,
            system_prompt: graph_core::prompts::chat_system_prompt(&self.config.user, &now),
            max_iterations: self.config.settings.max_agent_iterations,
        })
    }
}
