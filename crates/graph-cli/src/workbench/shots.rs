//! Scenario-driven docs screenshots: an integration harness, not a diorama.
//!
//! Each `docs/shots/*.yaml` spec describes a session — the plan, what the
//! user types, scripted LLM responses, scripted tool outcomes — and the
//! harness *runs* it through the production code path: the real agent
//! loop, the real pipeline (template rendering, condition evaluation,
//! catalog resolution), the real `UiGate` debugger, the real `ChannelSink`
//! → `update()` reducer. Only the two outermost seams are scripted: the
//! LLM provider and tool I/O. Step statuses, rendered inputs, chat
//! activity lines, and pause states are all *produced*, never stated —
//! so a behavioral change either shows up as an SVG diff or fails the
//! run loudly.
//!
//! Determinism: the "terminal" is a `TestBackend` at the spec's grid size
//! (the viewport is an input, not an environment property), the pipeline
//! date is fixed, and the only wall clock the UI shows (the turn timer)
//! is pinned to the spec's `turn_seconds` before rendering.
//!
//! Guard rails against stale fixtures: every scripted tool must exist in
//! the real catalog (packs + `.graph/tools`), and every scripted outcome
//! is validated against the tool's declared `output_schema`. Renames and
//! shape changes fail generation instead of rendering a plausible lie.
//!
//! Regenerate with `cargo test -p graph-cli shots -- --nocapture`
//! (or `mise run shots`); output lands in `docs/images/workbench/`.

use super::app::{self, App, Mode, Msg};
use super::chat::ChannelSink;
use super::effects::{run_effect, WorkbenchContext};
use super::runner::DebugControls;
use super::screenshot::buffer_to_svg;
use super::tools::{progress_tools, DraftState, WorkbenchTools};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use graph_config::{ModelChoice, ModelRoles};
use graph_core::pipeline::doc::load_plan_doc;
use graph_core::pipeline::{Pipeline, ToolCatalog};
use graph_core::user_tools::{available_packs, load_pack_tools, load_user_tools, UserToolRegistry};
use graph_core::{Agent, CompositeRegistry, Store, ToolDef, ToolError, ToolOutcome, ToolRegistry};
use graph_llm::types::{
    ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use graph_store::MemoryStore;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

// ── Scenario spec ────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShotSpec {
    /// Output name: `docs/images/workbench/<name>.svg`.
    name: String,
    /// Window-chrome title on the rendered SVG. Pane crops render without
    /// chrome and ignore it.
    title: String,
    /// Repo-relative path of the plan to load into the workbench.
    /// Omitted: the workbench starts with an empty draft (drafting shots).
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    grid: Grid,
    /// How `workbench__draft_plan` drafts (`oneshot` default,
    /// `incremental` for the drafting-overlay shots).
    #[serde(default)]
    draft_strategy: Option<graph_config::DraftStrategy>,
    /// The user's chat message, typed through the real reducer.
    /// Omitted: no agent turn runs (pure workspace shots).
    #[serde(default)]
    chat: Option<String>,
    /// Scripted LLM responses, consumed in order across all roles —
    /// interleave agent, solver, and planner responses in call order.
    #[serde(default)]
    llm: Vec<LlmScript>,
    /// Scripted tool outcomes per namespaced tool name, consumed in call
    /// order. Every name must resolve in the real catalog; outcomes are
    /// validated against the tool's declared output schema.
    #[serde(default)]
    tools: BTreeMap<String, VecDeque<Value>>,
    capture: Capture,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grid {
    cols: u16,
    rows: u16,
}

impl Default for Grid {
    fn default() -> Self {
        // Sized for a docs content column, not a maximized desktop.
        Self {
            cols: 110,
            rows: 32,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, untagged)]
enum LlmScript {
    /// An assistant tool call, optionally with text before it.
    Call {
        call: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        text: Option<String>,
    },
    /// A structured-output response (planner outline / step drafts).
    Structured { structured: Value },
    /// A plain assistant text response (chat replies, solver output).
    Text { text: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    /// Capture at the debugger pause on this step path (e.g. "E5", or
    /// "E1/do.1" for a map body call). Pauses before the target are
    /// answered with a real `c` keypress, so loop-line breakpoints step
    /// iteration by iteration to the one you want.
    #[serde(default)]
    pause_at: Option<String>,
    /// Named capture point: `loaded` (booted, validated, and any chat
    /// turn finished — no run) or `finished` (a run completed and the
    /// turn ended).
    #[serde(default)]
    at: Option<NamedCapture>,
    /// Capture when the incremental drafting overlay starts this
    /// (0-based) step.
    #[serde(default)]
    draft_step: Option<usize>,
    /// Keys fed through the reducer after the capture state is reached,
    /// before rendering — switch tabs ("tab", "2"), move selection ("j").
    #[serde(default)]
    keys: Vec<String>,
    /// What the status bar's turn timer shows, in seconds.
    #[serde(default)]
    turn_seconds: Option<f32>,
    /// Render only one pane instead of the full frame.
    #[serde(default)]
    crop: Option<CropPane>,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum NamedCapture {
    Loaded,
    Finished,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum CropPane {
    Chat,
    Workspace,
    /// The steps/tool list (borders included).
    Steps,
    /// The active tab's scrollable body (detail/debug/run).
    Body,
    /// The one-line status bar.
    Status,
}

// ── Scripted seams: the LLM provider and tool I/O ────────────────────────

struct ScriptedChat(Mutex<VecDeque<ChatResponse>>);

impl ScriptedChat {
    fn new(scripts: Vec<LlmScript>) -> Self {
        let responses = scripts
            .into_iter()
            .enumerate()
            .map(|(i, script)| match script {
                LlmScript::Text { text } => ChatResponse {
                    content: Some(text),
                    tool_calls: Vec::new(),
                    structured: None,
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                },
                LlmScript::Call { call, input, text } => ChatResponse {
                    content: text,
                    tool_calls: vec![ToolCall {
                        id: format!("shot-{i}"),
                        name: call,
                        arguments: input,
                    }],
                    structured: None,
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
                LlmScript::Structured { structured } => ChatResponse {
                    content: None,
                    tool_calls: Vec::new(),
                    structured: Some(structured),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                },
            })
            .collect();
        Self(Mutex::new(responses))
    }

    fn pop(&self) -> Result<ChatResponse, LlmError> {
        self.0.lock().unwrap().pop_front().ok_or_else(|| {
            LlmError::Unsupported(
                "scripted LLM responses exhausted — add more to the shot's `llm` list".to_string(),
            )
        })
    }
}

#[async_trait]
impl ChatProvider for ScriptedChat {
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.pop()
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
        use futures::StreamExt;
        let response = self.pop()?;
        let mut events: Vec<Result<StreamEvent, LlmError>> = Vec::new();
        if let Some(text) = &response.content {
            events.push(Ok(StreamEvent::TextDelta(text.clone())));
        }
        for call in &response.tool_calls {
            events.push(Ok(StreamEvent::ToolCallStarted {
                name: call.name.clone(),
            }));
        }
        events.push(Ok(StreamEvent::Completed(response)));
        Ok(futures::stream::iter(events).boxed())
    }
}

/// Real definitions, scripted invocation: `tools()` serves the catalog
/// built from the actual packs and user tools, `invoke` pops the shot's
/// canned outcomes. An unscripted or exhausted tool fails the run.
struct ScriptedTools {
    defs: Vec<ToolDef>,
    outcomes: Mutex<BTreeMap<String, VecDeque<Value>>>,
}

#[async_trait]
impl ToolRegistry for ScriptedTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(self.defs.clone())
    }

    async fn invoke(&self, name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
        let mut outcomes = self.outcomes.lock().unwrap();
        // Unknown (not Transport) for unscripted names: the composite
        // registry falls through to the workbench tools only on Unknown.
        // A plan step calling an unscripted tool still fails loudly —
        // the pipeline's registry is this one alone.
        let queue = outcomes
            .get_mut(name)
            .ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        let result = queue.pop_front().ok_or_else(|| {
            ToolError::Transport(format!("scripted outcomes for {name} exhausted"))
        })?;
        Ok(ToolOutcome {
            result,
            is_error: false,
        })
    }
}

/// The stale-fixture gate: every scripted tool must exist in the real
/// catalog, and outcomes must match the tool's declared output schema.
fn validate_outcomes(defs: &[ToolDef], tools: &BTreeMap<String, VecDeque<Value>>) -> Vec<String> {
    let by_name: HashMap<&str, &ToolDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
    let mut problems = Vec::new();
    for (name, outcomes) in tools {
        let Some(def) = by_name.get(name.as_str()) else {
            problems.push(format!(
                "{name}: not in the tool catalog — renamed or removed?"
            ));
            continue;
        };
        let Some(schema) = &def.output_schema else {
            continue;
        };
        let Ok(validator) = jsonschema::validator_for(schema) else {
            problems.push(format!("{name}: declared output_schema does not compile"));
            continue;
        };
        for (index, outcome) in outcomes.iter().enumerate() {
            if let Err(error) = validator.validate(outcome) {
                problems.push(format!("{name} outcome [{index}]: {error}"));
            }
        }
    }
    problems
}

// ── The harness ──────────────────────────────────────────────────────────

async fn run_shot(root: &Path, spec: ShotSpec) -> Result<PathBuf> {
    let doc = match &spec.plan {
        Some(plan) => Some(load_plan_doc(&root.join(plan)).context("loading the shot's plan")?),
        None => None,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Every model role resolves to the scripted provider.
    let chat_provider = Arc::new(ScriptedChat::new(spec.llm));
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("scripted".to_string(), chat_provider.clone());
    let roles = ModelRoles {
        default: Some(ModelChoice {
            provider: "scripted".to_string(),
            model: "scripted".to_string(),
            temperature: None,
            dimensions: None,
            description: None,
        }),
        ..Default::default()
    };
    let router = Arc::new(ModelRouter::with_providers(providers, roles));

    // The real catalog: every pack plus the repo's user tools.
    let packs: Vec<String> = available_packs().iter().map(|s| s.to_string()).collect();
    let pack_docs = load_pack_tools(&packs).map_err(anyhow::Error::msg)?;
    let user_docs = load_user_tools(&[root.join(".graph/tools")]).map_err(anyhow::Error::msg)?;
    let builtin_defs = UserToolRegistry::builtins(pack_docs, router.clone())
        .tools()
        .await?;
    let user_defs = UserToolRegistry::new(user_docs, router.clone())
        .tools()
        .await?;
    let mut defs = builtin_defs;
    defs.extend(user_defs);

    let problems = validate_outcomes(&defs, &spec.tools);
    if !problems.is_empty() {
        bail!(
            "stale fixture '{}':\n  {}",
            spec.name,
            problems.join("\n  ")
        );
    }
    let catalog = ToolCatalog {
        builtin_tools: defs
            .iter()
            .filter(|d| d.name.starts_with("builtin__"))
            .map(|d| d.name.clone())
            .collect(),
        user_tools: defs
            .iter()
            .filter(|d| d.name.starts_with("user__"))
            .map(|d| d.name.clone())
            .collect(),
        ..Default::default()
    };
    let scripted: Arc<ScriptedTools> = Arc::new(ScriptedTools {
        defs,
        outcomes: Mutex::new(spec.tools),
    });

    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let pipeline = Arc::new(Pipeline {
        router,
        registry: scripted.clone(),
        events: Arc::new(ChannelSink::plan_run(tx.clone())),
        plans: Arc::new(Vec::new()),
        call_stack: Vec::new(),
        store: Some(store.clone()),
        gate: None,
        catalog: Some(Arc::new(catalog)),
        user_context: String::new(),
        // Fixed: the pipeline date must not vary between regenerations.
        current_date: "2026-07-19".to_string(),
        max_attempts: 2,
        draft_strategy: spec.draft_strategy.unwrap_or_default(),
    });

    let debug = Arc::new(DebugControls::default());
    let draft = Arc::new(Mutex::new(DraftState::new(doc.clone())));
    let workbench_tools = Arc::new(WorkbenchTools::new(
        draft.clone(),
        pipeline.clone(),
        None,
        debug.clone(),
        tx.clone(),
    ));
    let registry: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
        scripted.clone(),
        workbench_tools,
    ]));

    // The real workbench system prompt, minus the config-derived base.
    let mut system_prompt = super::WORKBENCH_SYSTEM_PROMPT.to_string();
    system_prompt.push_str(super::CONTROL_STEP_NAMING);
    system_prompt.push_str(graph_core::pipeline::CONTROL_STEP_RULES);
    let agent = Agent {
        provider: chat_provider,
        registry: registry.clone(),
        events: Arc::new(ChannelSink::agent(tx.clone())),
        model: "scripted".to_string(),
        temperature: None,
        system_prompt,
        max_iterations: 8,
        progress_tools: progress_tools(),
    };

    let context = Arc::new(WorkbenchContext {
        agent,
        pipeline,
        history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        draft,
        catalog: scripted,
        store,
        plans_dir: None,
        debug,
        tx: tx.clone(),
    });

    // Boot exactly like `run_plan_workbench`, then type the chat message
    // through the reducer and submit it.
    let mut app = App::new(doc);
    run_effect(app::Effect::LoadContext, &context);
    if app.ws.doc.is_some() {
        run_effect(app::Effect::Validate, &context);
    }
    if let Some(chat) = &spec.chat {
        for ch in chat.chars() {
            feed_key(&mut app, KeyCode::Char(ch), &context);
        }
        feed_key(&mut app, KeyCode::Enter, &context);
    }

    // Drain engine messages through the reducer until the capture point.
    // Pauses before a `pause_at` target are answered with a real `c`
    // keypress — the user's continue — so the run steps to the target.
    let mut context_loaded = false;
    let mut validated = spec.plan.is_none();
    let mut turn_done = spec.chat.is_none();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .with_context(|| format!("shot '{}' timed out awaiting its capture point", spec.name))?
            .context("workbench channel closed before the capture point")?;
        if let Some((_, line)) = msg.log_line() {
            eprintln!("[shot {}] {line}", spec.name);
        }
        match &msg {
            Msg::ContextLoaded { .. } => context_loaded = true,
            Msg::Validated(_) => validated = true,
            Msg::TurnFinished(_) => turn_done = true,
            _ => {}
        }
        let draft_step_started = match (&msg, spec.capture.draft_step) {
            (Msg::DraftStepStarted { index, .. }, Some(target)) => *index == target,
            _ => false,
        };
        for effect in app::update(&mut app, msg) {
            run_effect(effect, &context);
        }
        if let Some(target) = &spec.capture.pause_at {
            match &app.mode {
                Mode::Paused(prompt) if prompt.path == *target => break,
                Mode::Paused(_) => feed_key(&mut app, KeyCode::Char('c'), &context),
                _ => {}
            }
        } else if spec.capture.draft_step.is_some() {
            if draft_step_started {
                break;
            }
        } else {
            let done = match spec.capture.at {
                Some(NamedCapture::Loaded) => context_loaded && validated && turn_done,
                Some(NamedCapture::Finished) => app.ws.outcome.is_some() && turn_done,
                None => bail!(
                    "shot '{}': capture needs one of pause_at, draft_step, or at",
                    spec.name
                ),
            };
            if done {
                break;
            }
        }
    }

    // Post-capture keys: tab switches, selection moves — through the
    // real reducer, so the shot shows exactly what a user would see.
    for name in &spec.capture.keys {
        feed_key(&mut app, key_code(name)?, &context);
    }

    // Pin the wall clocks the UI renders (the turn timer and the
    // in-flight call timer).
    if let Some(seconds) = spec.capture.turn_seconds {
        let past = std::time::Instant::now().checked_sub(Duration::from_secs_f32(seconds));
        if let (Some(past), Some(in_flight)) = (past, app.in_flight.as_mut()) {
            in_flight.started = past;
        }
        if app.turn_started.is_some() {
            app.turn_started = past;
        }
    }

    let backend = TestBackend::new(spec.grid.cols, spec.grid.rows);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| super::ui::draw(frame, &app))?;
    // Pane crops read the geometry `draw` just recorded; crops render
    // without window chrome.
    let (title, clip) = match spec.capture.crop {
        None => (Some(spec.title.as_str()), None),
        Some(pane) => {
            let regions = *app.regions.borrow();
            let rect = match pane {
                CropPane::Chat => regions.chat,
                CropPane::Workspace => regions.workspace,
                CropPane::Steps => regions.ws_list,
                CropPane::Body => regions.ws_body,
                CropPane::Status => ratatui::layout::Rect {
                    x: 0,
                    y: spec.grid.rows - 1,
                    width: spec.grid.cols,
                    height: 1,
                },
            };
            if rect.width == 0 || rect.height == 0 {
                bail!("shot '{}': crop pane has no rendered area", spec.name);
            }
            (None, Some(rect))
        }
    };
    let svg = buffer_to_svg(terminal.backend().buffer(), title, clip);
    let out_dir = root.join("docs/images/workbench");
    std::fs::create_dir_all(&out_dir)?;
    let out = out_dir.join(format!("{}.svg", spec.name));
    std::fs::write(&out, svg)?;
    Ok(out)
}

/// Feed one key through the reducer and run any effects it returns.
fn feed_key(app: &mut App, code: KeyCode, context: &Arc<WorkbenchContext>) {
    let msg = Msg::Term(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    for effect in app::update(app, msg) {
        run_effect(effect, context);
    }
}

/// A spec key name: a single character, or a named special key.
fn key_code(name: &str) -> Result<KeyCode> {
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(ch));
    }
    Ok(match name {
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "pgup" => KeyCode::PageUp,
        "pgdn" => KeyCode::PageDown,
        other => bail!("unknown key name '{other}'"),
    })
}

#[tokio::test]
async fn generate_docs_screenshots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root");
    let shots_dir = root.join("docs/shots");
    let mut specs: Vec<PathBuf> = std::fs::read_dir(&shots_dir)
        .expect("docs/shots exists")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    specs.sort();
    assert!(
        !specs.is_empty(),
        "no shot specs in {}",
        shots_dir.display()
    );
    for path in specs {
        let raw = std::fs::read_to_string(&path).expect("readable spec");
        let spec: ShotSpec = serde_yaml::from_str(&raw)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let name = spec.name.clone();
        let out = run_shot(&root, spec)
            .await
            .unwrap_or_else(|error| panic!("shot '{name}': {error:#}"));
        eprintln!("wrote {}", out.display());
    }
}
