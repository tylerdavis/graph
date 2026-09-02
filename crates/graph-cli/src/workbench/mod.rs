//! `graph workbench` — the interactive plan IDE: a dual-pane TUI with the
//! chat agent on the left and the plan workspace (structure, context, runs)
//! on the right. See docs/workbench/plan-workbench.mdx.

mod app;
mod chat;
mod editor;
mod effects;
mod fs_tools;
mod plan_ws;
mod runner;
#[cfg(test)]
mod screenshot;
#[cfg(test)]
mod shots;
mod tools;
mod ui;

use crate::cli::WorkbenchCommand;
use crate::runtime::Runtime;
use anyhow::{bail, Context, Result};
use app::{App, Msg};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use effects::WorkbenchContext;
use futures::StreamExt;
use graph_core::pipeline::doc::{load_plan_doc, PlanDoc};
use graph_core::{CompositeRegistry, EventSink, ExcludingRegistry, ToolRegistry};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{IsTerminal, Stdout};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Appended to the chat agent's system prompt inside the workbench;
/// `[prompts].workbench` in config replaces it. Also written into the
/// `config init` starter so users tune a copy instead of starting blank.
pub(crate) const WORKBENCH_SYSTEM_PROMPT: &str = include_str!("prompts/system.md").trim_ascii_end();

pub(crate) const WORKBENCH_TOOL_RULES: &str =
    include_str!("prompts/tool_rules.md").trim_ascii_end();

/// Control-step reference for the chat agent: the naming rules, then the
/// same usage rules the draft_plan planner sees (shared so they can't
/// drift) — the agent has the full schema and edits control flow directly.
const CONTROL_STEP_NAMING: &str = include_str!("prompts/control_step_naming.md").trim_ascii_end();

pub(crate) fn workbench_system_prompt(base: &str, override_text: Option<&str>) -> String {
    [
        base,
        override_text.unwrap_or(WORKBENCH_SYSTEM_PROMPT),
        WORKBENCH_TOOL_RULES,
        CONTROL_STEP_NAMING,
        graph_core::pipeline::CONTROL_STEP_RULES,
    ]
    .map(str::trim_end)
    .join("\n\n")
}

pub async fn run(command: WorkbenchCommand, verbosity: u8) -> Result<()> {
    let WorkbenchCommand::Plan { name_or_path } = command;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "the workbench needs an interactive terminal — \
             use `graph plan run` or `graph ask` for scripting"
        );
    }
    let runtime = Runtime::init()?;
    let log_path = init_debug_log(&runtime, verbosity);
    let result = run_plan_workbench(&runtime, name_or_path, log_path).await;
    // MCP child processes must shut down before the tokio runtime drops.
    runtime.shutdown().await;
    result
}

/// Route tracing to a log file: the TUI owns the terminal, so stderr
/// output would scribble over it. Always on — the default filter keeps
/// the workbench's own instrumentation at debug and everything else at
/// warn; `-v` flags raise it and `GRAPH_LOG` overrides it entirely.
/// The path resolves `GRAPH_WORKBENCH_LOG` → `[workbench].log_path` →
/// `<data_dir>/workbench.log` (appended across sessions).
fn init_debug_log(runtime: &Runtime, verbosity: u8) -> Option<std::path::PathBuf> {
    let path = log_path(&runtime.config, std::env::var_os("GRAPH_WORKBENCH_LOG"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("GRAPH_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_log_filter(verbosity)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init()
        .ok()?;
    tracing::info!(
        target: "workbench",
        "── session started (graph {}) ──",
        env!("CARGO_PKG_VERSION")
    );
    Some(path)
}

/// Env var → `[workbench].log_path` (tilde-expanded) → the data dir.
fn log_path(
    config: &graph_config::Config,
    env_override: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    if let Some(path) = env_override {
        return std::path::PathBuf::from(path);
    }
    match &config.workbench.log_path {
        Some(path) => graph_config::expand_tilde(path),
        None => graph_config::expand_tilde(&config.settings.data_dir).join("workbench.log"),
    }
}

/// `workbench` is the explicit target on every workbench log line, so the
/// instrumentation stays selectable regardless of module layout.
fn default_log_filter(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "warn,workbench=debug",
        1 => "info,workbench=debug",
        2 => "debug,workbench=trace",
        _ => "trace",
    }
}

async fn run_plan_workbench(
    runtime: &Runtime,
    name_or_path: Option<String>,
    log_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let doc = match &name_or_path {
        Some(arg) => Some(resolve_doc(runtime, arg)?),
        None => None,
    };
    let store = runtime.store()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Plan runs report through their own sink; gated runs add a UiGate.
    let run_sink: Arc<dyn EventSink> = Arc::new(chat::ChannelSink::plan_run(tx.clone()));
    // Either ChannelSink would do: both feed the same channel, and usage is
    // the one event they report identically regardless of kind.
    runtime.usage.attach_events(run_sink.clone());
    let pipeline = runtime.pipeline(&store, run_sink).await?;

    // The draft is shared between the reducer's world (via messages), the
    // workbench tools, and the effect executor.
    let draft = Arc::new(std::sync::Mutex::new(tools::DraftState::new(doc.clone())));

    // The chat agent: normal catalog + the workbench draft tools.
    let agent_sink: Arc<dyn EventSink> = Arc::new(chat::ChannelSink::agent(tx.clone()));
    let toolbox = runtime.toolbox(&store, agent_sink.clone()).await?;
    // The workbench doesn't yet support open-ended sub-tasks, so hide
    // `plan_and_execute` from both the chat agent's tool list and the
    // Context tab's catalog view without removing it from the shared catalog.
    let visible_catalog: Arc<dyn ToolRegistry> = Arc::new(ExcludingRegistry::new(
        toolbox.clone() as Arc<dyn ToolRegistry>,
        vec!["plan_and_execute".to_string()],
    ));
    let plans_dir = runtime.plans_dir();
    let debug = Arc::new(runner::DebugControls::default());
    let workbench_tools = Arc::new(tools::WorkbenchTools::new(
        draft.clone(),
        pipeline.clone(),
        plans_dir.clone(),
        debug.clone(),
        tx.clone(),
    ));
    let fs_tools = Arc::new(
        fs_tools::FsTools::new(std::env::current_dir()?)
            .context("failed to resolve the workbench project directory")?,
    );
    let registry: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
        visible_catalog.clone(),
        workbench_tools,
        fs_tools,
    ]));
    let mut agent = runtime.agent(agent_sink, registry)?;
    agent.system_prompt = workbench_system_prompt(
        &agent.system_prompt,
        runtime.config.prompts.workbench.as_deref(),
    );

    let context = Arc::new(WorkbenchContext {
        agent,
        pipeline,
        history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        draft,
        catalog: visible_catalog,
        store,
        plans_dir,
        debug,
        tx: tx.clone(),
    });

    let mut app = App::new(doc);
    app.log_path = log_path;
    effects::run_effect(app::Effect::LoadContext, &context);
    if app.ws.doc.is_some() {
        effects::run_effect(app::Effect::Validate, &context);
    }

    let mut terminal = setup_terminal()?;
    let loop_result = event_loop(&mut terminal, &mut app, &mut rx, &context).await;
    restore_terminal(&mut terminal)?;
    loop_result
}

fn resolve_doc(runtime: &Runtime, name_or_path: &str) -> Result<PlanDoc> {
    let path = std::path::Path::new(name_or_path);
    if path.exists() {
        return load_plan_doc(path).context("failed to load plan file");
    }
    let loaded = runtime.plan_docs();
    loaded
        .docs
        .iter()
        .find(|d| d.identifier == name_or_path)
        .cloned()
        .with_context(|| match loaded.skip_reason(name_or_path) {
            Some(reason) => format!("plan '{name_or_path}' failed to load — {reason}"),
            None => format!("'{name_or_path}' is neither a file nor a known plan identifier"),
        })
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
    context: &Arc<WorkbenchContext>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    // The animation heartbeat only fires while something is executing, so
    // an idle workbench draws nothing and paused states stay static.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        let msg = tokio::select! {
            maybe_event = term_events.next() => match maybe_event {
                Some(Ok(event)) => Msg::Term(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            Some(msg) = rx.recv() => msg,
            _ = ticker.tick(), if app.wants_tick() => Msg::Tick,
        };
        if let Some((level, line)) = msg.log_line() {
            match level {
                tracing::Level::TRACE => tracing::trace!(target: "workbench", "{line}"),
                _ => tracing::debug!(target: "workbench", "{line}"),
            }
        }
        let mode_before = app.mode.label();
        let status_before = app.status.clone();
        for effect in app::update(app, msg) {
            tracing::debug!(target: "workbench", "effect: {}", effect.label());
            effects::run_effect(effect, context);
        }
        if app.mode.label() != mode_before {
            tracing::debug!(
                target: "workbench",
                "mode: {mode_before} → {}",
                app.mode.label()
            );
        }
        if app.status != status_before {
            tracing::trace!(target: "workbench", "status: {}", app.status);
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

// ── Terminal lifecycle ───────────────────────────────────────────────────

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Bracketed paste: a multi-line paste arrives as one Event::Paste
    // instead of a stream of keypresses whose Enters would submit mid-paste.
    // Mouse capture routes clicks/scroll to us as Event::Mouse; native
    // text selection then needs the terminal's modifier (Shift/Option).
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    install_panic_hook();
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// A panic mid-draw must not leave the user's terminal in raw mode (or
/// with bracketed paste / mouse capture still on) with no visible error.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                DisableMouseCapture,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_resolves_env_then_config_then_data_dir() {
        let mut config = graph_config::Config::default();
        assert!(
            log_path(&config, None).ends_with("workbench.log"),
            "default lands in the data dir"
        );

        config.workbench.log_path = Some("~/logs/wb.log".into());
        let from_config = log_path(&config, None);
        assert!(from_config.ends_with("logs/wb.log"));
        assert!(
            !from_config.starts_with("~"),
            "config paths are tilde-expanded"
        );

        assert_eq!(
            log_path(&config, Some("/tmp/env.log".into())),
            std::path::PathBuf::from("/tmp/env.log"),
            "the env var beats the config"
        );
    }

    #[test]
    fn default_log_filter_scales_with_verbosity() {
        assert_eq!(default_log_filter(0), "warn,workbench=debug");
        assert_eq!(default_log_filter(1), "info,workbench=debug");
        assert_eq!(default_log_filter(2), "debug,workbench=trace");
        assert_eq!(default_log_filter(9), "trace");
    }

    #[test]
    fn tool_rules_and_control_steps_survive_a_prompt_override() {
        let prompt = workbench_system_prompt("BASE", Some("# House style\nBe terse."));
        assert!(prompt.starts_with("BASE\n\n# House style"));
        assert!(!prompt.contains("# Plan workbench"));
        assert!(prompt.contains(WORKBENCH_TOOL_RULES));
        assert!(prompt.contains(CONTROL_STEP_NAMING));
        assert!(prompt.contains(graph_core::pipeline::CONTROL_STEP_RULES));
    }

    #[test]
    fn default_prompt_used_when_unset() {
        let prompt = workbench_system_prompt("BASE", None);
        assert!(prompt.contains(WORKBENCH_SYSTEM_PROMPT));
        assert!(prompt.contains(WORKBENCH_TOOL_RULES));
    }
}
