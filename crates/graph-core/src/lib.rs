//! Core runtime: the ReAct agent loop and the plan-based execution pipeline.
//!
//! Defines the `ToolRegistry` and `Store` traits implemented by graph-mcp
//! and graph-store respectively.

pub mod agent;
pub mod pipeline;
pub mod prompts;
pub mod shapes;
pub mod store;
pub mod template;
pub mod toolbox;
pub mod tools;
pub mod user_tools;
#[cfg(test)]
mod user_tools_tests;

pub use agent::{Agent, AgentError, EventSink, NullSink, TurnOutcome};
pub use store::{Store, StoreError, ThreadMeta, ToolShape};
pub use tools::{
    CompositeRegistry, ExcludingRegistry, ToolDef, ToolError, ToolOutcome, ToolRegistry,
};
