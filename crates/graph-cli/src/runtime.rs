//! Shared wiring: config → providers → MCP registry → store → agent.

use anyhow::{Context, Result};
use graph_core::pipeline::{doc::PlanDoc, Pipeline};
use graph_core::toolbox::AgentToolbox;
use graph_core::user_tools::{CypherExecutor, UserToolRegistry};
use graph_core::{Agent, CompositeRegistry, EventSink, Store, ThreadMeta, ToolRegistry};
use graph_llm::ModelRouter;
use graph_mcp::McpManager;
use graph_store::{GraphStore, MemoryStore, RecordingRegistry};
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

    /// Open the store keeping the Cypher handle (ladybug backend only).
    pub fn store_handles(&self) -> Result<StoreHandles> {
        open_store(&self.config)
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

    /// Base tool catalog (MCP servers + user-defined tools), wrapped with
    /// shape recording.
    pub fn recording_registry(&self, handles: &StoreHandles) -> Result<Arc<dyn ToolRegistry>> {
        let user_tools = self.user_tools(handles.cypher.clone())?;
        let base: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
            self.registry.clone() as Arc<dyn ToolRegistry>,
            user_tools,
        ]));
        Ok(Arc::new(RecordingRegistry::new(
            base,
            handles.store.clone(),
        )))
    }

    /// User-defined tools from `[tools].paths`.
    pub fn user_tools(
        &self,
        cypher: Option<Arc<dyn CypherExecutor>>,
    ) -> Result<Arc<dyn ToolRegistry>> {
        let dirs: Vec<std::path::PathBuf> = self
            .config
            .tools
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect();
        let docs = graph_core::user_tools::load_user_tools(&dirs).map_err(anyhow::Error::msg)?;
        Ok(Arc::new(UserToolRegistry::new(
            docs,
            self.router.clone(),
            cypher,
        )))
    }

    /// Plan documents whose `requires_servers` are all configured.
    pub fn plan_docs(&self) -> Result<Vec<PlanDoc>> {
        let dirs: Vec<std::path::PathBuf> = self
            .config
            .plans
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect();
        let docs = graph_core::pipeline::doc::load_plan_docs(&dirs)?;
        Ok(docs
            .into_iter()
            .filter(|doc| {
                let ok = doc
                    .requires_servers
                    .iter()
                    .all(|server| self.config.mcp.contains_key(server));
                if !ok {
                    tracing::info!(
                        plan = doc.identifier,
                        "hidden: required MCP server not configured"
                    );
                }
                ok
            })
            .collect())
    }

    /// The plan pipeline over the base registry (shape-recording MCP +
    /// user tools).
    pub async fn pipeline(
        &self,
        handles: &StoreHandles,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<Pipeline>> {
        let base = self.recording_registry(handles)?;
        let user_context = user_context_text(&self.config.user);
        Ok(Arc::new(Pipeline {
            router: self.router.clone(),
            registry: base,
            events,
            plans: Arc::new(self.plan_docs()?),
            call_stack: Vec::new(),
            store: Some(handles.store.clone()),
            user_context,
            current_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            max_attempts: self.config.settings.planning_attempts.max(1),
        }))
    }

    /// The agent's full tool catalog: MCP + user tools + plan tools +
    /// plan_and_execute.
    pub async fn toolbox(
        &self,
        handles: &StoreHandles,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<AgentToolbox>> {
        let base = self.recording_registry(handles)?;
        let pipeline = self.pipeline(handles, events).await?;
        let plans = pipeline.plans.as_ref().clone();
        Ok(Arc::new(AgentToolbox::new(base, pipeline, plans)))
    }
}

/// The opened store plus backend-specific capabilities.
pub struct StoreHandles {
    pub store: Arc<dyn Store>,
    /// Present on the ladybug backend; user cypher tools need it.
    pub cypher: Option<Arc<dyn CypherExecutor>>,
}

fn user_context_text(user: &graph_config::UserConfig) -> String {
    let mut out = String::new();
    if let Some(name) = &user.name {
        out.push_str(&format!("Name: {name}\n"));
    }
    if let Some(context) = &user.context {
        out.push_str(context);
    }
    if out.is_empty() {
        out.push_str("(none provided)");
    }
    out
}

/// Open the configured runtime-state store. Backend selection:
/// `GRAPH_STORAGE` env var (`ladybug` | `memory`) wins over
/// `[storage].backend`; the default is the embedded LadybugDB.
pub fn open_store(config: &graph_config::Config) -> Result<StoreHandles> {
    let backend = match std::env::var("GRAPH_STORAGE").ok().as_deref() {
        Some("memory") => graph_config::StorageBackend::Memory,
        Some("ladybug") => graph_config::StorageBackend::Ladybug,
        Some(other) => anyhow::bail!("GRAPH_STORAGE must be 'ladybug' or 'memory', got '{other}'"),
        None => config.storage.backend,
    };
    match backend {
        graph_config::StorageBackend::Memory => Ok(StoreHandles {
            store: Arc::new(MemoryStore::new()),
            cypher: None,
        }),
        graph_config::StorageBackend::Ladybug => {
            let store = Arc::new(open_ladybug(config)?);
            Ok(StoreHandles {
                cypher: Some(store.clone()),
                store,
            })
        }
    }
}

/// Open the embedded LadybugDB directly (Ladybug-only surfaces such as
/// `graph db query`).
pub fn open_ladybug(config: &graph_config::Config) -> Result<GraphStore> {
    let dir = graph_config::expand_tilde(&config.settings.data_dir).join("db");
    GraphStore::open(&dir).map_err(Into::into)
}

/// Resolve which existing thread to continue, if any.
///
/// `None` → new thread; `Some(None)` (bare `--thread`) → most recent thread,
/// or a new one when none exist yet; `Some(Some(id))` → that thread or error.
pub async fn resolve_thread(
    store: &dyn Store,
    thread: Option<Option<String>>,
) -> Result<Option<ThreadMeta>> {
    match thread {
        None => Ok(None),
        Some(Some(id)) => {
            let meta = store
                .get_thread(&id)
                .await?
                .with_context(|| format!("no thread with id {id} (see `graph threads list`)"))?;
            Ok(Some(meta))
        }
        Some(None) => Ok(store.latest_thread().await?),
    }
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
