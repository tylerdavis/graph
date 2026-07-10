//! Run state and the control-flow bus.

use super::plan::{Plan, SolverData};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusKind {
    /// Plan defect or tool failure — replan-eligible.
    Error,
    /// The plan was fine but the data ran out — degrade to solver, never replan.
    EmptyData,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEntry {
    pub source: String,
    pub kind: BusKind,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunState {
    pub query: String,
    pub plan: Plan,
    /// Step id → result.
    pub results: Map<String, Value>,
    pub bus: Vec<BusEntry>,
    pub plan_attempts: u32,
    pub solver_data: SolverData,
}

impl RunState {
    /// Steps that have already executed (present in results) — preserved
    /// across replans so the planner continues instead of restarting.
    pub fn executed_steps(&self) -> Plan {
        self.plan
            .iter()
            .filter(|step| self.results.contains_key(&step.id))
            .cloned()
            .collect()
    }

    pub fn next_step_id(&self) -> String {
        let next = self
            .plan
            .iter()
            .filter(|step| self.results.contains_key(&step.id))
            .filter_map(|step| super::plan::step_number(&step.id))
            .map(|n| n + 1)
            .max()
            .unwrap_or(0);
        format!("E{next}")
    }

    /// The step after the last executed one, if any remain.
    pub fn next_pending_step(&self) -> Option<&super::plan::Step> {
        self.plan
            .iter()
            .find(|step| !self.results.contains_key(&step.id))
    }

    /// Number of executed steps (excludes the `input` root).
    pub fn steps_executed(&self) -> usize {
        self.results.keys().filter(|k| *k != "input").count()
    }

    pub fn last_error(&self) -> Option<&BusEntry> {
        self.bus
            .iter()
            .rev()
            .find(|entry| entry.kind == BusKind::Error)
    }

    pub fn push_bus(&mut self, source: &str, kind: BusKind, content: impl Into<String>) {
        self.bus.push(BusEntry {
            source: source.to_string(),
            kind,
            content: content.into(),
        });
    }
}
