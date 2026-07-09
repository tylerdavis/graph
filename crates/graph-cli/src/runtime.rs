//! Shared wiring: config → providers → MCP registry → store → agent.

use anyhow::{Context, Result};
use graph_core::{Agent, EventSink, Store, ThreadMeta, ToolRegistry};
use graph_llm::ModelRouter;
use graph_mcp::McpManager;
use graph_store::{GraphStore, RecordingRegistry};
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

    /// Open the embedded database at `[settings].data_dir/db`.
    pub fn store(&self) -> Result<Arc<GraphStore>> {
        Ok(Arc::new(open_store(&self.config)?))
    }

    /// Build the chat agent. Tool calls route through `registry` — pass a
    /// `RecordingRegistry` to feed the observed-shape cache.
    pub fn agent(
        &self,
        events: Arc<dyn EventSink>,
        registry: Arc<dyn ToolRegistry>,
    ) -> Result<Agent> {
        let (provider, choice) = self
            .router
            .resolve(graph_config::Role::Chat)
            .context("configure [models] chat or default in your config")?;
        let now = chrono::Local::now()
            .format("%A, %B %e %Y, %H:%M %Z")
            .to_string();
        Ok(Agent {
            provider,
            registry,
            events,
            model: choice.model.clone(),
            temperature: choice.temperature,
            system_prompt: graph_core::prompts::chat_system_prompt(&self.config.user, &now),
            max_iterations: self.config.settings.max_agent_iterations,
        })
    }

    /// Registry wrapped with shape recording.
    pub fn recording_registry(&self, store: Arc<GraphStore>) -> Arc<dyn ToolRegistry> {
        Arc::new(RecordingRegistry::new(self.registry.clone(), store))
    }
}

/// Open the store without the rest of the runtime (for `threads`, which
/// needs neither providers nor MCP servers).
pub fn open_store(config: &graph_config::Config) -> Result<GraphStore> {
    let dir = graph_config::expand_tilde(&config.settings.data_dir).join("db");
    GraphStore::open(&dir).map_err(Into::into)
}

/// Resolve which existing thread to continue, if any.
pub async fn resolve_thread(
    store: &dyn Store,
    thread: Option<String>,
    r#continue: bool,
) -> Result<Option<ThreadMeta>> {
    if let Some(id) = thread {
        let meta = store
            .get_thread(&id)
            .await?
            .with_context(|| format!("no thread with id {id} (see `graph threads list`)"))?;
        return Ok(Some(meta));
    }
    if r#continue {
        let meta = store
            .latest_thread()
            .await?
            .context("no threads yet — run without --continue first")?;
        return Ok(Some(meta));
    }
    Ok(None)
}

/// Derive a thread title from the first user message.
pub fn title_from(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default().trim();
    let mut title: String = first_line.chars().take(60).collect();
    if first_line.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        title = "untitled".to_string();
    }
    title
}
