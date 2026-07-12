//! `graph workbench` — the interactive plan IDE: a dual-pane TUI with the
//! chat agent on the left and the plan workspace (structure, context, runs)
//! on the right. See docs/using/plan-workbench.mdx.

mod app;
mod chat;
mod editor;
mod effects;
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

/// Appended to the chat agent's system prompt inside the workbench.
const WORKBENCH_SYSTEM_PROMPT: &str = "\n\n# Plan workbench\n\
You are running inside the graph plan workbench: a side pane shows the user \
the current draft plan, live. Operate on that draft with the workbench \
tools:\n\
- workbench__list_plans: enumerate the plan catalog when the user asks \
what exists.\n\
- workbench__load_plan: load an existing plan (identifier or YAML path) \
into the pane; it replaces the draft, so confirm first if there are \
unsaved changes.\n\
- workbench__draft_plan: create or revise the draft from a goal. Pass the \
user's request as a self-contained goal; pass feedback when revising after \
validation problems or user corrections.\n\
- workbench__get_plan: read the current draft YAML before targeted edits.\n\
- workbench__set_plan: replace the draft with complete YAML (get, modify, \
set) for surgical changes.\n\
- workbench__validate_plan: check the draft and surface the verdict in \
the pane.\n\
- workbench__run_plan: execute the draft when the user asks. Prefer \
gated=true for plans with side effects — each tool call then pauses for \
the USER to proceed/skip/abort (do not promise to answer those prompts \
yourself). Run one plan at a time.\n\
- workbench__save_plan: write the draft to disk when the user asks to \
save.\n\
Never claim the draft changed, ran, or was saved without calling the \
matching tool. The user can also drive everything with keybindings \
(v validate, r run, g gated run, Ctrl+S save) — never run a plan with \
side effects unprompted.";

pub async fn run(command: WorkbenchCommand) -> Result<()> {
    let WorkbenchCommand::Plan { name_or_path } = command;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "the workbench needs an interactive terminal — \
             use `graph plan run` or `graph ask` for scripting"
        );
    }
    let runtime = Runtime::init()?;
    let result = run_plan_workbench(&runtime, name_or_path).await;
    // MCP child processes must shut down before the tokio runtime drops.
    runtime.shutdown().await;
    result
}

async fn run_plan_workbench(runtime: &Runtime, name_or_path: Option<String>) -> Result<()> {
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
    let draft = Arc::new(std::sync::Mutex::new(doc.clone()));

    // The chat agent: normal catalog + the workbench draft tools.
    let agent_sink: Arc<dyn EventSink> = Arc::new(chat::ChannelSink::agent(tx.clone()));
    let toolbox = runtime.toolbox(&store, agent_sink.clone()).await?;
    let plans_dir = runtime
        .config
        .plans
        .paths
        .first()
        .map(|p| graph_config::expand_tilde(p));
    let workbench_tools = Arc::new(tools::WorkbenchTools::new(
        draft.clone(),
        pipeline.clone(),
        plans_dir.clone(),
        tx.clone(),
    ));
    let registry: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
        toolbox.clone() as Arc<dyn ToolRegistry>,
        workbench_tools,
    ]));
    let mut agent = runtime.agent(agent_sink, registry)?;
    agent.system_prompt.push_str(WORKBENCH_SYSTEM_PROMPT);

    let context = Arc::new(WorkbenchContext {
        agent,
        pipeline,
        history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        draft,
        catalog: toolbox as Arc<dyn ToolRegistry>,
        store,
        plans_dir,
        tx: tx.clone(),
    });

    let mut app = App::new(doc);
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
    let docs = runtime.plan_docs()?;
    docs.into_iter()
        .find(|d| d.identifier == name_or_path)
        .with_context(|| format!("'{name_or_path}' is neither a file nor a known plan identifier"))
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
    context: &Arc<WorkbenchContext>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        let msg = tokio::select! {
            maybe_event = term_events.next() => match maybe_event {
                Some(Ok(event)) => Msg::Term(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            Some(msg) = rx.recv() => msg,
        };
        for effect in app::update(app, msg) {
            effects::run_effect(effect, context);
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
