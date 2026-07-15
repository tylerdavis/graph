//! `graph workbench` — the interactive plan IDE: a dual-pane TUI with the
//! chat agent on the left and the plan workspace (structure, context, runs)
//! on the right. See docs/using/plan-workbench.mdx.

mod app;
mod chat;
mod editor;
mod effects;
mod fs_tools;
mod plan_ws;
mod runner;
mod tools;
mod ui;

use crate::cli::WorkbenchCommand;
use crate::runtime::Runtime;
use anyhow::{bail, Context, Result};
use app::{App, Msg};
use crossterm::event::EventStream;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use effects::WorkbenchContext;
use futures::StreamExt;
use graph_core::pipeline::doc::{load_plan_doc, PlanDoc};
use graph_core::{CompositeRegistry, EventSink, ToolRegistry};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{IsTerminal, Stdout};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Appended to the chat agent's system prompt inside the workbench;
/// `[prompts].workbench` in config replaces it.
const WORKBENCH_SYSTEM_PROMPT: &str = "# Plan workbench\n\
You are running inside the graph plan workbench: a side pane shows the user \
the current draft plan, live. Operate on that draft with the workbench \
tools:\n\
- workbench__list_plans: enumerate the plan catalog when the user asks \
what exists.\n\
- workbench__show_plan: read any catalog plan's YAML without touching \
the draft — always use this to inspect or reference existing plans.\n\
- workbench__load_plan: open a DIFFERENT plan the user explicitly names \
(identifier or YAML path). Never use it to edit, fix, or continue the \
current draft (use the editing tools) or to read a plan (use \
show_plan). It replaces the draft and FAILS if there are unsaved \
changes unless you pass overwrite_draft: true, which you may only do \
after the user confirms discarding them.\n\
- workbench__draft_plan: draft a plan from a goal when there is no draft \
yet, or when the user asks to start over from scratch. Pass the user's \
request as a self-contained goal; pass feedback when revising after \
validation problems or user corrections; pass fresh: true when the goal \
is a NEW plan — otherwise the current draft is treated as the plan \
under revision and keeps its identifier and metadata.\n\
- workbench__get_plan: re-read the draft YAML. The current draft is \
already included below in this prompt each turn — call this only to \
re-check after your own edits within the same turn.\n\
- workbench__update_metadata / workbench__add_step / \
workbench__update_step / workbench__delete_step: precise edits — patch \
the plan's metadata, insert a step (before/after an id, or appended), \
update one step's fields (newId renames it and rewrites downstream \
references), or remove a step. When the user asks to change the current \
plan, prefer these — however complex the change, control flow included: \
each edit is validated atomically and rejected with the problems if it \
would make the plan invalid, so sequential edits are safer than a \
wholesale re-draft.\n\
- workbench__set_plan: replace the draft with complete YAML (get, modify, \
set) for wholesale rewrites the precise tools don't cover.\n\
- workbench__restore_draft: one-level undo of the last draft replacement \
(again to redo) — use it when you or the user replaced the draft by \
mistake.\n\
- workbench__validate_plan: check the draft and surface the verdict in \
the pane.\n\
- workbench__run_plan: execute the draft when the user asks. Prefer \
gated=true for plans with side effects — a debug run pauses for the USER \
to step/continue/skip/abort and breaks on failing calls (do not promise \
to answer those prompts yourself). Pass breakpoints (top-level step ids) \
to run freely to a step of interest. Run one plan at a time.\n\
- workbench__save_plan: write the draft to disk when the user asks to \
save.\n\
- workbench__read_file / workbench__grep / workbench__glob: read-only \
research over the project directory the workbench was started in \
(paths outside it are rejected). Use them when the user asks about the \
codebase or a draft needs grounding in real files — glob to find files, \
grep to search contents, read_file for the surrounding context.\n\
Never claim the draft changed, ran, or was saved without calling the \
matching tool. The user can also drive everything with keybindings \
(v validate, r run, g gated run, Ctrl+S save, u undo) — never run a plan \
with side effects unprompted.";

/// Control-step reference for the chat agent: the naming rules, then the
/// same usage rules the draft_plan planner sees (shared so they can't
/// drift) — the agent has the full schema and edits control flow directly.
const CONTROL_STEP_NAMING: &str = "\n\n# Control steps\n\
Step toolNames are namespaced `server__tool` names from the catalog \
(e.g. linear__list_issues), plus ONLY these bare control steps: exit, \
decide, map, reduce, and plan_and_execute. There is no `gate`, `assert`, \
or `exit_gate` tool and no `gate:` field. A plan finishes with `solver` \
(LLM synthesis) OR `output` (a structured template map), never both.\n\n";

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
    let pipeline = runtime.pipeline(&store, run_sink).await?;

    // The draft is shared between the reducer's world (via messages), the
    // workbench tools, and the effect executor.
    let draft = Arc::new(std::sync::Mutex::new(tools::DraftState::new(doc.clone())));

    // The chat agent: normal catalog + the workbench draft tools.
    let agent_sink: Arc<dyn EventSink> = Arc::new(chat::ChannelSink::agent(tx.clone()));
    let toolbox = runtime.toolbox(&store, agent_sink.clone()).await?;
    let plans_dir = runtime
        .config
        .plans
        .paths
        .first()
        .map(|p| graph_config::expand_tilde(p));
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
        toolbox.clone() as Arc<dyn ToolRegistry>,
        workbench_tools,
        fs_tools,
    ]));
    let mut agent = runtime.agent(agent_sink, registry)?;
    agent.system_prompt.push_str("\n\n");
    agent.system_prompt.push_str(
        runtime
            .config
            .prompts
            .workbench
            .as_deref()
            .unwrap_or(WORKBENCH_SYSTEM_PROMPT),
    );
    agent.system_prompt.push_str(CONTROL_STEP_NAMING);
    agent
        .system_prompt
        .push_str(graph_core::pipeline::CONTROL_STEP_RULES);

    let context = Arc::new(WorkbenchContext {
        agent,
        pipeline,
        history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        draft,
        catalog: toolbox as Arc<dyn ToolRegistry>,
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
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    install_panic_hook();
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// A panic mid-draw must not leave the user's terminal in raw mode with no
/// visible error.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
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
}
