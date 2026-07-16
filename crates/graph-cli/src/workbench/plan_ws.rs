//! Plan workspace state: the draft document, per-step run status, the
//! context catalog, and the run transcript. Rendering lives in `ui`.

use graph_core::pipeline::body::{parse_branch, Branch};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::{DECIDE_TOOL, MAP_TOOL, MAX_STEP_ATTEMPTS, REDUCE_TOOL};
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

/// What a row represents, and what run-event paths land on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// The tree root: the plan identifier. Never matched by run events.
    Root,
    /// A top-level plan step; matches bare event paths ("E3").
    Step(String),
    /// A named decide branch head ("then"/"else") over an inline step
    /// list — a structural node, never matched by run events.
    BranchHead { step: String, body: String },
    /// A call inside a control-step body. The map item index is stripped
    /// from the body segment so every iteration lands on the same row
    /// ("E3/do.2/E10" → step E3, body "do", body_step E10).
    Body {
        step: String,
        body: String,
        body_step: Option<String>,
    },
    /// The finish stage (solver or output) — driven by synthesizing/
    /// run-finished, never by step events.
    Finish,
}

impl RowKey {
    /// Normalize a run-event path onto a row key.
    pub fn from_path(path: &str) -> Self {
        let mut parts = path.splitn(3, '/');
        let step = parts.next().unwrap_or(path).to_string();
        let Some(body) = parts.next() else {
            return RowKey::Step(step);
        };
        let body = body.split('.').next().unwrap_or(body).to_string();
        RowKey::Body {
            step,
            body,
            body_step: parts.next().map(str::to_string),
        }
    }

    /// The top-level step id this row belongs to (None for the root and
    /// finish rows).
    pub fn top_step(&self) -> Option<&str> {
        match self {
            RowKey::Step(id) => Some(id),
            RowKey::BranchHead { step, .. } => Some(step),
            RowKey::Body { step, .. } => Some(step),
            RowKey::Root | RowKey::Finish => None,
        }
    }
}

pub struct StepRow {
    /// Display id: the step id, a body sub-step id, or the body key
    /// itself ("then", "do") for single-call bodies, or "solver"/"output".
    pub id: String,
    pub tool: String,
    pub reasoning: Option<String>,
    pub input_template: Value,
    pub status: StepStatus,
    pub rendered_input: Option<Value>,
    pub result: Option<Value>,
    /// Structure lives here: `RowKey::Body` rows render under their
    /// branch's colored rail, everything else at the top level.
    pub key: RowKey,
}

pub enum RunLine {
    Info(String),
    Error(String),
}

/// One outline stage of an in-flight incremental draft.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutlineRow {
    pub summary: String,
    pub expected_tool: Option<String>,
}

/// A step the drafting loop has validated and accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRow {
    pub id: String,
    pub tool: String,
    pub reasoning: Option<String>,
}

/// The step currently being drafted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentDraftStep {
    pub index: usize,
    pub summary: String,
    /// The attempt currently in flight (1-based); > 1 means retrying.
    pub attempt: u32,
}

/// Live progress of an incremental `workbench__draft_plan` call, rendered
/// by the plan tab in place of the doc rows. Built purely from events —
/// the shared draft doc is untouched until the final publish.
#[derive(Debug, Default)]
pub struct DraftingProgress {
    pub outline: Vec<OutlineRow>,
    pub accepted: Vec<AcceptedRow>,
    pub current: Option<CurrentDraftStep>,
    /// The last failed attempt's validation problems, if the current step
    /// is retrying.
    pub failed: Option<Vec<String>>,
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
    /// An incremental draft in flight; the plan tab renders this instead
    /// of the doc rows while set.
    pub drafting: Option<DraftingProgress>,
}

impl PlanWorkspace {
    /// Install a (new or replaced) draft: rebuild step rows, clear run state.
    pub fn set_doc(&mut self, doc: PlanDoc) {
        self.steps = step_rows(&doc);
        self.selected = self.selected.min(self.steps.len().saturating_sub(1));
        self.diagnostics.clear();
        self.doc = Some(doc);
        self.outcome = None;
        self.drafting = None;
    }

    /// The outline arrived: show the drafting overlay.
    pub fn begin_drafting(&mut self, outline: Vec<OutlineRow>) {
        self.tab = WsTab::Plan;
        self.drafting = Some(DraftingProgress {
            outline,
            ..Default::default()
        });
    }

    pub fn draft_step_started(&mut self, index: usize, summary: &str) {
        if let Some(drafting) = &mut self.drafting {
            drafting.current = Some(CurrentDraftStep {
                index,
                summary: summary.to_string(),
                attempt: 1,
            });
            drafting.failed = None;
        }
    }

    pub fn draft_step_finished(
        &mut self,
        _index: usize,
        step: serde_json::Value,
        problems: &[String],
        attempt: u32,
    ) {
        let Some(drafting) = &mut self.drafting else {
            return;
        };
        if problems.is_empty() {
            let text = |key: &str| {
                step.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            drafting.accepted.push(AcceptedRow {
                id: text("id"),
                tool: text("toolName"),
                reasoning: step
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
            drafting.current = None;
            drafting.failed = None;
        } else {
            // The next attempt is already being produced — but the final
            // failed attempt has no successor, so clamp at the ceiling
            // rather than show "retry 4/3".
            if let Some(current) = &mut drafting.current {
                current.attempt = (attempt + 1).min(MAX_STEP_ATTEMPTS);
            }
            drafting.failed = Some(problems.to_vec());
        }
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

    /// The row a run-event path lands on: the exact body sub-step row when
    /// one exists, else the owning top-level step's row.
    pub fn find_path(&self, path: &str) -> Option<usize> {
        let key = RowKey::from_path(path);
        self.steps
            .iter()
            .position(|row| row.key == key)
            .or_else(|| {
                let top = RowKey::Step(key.top_step()?.to_string());
                self.steps.iter().position(|row| row.key == top)
            })
    }

    /// Select the row for the given event path, if present.
    pub fn select_path(&mut self, path: &str) {
        if let Some(index) = self.find_path(path) {
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
        // after a draft change mid-session). Only top-level rows: body
        // results are scoped and never enter the results map.
        for row in &mut self.steps {
            let RowKey::Step(id) = &row.key else { continue };
            if row.result.is_none() {
                if let Some(value) = results.get(id) {
                    row.result = Some(value.clone());
                    if matches!(row.status, StepStatus::Pending | StepStatus::Running) {
                        row.status = StepStatus::Ok;
                    }
                }
            }
        }
        // Settle the finish row: a successful run means the solver/output
        // stage completed; on failure it errored only if it had started.
        if let Some(row) = self.steps.iter_mut().find(|row| row.key == RowKey::Finish) {
            if !is_error {
                row.status = StepStatus::Ok;
                if !self.solver_text.is_empty() {
                    row.result = Some(Value::String(self.solver_text.clone()));
                }
            } else if matches!(row.status, StepStatus::Running) {
                row.status = StepStatus::Err;
            }
        }
        if is_error {
            self.run_log_error(headline);
        } else {
            self.run_log_info(headline);
        }
        self.outcome = Some((headline.to_string(), is_error));
    }

    /// The finish stage's LLM call is starting. (A nested plan's solver
    /// reports the same event; the finish row settles at run end either
    /// way, so a briefly running finish row is acceptable.)
    pub fn synthesizing(&mut self) {
        if let Some(row) = self.steps.iter_mut().find(|row| row.key == RowKey::Finish) {
            row.status = StepStatus::Running;
        }
    }

    pub fn run_log_info(&mut self, line: &str) {
        self.run_log.push(RunLine::Info(line.to_string()));
    }

    pub fn run_log_error(&mut self, line: &str) {
        self.run_log.push(RunLine::Error(line.to_string()));
    }

    fn row_mut(&mut self, path: &str) -> Option<&mut StepRow> {
        let key = RowKey::from_path(path);
        self.steps.iter_mut().find(|row| row.key == key)
    }

    pub fn step_started(&mut self, path: &str, input: Value) {
        if let Some(row) = self.row_mut(path) {
            row.status = StepStatus::Running;
            row.rendered_input = Some(input);
        }
    }

    pub fn step_running(&mut self, path: &str) {
        // A gate pause names the call's full path; highlight its exact row
        // when one exists, else the owning top-level step.
        if let Some(index) = self.find_path(path) {
            let row = &mut self.steps[index];
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
        if let Some(row) = self.row_mut(path) {
            row.status = StepStatus::Skipped;
            row.result = Some(injected);
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

    /// Select a specific row (from a mouse click); no-op past the list.
    /// Resets detail scroll, matching keyboard selection.
    pub fn select_to(&mut self, index: usize) {
        if index < self.list_len() {
            self.selected = index;
            self.detail_scroll.set(0);
        }
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

    pub fn list_len(&self) -> usize {
        match self.tab {
            WsTab::Plan => self.steps.len(),
            WsTab::Context => self.tools.len(),
            WsTab::Run => 0,
        }
    }
}

/// Flatten a plan into display rows, `tree`-style: the plan identifier as
/// the root, every top-level step, decide's step-list branches under a
/// named branch head, map/reduce bodies hanging directly beneath their
/// owner, and a final row for the finish stage (solver or output; silent
/// plans get none).
fn step_rows(doc: &PlanDoc) -> Vec<StepRow> {
    let row =
        |id: String, tool: String, reasoning: Option<String>, input: Value, key: RowKey| StepRow {
            id,
            tool,
            reasoning,
            input_template: input,
            status: StepStatus::Pending,
            rendered_input: None,
            result: None,
            key,
        };
    let mut rows = Vec::new();
    rows.push(row(
        doc.identifier.clone(),
        String::new(),
        None,
        serde_json::json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "description": doc.description,
        }),
        RowKey::Root,
    ));
    for step in &doc.steps {
        rows.push(row(
            step.id.clone(),
            step.tool_name.clone(),
            step.reasoning.clone(),
            Value::Object(step.input.clone()),
            RowKey::Step(step.id.clone()),
        ));
        for body in body_keys(&step.tool_name) {
            let Some(raw) = step.input.get(*body) else {
                continue;
            };
            // Invalid bodies get no rows — validation reports them.
            match parse_branch(body, raw) {
                Ok(Branch::Call(call)) => rows.push(row(
                    (*body).to_string(),
                    call.tool_name,
                    call.reasoning,
                    Value::Object(call.input),
                    RowKey::Body {
                        step: step.id.clone(),
                        body: (*body).to_string(),
                        body_step: None,
                    },
                )),
                Ok(Branch::Steps(steps)) => {
                    // Decide branches are named forks — the step list gets
                    // a branch-head node, like a directory in `tree`. Map
                    // and reduce have one anonymous body: no head.
                    if step.tool_name == DECIDE_TOOL {
                        rows.push(row(
                            (*body).to_string(),
                            String::new(),
                            None,
                            raw.clone(),
                            RowKey::BranchHead {
                                step: step.id.clone(),
                                body: (*body).to_string(),
                            },
                        ));
                    }
                    for sub in steps {
                        rows.push(row(
                            sub.id.clone(),
                            sub.tool_name,
                            sub.reasoning,
                            Value::Object(sub.input),
                            RowKey::Body {
                                step: step.id.clone(),
                                body: (*body).to_string(),
                                body_step: Some(sub.id),
                            },
                        ));
                    }
                }
                Err(_) => {}
            }
        }
    }
    if let Some(solver) = &doc.solver {
        let mut input = Map::new();
        input.insert(
            "queryToAnswer".to_string(),
            Value::String(solver.query_to_answer.clone()),
        );
        if !solver.data.is_empty() {
            input.insert("data".to_string(), Value::Object(solver.data.clone()));
        }
        rows.push(row(
            "solver".to_string(),
            "synthesizes the answer".to_string(),
            None,
            Value::Object(input),
            RowKey::Finish,
        ));
    } else if let Some(output) = &doc.output {
        rows.push(row(
            "output".to_string(),
            "renders the output".to_string(),
            None,
            Value::Object(output.clone()),
            RowKey::Finish,
        ));
    }
    rows
}

/// The body slots a control step carries; empty for real tool steps.
fn body_keys(tool: &str) -> &'static [&'static str] {
    match tool {
        DECIDE_TOOL => &["then", "else"],
        MAP_TOOL | REDUCE_TOOL => &["do"],
        _ => &[],
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// map with an inline step-list body, decide with single-call
    /// branches, and a solver finish.
    fn control_doc() -> PlanDoc {
        serde_yaml::from_str(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E1
    tool_name: map
    input:
      over: "{{E0.items}}"
      do:
        - id: E2
          tool_name: t__fetch
          input: { url: "{{item.url}}" }
  - id: E3
    tool_name: decide
    input:
      if: { value: "{{E0.count}}", greaterThan: 0 }
      then: { toolName: t__notify, input: { message: hit } }
      else: { toolName: t__log, input: { message: miss } }
solver:
  query_to_answer: what happened?
"#,
        )
        .unwrap()
    }

    fn workspace(doc: PlanDoc) -> PlanWorkspace {
        let mut ws = PlanWorkspace::default();
        ws.set_doc(doc);
        ws
    }

    #[test]
    fn set_doc_expands_bodies_and_appends_the_finish_row() {
        let ws = workspace(control_doc());
        let rows: Vec<(&str, &str, bool)> = ws
            .steps
            .iter()
            .map(|row| {
                (
                    row.id.as_str(),
                    row.tool.as_str(),
                    matches!(row.key, RowKey::Body { .. }),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("demo", "", false), // root: the plan identifier
                ("E0", "t__search", false),
                ("E1", "map", false),
                ("E2", "t__fetch", true),
                ("E3", "decide", false),
                ("then", "t__notify", true),
                ("else", "t__log", true),
                ("solver", "synthesizes the answer", false),
            ]
        );
        assert_eq!(ws.steps[0].key, RowKey::Root);
        assert_eq!(
            ws.steps[3].key,
            RowKey::Body {
                step: "E1".into(),
                body: "do".into(),
                body_step: Some("E2".into()),
            }
        );
        assert_eq!(ws.steps[7].key, RowKey::Finish);
        assert_eq!(
            ws.steps[7].input_template["queryToAnswer"],
            json!("what happened?")
        );
    }

    #[test]
    fn silent_plans_get_no_finish_row_and_output_plans_get_one() {
        let mut doc = control_doc();
        doc.solver = None;
        assert!(!workspace(doc.clone())
            .steps
            .iter()
            .any(|row| row.key == RowKey::Finish));

        doc.output = Some(
            json!({"count": "{{E0.count}}"})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let ws = workspace(doc);
        let finish = ws.steps.last().unwrap();
        assert_eq!(
            (finish.id.as_str(), finish.tool.as_str()),
            ("output", "renders the output")
        );
    }

    #[test]
    fn body_events_land_on_their_sub_step_rows() {
        let mut ws = workspace(control_doc());
        // Map iterations: every item lands on the one structural row.
        ws.step_started("E1/do.0/E2", json!({"url": "a"}));
        assert_eq!(ws.steps[3].status, StepStatus::Running);
        ws.step_finished("E1/do.0/E2", json!({"ok": 1}), false);
        ws.step_started("E1/do.1/E2", json!({"url": "b"}));
        ws.step_finished("E1/do.1/E2", json!({"ok": 2}), false);
        assert_eq!(ws.steps[3].status, StepStatus::Ok);
        assert_eq!(ws.steps[3].result, Some(json!({"ok": 2})));
        // The owning map row is untouched by body events.
        assert_eq!(ws.steps[2].status, StepStatus::Pending);

        // Single-call decide branch: the path has no body step id.
        ws.step_started("E3/then", json!({"message": "hit"}));
        assert_eq!(ws.steps[5].status, StepStatus::Running);
        ws.step_skipped("E3/then", json!({"sent": false}));
        assert_eq!(ws.steps[5].status, StepStatus::Skipped);
        assert_eq!(ws.steps[5].result, Some(json!({"sent": false})));
    }

    #[test]
    fn find_path_falls_back_to_the_owning_step() {
        let ws = workspace(control_doc());
        assert_eq!(ws.find_path("E1/do.0/E2"), Some(3));
        assert_eq!(ws.find_path("E3/else"), Some(6));
        // An unknown sub-path highlights the owning control step.
        assert_eq!(ws.find_path("E1/do.0/E99"), Some(2));
        assert_eq!(ws.find_path("E99"), None);
    }

    #[test]
    fn drafting_progress_transitions() {
        let mut ws = PlanWorkspace {
            tab: WsTab::Run,
            ..Default::default()
        };
        ws.begin_drafting(vec![
            OutlineRow {
                summary: "search".into(),
                expected_tool: Some("t__search".into()),
            },
            OutlineRow {
                summary: "report".into(),
                expected_tool: None,
            },
        ]);
        assert_eq!(ws.tab, WsTab::Plan, "drafting shows the plan tab");
        assert!(ws.drafting.is_some());

        ws.draft_step_started(0, "search");
        {
            let drafting = ws.drafting.as_ref().unwrap();
            let current = drafting.current.as_ref().unwrap();
            assert_eq!((current.index, current.attempt), (0, 1));
            assert_eq!(current.summary, "search");
        }

        // A failed attempt bumps the in-flight attempt and records why.
        ws.draft_step_finished(
            0,
            json!({"id": "E0", "toolName": "t__bad", "input": {}}),
            &["bad reference".to_string()],
            1,
        );
        {
            let drafting = ws.drafting.as_ref().unwrap();
            assert_eq!(drafting.current.as_ref().unwrap().attempt, 2);
            assert_eq!(
                drafting.failed.as_deref(),
                Some(&["bad reference".to_string()][..])
            );
            assert!(drafting.accepted.is_empty());
        }

        // The final failed attempt has no successor: the displayed attempt
        // clamps at the ceiling rather than overshooting to "retry 4/3".
        ws.draft_step_finished(
            0,
            json!({"id": "E0", "toolName": "t__bad", "input": {}}),
            &["bad reference".to_string()],
            MAX_STEP_ATTEMPTS,
        );
        assert_eq!(
            ws.drafting
                .as_ref()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .attempt,
            MAX_STEP_ATTEMPTS,
        );

        // Acceptance appends the row and clears current/failed.
        ws.draft_step_finished(
            0,
            json!({"id": "E0", "toolName": "t__search", "input": {},
                   "reasoning": "find x"}),
            &[],
            2,
        );
        {
            let drafting = ws.drafting.as_ref().unwrap();
            assert_eq!(drafting.accepted.len(), 1);
            assert_eq!(drafting.accepted[0].id, "E0");
            assert_eq!(drafting.accepted[0].tool, "t__search");
            assert_eq!(drafting.accepted[0].reasoning.as_deref(), Some("find x"));
            assert!(drafting.current.is_none());
            assert!(drafting.failed.is_none());
        }

        // Publishing the finished doc dismisses the overlay.
        ws.set_doc(control_doc());
        assert!(ws.drafting.is_none());
    }

    #[test]
    fn the_finish_row_tracks_synthesis_and_run_end() {
        let mut ws = workspace(control_doc());
        ws.synthesizing();
        assert_eq!(ws.steps[7].status, StepStatus::Running);
        ws.solver_text = "the answer".to_string();
        ws.run_finished("done", false, Map::new());
        assert_eq!(ws.steps[7].status, StepStatus::Ok);
        assert_eq!(ws.steps[7].result, Some(json!("the answer")));

        // A failed run marks a started finish as errored, not an idle one.
        let mut ws = workspace(control_doc());
        ws.run_finished("boom", true, Map::new());
        assert_eq!(ws.steps[7].status, StepStatus::Pending);
        let mut ws = workspace(control_doc());
        ws.synthesizing();
        ws.run_finished("boom", true, Map::new());
        assert_eq!(ws.steps[7].status, StepStatus::Err);
    }
}
