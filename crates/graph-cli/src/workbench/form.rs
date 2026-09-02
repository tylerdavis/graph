use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use tui_textarea::{CursorMove, TextArea};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Json,
    Auto,
    Lines,
}

impl FieldKind {
    pub fn label(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Json => "json",
            FieldKind::Auto => "json or text",
            FieldKind::Lines => "one per line",
        }
    }
}

pub struct Field {
    pub key: String,
    pub label: String,
    pub hint: Option<String>,
    pub kind: FieldKind,
    pub multiline: bool,
    pub required: bool,
    pub textarea: TextArea<'static>,
    pub error: Option<String>,
}

const MIN_MULTILINE_ROWS: usize = 2;
const MAX_MULTILINE_ROWS: usize = 8;

impl Field {
    pub fn new(key: &str, label: &str, kind: FieldKind, multiline: bool) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        let mut field = Self {
            key: key.to_string(),
            label: label.to_string(),
            hint: None,
            kind,
            multiline,
            required: false,
            textarea,
            error: None,
        };
        field.set_focused(false);
        field
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        let hint = hint.into();
        if !hint.is_empty() {
            self.hint = Some(hint);
        }
        self
    }

    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    pub fn value(mut self, value: Option<&Value>) -> Self {
        let text = match (self.kind, value) {
            (_, None) => String::new(),
            (FieldKind::Text | FieldKind::Auto, Some(Value::String(s))) => s.clone(),
            (FieldKind::Lines, Some(Value::Array(items))) => items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            (_, Some(other)) => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        self.textarea = TextArea::from(text.lines().map(str::to_string).collect::<Vec<_>>());
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::End);
        self.set_focused(false);
        self
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn read(&self) -> Result<Option<Value>, String> {
        let text = self.text();
        if text.trim().is_empty() {
            return Ok(None);
        }
        match self.kind {
            FieldKind::Text => Ok(Some(Value::String(text))),
            FieldKind::Json => serde_json::from_str(text.trim())
                .map(Some)
                .map_err(|error| format!("{}: invalid JSON — {error}", self.label)),
            FieldKind::Auto => Ok(Some(
                serde_json::from_str(text.trim()).unwrap_or(Value::String(text)),
            )),
            FieldKind::Lines => Ok(Some(Value::Array(
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(|line| Value::String(line.to_string()))
                    .collect(),
            ))),
        }
    }

    pub fn height(&self) -> u16 {
        let rows = if self.multiline {
            self.textarea
                .lines()
                .len()
                .clamp(MIN_MULTILINE_ROWS, MAX_MULTILINE_ROWS)
        } else {
            1
        };
        rows as u16 + 2
    }

    pub fn set_focused(&mut self, focused: bool) {
        let mut title = format!(" {} · {}", self.label, self.kind.label());
        if self.required {
            title.push_str(", required");
        }
        title.push(' ');
        let border = if focused {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        let mut block = Block::bordered().border_style(border).title(title);
        if let Some(hint) = &self.hint {
            block = block.title_bottom(
                ratatui::text::Line::styled(
                    format!(" {hint} "),
                    Style::new().add_modifier(Modifier::DIM),
                )
                .right_aligned(),
            );
        }
        self.textarea.set_block(block);
        self.textarea.set_cursor_style(if focused {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        });
    }

    fn at_first_line(&self) -> bool {
        self.textarea.cursor().0 == 0
    }

    fn at_last_line(&self) -> bool {
        self.textarea.cursor().0 + 1 >= self.textarea.lines().len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Valid {
        pre_existing: Vec<String>,
    },
    Invalid {
        problems: Vec<String>,
        pre_existing: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormAction {
    None,
    Save,
    Validate,
    Cancel,
}

pub struct Form {
    pub title: String,
    pub header: String,
    pub fields: Vec<Field>,
    pub focused: usize,
    pub verdict: Option<Verdict>,
    pub scroll: Cell<u16>,
    pub view_rows: Cell<u16>,
    pub visible_fields: RefCell<Vec<(usize, Rect)>>,
}

impl Form {
    pub fn new(title: impl Into<String>, header: impl Into<String>, fields: Vec<Field>) -> Self {
        let mut form = Self {
            title: title.into(),
            header: header.into(),
            fields,
            focused: 0,
            verdict: None,
            scroll: Cell::new(0),
            view_rows: Cell::new(0),
            visible_fields: RefCell::new(Vec::new()),
        };
        form.set_focus(0);
        form
    }

    pub fn layout(&self) -> Vec<(u16, u16)> {
        let mut top = 0u16;
        self.fields
            .iter()
            .map(|field| {
                let height = field.height();
                let entry = (top, height);
                top = top.saturating_add(height);
                entry
            })
            .collect()
    }

    pub fn total_height(&self) -> u16 {
        self.layout()
            .last()
            .map(|(top, height)| top + height)
            .unwrap_or(0)
    }

    pub fn set_focus(&mut self, index: usize) {
        if self.fields.is_empty() {
            return;
        }
        let index = index.min(self.fields.len() - 1);
        for (i, field) in self.fields.iter_mut().enumerate() {
            field.set_focused(i == index);
        }
        self.focused = index;
        self.follow_focus();
    }

    fn focus_next(&mut self) {
        if self.focused + 1 < self.fields.len() {
            self.set_focus(self.focused + 1);
        }
    }

    fn focus_previous(&mut self) {
        if self.focused > 0 {
            self.set_focus(self.focused - 1);
        }
    }

    fn follow_focus(&self) {
        let rows = self.view_rows.get();
        if rows == 0 {
            return;
        }
        let Some(&(top, height)) = self.layout().get(self.focused) else {
            return;
        };
        let scroll = self.scroll.get();
        if top < scroll {
            self.scroll.set(top);
        } else if top + height > scroll + rows {
            self.scroll.set((top + height).saturating_sub(rows));
        }
    }

    pub fn scroll_by(&self, up: bool, amount: u16) {
        let current = self.scroll.get();
        self.scroll.set(if up {
            current.saturating_sub(amount)
        } else {
            current.saturating_add(amount)
        });
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Char('s') if ctrl => return FormAction::Save,
            KeyCode::Char('t') if ctrl => return FormAction::Validate,
            KeyCode::Tab => {
                self.focus_next();
                return FormAction::None;
            }
            KeyCode::BackTab => {
                self.focus_previous();
                return FormAction::None;
            }
            KeyCode::PageDown => {
                self.scroll_by(false, self.view_rows.get().max(1) / 2);
                return FormAction::None;
            }
            KeyCode::PageUp => {
                self.scroll_by(true, self.view_rows.get().max(1) / 2);
                return FormAction::None;
            }
            _ => {}
        }
        let Some(field) = self.fields.get_mut(self.focused) else {
            return FormAction::None;
        };
        match key.code {
            KeyCode::Enter => {
                let newline = key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                if newline && field.multiline {
                    field.textarea.insert_newline();
                } else {
                    self.focus_next();
                }
            }
            KeyCode::Up if field.at_first_line() => self.focus_previous(),
            KeyCode::Down if field.at_last_line() => self.focus_next(),
            KeyCode::Up => field.textarea.move_cursor(CursorMove::Up),
            KeyCode::Down => field.textarea.move_cursor(CursorMove::Down),
            _ => {
                field.textarea.input(key);
            }
        }
        FormAction::None
    }

    pub fn paste(&mut self, text: &str) {
        let Some(field) = self.fields.get_mut(self.focused) else {
            return;
        };
        if field.multiline {
            field.textarea.insert_str(text);
        } else {
            field.textarea.insert_str(text.replace('\n', " "));
        }
    }

    pub fn read(&mut self) -> Result<serde_json::Map<String, Value>, Vec<String>> {
        let mut values = serde_json::Map::new();
        let mut problems = Vec::new();
        for field in &mut self.fields {
            field.error = None;
            match field.read() {
                Ok(Some(value)) => {
                    values.insert(field.key.clone(), value);
                }
                Ok(None) if field.required => {
                    let problem = format!("{} is required", field.label);
                    field.error = Some(problem.clone());
                    problems.push(problem);
                }
                Ok(None) => {}
                Err(problem) => {
                    field.error = Some(problem.clone());
                    problems.push(problem);
                }
            }
        }
        if problems.is_empty() {
            Ok(values)
        } else {
            Err(problems)
        }
    }

    pub fn focus_first_error(&mut self) {
        if let Some(index) = self.fields.iter().position(|field| field.error.is_some()) {
            self.set_focus(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn form() -> Form {
        Form::new(
            "t",
            "h",
            vec![
                Field::new("id", "id", FieldKind::Text, false)
                    .required(true)
                    .value(Some(&json!("E0"))),
                Field::new("note", "note", FieldKind::Text, true).value(Some(&json!("a\nb"))),
                Field::new("count", "count", FieldKind::Json, false).value(Some(&json!(3))),
                Field::new("over", "over", FieldKind::Auto, false),
                Field::new("tags", "tags", FieldKind::Lines, true).value(Some(&json!(["x", "y"]))),
            ],
        )
    }

    #[test]
    fn fields_prefill_and_read_back_by_kind() {
        let mut form = form();
        assert_eq!(form.fields[1].text(), "a\nb");
        assert_eq!(form.fields[2].text(), "3");
        assert_eq!(form.fields[4].text(), "x\ny");
        let values = form.read().unwrap();
        assert_eq!(values["id"], json!("E0"));
        assert_eq!(values["note"], json!("a\nb"));
        assert_eq!(values["count"], json!(3));
        assert!(!values.contains_key("over"), "empty fields are absent");
        assert_eq!(values["tags"], json!(["x", "y"]));
    }

    #[test]
    fn auto_fields_fall_back_to_text_and_json_fields_report_parse_errors() {
        let mut form = form();
        form.fields[3].textarea.insert_str("{{E0.items}}");
        form.fields[2].textarea = TextArea::from(["{oops"]);
        let problems = form.read().unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].starts_with("count: invalid JSON"),
            "{problems:?}"
        );
        assert!(form.fields[2].error.is_some());

        form.fields[2].textarea = TextArea::from(["[1, 2]"]);
        let values = form.read().unwrap();
        assert_eq!(values["over"], json!("{{E0.items}}"));
        assert_eq!(values["count"], json!([1, 2]));
        assert!(
            form.fields[2].error.is_none(),
            "errors clear on a clean read"
        );

        form.fields[3].textarea = TextArea::from(["true"]);
        assert_eq!(form.read().unwrap()["over"], json!(true));
    }

    #[test]
    fn required_empty_fields_fail_the_read() {
        let mut form = form();
        form.fields[0].textarea = TextArea::from([""]);
        let problems = form.read().unwrap_err();
        assert_eq!(problems, vec!["id is required".to_string()]);
        form.focus_first_error();
        assert_eq!(form.focused, 0);
    }

    #[test]
    fn navigation_keys_move_focus_and_edges_fall_through() {
        let mut form = form();
        assert_eq!(form.focused, 0);
        form.handle_key(key(KeyCode::Tab));
        assert_eq!(form.focused, 1);
        form.handle_key(key(KeyCode::BackTab));
        assert_eq!(form.focused, 0);
        form.handle_key(with(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(form.focused, 1, "a single-line field never takes a newline");
        assert_eq!(form.fields[0].text(), "E0");
        form.handle_key(with(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(form.fields[1].textarea.lines().len(), 3);
        assert_eq!(form.focused, 1);
        form.handle_key(key(KeyCode::Enter));
        assert_eq!(form.focused, 2);
        form.set_focus(1);
        form.fields[1].textarea.move_cursor(CursorMove::Bottom);
        form.handle_key(key(KeyCode::Up));
        assert_eq!(form.focused, 1, "moved within the field");
        form.handle_key(key(KeyCode::Up));
        assert_eq!(form.focused, 1);
        form.handle_key(key(KeyCode::Up));
        assert_eq!(form.focused, 0, "first line: previous field");
        form.handle_key(key(KeyCode::Down));
        assert_eq!(form.focused, 1, "single line is both first and last");
        assert_eq!(form.handle_key(key(KeyCode::Esc)), FormAction::Cancel);
        assert_eq!(
            form.handle_key(with(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            FormAction::Save
        );
        assert_eq!(
            form.handle_key(with(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            FormAction::Validate
        );
        form.set_focus(0);
        form.handle_key(key(KeyCode::Char('1')));
        assert_eq!(form.fields[0].text(), "E01");
    }

    #[test]
    fn focus_follows_into_view_and_paging_scrolls() {
        let mut form = form();
        form.view_rows.set(6);
        assert_eq!(form.total_height(), 17);
        form.set_focus(4);
        assert_eq!(
            form.scroll.get(),
            11,
            "tags (13..17) pulled into a 6-row view"
        );
        form.set_focus(0);
        assert_eq!(form.scroll.get(), 0);
        form.handle_key(key(KeyCode::PageDown));
        assert_eq!(form.scroll.get(), 3);
        form.handle_key(key(KeyCode::PageUp));
        assert_eq!(form.scroll.get(), 0);
    }

    #[test]
    fn paste_flattens_into_single_line_fields() {
        let mut form = form();
        form.paste("x\ny");
        assert_eq!(form.fields[0].text(), "E0x y");
        form.set_focus(1);
        form.paste("\nz");
        assert_eq!(form.fields[1].textarea.lines().len(), 3);
    }
}
