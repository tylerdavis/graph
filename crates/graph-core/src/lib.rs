//! Core runtime: the ReAct agent loop and the plan-based execution pipeline.
//!
//! Defines the `ToolRegistry` and `Store` traits implemented by graph-mcp
//! and graph-store respectively.

pub mod agent;
pub mod prompts;
pub mod tools;

pub use agent::{Agent, AgentError, EventSink, NullSink, TurnOutcome};
pub use tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
