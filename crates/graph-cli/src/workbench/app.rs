//! Workbench application state and the pure reducer: `update` consumes one
//! [`Msg`] and returns [`Effect`]s for the executor to spawn — no I/O here,
//! so the whole interaction model is testable headless.

use super::editor::{EditorContext, EditorState};
use super::plan_ws::{PlanWorkspace, WsTab};
use super::runner::UiDecision;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::{ToolDef, ToolShape};
use serde_json::{Map, Value};
use std::collections::HashSet;
use tokio::sync::oneshot;
use tui_textarea::TextArea;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chat,
    Workspace,
}

/// Why the debugger paused.
#[derive(Debug, Clone)]
pub enum GateKind {
    /// Before a tool call: step, continue, skip, or abort.
    BeforeCall,
    /// The call failed (break-on-exception): replace, let it fail, or abort.
    OnError { error: Value },
}

/// A pending debugger decision: the run is parked on `reply`.
pub struct GatePrompt {
    pub kind: GateKind,
    pub path: String,
    pub tool: String,
    pub input: Value,
    /// Plan-call nesting at the pause; empty at the top level.
    pub call_stack: Vec<String>,
    /// The template scope at the pause — the debugger's locals.
    pub scope: Map<String, Value>,
    pub reply: Option<oneshot::Sender<UiDecision>>,
}

impl GatePrompt {
    /// The top-level step id this pause belongs to, when it's a step of
    /// the draft being debugged (empty call stack).
    pub fn top_level_step(&self) -> Option<&str> {
        if self.call_stack.is_empty() {
            self.path.split('/').next()
        } else {
            None
        }
    }
}

/// The call (or phase) currently executing, for the live indicator.
pub struct InFlight {
    pub path: String,
    pub tool: String,
    pub started: std::time::Instant,
}

pub enum Mode {
    Idle,
    /// An agent turn is in flight.
    Chatting,
    /// A plan run is in flight.
    Running {
        gated: bool,
    },
    /// A debug run is paused (before a call, or on an error), awaiting a
    /// decision. Non-modal: the workspace stays navigable.
    Paused(GatePrompt),
    /// A modal editor (inject result, run input) or confirm dialog is open.
    Editing(Box<EditorState>),
}

impl Mode {
    /// Short name for the debug log's mode-transition lines.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Chatting => "chatting",
            Mode::Running { gated: false } => "running",
            Mode::Running { gated: true } => "debug-running",
            Mode::Paused(_) => "paused",
            Mode::Editing(_) => "editing",
        }
    }
}

pub enum ChatEntry {
    User(String),
    Assistant(String),
    /// Dim progress lines: tool activity, errors.
    Activity(String),
}

pub struct ChatState {
    pub entries: Vec<ChatEntry>,
    pub input: TextArea<'static>,
    /// Scrollback offset in lines from the bottom; 0 follows. `Cell` so
    /// the renderer can clamp it to the actual (wrapped) content height.
    pub scroll: std::cell::Cell<u16>,
}

impl Default for ChatState {
    fn default() -> Self {
        let mut input = TextArea::default();
        input.set_placeholder_text("describe the plan you want, or ask about tools…");
        Self {
            entries: Vec::new(),
            input,
            scroll: std::cell::Cell::new(0),
        }
    }
}

pub struct App {
    pub focus: Focus,
    pub mode: Mode,
    pub chat: ChatState,
    pub ws: PlanWorkspace,
    /// One-line status shown in the bottom bar.
    pub status: String,
    pub dirty: bool,
    /// An agent turn is executing (independent of `mode`: a tool-driven
    /// plan run pauses and resumes within a turn).
    pub turn_in_flight: bool,
    /// Display copy of the debugger's breakpoints (step ids). Source of
    /// truth for the gutter; synced to the run task via Effect::SyncDebug.
    /// Survives draft replacement — stale ids simply never match.
    pub breakpoints: HashSet<String>,
    /// Animation frame counter, bumped by Msg::Tick while work is running.
    pub tick: u8,
    /// The call currently executing, for the status-bar indicator.
    pub in_flight: Option<InFlight>,
    /// The help overlay — independent of `mode`, so it works while paused
    /// without touching the parked gate reply. Any key closes it.
    pub show_help: bool,
    /// Where the debug log is being written, shown in the help overlay.
    pub log_path: Option<std::path::PathBuf>,
    pub should_quit: bool,
}

impl App {
    pub fn new(doc: Option<PlanDoc>) -> Self {
        let mut ws = PlanWorkspace::default();
        if let Some(doc) = doc {
            ws.set_doc(doc);
        }
        Self {
            focus: Focus::Chat,
            mode: Mode::Idle,
            chat: ChatState::default(),
            ws,
            status: "Tab focus · ? help · Ctrl+C quit".to_string(),
            dirty: false,
            turn_in_flight: false,
            breakpoints: HashSet::new(),
            tick: 0,
            in_flight: None,
            show_help: false,
            log_path: None,
            should_quit: false,
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.mode, Mode::Idle)
    }

    /// The event loop only ticks while something is actually executing —
    /// a pause is static, so paused-state tests stay deterministic.
    pub fn wants_tick(&self) -> bool {
        self.turn_in_flight || matches!(self.mode, Mode::Running { .. } | Mode::Chatting)
    }

    /// Where a paused debug run returns to: the agent's turn when it
    /// launched the run, otherwise the keyboard-run state.
    fn resume_mode(&self) -> Mode {
        if self.turn_in_flight {
            Mode::Chatting
        } else {
            Mode::Running { gated: true }
        }
    }
}

/// Everything that can happen: terminal input and engine callbacks.
pub enum Msg {
    Term(Event),
    /// Animation heartbeat from the event loop, only while work runs.
    Tick,
    // Chat / agent turn
    AgentDelta(String),
    AgentToolStarted(String),
    AgentToolFinished {
        name: String,
        is_error: bool,
    },
    TurnFinished(Result<(), String>),
    // Draft changes published by the workbench tools; `dirty` is false
    // when the draft came straight from disk (load), true for edits.
    DraftReplaced {
        doc: Box<PlanDoc>,
        dirty: bool,
    },
    // Plan run
    /// A run was launched by the agent's `workbench__run_plan` tool (the
    /// keyboard path resets the pane directly). `breakpoints` is Some when
    /// the agent replaced the set.
    RunStarted {
        gated: bool,
        breakpoints: Option<Vec<String>>,
    },
    Planning,
    Synthesizing,
    StepStarted {
        path: String,
        tool: String,
        input: Value,
        top_level: bool,
    },
    StepFinished {
        path: String,
        result: Value,
        is_error: bool,
        top_level: bool,
    },
    SolverDelta(String),
    GateAsk {
        kind: GateKind,
        path: String,
        tool: String,
        input: Value,
        call_stack: Vec<String>,
        scope: Map<String, Value>,
        reply: oneshot::Sender<UiDecision>,
    },
    RunFinished {
        headline: String,
        is_error: bool,
        results: Map<String, Value>,
    },
    // Effect completions
    Validated(Vec<String>),
    ContextLoaded {
        tools: Vec<ToolDef>,
        shapes: Vec<ToolShape>,
    },
    Saved(Result<String, String>),
}

impl Msg {
    /// A one-line summary for the debug log, with the level it should log
    /// at: TRACE for high-frequency noise (keys, stream deltas), DEBUG for
    /// everything meaningful. None for the pure animation heartbeat.
    pub fn log_line(&self) -> Option<(tracing::Level, String)> {
        use tracing::Level;
        let line = match self {
            Msg::Tick => return None,
            Msg::Term(Event::Key(key)) => {
                return Some((
                    Level::TRACE,
                    format!("key {:?} {:?}", key.modifiers, key.code),
                ))
            }
            Msg::Term(event) => return Some((Level::TRACE, format!("term event {event:?}"))),
            Msg::AgentDelta(text) => {
                return Some((Level::TRACE, format!("agent delta ({} chars)", text.len())))
            }
            Msg::SolverDelta(text) => {
                return Some((Level::TRACE, format!("solver delta ({} chars)", text.len())))
            }
            Msg::AgentToolStarted(name) => format!("agent tool started: {name}"),
            Msg::AgentToolFinished { name, is_error } => {
                format!(
                    "agent tool finished: {name}{}",
                    if *is_error { " (error)" } else { "" }
                )
            }
            Msg::TurnFinished(result) => match result {
                Ok(()) => "agent turn finished".to_string(),
                Err(error) => format!("agent turn failed: {error}"),
            },
            Msg::DraftReplaced { doc, dirty } => format!(
                "draft replaced: '{}' ({} steps, dirty={dirty})",
                doc.identifier,
                doc.steps.len()
            ),
            Msg::RunStarted { gated, breakpoints } => {
                format!("agent run started (gated={gated}, breakpoints={breakpoints:?})")
            }
            Msg::Planning => "planning…".to_string(),
            Msg::Synthesizing => "synthesizing…".to_string(),
            Msg::StepStarted { path, tool, .. } => format!("step started: {path} {tool}"),
            Msg::StepFinished {
                path,
                result,
                is_error,
                ..
            } => format!(
                "step finished: {path}{} ({} result bytes)",
                if *is_error { " (error)" } else { "" },
                result.to_string().len()
            ),
            Msg::GateAsk {
                kind,
                path,
                tool,
                call_stack,
                ..
            } => format!(
                "gate ask: {} at {path} ({tool}), call-stack depth {}",
                match kind {
                    GateKind::BeforeCall => "before-call",
                    GateKind::OnError { .. } => "on-error",
                },
                call_stack.len()
            ),
            Msg::RunFinished {
                headline, is_error, ..
            } => format!("run finished (is_error={is_error}): {headline}"),
            Msg::Validated(problems) => format!("validated: {} problem(s)", problems.len()),
            Msg::ContextLoaded { tools, shapes } => format!(
                "context loaded: {} tools, {} shapes",
                tools.len(),
                shapes.len()
            ),
            Msg::Saved(result) => match result {
                Ok(path) => format!("saved to {path}"),
                Err(error) => format!("save failed: {error}"),
            },
        };
        Some((Level::DEBUG, line))
    }
}

/// Work the reducer wants done; the executor spawns it and reports back
/// with a `Msg`.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    RunAgentTurn {
        message: String,
    },
    StartRun {
        gated: bool,
        input: Value,
    },
    Validate,
    LoadContext,
    SavePlan,
    /// Mirror the display breakpoints into the shared debug controls.
    SyncDebug {
        breakpoints: HashSet<String>,
    },
}

impl Effect {
    /// Short name for the debug log (details are logged by the executor).
    pub fn label(&self) -> &'static str {
        match self {
            Effect::RunAgentTurn { .. } => "run-agent-turn",
            Effect::StartRun { gated: false, .. } => "start-run",
            Effect::StartRun { gated: true, .. } => "start-debug-run",
            Effect::Validate => "validate",
            Effect::LoadContext => "load-context",
            Effect::SavePlan => "save-plan",
            Effect::SyncDebug { .. } => "sync-debug",
        }
    }
}

pub fn update(app: &mut App, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Term(event) => on_terminal_event(app, event),

        Msg::Tick => {
            app.tick = app.tick.wrapping_add(1);
            Vec::new()
        }

        Msg::AgentDelta(text) => {
            if let Some(ChatEntry::Assistant(buffer)) = app.chat.entries.last_mut() {
                buffer.push_str(&text);
            } else {
                app.chat.entries.push(ChatEntry::Assistant(text));
            }
            app.chat.scroll.set(0);
            Vec::new()
        }
        Msg::AgentToolStarted(name) => {
            app.chat
                .entries
                .push(ChatEntry::Activity(format!("→ {name}")));
            Vec::new()
        }
        Msg::AgentToolFinished { name, is_error } => {
            if is_error {
                app.chat
                    .entries
                    .push(ChatEntry::Activity(format!("✗ {name} failed")));
            }
            Vec::new()
        }
        Msg::TurnFinished(result) => {
            app.mode = Mode::Idle;
            app.turn_in_flight = false;
            app.in_flight = None;
            if let Err(error) = result {
                app.chat
                    .entries
                    .push(ChatEntry::Activity(format!("error: {error}")));
            }
            Vec::new()
        }

        Msg::DraftReplaced { doc, dirty } => {
            app.status = if dirty {
                "draft updated".to_string()
            } else {
                format!("loaded plan '{}'", doc.identifier)
            };
            app.ws.set_doc(*doc);
            app.dirty = dirty;
            vec![Effect::Validate]
        }

        Msg::RunStarted { gated, breakpoints } => {
            if let Some(breakpoints) = breakpoints {
                app.breakpoints = breakpoints.into_iter().collect();
            }
            app.ws.run_starting(gated);
            app.status = if gated {
                "agent started a debug run — pauses will ask you".to_string()
            } else {
                "agent is running the plan…".to_string()
            };
            Vec::new()
        }
        Msg::Planning => {
            app.ws.run_log_info("planning…");
            app.in_flight = Some(InFlight {
                path: "planning".to_string(),
                tool: String::new(),
                started: std::time::Instant::now(),
            });
            Vec::new()
        }
        Msg::Synthesizing => {
            app.ws.run_log_info("synthesizing…");
            app.in_flight = Some(InFlight {
                path: "synthesizing".to_string(),
                tool: String::new(),
                started: std::time::Instant::now(),
            });
            Vec::new()
        }
        Msg::StepStarted {
            path,
            tool,
            input,
            top_level,
        } => {
            app.ws.run_log_info(&format!("▸ {path} {tool}"));
            app.in_flight = Some(InFlight {
                path: path.clone(),
                tool: tool.clone(),
                started: std::time::Instant::now(),
            });
            if top_level {
                app.ws.step_started(&path, input);
            }
            Vec::new()
        }
        Msg::StepFinished {
            path,
            result,
            is_error,
            top_level,
        } => {
            if is_error {
                app.ws.run_log_error(&format!("✗ {path} failed"));
            } else {
                app.ws.run_log_info(&format!("✓ {path}"));
            }
            // Concurrent map items: a finish for an older call must not
            // clear a newer start's indicator.
            if app.in_flight.as_ref().is_some_and(|f| f.path == path) {
                app.in_flight = None;
            }
            if top_level {
                app.ws.step_finished(&path, result, is_error);
            }
            Vec::new()
        }
        Msg::SolverDelta(text) => {
            app.ws.solver_text.push_str(&text);
            Vec::new()
        }
        Msg::GateAsk {
            kind,
            path,
            tool,
            input,
            call_stack,
            scope,
            reply,
        } => {
            app.in_flight = None;
            app.ws.tab = WsTab::Plan;
            let prompt = GatePrompt {
                kind,
                path,
                tool,
                input,
                call_stack,
                scope,
                reply: Some(reply),
            };
            if let Some(step) = prompt.top_level_step() {
                app.ws.select_step(step);
            }
            match &prompt.kind {
                GateKind::BeforeCall => {
                    app.ws.step_running(&prompt.path);
                    app.status = format!(
                        "paused at {} — n next step · c continue · s skip · b breakpoint · a abort",
                        prompt.path
                    );
                }
                GateKind::OnError { .. } => {
                    app.status = format!(
                        "✗ {} failed — s inject result · n let it fail · a abort",
                        prompt.path
                    );
                }
            }
            app.mode = Mode::Paused(prompt);
            Vec::new()
        }
        Msg::RunFinished {
            headline,
            is_error,
            results,
        } => {
            app.mode = if app.turn_in_flight {
                Mode::Chatting
            } else {
                Mode::Idle
            };
            app.in_flight = None;
            app.ws.run_finished(&headline, is_error, results);
            app.status = headline;
            Vec::new()
        }

        Msg::Validated(problems) => {
            app.status = if problems.is_empty() {
                "✓ plan is valid".to_string()
            } else {
                format!("✗ {} validation problem(s) — see plan tab", problems.len())
            };
            app.ws.diagnostics = problems;
            Vec::new()
        }
        Msg::ContextLoaded { tools, shapes } => {
            app.ws.set_context(tools, shapes);
            Vec::new()
        }
        Msg::Saved(result) => {
            match result {
                Ok(path) => {
                    app.dirty = false;
                    app.status = format!("saved to {path}");
                }
                Err(error) => app.status = format!("save failed: {error}"),
            }
            Vec::new()
        }
    }
}

fn on_terminal_event(app: &mut App, event: Event) -> Vec<Effect> {
    let Event::Key(key) = event else {
        return Vec::new();
    };
    if key.kind != KeyEventKind::Press {
        return Vec::new();
    }

    // The help overlay swallows one key and closes, whatever the mode.
    if app.show_help {
        app.show_help = false;
        return Vec::new();
    }

    match &mut app.mode {
        // Paused is non-modal: debug keys first, navigation falls through.
        Mode::Paused(_) => return on_paused_key(app, key),
        Mode::Editing(_) => return on_editor_key(app, key),
        _ => {}
    }

    // Global bindings.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return request_quit(app);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        return save(app);
    }
    if let Some(effects) = on_nav_key(app, key) {
        return effects;
    }

    match app.focus {
        Focus::Chat => on_chat_key(app, key),
        Focus::Workspace => on_workspace_key(app, key),
    }
}

/// Navigation shared by normal and paused states: focus, tabs, selection,
/// detail scrolling, breakpoints. Returns None when the key isn't
/// navigation (so callers route it further). Selection/breakpoint keys
/// only apply with workspace focus in normal states — while paused the
/// caller passes `force` because the chat input is inert.
fn on_nav_key(app: &mut App, key: KeyEvent) -> Option<Vec<Effect>> {
    if key.code == KeyCode::Tab {
        app.focus = match app.focus {
            Focus::Chat => Focus::Workspace,
            Focus::Workspace => Focus::Chat,
        };
        return Some(Vec::new());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c @ '1'..='3') = key.code {
            app.ws.tab = tab_for(c);
            return Some(Vec::new());
        }
    }
    None
}

/// Workspace-scoped navigation: shared by on_workspace_key and the paused
/// fall-through. Returns None for keys it doesn't handle.
fn on_workspace_nav(app: &mut App, key: KeyEvent) -> Option<Vec<Effect>> {
    match key.code {
        KeyCode::Char(c @ '1'..='3') => {
            app.ws.tab = tab_for(c);
            Some(Vec::new())
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.ws.select_next();
            Some(Vec::new())
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.ws.select_previous();
            Some(Vec::new())
        }
        KeyCode::PageDown => {
            app.ws.scroll_by(false, 5);
            Some(Vec::new())
        }
        KeyCode::PageUp => {
            app.ws.scroll_by(true, 5);
            Some(Vec::new())
        }
        KeyCode::Char('b') => Some(toggle_breakpoint(app)),
        KeyCode::Char('v') => {
            if app.ws.doc.is_none() {
                app.status = "no plan to validate".to_string();
                return Some(Vec::new());
            }
            Some(vec![Effect::Validate])
        }
        _ => None,
    }
}

fn toggle_breakpoint(app: &mut App) -> Vec<Effect> {
    if app.ws.tab != WsTab::Plan {
        return Vec::new();
    }
    let Some(row) = app.ws.steps.get(app.ws.selected) else {
        return Vec::new();
    };
    let id = row.id.clone();
    if app.breakpoints.remove(&id) {
        app.status = format!("breakpoint cleared from {id}");
    } else {
        app.breakpoints.insert(id.clone());
        app.status = format!("breakpoint set on {id}");
    }
    vec![Effect::SyncDebug {
        breakpoints: app.breakpoints.clone(),
    }]
}

fn tab_for(c: char) -> WsTab {
    match c {
        '1' => WsTab::Plan,
        '2' => WsTab::Context,
        _ => WsTab::Run,
    }
}

fn request_quit(app: &mut App) -> Vec<Effect> {
    if app.busy() || app.dirty {
        let reason = if app.dirty {
            "unsaved draft — quit anyway? (y/n)"
        } else {
            "a task is still running — quit anyway? (y/n)"
        };
        app.mode = Mode::Editing(Box::new(EditorState::confirm_quit(reason)));
    } else {
        app.should_quit = true;
    }
    Vec::new()
}

fn save(app: &mut App) -> Vec<Effect> {
    if app.ws.doc.is_none() {
        app.status = "nothing to save — no draft yet".to_string();
        return Vec::new();
    }
    vec![Effect::SavePlan]
}

fn on_chat_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            app.chat.input.insert_newline();
            Vec::new()
        }
        KeyCode::Enter => {
            if !matches!(app.mode, Mode::Idle) {
                app.status = "busy — wait for the current task to finish".to_string();
                return Vec::new();
            }
            let message = app.chat.input.lines().join("\n").trim().to_string();
            if message.is_empty() {
                return Vec::new();
            }
            app.chat.input = TextArea::default();
            app.chat.entries.push(ChatEntry::User(message.clone()));
            app.chat.scroll.set(0);
            app.mode = Mode::Chatting;
            app.turn_in_flight = true;
            vec![Effect::RunAgentTurn { message }]
        }
        // PgUp/PgDn is THE scroll binding: with chat focus it scrolls
        // the conversation.
        KeyCode::PageUp => {
            app.chat.scroll.set(app.chat.scroll.get().saturating_add(5));
            Vec::new()
        }
        KeyCode::PageDown => {
            app.chat.scroll.set(app.chat.scroll.get().saturating_sub(5));
            Vec::new()
        }
        _ => {
            app.chat.input.input(key);
            Vec::new()
        }
    }
}

fn on_workspace_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    if let Some(effects) = on_workspace_nav(app, key) {
        return effects;
    }
    match key.code {
        KeyCode::Char('?') => {
            app.show_help = true;
            Vec::new()
        }
        KeyCode::Char('r') => start_run(app, false),
        KeyCode::Char('g') => start_run(app, true),
        KeyCode::Char('q') => {
            if matches!(app.mode, Mode::Idle) {
                request_quit(app)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn start_run(app: &mut App, gated: bool) -> Vec<Effect> {
    if !matches!(app.mode, Mode::Idle) {
        app.status = "busy — wait for the current task to finish".to_string();
        return Vec::new();
    }
    let Some(doc) = &app.ws.doc else {
        app.status = "no plan to run — draft one in chat first".to_string();
        return Vec::new();
    };
    let mut input = Value::Object(Map::new());
    if let Some(schema) = &doc.input_schema {
        graph_core::pipeline::doc::apply_schema_defaults(schema, &mut input);
        if graph_core::pipeline::doc::validate_input(doc, &input).is_err() {
            // Required inputs remain — collect them in the editor.
            app.mode = Mode::Editing(Box::new(EditorState::run_input(gated, schema, &input)));
            return Vec::new();
        }
    }
    launch_run(app, gated, input)
}

fn launch_run(app: &mut App, gated: bool, input: Value) -> Vec<Effect> {
    app.mode = Mode::Running { gated };
    app.ws.run_starting(gated);
    app.status = if gated {
        if app.breakpoints.is_empty() {
            "debug run — pausing before each tool call".to_string()
        } else {
            "debug run — continuing to the first breakpoint".to_string()
        }
    } else {
        "running…".to_string()
    };
    vec![Effect::StartRun { gated, input }]
}

/// Paused (non-modal): decision keys act on the pause; quit/help/run keys
/// are blocked (they would drop the reply and silently abort); everything
/// else is ordinary navigation, regardless of focus — the chat input is
/// inert while paused so single-letter debug keys are unambiguous.
fn on_paused_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    // Decision keys.
    match key.code {
        KeyCode::Char('n') | KeyCode::Char('y') | KeyCode::Enter => {
            return decide(
                app,
                UiDecision::Proceed {
                    continue_mode: false,
                },
            );
        }
        KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            return decide(
                app,
                UiDecision::Proceed {
                    continue_mode: true,
                },
            );
        }
        KeyCode::Char('a') => {
            return decide(app, UiDecision::Abort);
        }
        KeyCode::Char('s') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            return open_inject_editor(app);
        }
        KeyCode::Char('?') => {
            app.show_help = true;
            return Vec::new();
        }
        // The app-exit controls abort the run — like Ctrl+C cancelling a
        // foreground task. Quit still requires the run to stop first.
        KeyCode::Char('q') => {
            return decide(app, UiDecision::Abort);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return decide(app, UiDecision::Abort);
        }
        // Blocked: these would replace Mode::Paused and drop the reply
        // sender — the run would silently abort.
        KeyCode::Char('r') | KeyCode::Char('g') => {
            app.status =
                "paused — decide first: n next step · c continue · s skip · a abort".to_string();
            return Vec::new();
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+S (save) waits for a decision too.
            app.status =
                "paused — decide first: n next step · c continue · s skip · a abort".to_string();
            return Vec::new();
        }
        _ => {}
    }

    // Navigation falls through, focus-independent.
    if let Some(effects) = on_nav_key(app, key) {
        return effects;
    }
    on_workspace_nav(app, key).unwrap_or_default()
}

fn decide(app: &mut App, decision: UiDecision) -> Vec<Effect> {
    let Mode::Paused(prompt) = &mut app.mode else {
        return Vec::new();
    };
    app.status = match &decision {
        UiDecision::Proceed {
            continue_mode: true,
        } => "continuing…".to_string(),
        UiDecision::Proceed { .. } => match prompt.kind {
            GateKind::BeforeCall => "stepping…".to_string(),
            GateKind::OnError { .. } => "letting the step fail…".to_string(),
        },
        UiDecision::Skip { .. } => "skipping…".to_string(),
        UiDecision::Abort => "aborting…".to_string(),
    };
    if let Some(reply) = prompt.reply.take() {
        let _ = reply.send(decision);
    }
    app.mode = app.resume_mode();
    Vec::new()
}

fn open_inject_editor(app: &mut App) -> Vec<Effect> {
    let Mode::Paused(_) = app.mode else {
        return Vec::new();
    };
    let Mode::Paused(prompt) = std::mem::replace(&mut app.mode, Mode::Idle) else {
        unreachable!()
    };
    let (prefill, provenance) = app.ws.prefill_for(&prompt.tool);
    let references = prompt
        .top_level_step()
        .map(|step| app.ws.downstream_references(step))
        .unwrap_or_default();
    app.mode = Mode::Editing(Box::new(EditorState::inject_result(
        prompt, prefill, provenance, references,
    )));
    Vec::new()
}

fn on_editor_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    let Mode::Editing(editor) = &mut app.mode else {
        return Vec::new();
    };

    // Confirm dialogs are single-key.
    if let EditorContext::ConfirmQuit = editor.context {
        match key.code {
            KeyCode::Char('y') => app.should_quit = true,
            KeyCode::Char('n') | KeyCode::Esc => app.mode = Mode::Idle,
            _ => {}
        }
        return Vec::new();
    }

    let submit = (key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('s') | KeyCode::Enter))
        || key.code == KeyCode::F(2);
    if submit {
        return submit_editor(app);
    }
    match key.code {
        KeyCode::Esc => {
            // Cancel: an inject editor returns to the pause; a run input
            // editor returns to idle.
            let editor = match std::mem::replace(&mut app.mode, Mode::Idle) {
                Mode::Editing(editor) => editor,
                _ => unreachable!(),
            };
            if let EditorContext::InjectResult { prompt } = editor.context {
                app.mode = Mode::Paused(prompt);
            }
            Vec::new()
        }
        _ => {
            editor.textarea.input(key);
            Vec::new()
        }
    }
}

fn submit_editor(app: &mut App) -> Vec<Effect> {
    let Mode::Editing(editor) = &mut app.mode else {
        return Vec::new();
    };
    let text = editor.textarea.lines().join("\n");
    let parsed: Result<Value, _> = serde_json::from_str(&text);
    let value = match parsed {
        Ok(value) => value,
        Err(error) => {
            editor.error = Some(format!("invalid JSON: {error}"));
            return Vec::new();
        }
    };

    let editor = match std::mem::replace(&mut app.mode, Mode::Idle) {
        Mode::Editing(editor) => editor,
        _ => unreachable!(),
    };
    match editor.context {
        EditorContext::InjectResult { mut prompt } => {
            if let Some(reply) = prompt.reply.take() {
                let _ = reply.send(UiDecision::Skip {
                    result: value.clone(),
                });
            }
            match &prompt.kind {
                GateKind::BeforeCall => {
                    app.ws.step_skipped(&prompt.path, value);
                    app.status = format!("skipped {} with an injected result", prompt.path);
                }
                GateKind::OnError { .. } => {
                    // The engine treats this as a replacement: its
                    // step_finished(is_error=false) marks the row ✓.
                    app.ws.run_log_info(&format!(
                        "↻ {} error replaced with injected result",
                        prompt.path
                    ));
                    app.status =
                        format!("replaced {}'s error with an injected result", prompt.path);
                }
            }
            app.mode = app.resume_mode();
            Vec::new()
        }
        EditorContext::RunInput { gated } => {
            if let Some(doc) = &app.ws.doc {
                if let Err(problems) = graph_core::pipeline::doc::validate_input(doc, &value) {
                    let mut editor = editor;
                    editor.error = Some(problems.join("; "));
                    app.mode = Mode::Editing(editor);
                    return Vec::new();
                }
            }
            launch_run(app, gated, value)
        }
        EditorContext::ConfirmQuit => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbench::plan_ws::StepStatus;
    use crossterm::event::KeyEvent;
    use graph_core::pipeline::doc::PlanDoc;
    use serde_json::json;

    fn key(code: KeyCode) -> Msg {
        Msg::Term(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(c: char) -> Msg {
        Msg::Term(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )))
    }

    fn doc(yaml: &str) -> PlanDoc {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn two_step_doc() -> PlanDoc {
        doc(r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E1
    tool_name: t__issues
    input: { team: "{{E0.values.0.id}}" }
"#)
    }

    fn gate_ask(kind: GateKind, path: &str) -> (Msg, oneshot::Receiver<UiDecision>) {
        let (reply, receiver) = oneshot::channel();
        (
            Msg::GateAsk {
                kind,
                path: path.to_string(),
                tool: "t__search".to_string(),
                input: json!({"query": "x"}),
                call_stack: Vec::new(),
                scope: serde_json::from_value(json!({"input": {"team": "core"}})).unwrap(),
                reply,
            },
            receiver,
        )
    }

    #[test]
    fn chat_enter_sends_a_turn_and_blocks_while_busy() {
        let mut app = App::new(None);
        app.chat.input.insert_str("draft a plan");
        let effects = update(&mut app, key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::RunAgentTurn {
                message: "draft a plan".to_string()
            }]
        );
        assert!(matches!(app.mode, Mode::Chatting));
        assert!(matches!(app.chat.entries[0], ChatEntry::User(_)));

        // A second Enter while chatting does nothing.
        app.chat.input.insert_str("again");
        assert!(update(&mut app, key(KeyCode::Enter)).is_empty());

        // Turn completion returns to idle.
        update(&mut app, Msg::TurnFinished(Ok(())));
        assert!(matches!(app.mode, Mode::Idle));
    }

    #[test]
    fn draft_replacement_resets_steps_and_marks_dirty() {
        let mut app = App::new(None);
        let effects = update(
            &mut app,
            Msg::DraftReplaced {
                doc: Box::new(two_step_doc()),
                dirty: true,
            },
        );
        assert_eq!(effects, vec![Effect::Validate]);
        assert!(app.dirty);
        assert_eq!(app.ws.steps.len(), 2);
        assert!(matches!(app.ws.steps[0].status, StepStatus::Pending));
    }

    #[test]
    fn loading_a_plan_from_chat_is_not_dirty() {
        let mut app = App::new(None);
        update(
            &mut app,
            Msg::DraftReplaced {
                doc: Box::new(two_step_doc()),
                dirty: false,
            },
        );
        assert!(!app.dirty, "a freshly loaded plan has no unsaved changes");
        assert_eq!(app.ws.steps.len(), 2);
        assert!(app.status.contains("loaded plan 'demo'"), "{}", app.status);
    }

    #[test]
    fn run_key_starts_a_run_and_step_events_advance_statuses() {
        let mut app = App::new(Some(two_step_doc()));
        app.focus = Focus::Workspace;
        let effects = update(&mut app, key(KeyCode::Char('r')));
        assert_eq!(
            effects,
            vec![Effect::StartRun {
                gated: false,
                input: json!({}),
            }]
        );
        assert!(matches!(app.mode, Mode::Running { gated: false }));
        assert_eq!(app.ws.tab, WsTab::Run);

        update(
            &mut app,
            Msg::StepStarted {
                path: "E0".into(),
                tool: "t__search".into(),
                input: json!({"query": "x"}),
                top_level: true,
            },
        );
        assert!(matches!(app.ws.steps[0].status, StepStatus::Running));
        assert!(app.in_flight.as_ref().is_some_and(|f| f.path == "E0"));

        update(
            &mut app,
            Msg::StepFinished {
                path: "E0".into(),
                result: json!({"values": []}),
                is_error: false,
                top_level: true,
            },
        );
        assert!(matches!(app.ws.steps[0].status, StepStatus::Ok));
        assert_eq!(app.ws.steps[0].result, Some(json!({"values": []})));
        assert!(app.in_flight.is_none());

        update(
            &mut app,
            Msg::RunFinished {
                headline: "done".into(),
                is_error: false,
                results: Map::new(),
            },
        );
        assert!(matches!(app.mode, Mode::Idle));
    }

    #[test]
    fn required_inputs_open_the_editor_before_running() {
        let mut app = App::new(Some(doc(r#"
identifier: needs_input
name: Needs input
description: has a required input
input_schema:
  type: object
  required: [team]
  properties:
    team: { type: string }
steps:
  - id: E0
    tool_name: t__search
    input: { query: "{{input.team}}" }
"#)));
        app.focus = Focus::Workspace;
        let effects = update(&mut app, key(KeyCode::Char('r')));
        assert!(effects.is_empty());
        match &app.mode {
            Mode::Editing(editor) => {
                assert!(matches!(
                    editor.context,
                    EditorContext::RunInput { gated: false }
                ))
            }
            _ => panic!("expected the run-input editor"),
        }
    }

    #[test]
    fn breakpoint_toggle_emits_sync_and_ignores_other_tabs() {
        let mut app = App::new(Some(two_step_doc()));
        app.focus = Focus::Workspace;
        app.ws.selected = 1;
        let effects = update(&mut app, key(KeyCode::Char('b')));
        assert_eq!(
            effects,
            vec![Effect::SyncDebug {
                breakpoints: ["E1".to_string()].into()
            }]
        );
        assert!(app.breakpoints.contains("E1"));

        // Toggling off.
        let effects = update(&mut app, key(KeyCode::Char('b')));
        assert_eq!(
            effects,
            vec![Effect::SyncDebug {
                breakpoints: HashSet::new()
            }]
        );

        // No-op on the context tab.
        app.ws.tab = WsTab::Context;
        assert!(update(&mut app, key(KeyCode::Char('b'))).is_empty());
    }

    #[test]
    fn gate_ask_is_non_modal_and_navigation_keeps_the_prompt() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        app.ws.tab = WsTab::Run;
        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E1");
        update(&mut app, msg);
        assert!(matches!(app.mode, Mode::Paused(_)));
        assert_eq!(app.ws.tab, WsTab::Plan, "pause switches to the plan tab");
        assert_eq!(app.ws.selected, 1, "paused step auto-selected");

        // Navigation while paused: selection moves, tabs switch, the
        // prompt (and its reply) stay intact.
        update(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.ws.selected, 0);
        update(&mut app, key(KeyCode::Char('2')));
        assert_eq!(app.ws.tab, WsTab::Context);
        assert!(matches!(app.mode, Mode::Paused(_)));
        assert!(receiver.try_recv().is_err(), "no decision sent");

        // n steps.
        update(&mut app, key(KeyCode::Char('n')));
        match receiver.try_recv().unwrap() {
            UiDecision::Proceed { continue_mode } => assert!(!continue_mode),
            other => panic!("expected Proceed, got {other:?}"),
        }
        assert!(matches!(app.mode, Mode::Running { gated: true }));
    }

    #[test]
    fn continue_key_requests_continue_mode() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E0");
        update(&mut app, msg);
        update(&mut app, key(KeyCode::Char('c')));
        match receiver.try_recv().unwrap() {
            UiDecision::Proceed { continue_mode } => assert!(continue_mode),
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn paused_blocks_run_keys() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E0");
        update(&mut app, msg);
        for blocked in [key(KeyCode::Char('r')), key(KeyCode::Char('g'))] {
            assert!(update(&mut app, blocked).is_empty());
            assert!(
                matches!(app.mode, Mode::Paused(_)),
                "blocked keys must not leave the pause"
            );
        }
        assert!(!app.should_quit);
        assert!(receiver.try_recv().is_err(), "reply undelivered");
    }

    #[test]
    fn exit_controls_abort_the_paused_run() {
        for exit_key in [key(KeyCode::Char('q')), ctrl('c')] {
            let mut app = App::new(Some(two_step_doc()));
            app.mode = Mode::Running { gated: true };
            let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E0");
            update(&mut app, msg);
            update(&mut app, exit_key);
            assert!(matches!(receiver.try_recv().unwrap(), UiDecision::Abort));
            assert!(!app.should_quit, "abort the run, not the app");
            assert!(matches!(app.mode, Mode::Running { gated: true }));
        }
    }

    #[test]
    fn help_works_while_paused_and_any_key_closes_it() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E0");
        update(&mut app, msg);

        update(&mut app, key(KeyCode::Char('?')));
        assert!(app.show_help, "help opens over the pause");
        assert!(matches!(app.mode, Mode::Paused(_)), "the pause is intact");

        // Any key closes help without acting — even a decision key.
        update(&mut app, key(KeyCode::Char('n')));
        assert!(!app.show_help);
        assert!(matches!(app.mode, Mode::Paused(_)));
        assert!(receiver.try_recv().is_err(), "no decision was sent");

        // Now the decision key acts normally.
        update(&mut app, key(KeyCode::Char('n')));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            UiDecision::Proceed { .. }
        ));
    }

    #[test]
    fn page_keys_scroll_the_focused_pane() {
        let mut app = App::new(Some(two_step_doc()));
        // Chat focus: PgUp scrolls the conversation.
        update(&mut app, key(KeyCode::PageUp));
        assert_eq!(app.chat.scroll.get(), 5);
        update(&mut app, key(KeyCode::PageDown));
        assert_eq!(app.chat.scroll.get(), 0);

        // Workspace focus, plan tab: PgDn scrolls the detail pane.
        app.focus = Focus::Workspace;
        update(&mut app, key(KeyCode::PageDown));
        assert_eq!(app.ws.detail_scroll.get(), 5);

        // Run tab: the same keys scroll the transcript (from the bottom).
        app.ws.tab = WsTab::Run;
        update(&mut app, key(KeyCode::PageUp));
        assert_eq!(app.ws.run_scroll.get(), 5);
        update(&mut app, key(KeyCode::PageDown));
        assert_eq!(app.ws.run_scroll.get(), 0);
    }

    #[test]
    fn on_error_inject_round_trips_the_editor() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (msg, mut receiver) = gate_ask(
            GateKind::OnError {
                error: json!({"error": "boom"}),
            },
            "E0",
        );
        update(&mut app, msg);
        assert!(app.status.contains("failed"), "{}", app.status);

        // 's' opens the inject editor; Esc restores the pause with kind intact.
        update(&mut app, key(KeyCode::Char('s')));
        assert!(matches!(app.mode, Mode::Editing(_)));
        update(&mut app, key(KeyCode::Esc));
        let Mode::Paused(prompt) = &app.mode else {
            panic!("expected the pause restored");
        };
        assert!(matches!(prompt.kind, GateKind::OnError { .. }));

        // Again, and submit a replacement.
        update(&mut app, key(KeyCode::Char('s')));
        if let Mode::Editing(editor) = &mut app.mode {
            editor.textarea = TextArea::from(vec![r#"{"patched": true}"#.to_string()]);
        }
        update(&mut app, ctrl('s'));
        match receiver.try_recv().unwrap() {
            UiDecision::Skip { result } => assert_eq!(result, json!({"patched": true})),
            other => panic!("expected Skip, got {other:?}"),
        }
        // The row is NOT marked skipped — the engine's step_finished with
        // the replacement will mark it Ok.
        assert!(!matches!(app.ws.steps[0].status, StepStatus::Skipped));

        update(
            &mut app,
            Msg::StepFinished {
                path: "E0".into(),
                result: json!({"patched": true}),
                is_error: false,
                top_level: true,
            },
        );
        assert!(matches!(app.ws.steps[0].status, StepStatus::Ok));
    }

    #[test]
    fn before_call_skip_marks_the_row_skipped() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E0");
        update(&mut app, msg);
        update(&mut app, key(KeyCode::Char('s')));
        if let Mode::Editing(editor) = &mut app.mode {
            editor.textarea = TextArea::from(vec![r#"{"values": []}"#.to_string()]);
        }
        update(&mut app, ctrl('s'));
        match receiver.try_recv().unwrap() {
            UiDecision::Skip { result } => assert_eq!(result, json!({"values": []})),
            other => panic!("expected Skip, got {other:?}"),
        }
        assert!(matches!(app.ws.steps[0].status, StepStatus::Skipped));
    }

    #[test]
    fn agent_driven_run_pauses_and_resumes_within_the_turn() {
        let mut app = App::new(Some(two_step_doc()));
        app.chat.input.insert_str("run it gated please");
        update(&mut app, key(KeyCode::Enter));
        assert!(app.turn_in_flight);

        update(
            &mut app,
            Msg::RunStarted {
                gated: true,
                breakpoints: Some(vec!["E1".to_string()]),
            },
        );
        assert!(matches!(app.mode, Mode::Chatting));
        assert!(
            app.breakpoints.contains("E1"),
            "agent breakpoints displayed"
        );
        assert_eq!(app.ws.tab, WsTab::Run);

        let (msg, mut receiver) = gate_ask(GateKind::BeforeCall, "E1");
        update(&mut app, msg);
        assert!(matches!(app.mode, Mode::Paused(_)));
        update(&mut app, key(KeyCode::Char('n')));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            UiDecision::Proceed { .. }
        ));
        assert!(matches!(app.mode, Mode::Chatting), "resumes to the turn");

        update(
            &mut app,
            Msg::RunFinished {
                headline: "done".into(),
                is_error: false,
                results: Map::new(),
            },
        );
        assert!(matches!(app.mode, Mode::Chatting));
        update(&mut app, Msg::TurnFinished(Ok(())));
        assert!(matches!(app.mode, Mode::Idle));
    }

    #[test]
    fn tick_and_in_flight_lifecycle() {
        let mut app = App::new(Some(two_step_doc()));
        assert!(!app.wants_tick(), "idle does not tick");
        app.mode = Mode::Running { gated: false };
        assert!(app.wants_tick());

        update(
            &mut app,
            Msg::StepStarted {
                path: "E0".into(),
                tool: "t__search".into(),
                input: json!({}),
                top_level: true,
            },
        );
        // A finish for a different path must not clear the indicator.
        update(
            &mut app,
            Msg::StepFinished {
                path: "E9".into(),
                result: json!(null),
                is_error: false,
                top_level: false,
            },
        );
        assert!(app.in_flight.is_some());

        let before = app.tick;
        assert!(update(&mut app, Msg::Tick).is_empty());
        assert_eq!(app.tick, before.wrapping_add(1));

        // A pause is static: no tick, no in-flight.
        let (msg, _receiver) = gate_ask(GateKind::BeforeCall, "E1");
        update(&mut app, msg);
        assert!(app.in_flight.is_none());
        assert!(!app.wants_tick());
    }

    #[test]
    fn quit_confirms_when_dirty_and_quits_clean_otherwise() {
        let mut app = App::new(None);
        update(&mut app, ctrl('c'));
        assert!(app.should_quit);

        let mut app = App::new(Some(two_step_doc()));
        app.dirty = true;
        update(&mut app, ctrl('c'));
        assert!(!app.should_quit);
        match &app.mode {
            Mode::Editing(editor) => {
                assert!(matches!(editor.context, EditorContext::ConfirmQuit))
            }
            _ => panic!("expected the quit confirm"),
        }
        update(&mut app, key(KeyCode::Char('y')));
        assert!(app.should_quit);
    }

    #[test]
    fn log_lines_summarize_messages_at_the_right_level() {
        assert!(
            Msg::Tick.log_line().is_none(),
            "the heartbeat is not logged"
        );

        let (level, line) = key(KeyCode::Char('n')).log_line().unwrap();
        assert_eq!(level, tracing::Level::TRACE, "keys are trace-level noise");
        assert!(line.contains("Char('n')"), "{line}");

        let (level, line) = Msg::StepStarted {
            path: "E3/do.2/E10".into(),
            tool: "user__echo".into(),
            input: json!({}),
            top_level: false,
        }
        .log_line()
        .unwrap();
        assert_eq!(level, tracing::Level::DEBUG);
        assert!(
            line.contains("E3/do.2/E10") && line.contains("user__echo"),
            "{line}"
        );

        let (msg, _receiver) = gate_ask(
            GateKind::OnError {
                error: json!({"error": "boom"}),
            },
            "E2",
        );
        let (level, line) = msg.log_line().unwrap();
        assert_eq!(level, tracing::Level::DEBUG);
        assert!(line.contains("on-error") && line.contains("E2"), "{line}");
    }

    #[test]
    fn labels_name_modes_and_effects() {
        assert_eq!(Mode::Idle.label(), "idle");
        assert_eq!(Mode::Running { gated: true }.label(), "debug-running");
        assert_eq!(
            Effect::StartRun {
                gated: true,
                input: json!({})
            }
            .label(),
            "start-debug-run"
        );
        assert_eq!(
            Effect::SyncDebug {
                breakpoints: HashSet::new()
            }
            .label(),
            "sync-debug"
        );
    }

    #[test]
    fn focus_and_tab_switching() {
        let mut app = App::new(None);
        assert_eq!(app.focus, Focus::Chat);
        update(&mut app, key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Workspace);
        update(&mut app, key(KeyCode::Char('2')));
        assert_eq!(app.ws.tab, WsTab::Context);
        // Alt+3 works even with chat focus.
        update(&mut app, key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Chat);
        update(
            &mut app,
            Msg::Term(Event::Key(KeyEvent::new(
                KeyCode::Char('3'),
                KeyModifiers::ALT,
            ))),
        );
        assert_eq!(app.ws.tab, WsTab::Run);
    }
}
