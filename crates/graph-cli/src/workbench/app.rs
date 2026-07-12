//! Workbench application state and the pure reducer: `update` consumes one
//! [`Msg`] and returns [`Effect`]s for the executor to spawn — no I/O here,
//! so the whole interaction model is testable headless.

use super::editor::{EditorContext, EditorState};
use super::plan_ws::{PlanWorkspace, WsTab};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::GateDecision;
use graph_core::{ToolDef, ToolShape};
use serde_json::{Map, Value};
use tokio::sync::oneshot;
use tui_textarea::TextArea;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chat,
    Workspace,
}

/// A pending gate decision: the run is parked on `reply`.
pub struct GatePrompt {
    pub path: String,
    pub tool: String,
    pub input: Value,
    pub reply: Option<oneshot::Sender<GateDecision>>,
}

pub enum Mode {
    Idle,
    /// An agent turn is in flight.
    Chatting,
    /// A plan run is in flight.
    Running {
        gated: bool,
    },
    /// A gated run is paused on a tool call, awaiting y / s / a.
    Paused(GatePrompt),
    /// A modal editor (inject result, run input) or confirm dialog is open.
    Editing(Box<EditorState>),
    /// The help overlay is open.
    Help,
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
    /// Scrollback offset in lines from the bottom; 0 follows.
    pub scroll: u16,
}

impl Default for ChatState {
    fn default() -> Self {
        let mut input = TextArea::default();
        input.set_placeholder_text("describe the plan you want, or ask about tools…");
        Self {
            entries: Vec::new(),
            input,
            scroll: 0,
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
            should_quit: false,
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.mode, Mode::Idle | Mode::Help)
    }

    /// Where a paused gated run returns to: the agent's turn when it
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
    /// keyboard path resets the pane directly).
    RunStarted {
        gated: bool,
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
        path: String,
        tool: String,
        input: Value,
        reply: oneshot::Sender<GateDecision>,
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

/// Work the reducer wants done; the executor spawns it and reports back
/// with a `Msg`.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    RunAgentTurn { message: String },
    StartRun { gated: bool, input: Value },
    Validate,
    LoadContext,
    SavePlan,
}

pub fn update(app: &mut App, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Term(event) => on_terminal_event(app, event),

        Msg::AgentDelta(text) => {
            if let Some(ChatEntry::Assistant(buffer)) = app.chat.entries.last_mut() {
                buffer.push_str(&text);
            } else {
                app.chat.entries.push(ChatEntry::Assistant(text));
            }
            app.chat.scroll = 0;
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

        Msg::RunStarted { gated } => {
            app.ws.run_starting(gated);
            app.status = if gated {
                "agent started a gated run — each tool call will ask you first".to_string()
            } else {
                "agent is running the plan…".to_string()
            };
            Vec::new()
        }
        Msg::Planning => {
            app.ws.run_log_info("planning…");
            Vec::new()
        }
        Msg::Synthesizing => {
            app.ws.run_log_info("synthesizing…");
            Vec::new()
        }
        Msg::StepStarted {
            path,
            tool,
            input,
            top_level,
        } => {
            app.ws.run_log_info(&format!("▸ {path} {tool}"));
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
            path,
            tool,
            input,
            reply,
        } => {
            app.ws.step_running(&path);
            app.status = format!("paused at {path} — y proceed · s skip · a abort");
            app.mode = Mode::Paused(GatePrompt {
                path,
                tool,
                input,
                reply: Some(reply),
            });
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

    // Modal states capture the keyboard entirely.
    match &mut app.mode {
        Mode::Paused(_) => return on_paused_key(app, key),
        Mode::Editing(_) => return on_editor_key(app, key),
        Mode::Help => {
            app.mode = Mode::Idle;
            return Vec::new();
        }
        _ => {}
    }

    // Global bindings.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return request_quit(app);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        return save(app);
    }
    if key.code == KeyCode::Tab {
        app.focus = match app.focus {
            Focus::Chat => Focus::Workspace,
            Focus::Workspace => Focus::Chat,
        };
        return Vec::new();
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c @ '1'..='3') = key.code {
            app.ws.tab = tab_for(c);
            return Vec::new();
        }
    }

    match app.focus {
        Focus::Chat => on_chat_key(app, key),
        Focus::Workspace => on_workspace_key(app, key),
    }
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
            app.chat.scroll = 0;
            app.mode = Mode::Chatting;
            app.turn_in_flight = true;
            vec![Effect::RunAgentTurn { message }]
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.scroll = app.chat.scroll.saturating_add(3);
            Vec::new()
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.scroll = app.chat.scroll.saturating_sub(3);
            Vec::new()
        }
        _ => {
            app.chat.input.input(key);
            Vec::new()
        }
    }
}

fn on_workspace_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Char(c @ '1'..='3') => {
            app.ws.tab = tab_for(c);
            Vec::new()
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.ws.select_next();
            Vec::new()
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.ws.select_previous();
            Vec::new()
        }
        KeyCode::PageDown => {
            app.ws.detail_scroll = app.ws.detail_scroll.saturating_add(5);
            Vec::new()
        }
        KeyCode::PageUp => {
            app.ws.detail_scroll = app.ws.detail_scroll.saturating_sub(5);
            Vec::new()
        }
        KeyCode::Char('?') => {
            if matches!(app.mode, Mode::Idle) {
                app.mode = Mode::Help;
            }
            Vec::new()
        }
        KeyCode::Char('v') => {
            if app.ws.doc.is_none() {
                app.status = "no plan to validate".to_string();
                return Vec::new();
            }
            vec![Effect::Validate]
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
        "gated run — pausing before each tool call".to_string()
    } else {
        "running…".to_string()
    };
    vec![Effect::StartRun { gated, input }]
}

fn on_paused_key(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    let Mode::Paused(prompt) = &mut app.mode else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let Some(reply) = prompt.reply.take() {
                let _ = reply.send(GateDecision::Proceed);
            }
            app.status = "proceeding…".to_string();
            app.mode = app.resume_mode();
            Vec::new()
        }
        KeyCode::Char('a') => {
            if let Some(reply) = prompt.reply.take() {
                let _ = reply.send(GateDecision::Abort);
            }
            app.status = "aborting…".to_string();
            app.mode = app.resume_mode();
            Vec::new()
        }
        KeyCode::Char('s') => {
            // Move the pending reply into the inject editor.
            let path = prompt.path.clone();
            let tool = prompt.tool.clone();
            let input = prompt.input.clone();
            let reply = prompt.reply.take();
            let prefill = app.ws.output_example_for(&tool);
            app.mode = Mode::Editing(Box::new(EditorState::inject_result(
                path, tool, input, reply, prefill,
            )));
            Vec::new()
        }
        _ => Vec::new(),
    }
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
            // Cancel: an inject editor returns to the gate prompt; a run
            // input editor returns to idle.
            let editor = match std::mem::replace(&mut app.mode, Mode::Idle) {
                Mode::Editing(editor) => editor,
                _ => unreachable!(),
            };
            if let EditorContext::InjectResult {
                path,
                tool,
                input,
                reply,
            } = editor.context
            {
                app.mode = Mode::Paused(GatePrompt {
                    path,
                    tool,
                    input,
                    reply,
                });
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
        EditorContext::InjectResult { path, reply, .. } => {
            if let Some(reply) = reply {
                let _ = reply.send(GateDecision::Skip {
                    result: value.clone(),
                });
            }
            app.ws.step_skipped(&path, value);
            app.status = format!("skipped {path} with an injected result");
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
    fn gate_flow_pause_skip_preserves_the_reply_through_the_editor() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (reply, mut receiver) = oneshot::channel();
        update(
            &mut app,
            Msg::GateAsk {
                path: "E0".into(),
                tool: "t__search".into(),
                input: json!({"query": "x"}),
                reply,
            },
        );
        assert!(matches!(app.mode, Mode::Paused(_)));

        // 's' opens the inject editor, carrying the reply along.
        update(&mut app, key(KeyCode::Char('s')));
        let Mode::Editing(editor) = &app.mode else {
            panic!("expected inject editor");
        };
        assert!(matches!(editor.context, EditorContext::InjectResult { .. }));
        assert!(receiver.try_recv().is_err(), "no decision sent yet");

        // Type a JSON value and submit with Ctrl+S.
        if let Mode::Editing(editor) = &mut app.mode {
            editor.textarea = TextArea::from(vec![r#"{"values": []}"#.to_string()]);
        }
        update(&mut app, ctrl('s'));
        assert!(matches!(app.mode, Mode::Running { gated: true }));
        match receiver.try_recv().unwrap() {
            GateDecision::Skip { result } => assert_eq!(result, json!({"values": []})),
            _ => panic!("expected Skip"),
        }
        assert!(matches!(app.ws.steps[0].status, StepStatus::Skipped));
    }

    #[test]
    fn gate_abort_sends_the_decision() {
        let mut app = App::new(Some(two_step_doc()));
        app.mode = Mode::Running { gated: true };
        let (reply, mut receiver) = oneshot::channel();
        update(
            &mut app,
            Msg::GateAsk {
                path: "E0".into(),
                tool: "t__search".into(),
                input: json!({}),
                reply,
            },
        );
        update(&mut app, key(KeyCode::Char('a')));
        assert!(matches!(receiver.try_recv().unwrap(), GateDecision::Abort));
    }

    #[test]
    fn agent_driven_run_pauses_and_resumes_within_the_turn() {
        let mut app = App::new(Some(two_step_doc()));
        // The user sends a message; the agent's run_plan tool fires mid-turn.
        app.chat.input.insert_str("run it gated please");
        update(&mut app, key(KeyCode::Enter));
        assert!(app.turn_in_flight);

        update(&mut app, Msg::RunStarted { gated: true });
        assert!(
            matches!(app.mode, Mode::Chatting),
            "mode stays with the turn"
        );
        assert_eq!(app.ws.tab, WsTab::Run);

        // Gate pause → proceed resumes to the turn, not to a keyboard run.
        let (reply, mut receiver) = oneshot::channel();
        update(
            &mut app,
            Msg::GateAsk {
                path: "E0".into(),
                tool: "t__search".into(),
                input: json!({}),
                reply,
            },
        );
        assert!(matches!(app.mode, Mode::Paused(_)));
        update(&mut app, key(KeyCode::Char('y')));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            GateDecision::Proceed
        ));
        assert!(matches!(app.mode, Mode::Chatting));

        // The run finishing mid-turn leaves the turn in charge…
        update(
            &mut app,
            Msg::RunFinished {
                headline: "done".into(),
                is_error: false,
                results: Map::new(),
            },
        );
        assert!(matches!(app.mode, Mode::Chatting));
        // …and the turn finishing returns to idle.
        update(&mut app, Msg::TurnFinished(Ok(())));
        assert!(matches!(app.mode, Mode::Idle));
        assert!(!app.turn_in_flight);
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
