//! Shared wiring: config → providers → MCP registry → store → agent.

use anyhow::{Context, Result};
use graph_core::pipeline::{doc::LoadedPlans, Pipeline, ToolCatalog};
use graph_core::toolbox::AgentToolbox;
use graph_core::user_tools::UserToolRegistry;
use graph_core::{Agent, CompositeRegistry, EventSink, Store, ThreadMeta, ToolRegistry};
use graph_llm::ModelRouter;
use graph_mcp::McpManager;
use graph_store::{FileStore, MemoryStore, RecordingRegistry};
use std::sync::Arc;

pub struct Runtime {
    pub config: graph_config::Config,
    pub registry: Arc<McpManager>,
    router: Arc<ModelRouter>,
    /// One warning per skipped plan file per command, even though several
    /// components (pipeline, toolbox, commands) each load the catalog.
    plans_warned: std::sync::atomic::AtomicBool,
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
            plans_warned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Gracefully close MCP connections. Call before returning from a
    /// command; skipping it orphans stdio MCP servers (their async-Drop
    /// cleanup never runs once the tokio runtime starts shutting down).
    pub async fn shutdown(&self) {
        self.registry.shutdown().await;
    }

    /// Open the configured runtime-state store.
    pub fn store(&self) -> Result<Arc<dyn Store>> {
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
            system_prompt: graph_core::prompts::chat_system_prompt(
                &self.config.user,
                &now,
                self.config.prompts.chat.as_deref(),
            ),
            max_iterations: self.config.settings.max_agent_iterations,
        })
    }

    /// Base tool catalog (MCP servers + builtin packs + user-defined
    /// tools), wrapped with shape recording.
    pub fn recording_registry(&self, store: &Arc<dyn Store>) -> Result<Arc<dyn ToolRegistry>> {
        let base: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
            self.registry.clone() as Arc<dyn ToolRegistry>,
            self.builtin_tools()?,
            self.user_tools()?,
        ]));
        Ok(Arc::new(RecordingRegistry::new(base, store.clone())))
    }

    /// The enabled pack names: the default packs (always on) plus whatever
    /// `[tools].packs` opts into.
    fn pack_names(&self) -> Vec<String> {
        let mut packs: Vec<String> = graph_core::user_tools::DEFAULT_PACKS
            .iter()
            .map(|p| p.to_string())
            .collect();
        for pack in &self.config.tools.packs {
            if !packs.contains(pack) {
                packs.push(pack.clone());
            }
        }
        packs
    }

    /// The directories user tools load from (`[tools].paths`, expanded).
    fn tool_dirs(&self) -> Vec<std::path::PathBuf> {
        self.config
            .tools
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect()
    }

    /// Bundled pack tools, served under `builtin__`.
    pub fn builtin_tools(&self) -> Result<Arc<dyn ToolRegistry>> {
        let docs = graph_core::user_tools::load_pack_tools(&self.pack_names())
            .map_err(anyhow::Error::msg)?;
        Ok(Arc::new(UserToolRegistry::builtins(
            docs,
            self.router.clone(),
        )))
    }

    /// User-defined tools from `[tools].paths`, served under `user__`.
    pub fn user_tools(&self) -> Result<Arc<dyn ToolRegistry>> {
        let docs = graph_core::user_tools::load_user_tools(&self.tool_dirs())
            .map_err(anyhow::Error::msg)?;
        Ok(Arc::new(UserToolRegistry::new(docs, self.router.clone())))
    }

    /// The loadable-tool catalog for catalog-aware plan validation: what a
    /// plan step can actually resolve to at run time. Built from config
    /// alone — MCP servers are included by *name*, never connected to.
    pub fn tool_catalog(
        &self,
        plan_docs: &[graph_core::pipeline::doc::PlanDoc],
    ) -> Result<ToolCatalog> {
        let builtin_tools = graph_core::user_tools::load_pack_tools(&self.pack_names())
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|doc| {
                format!(
                    "{}{}",
                    graph_core::user_tools::BUILTIN_TOOL_PREFIX,
                    doc.name
                )
            })
            .collect();
        let user_tools = graph_core::user_tools::load_user_tools(&self.tool_dirs())
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|doc| format!("{}{}", graph_core::user_tools::USER_TOOL_PREFIX, doc.name))
            .collect();
        Ok(ToolCatalog {
            builtin_tools,
            user_tools,
            plans: plan_docs.iter().map(|d| d.identifier.clone()).collect(),
            mcp_servers: self.config.mcp.keys().cloned().collect(),
        })
    }

    /// The plan catalog, kept to documents whose `requires_servers` are all
    /// configured. Files that fail to load stay in `skipped` and are warned
    /// about here — a broken plan never takes the command down.
    pub fn plan_docs(&self) -> LoadedPlans {
        let dirs: Vec<std::path::PathBuf> = self
            .config
            .plans
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect();
        let mut loaded = graph_core::pipeline::doc::load_plan_docs(&dirs);
        if !self
            .plans_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            for error in &loaded.skipped {
                tracing::warn!("skipping plan file — {error}");
            }
        }
        let (visible, hidden): (Vec<_>, Vec<_>) = loaded.docs.drain(..).partition(|doc| {
            doc.requires_servers
                .iter()
                .all(|server| self.config.mcp.contains_key(server))
        });
        loaded.docs = visible;
        for doc in hidden {
            tracing::info!(
                plan = doc.identifier,
                "hidden: required MCP server not configured"
            );
            loaded.hidden.push(graph_core::pipeline::doc::HiddenPlan {
                missing_servers: doc
                    .requires_servers
                    .iter()
                    .filter(|server| !self.config.mcp.contains_key(*server))
                    .cloned()
                    .collect(),
                identifier: doc.identifier,
            });
        }
        loaded
    }

    /// The plan pipeline over the base registry (shape-recording MCP +
    /// user tools).
    pub async fn pipeline(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<Pipeline>> {
        let base = self.recording_registry(store)?;
        let user_context = user_context_text(&self.config.user);
        let plans = self.plan_docs().docs;
        let catalog = self.tool_catalog(&plans)?;
        Ok(Arc::new(Pipeline {
            router: self.router.clone(),
            registry: base,
            events,
            plans: Arc::new(plans),
            call_stack: Vec::new(),
            store: Some(store.clone()),
            gate: None,
            catalog: Some(Arc::new(catalog)),
            user_context,
            current_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            max_attempts: self.config.settings.planning_attempts.max(1),
            draft_strategy: draft_strategy(&self.config)?,
        }))
    }

    /// The agent's full tool catalog: MCP + user tools + plan tools +
    /// plan_and_execute.
    pub async fn toolbox(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<AgentToolbox>> {
        let base = self.recording_registry(store)?;
        let pipeline = self.pipeline(store, events).await?;
        let plans = pipeline.plans.as_ref().clone();
        Ok(Arc::new(AgentToolbox::new(base, pipeline, plans)))
    }
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

/// Resolve the plan-drafting strategy: `GRAPH_DRAFT_STRATEGY` env var
/// (`oneshot` | `incremental`) wins over `[planner].draft_strategy`; the
/// default is one-shot.
pub fn draft_strategy(config: &graph_config::Config) -> Result<graph_config::DraftStrategy> {
    match std::env::var("GRAPH_DRAFT_STRATEGY").ok().as_deref() {
        Some("oneshot") => Ok(graph_config::DraftStrategy::Oneshot),
        Some("incremental") => Ok(graph_config::DraftStrategy::Incremental),
        Some(other) => {
            anyhow::bail!("GRAPH_DRAFT_STRATEGY must be 'oneshot' or 'incremental', got '{other}'")
        }
        None => Ok(config.planner.draft_strategy),
    }
}

/// Open the configured runtime-state store. Backend selection:
/// `GRAPH_STORAGE` env var (`file` | `memory`) wins over
/// `[storage].backend`; the default is plain files under `data_dir`.
pub fn open_store(config: &graph_config::Config) -> Result<Arc<dyn Store>> {
    let backend = match std::env::var("GRAPH_STORAGE").ok().as_deref() {
        Some("memory") => graph_config::StorageBackend::Memory,
        Some("file") => graph_config::StorageBackend::File,
        Some(other) => anyhow::bail!("GRAPH_STORAGE must be 'file' or 'memory', got '{other}'"),
        None => config.storage.backend,
    };
    match backend {
        graph_config::StorageBackend::Memory => Ok(Arc::new(MemoryStore::new())),
        graph_config::StorageBackend::File => {
            let root = graph_config::expand_tilde(&config.settings.data_dir);
            Ok(Arc::new(
                FileStore::open(&root).map_err(anyhow::Error::msg)?,
            ))
        }
    }
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
