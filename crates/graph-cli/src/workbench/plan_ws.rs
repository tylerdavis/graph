//! Plan workspace state: the draft document, per-step run status, the
//! context catalog, and the run transcript. Rendering lives in `ui`.

use graph_core::pipeline::doc::PlanDoc;
use graph_core::{ToolDef, ToolShape};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WsTab {
    #[default]
    Plan,
    Context,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Ok,
    Err,
    Skipped,
}

impl StepStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            StepStatus::Pending => "○",
            StepStatus::Running => "◐",
            StepStatus::Ok => "✓",
            StepStatus::Err => "✗",
            StepStatus::Skipped => "⊘",
        }
    }
}

pub struct StepRow {
    pub id: String,
    pub tool: String,
    pub reasoning: Option<String>,
    pub input_template: Value,
    pub status: StepStatus,
    pub rendered_input: Option<Value>,
    pub result: Option<Value>,
}

pub enum RunLine {
    Info(String),
    Error(String),
}

#[derive(Default)]
pub struct PlanWorkspace {
    pub tab: WsTab,
    pub doc: Option<PlanDoc>,
    pub diagnostics: Vec<String>,
    pub steps: Vec<StepRow>,
    pub selected: usize,
    /// Detail/debug pane scroll offset (lines from the top). `Cell` so the
    /// renderer can clamp it to the actual content height.
    pub detail_scroll: Cell<u16>,
    /// Run-transcript scroll offset in lines from the BOTTOM; 0 follows.
    pub run_scroll: Cell<u16>,
    pub tools: Vec<ToolDef>,
    pub shapes: HashMap<String, ToolShape>,
    pub run_log: Vec<RunLine>,
    pub solver_text: String,
    /// Set after a run: the headline plus whether it was an error.
    pub outcome: Option<(String, bool)>,
}

impl PlanWorkspace {
    /// Install a (new or replaced) draft: rebuild step rows, clear run state.
    pub fn set_doc(&mut self, doc: PlanDoc) {
        self.steps = doc
            .steps
            .iter()
            .map(|step| StepRow {
                id: step.id.clone(),
                tool: step.tool_name.clone(),
                reasoning: step.reasoning.clone(),
                input_template: Value::Object(step.input.clone()),
                status: StepStatus::Pending,
                rendered_input: None,
                result: None,
            })
            .collect();
        self.selected = self.selected.min(self.steps.len().saturating_sub(1));
        self.diagnostics.clear();
        self.doc = Some(doc);
        self.outcome = None;
    }

    pub fn set_context(&mut self, tools: Vec<ToolDef>, shapes: Vec<ToolShape>) {
        self.shapes = shapes
            .into_iter()
            .map(|shape| (shape.tool.clone(), shape))
            .collect();
        self.tools = tools;
    }

    /// The inject editor's prefill for a tool, with where it came from:
    /// declared example → observed example → schema skeleton → null.
    pub fn prefill_for(&self, tool: &str) -> (Value, &'static str) {
        let def = self.tools.iter().find(|t| t.name == tool);
        if let Some(example) = def.and_then(|d| d.output_example.clone()) {
            return (example, "the tool's declared output example");
        }
        if let Some(shape) = self.shapes.get(tool) {
            return (shape.example.clone(), "an observed output example");
        }
        if let Some(schema) = def.and_then(|d| d.output_schema.as_ref()) {
            return (
                super::editor::schema_skeleton(schema),
                "a skeleton from the tool's output schema",
            );
        }
        (Value::Null, "empty — no example or schema known")
    }

    /// Select the row with the given step id, if present.
    pub fn select_step(&mut self, id: &str) {
        if let Some(index) = self.steps.iter().position(|row| row.id == id) {
            self.selected = index;
            self.detail_scroll.set(0);
        }
    }

    /// Template paths that later steps (and the finish) read from the given
    /// step — the fields an injected value must contain. Advisory: scans
    /// the raw templates of everything after `step_id` plus the solver data
    /// and output map.
    pub fn downstream_references(&self, step_id: &str) -> Vec<String> {
        let Some(doc) = &self.doc else {
            return Vec::new();
        };
        let position = doc.steps.iter().position(|s| s.id == step_id);
        let mut raw: Vec<String> = Vec::new();
        for step in doc.steps.iter().skip(position.map_or(0, |p| p + 1)) {
            gather_template_strings(&Value::Object(step.input.clone()), &mut raw);
        }
        if let Some(solver) = &doc.solver {
            raw.push(solver.query_to_answer.clone());
            gather_template_strings(&Value::Object(solver.data.clone()), &mut raw);
        }
        if let Some(output) = &doc.output {
            gather_template_strings(&Value::Object(output.clone()), &mut raw);
        }
        let prefix = format!("{step_id}.");
        let mut references = Vec::new();
        for template in raw {
            if !template.contains("{{") {
                continue;
            }
            if let Ok(paths) = graph_core::template::referenced_paths(&template) {
                for path in paths {
                    if (path == step_id || path.starts_with(&prefix)) && !references.contains(&path)
                    {
                        references.push(path);
                    }
                }
            }
        }
        references
    }

    pub fn run_starting(&mut self, gated: bool) {
        for row in &mut self.steps {
            row.status = StepStatus::Pending;
            row.rendered_input = None;
            row.result = None;
        }
        self.run_log.clear();
        self.solver_text.clear();
        self.run_scroll.set(0);
        self.outcome = None;
        self.tab = WsTab::Run;
        self.run_log_info(if gated {
            "debug run started — n next step · c continue · b breakpoint"
        } else {
            "run started"
        });
    }

    pub fn run_finished(&mut self, headline: &str, is_error: bool, results: Map<String, Value>) {
        // Backfill results the events may have missed (e.g. cached rows
        // after a draft change mid-session).
        for row in &mut self.steps {
            if row.result.is_none() {
                if let Some(value) = results.get(&row.id) {
                    row.result = Some(value.clone());
                    if matches!(row.status, StepStatus::Pending | StepStatus::Running) {
                        row.status = StepStatus::Ok;
                    }
                }
            }
        }
        if is_error {
            self.run_log_error(headline);
        } else {
            self.run_log_info(headline);
        }
        self.outcome = Some((headline.to_string(), is_error));
    }

    pub fn run_log_info(&mut self, line: &str) {
        self.run_log.push(RunLine::Info(line.to_string()));
    }

    pub fn run_log_error(&mut self, line: &str) {
        self.run_log.push(RunLine::Error(line.to_string()));
    }

    fn row_mut(&mut self, path: &str) -> Option<&mut StepRow> {
        self.steps.iter_mut().find(|row| row.id == path)
    }

    pub fn step_started(&mut self, path: &str, input: Value) {
        if let Some(row) = self.row_mut(path) {
            row.status = StepStatus::Running;
            row.rendered_input = Some(input);
        }
    }

    pub fn step_running(&mut self, path: &str) {
        // A gate pause names the call's full path; the top-level component
        // is the row to highlight.
        let top = path.split('/').next().unwrap_or(path).to_string();
        if let Some(row) = self.row_mut(&top) {
            if matches!(row.status, StepStatus::Pending) {
                row.status = StepStatus::Running;
            }
        }
    }

    pub fn step_finished(&mut self, path: &str, result: Value, is_error: bool) {
        if let Some(row) = self.row_mut(path) {
            // A skip decision already marked the row; don't overwrite it.
            if !matches!(row.status, StepStatus::Skipped) {
                row.status = if is_error {
                    StepStatus::Err
                } else {
                    StepStatus::Ok
                };
            }
            row.result = Some(result);
        }
    }

    pub fn step_skipped(&mut self, path: &str, injected: Value) {
        let top = path.split('/').next().unwrap_or(path).to_string();
        if let Some(row) = self.row_mut(&top) {
            if top == path {
                row.status = StepStatus::Skipped;
                row.result = Some(injected);
            }
        }
        self.run_log_info(&format!("⊘ {path} skipped (result injected)"));
    }

    pub fn select_next(&mut self) {
        let len = self.list_len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
            self.detail_scroll.set(0);
        }
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.detail_scroll.set(0);
    }

    /// Scroll the pane the current tab shows: the run transcript (offset
    /// from the bottom) or the detail/debug pane (offset from the top).
    pub fn scroll_by(&mut self, up: bool, amount: u16) {
        match self.tab {
            WsTab::Run => {
                let current = self.run_scroll.get();
                self.run_scroll.set(if up {
                    current.saturating_add(amount)
                } else {
                    current.saturating_sub(amount)
                });
            }
            _ => {
                let current = self.detail_scroll.get();
                self.detail_scroll.set(if up {
                    current.saturating_sub(amount)
                } else {
                    current.saturating_add(amount)
                });
            }
        }
    }

    fn list_len(&self) -> usize {
        match self.tab {
            WsTab::Plan => self.steps.len(),
            WsTab::Context => self.tools.len(),
            WsTab::Run => 0,
        }
    }
}

/// Every string in a JSON value tree, for template scanning.
fn gather_template_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| gather_template_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| gather_template_strings(v, out)),
        _ => {}
    }
}
