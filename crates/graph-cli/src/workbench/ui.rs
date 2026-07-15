//! Rendering: the generic dual-pane shell (chat · workspace · status bar ·
//! modals) with the plan workspace as the right-pane body.

use super::app::{App, ChatEntry, Focus, GateKind, GatePrompt, Mode};
use super::editor::EditorContext;
use super::plan_ws::{PlanWorkspace, RowKey, RunLine, StepRow, StepStatus, WsTab};
use graph_core::pipeline::{DECIDE_TOOL, EXIT_TOOL, MAP_TOOL, REDUCE_TOOL};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
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
        draw_help(frame, app);
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
    let block = pane_block(" chat ", app.focus == Focus::Chat).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Soft-wrap the input first: the box grows with its wrapped rows
    // (within bounds), so the layout depends on the wrap.
    let input_width = inner.width.saturating_sub(2).max(1) as usize;
    let (input_rows, cursor) =
        wrap_input(app.chat.input.lines(), app.chat.input.cursor(), input_width);
    let input_height = (input_rows.len() as u16).clamp(2, 6) + 2;
    let [scrollback, input] =
        *Layout::vertical([Constraint::Min(1), Constraint::Length(input_height)]).split(inner)
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
        lines.push(Line::styled(spinner_frame(app.tick), DIM));
    }

    let height = scrollback.height as usize;
    let total = lines.len();
    let bottom_offset = total.saturating_sub(height);
    app.chat
        .scroll
        .set(app.chat.scroll.get().min(bottom_offset as u16));
    let offset = bottom_offset.saturating_sub(app.chat.scroll.get() as usize);
    let paragraph = Paragraph::new(lines).scroll((offset as u16, 0));
    frame.render_widget(paragraph, scrollback);
    if total > height {
        let mut state = ScrollbarState::new(bottom_offset).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollback,
            &mut state,
        );
    }

    // Rendered by hand instead of the TextArea widget: tui-textarea has no
    // soft wrap, so long lines would run off the edge. The TextArea still
    // owns all editing state; this is a display of its lines and cursor.
    let input_block = Block::bordered()
        .border_style(DIM)
        .title(" Enter send · Alt+Enter newline ");
    let input_inner = input_block.inner(input);
    frame.render_widget(input_block, input);
    let empty = input_rows.len() == 1 && input_rows[0].is_empty();
    let text: Vec<Line> = if empty {
        vec![Line::styled(app.chat.input.placeholder_text(), DIM)]
    } else {
        input_rows.into_iter().map(Line::from).collect()
    };
    let visible = input_inner.height.max(1) as usize;
    let scroll = cursor.0.saturating_sub(visible - 1);
    frame.render_widget(Paragraph::new(text).scroll((scroll as u16, 0)), input_inner);
    if app.focus == Focus::Chat && !matches!(app.mode, Mode::Editing(_)) && !app.show_help {
        frame.set_cursor_position((
            input_inner.x + cursor.1 as u16,
            input_inner.y + (cursor.0 - scroll) as u16,
        ));
    }
}

/// Per-branch pipe color. Deliberately distinct from the status palette
/// (green=ok, red=error, yellow=running, magenta glyph=skipped) so a pipe
/// never reads as an outcome.
fn branch_color(body: &str) -> Color {
    match body {
        "then" => Color::Blue,
        "else" => Color::Magenta,
        // "do" — map/reduce bodies
        _ => Color::Cyan,
    }
}

/// The junction icon for a control-flow tool, with its color: the icon
/// takes the color of what flows out of it.
fn control_icon(tool: &str) -> Option<(&'static str, Color)> {
    match tool {
        DECIDE_TOOL => Some(("⑂", Color::Blue)),
        MAP_TOOL => Some(("⟳", Color::Cyan)),
        REDUCE_TOOL => Some(("∑", Color::Cyan)),
        EXIT_TOOL => Some(("⎋", Color::Magenta)),
        "plan_and_execute" => Some(("ƒ", Color::White)),
        tool if tool.starts_with("plan__") => Some(("ƒ", Color::White)),
        _ => None,
    }
}

/// Tree piping for one row, `tree`-style: trunk connectors in structural
/// gray, decide's named branches and map/reduce bodies piping down in
/// their branch color, control icons fused into the junction cell (├─⑂).
/// Also returns whether the row's id column should render — branch heads
/// and single-call bodies carry their label inside the piping instead.
fn tree_spans(rows: &[StepRow], index: usize) -> (Vec<Span<'static>>, bool) {
    let row = &rows[index];
    let is_top = |key: &RowKey| matches!(key, RowKey::Step(_) | RowKey::Finish);
    match &row.key {
        RowKey::Root => (Vec::new(), false),
        RowKey::Step(_) | RowKey::Finish => {
            let last_top = !rows[index + 1..].iter().any(|r| is_top(&r.key));
            let elbow = if last_top { "└─" } else { "├─" };
            match control_icon(&row.tool) {
                Some((icon, color)) => (
                    vec![
                        Span::styled(elbow, DIM),
                        Span::styled(
                            format!("{icon} "),
                            Style::new().fg(color).add_modifier(Modifier::BOLD),
                        ),
                    ],
                    true,
                ),
                None => (vec![Span::styled(format!("{elbow}─ "), DIM)], true),
            }
        }
        RowKey::BranchHead { step, body } => {
            let color = branch_color(body);
            let elbow = if later_branch(rows, index, step, body) {
                "├── "
            } else {
                "└── "
            };
            (
                vec![
                    trunk_cont(rows, index),
                    Span::styled(elbow, Style::new().fg(color)),
                    Span::styled(
                        row.id.clone(),
                        Style::new().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ],
                false,
            )
        }
        RowKey::Body {
            step,
            body,
            body_step,
        } => {
            let color = branch_color(body);
            let last_in_group = !rows[index + 1..].iter().any(|r| {
                matches!(&r.key, RowKey::Body { step: s, body: b, .. } if s == step && b == body)
            });
            let mut spans = vec![trunk_cont(rows, index)];
            // Under a branch head, the head's own guide continues.
            let has_head = rows[..index].iter().any(|r| {
                matches!(&r.key, RowKey::BranchHead { step: s, body: b } if s == step && b == body)
            });
            if has_head {
                let guide = if later_branch(rows, index, step, body) {
                    "│   "
                } else {
                    "    "
                };
                spans.push(Span::styled(guide, Style::new().fg(color)));
            }
            let elbow = if last_in_group { "└─" } else { "├─" };
            match control_icon(&row.tool) {
                Some((icon, icon_color)) => {
                    spans.push(Span::styled(elbow, Style::new().fg(color)));
                    spans.push(Span::styled(
                        format!("{icon} "),
                        Style::new().fg(icon_color).add_modifier(Modifier::BOLD),
                    ));
                }
                None => spans.push(Span::styled(format!("{elbow}─ "), Style::new().fg(color))),
            }
            let show_id = if body_step.is_none() {
                // Single-call body: the branch label rides the piping.
                spans.push(Span::styled(
                    format!("{body} "),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ));
                false
            } else {
                true
            };
            (spans, show_id)
        }
    }
}

/// The trunk continuation for rows nested under a control step: a │ while
/// later top-level rows exist, blank once the owner is the trunk's last.
fn trunk_cont(rows: &[StepRow], index: usize) -> Span<'static> {
    let later_top = rows[index + 1..]
        .iter()
        .any(|r| matches!(&r.key, RowKey::Step(_) | RowKey::Finish));
    Span::styled(if later_top { "│  " } else { "   " }, DIM)
}

/// Whether another branch group of the same control step follows this
/// row — i.e. this branch's guide must keep flowing past its subtree.
/// Counts branch heads and body rows alike, so a single-call `else`
/// (which has no head row) still holds the `then` guide open.
fn later_branch(rows: &[StepRow], index: usize, step: &str, body: &str) -> bool {
    rows[index + 1..].iter().any(|r| match &r.key {
        RowKey::BranchHead { step: s, body: b } => s == step && b != body,
        RowKey::Body {
            step: s, body: b, ..
        } => s == step && b != body,
        _ => false,
    })
}

/// Character-level soft wrap for the input box: each logical line becomes
/// ceil(len/width) visual rows (at least one), and the cursor maps to its
/// (visual_row, x). A cursor sitting exactly at the end of a full row gets
/// an empty continuation row so it stays inside the box.
fn wrap_input(
    lines: &[String],
    cursor: (usize, usize),
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_pos = (0, 0);
    for (i, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let start = rows.len();
        if chars.is_empty() {
            rows.push(String::new());
        } else {
            for chunk in chars.chunks(width) {
                rows.push(chunk.iter().collect());
            }
        }
        if i == cursor.0 {
            let vrow = start + cursor.1 / width;
            cursor_pos = (vrow, cursor.1 % width);
            while rows.len() <= vrow {
                rows.push(String::new());
            }
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    (rows, cursor_pos)
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
    let inner_width = area.width.saturating_sub(2);
    let widget = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    // Post-wrap height: long JSON lines wrap to multiple rows, so the raw
    // line count would under-scroll and strand the tail off-screen.
    let total = widget.line_count(inner_width);
    let max_scroll = total.saturating_sub(viewport) as u16;
    scroll.set(scroll.get().min(max_scroll));
    let offset = scroll.get();
    let widget = widget.scroll((offset, 0));
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

    // Step list, tree-style: breakpoint gutter · status glyph · piping ·
    // id · tool. The plan identifier is the root; steps fork off a gray
    // trunk; control steps fuse their icon into the junction (├─⑂) and
    // their subtrees pipe down in the branch's color. The paused row's id
    // renders yellow-bold; running rows animate.
    let paused_row = paused_prompt(app)
        .filter(|p| p.call_stack.is_empty())
        .and_then(|p| ws.find_path(&p.path));
    let items: Vec<ListItem> = ws
        .steps
        .iter()
        .enumerate()
        .map(|(index, row)| {
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
            let gutter = if matches!(row.key, RowKey::Step(_)) && app.breakpoints.contains(&row.id)
            {
                Span::styled("●", ERROR)
            } else {
                Span::raw(" ")
            };
            let id_style = if paused_row == Some(index) {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            // Structural rows (root, branch heads) never run: no glyph.
            let structural = matches!(row.key, RowKey::Root | RowKey::BranchHead { .. });
            let mut spans = vec![gutter];
            if structural {
                spans.push(Span::raw("  "));
            } else {
                spans.push(Span::styled(format!("{glyph} "), style));
            }
            let (tree, show_id) = tree_spans(&ws.steps, index);
            spans.extend(tree);
            if matches!(row.key, RowKey::Root) {
                spans.push(Span::styled(
                    row.id.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
            } else if show_id {
                spans.push(Span::styled(format!("{:4} ", row.id), id_style));
            }
            spans.push(Span::raw(row.tool.clone()));
            ListItem::new(Line::from(spans))
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

    // While the selected row is the paused call (or the pause is inside a
    // nested plan), the detail pane becomes the debug panel.
    let show_debug = paused_prompt(app).is_some_and(|p| {
        if p.call_stack.is_empty() {
            ws.find_path(&p.path) == Some(ws.selected)
        } else {
            true
        }
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
            .title(" detail ─ PgUp/PgDn scroll "),
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
            .title(" detail ─ PgUp/PgDn scroll "),
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
                .title(" run ─ PgUp/PgDn scroll "),
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
    // A turn with no tool in flight is the model thinking — without this,
    // a slow completion (or a silent rate-limit backoff inside it) is
    // indistinguishable from a hang.
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
        } else if let Some(started) = &app.turn_started {
            spans.push(Span::styled(
                format!(
                    "{} thinking {:.0}s ",
                    spinner_frame(app.tick),
                    started.elapsed().as_secs_f32()
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

    // Scope: every root in full, pretty-printed — input first, step ids by
    // number, then the body pseudo-roots. The panel scrolls (PgUp/PgDn).
    lines.push(Line::styled("scope:", DIM));
    lines.push(Line::default());
    let mut roots: Vec<&String> = prompt.scope.keys().collect();
    roots.sort_by_key(|name| scope_order(name));
    for name in roots {
        lines.push(Line::styled(
            name.clone(),
            ACCENT.add_modifier(Modifier::BOLD),
        ));
        let pretty = serde_json::to_string_pretty(&prompt.scope[name.as_str()]).unwrap_or_default();
        for raw in pretty.lines() {
            lines.push(Line::from(raw.to_string()));
        }
        lines.push(Line::default());
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

fn draw_help(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 60, 70);
    frame.render_widget(Clear, area);
    let bindings = [
        ("Tab", "toggle focus chat ↔ workspace"),
        ("1 / 2 / 3", "workspace tab (Alt+n from anywhere)"),
        ("Enter", "send chat message (Alt+Enter for newline)"),
        ("j / k", "select step or tool"),
        (
            "PgUp / PgDn",
            "scroll the focused pane (chat · detail · debug · run)",
        ),
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
        ("u", "undo the last draft replacement (again to redo)"),
        ("Ctrl+S", "save the draft to the plans directory"),
        ("Ctrl+C, q", "quit (while paused: abort the run)"),
    ];
    let mut lines: Vec<Line> = bindings
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:>16}  "), ACCENT),
                Span::raw(*what),
            ])
        })
        .collect();
    if let Some(path) = &app.log_path {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("debug log: {}", path.display()), DIM));
    }
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
    for raw in pretty.lines() {
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

#[cfg(test)]
mod tests {
    use super::{branch_color, wrap_input};

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_input_chunks_lines_and_maps_the_cursor() {
        // Short line: untouched, cursor passes through.
        let (rows, cursor) = wrap_input(&s(&["hello"]), (0, 3), 10);
        assert_eq!(rows, s(&["hello"]));
        assert_eq!(cursor, (0, 3));

        // A long line wraps at the width; the cursor lands mid-chunk.
        let (rows, cursor) = wrap_input(&s(&["abcdefghij"]), (0, 7), 4);
        assert_eq!(rows, s(&["abcd", "efgh", "ij"]));
        assert_eq!(cursor, (1, 3));

        // Multiple logical lines: visual rows accumulate.
        let (rows, cursor) = wrap_input(&s(&["abcdef", "xy"]), (1, 2), 4);
        assert_eq!(rows, s(&["abcd", "ef", "xy"]));
        assert_eq!(cursor, (2, 2));
    }

    #[test]
    fn branch_colors_are_distinct() {
        let colors = [
            branch_color("then"),
            branch_color("else"),
            branch_color("do"),
        ];
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    fn tree_texts(doc_yaml: &str) -> Vec<String> {
        let doc: graph_core::pipeline::doc::PlanDoc = serde_yaml::from_str(doc_yaml).unwrap();
        let mut ws = super::super::plan_ws::PlanWorkspace::default();
        ws.set_doc(doc);
        (0..ws.steps.len())
            .map(|index| {
                let (spans, _) = super::tree_spans(&ws.steps, index);
                spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn tree_piping_forks_branches_and_closes_the_trunk() {
        let texts = tree_texts(
            r#"
identifier: demo
name: Demo
description: d
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E1
    tool_name: decide
    input:
      if: { value: 1, op: gt, to: 0 }
      then:
        - { id: warn, tool_name: t__notify, input: {} }
        - { id: bail, tool_name: exit, input: { status: error } }
      else:
        tool_name: t__log
        input: {}
  - id: E2
    tool_name: map
    input:
      over: "{{E0.values}}"
      do:
        - { id: note, tool_name: t__notify, input: {} }
solver:
  queryToAnswer: "q"
"#,
        );
        assert_eq!(
            texts,
            vec![
                "".to_string(),             // root
                "├── ".to_string(),         // E0
                "├─⑂ ".to_string(),         // E1 decide — icon in the junction
                "│  ├── then".to_string(),  // branch head, trunk continuing
                "│  │   ├── ".to_string(),  // warn
                "│  │   └─⎋ ".to_string(),  // bail — exit terminates the line
                "│  └── else ".to_string(), // single-call branch label rides the piping
                "├─⟳ ".to_string(),         // E2 map
                "│  └── ".to_string(),      // note — anonymous body, no head
                "└── ".to_string(),         // solver closes the trunk
            ]
        );
    }

    #[test]
    fn wrap_input_edges() {
        // Empty input: one empty row, cursor at origin.
        let (rows, cursor) = wrap_input(&s(&[""]), (0, 0), 8);
        assert_eq!(rows, s(&[""]));
        assert_eq!(cursor, (0, 0));

        // Cursor exactly at the end of a full row wraps to an empty
        // continuation row instead of the border column.
        let (rows, cursor) = wrap_input(&s(&["abcd"]), (0, 4), 4);
        assert_eq!(rows, s(&["abcd", ""]));
        assert_eq!(cursor, (1, 0));
    }
}
