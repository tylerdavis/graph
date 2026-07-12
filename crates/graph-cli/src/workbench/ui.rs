//! Rendering: the generic dual-pane shell (chat · workspace · status bar ·
//! modals) with the plan workspace as the right-pane body.

use super::app::{App, ChatEntry, Focus, Mode};
use super::editor::EditorContext;
use super::plan_ws::{PlanWorkspace, RunLine, StepStatus, WsTab};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
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

    match &app.mode {
        Mode::Paused(prompt) => draw_gate_prompt(frame, prompt),
        Mode::Editing(editor) => draw_editor(frame, editor),
        Mode::Help => draw_help(frame),
        _ => {}
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
        WsTab::Plan => draw_plan_tab(frame, &app.ws, app.dirty, body),
        WsTab::Context => draw_context_tab(frame, &app.ws, body),
        WsTab::Run => draw_run_tab(frame, &app.ws, body),
    }
}

fn draw_plan_tab(frame: &mut Frame, ws: &PlanWorkspace, dirty: bool, area: Rect) {
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

    // Step list.
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
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", row.status.glyph()), style),
                Span::styled(
                    format!("{:4} ", row.id),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::raw(row.tool.clone()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_style(DIM)
                .title(" steps ─ j/k select · v validate · r run · g gated run "),
        )
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(ws.selected));
    frame.render_stateful_widget(list, steps_area, &mut state);

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
    let detail_widget = Paragraph::new(lines)
        .block(
            Block::bordered()
                .border_style(DIM)
                .title(" detail ─ PgUp/PgDn scroll "),
        )
        .wrap(Wrap { trim: false })
        .scroll((ws.detail_scroll, 0));
    frame.render_widget(detail_widget, detail);
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
    let detail_widget = Paragraph::new(lines)
        .block(Block::bordered().border_style(DIM).title(" detail "))
        .wrap(Wrap { trim: false })
        .scroll((ws.detail_scroll, 0));
    frame.render_widget(detail_widget, detail);
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
    let height = area.height.saturating_sub(2) as usize;
    let offset = lines.len().saturating_sub(height) as u16;
    let widget = Paragraph::new(lines)
        .block(Block::bordered().border_style(DIM).title(" run "))
        .scroll((offset, 0));
    frame.render_widget(widget, area);
}

// ── Status bar and modals ────────────────────────────────────────────────

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mode = match &app.mode {
        Mode::Idle => "idle",
        Mode::Chatting => "agent…",
        Mode::Running { gated: false } => "running",
        Mode::Running { gated: true } => "running (gated)",
        Mode::Paused(_) => "paused",
        Mode::Editing(_) => "editing",
        Mode::Help => "help",
    };
    let line = Line::from(vec![
        Span::styled(format!(" {mode} "), ACCENT.add_modifier(Modifier::REVERSED)),
        Span::raw(" "),
        Span::styled(app.status.clone(), DIM),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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

fn draw_gate_prompt(frame: &mut Frame, prompt: &super::app::GatePrompt) {
    let area = centered(frame.area(), 70, 60);
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(&prompt.path, ACCENT.add_modifier(Modifier::BOLD)),
            Span::raw(" → "),
            Span::styled(&prompt.tool, Style::new().add_modifier(Modifier::BOLD)),
        ]),
        Line::default(),
    ];
    push_json_section(&mut lines, "rendered input", &prompt.input);
    lines.push(Line::default());
    lines.push(Line::styled(
        "y proceed · s skip (inject a result) · a abort",
        ACCENT,
    ));
    let widget = Paragraph::new(lines)
        .block(
            Block::bordered()
                .border_style(Style::new().fg(Color::Yellow))
                .title(" tool call paused "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
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

    let area = centered(frame.area(), 70, 60);
    frame.render_widget(Clear, area);
    let [body, footer] = *Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area)
    else {
        return;
    };
    let mut textarea = editor.textarea.clone();
    textarea.set_block(
        Block::bordered()
            .border_style(Style::new().fg(Color::Yellow))
            .title(format!(" {} ", editor.title)),
    );
    frame.render_widget(&textarea, body);
    let hint = match &editor.error {
        Some(error) => Line::styled(format!(" {error}"), ERROR),
        None => Line::styled(" Ctrl+S submit · Esc cancel", DIM),
    };
    frame.render_widget(Paragraph::new(hint), footer);
}

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 60, 70);
    frame.render_widget(Clear, area);
    let bindings = [
        ("Tab", "toggle focus chat ↔ workspace"),
        ("1 / 2 / 3", "workspace tab (Alt+n from anywhere)"),
        ("Enter", "send chat message (Alt+Enter for newline)"),
        ("Ctrl+↑ / Ctrl+↓", "scroll chat"),
        ("j / k", "select step or tool"),
        ("PgUp / PgDn", "scroll detail"),
        ("v", "validate the plan"),
        ("r", "run the plan"),
        ("g", "gated run — pause before every tool call"),
        (
            "y / s / a",
            "paused: proceed / skip with injected result / abort",
        ),
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
