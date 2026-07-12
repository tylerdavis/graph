//! Rendering: the generic dual-pane shell (chat · workspace · status bar ·
//! modals) with the plan workspace as the right-pane body.

use super::app::{App, ChatEntry, Focus, GateKind, GatePrompt, Mode};
use super::editor::EditorContext;
use super::plan_ws::{PlanWorkspace, RunLine, StepStatus, WsTab};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Tabs, Wrap,
};
use ratatui::Frame;

const DIM: Style = Style::new().fg(Color::DarkGray);
const ERROR: Style = Style::new().fg(Color::Red);
const OK: Style = Style::new().fg(Color::Green);
const ACCENT: Style = Style::new().fg(Color::Cyan);

pub fn draw(frame: &mut Frame, app: &App) {
    let [main, status] =
        *Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(frame.area())
    else {
        return;
    };
    let [chat, workspace] =
        *Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(main)
    else {
        return;
    };

    draw_chat(frame, app, chat);
    draw_workspace(frame, app, workspace);
    draw_status_bar(frame, app, status);

    // Paused is non-modal — the plan tab renders the debug panel — so the
    // only modal overlay is the editor, plus help on top of anything.
    if let Mode::Editing(editor) = &app.mode {
        draw_editor(frame, editor);
    }
    if app.show_help {
        draw_help(frame);
    }
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let block = Block::bordered().title(title);
    if focused {
        block.border_style(ACCENT)
    } else {
        block.border_style(DIM)
    }
}

// ── Chat pane ────────────────────────────────────────────────────────────

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let block = pane_block(" chat ", app.focus == Focus::Chat);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [scrollback, input] =
        *Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).split(inner)
    else {
        return;
    };

    let width = scrollback.width.max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.chat.entries {
        match entry {
            ChatEntry::User(text) => {
                for (i, wrapped) in wrap_text(text, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 { "❯ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, ACCENT),
                        Span::styled(wrapped, Style::new().add_modifier(Modifier::BOLD)),
                    ]));
                }
            }
            ChatEntry::Assistant(text) => {
                for wrapped in wrap_text(text, width) {
                    lines.push(Line::from(wrapped));
                }
            }
            ChatEntry::Activity(text) => {
                for wrapped in wrap_text(text, width) {
                    lines.push(Line::styled(wrapped, DIM));
                }
            }
        }
        lines.push(Line::default());
    }
    if matches!(app.mode, Mode::Chatting) {
        lines.push(Line::styled("…", DIM));
    }

    let height = scrollback.height as usize;
    let bottom_offset = lines.len().saturating_sub(height);
    let offset = bottom_offset.saturating_sub(app.chat.scroll as usize);
    let paragraph = Paragraph::new(lines).scroll((offset as u16, 0));
    frame.render_widget(paragraph, scrollback);

    let mut input_widget = app.chat.input.clone();
    input_widget.set_block(
        Block::bordered()
            .border_style(DIM)
            .title(" Enter send · Alt+Enter newline "),
    );
    frame.render_widget(&input_widget, input);
}

// ── Workspace pane ───────────────────────────────────────────────────────

fn draw_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let block = pane_block(" workspace ", app.focus == Focus::Workspace);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [tabs_area, body] =
        *Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner)
    else {
        return;
    };

    let selected = match app.ws.tab {
        WsTab::Plan => 0,
        WsTab::Context => 1,
        WsTab::Run => 2,
    };
    let tabs = Tabs::new(vec!["1 plan", "2 context", "3 run"])
        .select(selected)
        .style(DIM)
        .highlight_style(ACCENT.add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, tabs_area);

    match app.ws.tab {
        WsTab::Plan => draw_plan_tab(frame, app, body),
        WsTab::Context => draw_context_tab(frame, &app.ws, body),
        WsTab::Run => draw_run_tab(frame, &app.ws, body),
    }
}

/// Clamp a scroll cell to the content, render the paragraph, and draw a
/// scrollbar when the content overflows the pane.
fn render_scrolled(
    frame: &mut Frame,
    lines: Vec<Line>,
    block: Block,
    area: Rect,
    scroll: &std::cell::Cell<u16>,
) {
    let viewport = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(viewport) as u16;
    scroll.set(scroll.get().min(max_scroll));
    let offset = scroll.get();
    let widget = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(widget, area);
    if total > viewport {
        let mut state = ScrollbarState::new(max_scroll as usize).position(offset as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn spinner_frame(tick: u8) -> &'static str {
    SPINNER[tick as usize % SPINNER.len()]
}

/// The prompt behind the current pause, if any.
fn paused_prompt(app: &App) -> Option<&GatePrompt> {
    match &app.mode {
        Mode::Paused(prompt) => Some(prompt),
        _ => None,
    }
}

fn draw_plan_tab(frame: &mut Frame, app: &App, area: Rect) {
    let ws = &app.ws;
    let dirty = app.dirty;
    let Some(doc) = &ws.doc else {
        let empty = Paragraph::new(
            "no plan loaded\n\nask the chat agent to draft one:\n  \"draft a plan that …\"",
        )
        .style(DIM)
        .alignment(Alignment::Center);
        frame.render_widget(empty, area);
        return;
    };

    let [header, steps_area, detail] = *Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(40),
        Constraint::Min(3),
    ])
    .split(area) else {
        return;
    };

    // Header: identity, finish mode, validation.
    let finish = if doc.solver.is_some() {
        "solver"
    } else if doc.output.is_some() {
        "output"
    } else {
        "silent"
    };
    let mut identity = vec![
        Span::styled(&doc.identifier, ACCENT.add_modifier(Modifier::BOLD)),
        Span::raw(" — "),
        Span::raw(&doc.name),
    ];
    if dirty {
        identity.push(Span::styled("  [unsaved]", Style::new().fg(Color::Yellow)));
    }
    let mut meta = vec![Span::styled(format!("finish: {finish}"), DIM)];
    if doc.input_schema.is_some() {
        meta.push(Span::styled("  ·  takes input", DIM));
    }
    let validation = if ws.diagnostics.is_empty() {
        Line::styled("✓ valid", OK)
    } else {
        Line::styled(
            format!("✗ {} problem(s) — details below", ws.diagnostics.len()),
            ERROR,
        )
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(identity), Line::from(meta), validation]),
        header,
    );

    // Step list: breakpoint gutter · status glyph · id · tool. The paused
    // step's id renders yellow-bold; running rows animate.
    let paused_step = paused_prompt(app).and_then(|p| p.top_level_step().map(str::to_string));
    let items: Vec<ListItem> = ws
        .steps
        .iter()
        .map(|row| {
            let style = match row.status {
                StepStatus::Pending => DIM,
                StepStatus::Running => Style::new().fg(Color::Yellow),
                StepStatus::Ok => OK,
                StepStatus::Err => ERROR,
                StepStatus::Skipped => Style::new().fg(Color::Magenta),
            };
            let glyph = if matches!(row.status, StepStatus::Running)
                && !matches!(app.mode, Mode::Paused(_))
            {
                spinner_frame(app.tick)
            } else {
                row.status.glyph()
            };
            let gutter = if app.breakpoints.contains(&row.id) {
                Span::styled("●", ERROR)
            } else {
                Span::raw(" ")
            };
            let id_style = if paused_step.as_deref() == Some(row.id.as_str()) {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            ListItem::new(Line::from(vec![
                gutter,
                Span::styled(format!("{glyph} "), style),
                Span::styled(format!("{:4} ", row.id), id_style),
                Span::raw(row.tool.clone()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_style(DIM)
                .title(" steps ─ j/k select · b breakpoint · v validate · r run · g debug "),
        )
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(ws.selected));
    frame.render_stateful_widget(list, steps_area, &mut state);

    // While paused on the selected step (or any nested/body pause), the
    // detail pane becomes the debug panel.
    let show_debug = paused_prompt(app).is_some_and(|p| match p.top_level_step() {
        Some(step) => ws.steps.get(ws.selected).is_some_and(|row| row.id == step),
        None => true,
    });
    if show_debug {
        let prompt = paused_prompt(app).expect("checked above");
        draw_debug_panel(frame, prompt, detail, &ws.detail_scroll);
        return;
    }

    // Detail: diagnostics, then the selected step's I/O.
    let mut lines: Vec<Line> = Vec::new();
    for problem in &ws.diagnostics {
        lines.push(Line::styled(format!("✗ {problem}"), ERROR));
    }
    if !ws.diagnostics.is_empty() {
        lines.push(Line::default());
    }
    if let Some(row) = ws.steps.get(ws.selected) {
        lines.push(Line::from(vec![
            Span::styled(&row.id, ACCENT.add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw(&row.tool),
        ]));
        if let Some(reasoning) = &row.reasoning {
            lines.push(Line::styled(reasoning.clone(), DIM));
        }
        lines.push(Line::default());
        push_json_section(&mut lines, "input template", &row.input_template);
        if let Some(rendered) = &row.rendered_input {
            push_json_section(&mut lines, "rendered input", rendered);
        }
        if let Some(result) = &row.result {
            push_json_section(&mut lines, "result", result);
        }
    }
    render_scrolled(
        frame,
        lines,
        Block::bordered()
            .border_style(DIM)
            .title(" detail ─ J/K scroll "),
        detail,
        &ws.detail_scroll,
    );
}

fn draw_context_tab(frame: &mut Frame, ws: &PlanWorkspace, area: Rect) {
    let [list_area, detail] =
        *Layout::vertical([Constraint::Percentage(40), Constraint::Min(3)]).split(area)
    else {
        return;
    };
    let items: Vec<ListItem> = ws
        .tools
        .iter()
        .map(|tool| {
            let mut spans = vec![Span::raw(tool.name.clone())];
            if tool.read_only == Some(true) {
                spans.push(Span::styled("  ro", DIM));
            }
            if let Some(shape) = ws.shapes.get(&tool.name) {
                spans.push(Span::styled(format!("  seen ×{}", shape.seen_count), DIM));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().border_style(DIM).title(format!(
            " tool catalog ({}) — what the planner sees ",
            ws.tools.len()
        )))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(ws.selected));
    frame.render_stateful_widget(list, list_area, &mut state);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(tool) = ws.tools.get(ws.selected) {
        lines.push(Line::styled(
            tool.name.clone(),
            ACCENT.add_modifier(Modifier::BOLD),
        ));
        for wrapped in wrap_text(&tool.description, detail.width.saturating_sub(2) as usize) {
            lines.push(Line::from(wrapped));
        }
        lines.push(Line::default());
        push_json_section(&mut lines, "input schema", &tool.input_schema);
        if let Some(schema) = &tool.output_schema {
            push_json_section(&mut lines, "output schema", schema);
        } else if let Some(shape) = ws.shapes.get(&tool.name) {
            push_json_section(&mut lines, "observed output shape", &shape.schema);
            push_json_section(&mut lines, "observed example", &shape.example);
        }
        if let Some(example) = &tool.output_example {
            push_json_section(&mut lines, "output example", example);
        }
    }
    render_scrolled(
        frame,
        lines,
        Block::bordered()
            .border_style(DIM)
            .title(" detail ─ J/K scroll "),
        detail,
        &ws.detail_scroll,
    );
}

fn draw_run_tab(frame: &mut Frame, ws: &PlanWorkspace, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for line in &ws.run_log {
        match line {
            RunLine::Info(text) => lines.push(Line::styled(text.clone(), DIM)),
            RunLine::Error(text) => lines.push(Line::styled(text.clone(), ERROR)),
        }
    }
    if !ws.solver_text.is_empty() {
        lines.push(Line::default());
        for wrapped in wrap_text(&ws.solver_text, width) {
            lines.push(Line::from(wrapped));
        }
    }
    if let Some((headline, is_error)) = &ws.outcome {
        lines.push(Line::default());
        lines.push(Line::styled(
            headline.clone(),
            if *is_error { ERROR } else { OK }.add_modifier(Modifier::BOLD),
        ));
    }
    // Follow the bottom, minus the user's scroll-back (clamped).
    let viewport = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let bottom = total.saturating_sub(viewport) as u16;
    ws.run_scroll.set(ws.run_scroll.get().min(bottom));
    let offset = bottom.saturating_sub(ws.run_scroll.get());
    let widget = Paragraph::new(lines)
        .block(
            Block::bordered()
                .border_style(DIM)
                .title(" run ─ j/k scroll "),
        )
        .scroll((offset, 0));
    frame.render_widget(widget, area);
    if total > viewport {
        let mut state = ScrollbarState::new(bottom as usize).position(offset as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

// ── Status bar and modals ────────────────────────────────────────────────

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mode = match &app.mode {
        Mode::Idle => "idle",
        Mode::Chatting => "agent…",
        Mode::Running { gated: false } => "running",
        Mode::Running { gated: true } => "debugging",
        Mode::Paused(prompt) => match prompt.kind {
            GateKind::BeforeCall => "paused",
            GateKind::OnError { .. } => "paused (error)",
        },
        Mode::Editing(_) => "editing",
    };
    let mut spans = vec![
        Span::styled(format!(" {mode} "), ACCENT.add_modifier(Modifier::REVERSED)),
        Span::raw(" "),
    ];
    // The live indicator: what's executing right now, with elapsed time.
    if app.wants_tick() {
        if let Some(in_flight) = &app.in_flight {
            spans.push(Span::styled(
                format!(
                    "{} {} {} {:.1}s ",
                    spinner_frame(app.tick),
                    in_flight.path,
                    in_flight.tool,
                    in_flight.started.elapsed().as_secs_f32()
                ),
                Style::new().fg(Color::Yellow),
            ));
        }
    }
    spans.push(Span::styled(app.status.clone(), DIM));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [_, vertical, _] = *Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area) else {
        return area;
    };
    let [_, horizontal, _] = *Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical)
    else {
        return vertical;
    };
    horizontal
}

/// The debugger's view at a pause: position, call stack, error (when
/// paused on one), the rendered input, and the scope — everything a
/// template could read at this point.
fn draw_debug_panel(
    frame: &mut Frame,
    prompt: &GatePrompt,
    area: Rect,
    scroll: &std::cell::Cell<u16>,
) {
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    match &prompt.kind {
        GateKind::BeforeCall => lines.push(Line::from(vec![
            Span::styled("⏸ paused before ", Style::new().fg(Color::Yellow)),
            Span::styled(&prompt.path, ACCENT.add_modifier(Modifier::BOLD)),
            Span::raw(" → "),
            Span::styled(&prompt.tool, Style::new().add_modifier(Modifier::BOLD)),
        ])),
        GateKind::OnError { .. } => lines.push(Line::from(vec![
            Span::styled("✗ ", ERROR),
            Span::styled(&prompt.path, ACCENT.add_modifier(Modifier::BOLD)),
            Span::raw(" → "),
            Span::styled(&prompt.tool, Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" failed", ERROR.add_modifier(Modifier::BOLD)),
        ])),
    }
    if !prompt.call_stack.is_empty() {
        lines.push(Line::styled(
            format!("call stack: {}", prompt.call_stack.join(" → ")),
            DIM,
        ));
    }
    lines.push(Line::default());
    if let GateKind::OnError { error } = &prompt.kind {
        push_json_section(&mut lines, "error", error);
    }
    push_json_section(&mut lines, "rendered input", &prompt.input);

    // Scope: one compact line per root — input first, step ids by number,
    // then the body pseudo-roots. Full values: select the step's row.
    lines.push(Line::styled("scope:", DIM));
    let mut roots: Vec<&String> = prompt.scope.keys().collect();
    roots.sort_by_key(|name| scope_order(name));
    for name in roots {
        let compact = serde_json::to_string(&prompt.scope[name.as_str()]).unwrap_or_default();
        let budget = width.saturating_sub(16).max(8);
        let preview: String = if compact.chars().count() > budget {
            format!("{}…", compact.chars().take(budget).collect::<String>())
        } else {
            compact
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{name:>12}  "), ACCENT),
            Span::raw(preview),
        ]));
    }

    let title = match &prompt.kind {
        GateKind::BeforeCall => " debug ─ n next step · c continue · s skip · a abort ",
        GateKind::OnError { .. } => " debug ─ s inject result · n let it fail · a abort ",
    };
    render_scrolled(
        frame,
        lines,
        Block::bordered()
            .border_style(Style::new().fg(Color::Yellow))
            .title(title),
        area,
        scroll,
    );
}

/// Scope display order: input, step ids ascending, pseudo-roots, rest.
fn scope_order(name: &str) -> (u8, u32) {
    if name == "input" {
        return (0, 0);
    }
    if let Some(number) = graph_core::pipeline::plan::step_number(name) {
        return (1, number);
    }
    match name {
        "item" => (2, 0),
        "index" => (2, 1),
        "accumulator" => (2, 2),
        _ => (3, 0),
    }
}

fn draw_editor(frame: &mut Frame, editor: &super::editor::EditorState) {
    if matches!(editor.context, EditorContext::ConfirmQuit) {
        let area = centered(frame.area(), 50, 20);
        frame.render_widget(Clear, area);
        let widget = Paragraph::new(editor.title.clone())
            .alignment(Alignment::Center)
            .block(Block::bordered().border_style(Style::new().fg(Color::Yellow)));
        frame.render_widget(widget, area);
        return;
    }

    let area = centered(frame.area(), 74, 70);
    frame.render_widget(Clear, area);
    let header_height = editor.header.len().min(4) as u16;
    let [header_area, body, footer] = *Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(area) else {
        return;
    };
    let header_lines: Vec<Line> = editor
        .header
        .iter()
        .map(|line| Line::styled(line.clone(), ACCENT))
        .collect();
    frame.render_widget(
        Paragraph::new(header_lines).wrap(Wrap { trim: false }),
        header_area,
    );
    let mut textarea = editor.textarea.clone();
    textarea.set_block(
        Block::bordered()
            .border_style(Style::new().fg(Color::Yellow))
            .title(format!(" {} ", editor.title)),
    );
    frame.render_widget(&textarea, body);
    // The key hints never disappear — the error renders alongside them.
    let mut footer_lines = vec![Line::styled(" Ctrl+S submit · Esc cancel", DIM)];
    if let Some(error) = &editor.error {
        footer_lines.insert(0, Line::styled(format!(" {error}"), ERROR));
    }
    frame.render_widget(Paragraph::new(footer_lines), footer);
}

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 60, 70);
    frame.render_widget(Clear, area);
    let bindings = [
        ("Tab", "toggle focus chat ↔ workspace"),
        ("1 / 2 / 3", "workspace tab (Alt+n from anywhere)"),
        ("Enter", "send chat message (Alt+Enter for newline)"),
        ("Ctrl+↑ / Ctrl+↓", "scroll chat"),
        ("j / k", "select step or tool (run tab: scroll)"),
        ("J / K, PgUp / PgDn", "scroll the detail / debug / run pane"),
        ("b", "toggle breakpoint on the selected step"),
        ("v", "validate the plan"),
        ("r", "run the plan"),
        (
            "g",
            "debug run — to the first breakpoint, or pause at every call",
        ),
        (
            "n / Enter",
            "paused: next step — proceed this call, pause at the next",
        ),
        ("c", "paused: continue to the next breakpoint (or error)"),
        (
            "s",
            "paused: skip / inject (on error: replace the failed result)",
        ),
        ("a", "paused: abort the run"),
        ("Ctrl+S", "save the draft to the plans directory"),
        ("Ctrl+C, q", "quit"),
    ];
    let lines: Vec<Line> = bindings
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:>16}  "), ACCENT),
                Span::raw(*what),
            ])
        })
        .collect();
    let widget = Paragraph::new(lines).block(
        Block::bordered()
            .border_style(ACCENT)
            .title(" keys — any key to close "),
    );
    frame.render_widget(widget, area);
}

// ── Text helpers ─────────────────────────────────────────────────────────

fn push_json_section(lines: &mut Vec<Line>, title: &str, value: &serde_json::Value) {
    lines.push(Line::styled(format!("{title}:"), DIM));
    let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
    for raw in pretty.lines().take(200) {
        lines.push(Line::from(raw.to_string()));
    }
    lines.push(Line::default());
}

/// Greedy word wrap; long unbreakable words split at the width boundary.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.chars().count() <= width {
            out.push(raw.to_string());
            continue;
        }
        let mut line = String::new();
        for word in raw.split(' ') {
            let word_len = word.chars().count();
            let line_len = line.chars().count();
            if line_len > 0 && line_len + 1 + word_len > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            if word_len > width {
                let mut rest: String = word.to_string();
                while rest.chars().count() > width {
                    let chunk: String = rest.chars().take(width).collect();
                    out.push(chunk.clone());
                    rest = rest.chars().skip(width).collect();
                }
                line.push_str(&rest);
            } else {
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
