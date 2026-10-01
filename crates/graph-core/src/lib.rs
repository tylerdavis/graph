//! Core runtime: the ReAct agent loop and the plan-based execution pipeline.
//!
//! Defines the `ToolRegistry` and `Store` traits implemented by graph-mcp
//! and graph-store respectively.

pub mod agent;
pub mod format;
pub mod pipeline;
pub mod prompts;
pub mod shapes;
pub mod store;
pub mod template;
pub mod toolbox;
pub mod tools;
pub mod usage;
pub mod user_tools;
#[cfg(test)]
mod user_tools_tests;

pub use agent::{Agent, AgentError, EventSink, NullSink, RunStart, TeeSink, TurnOutcome};
pub use store::{
    conversation, message_entries, EntryBody, NewEntry, Store, StoreError, ThreadEntry, ThreadMeta,
    ToolShape, USER_AUTHOR,
};
pub use tools::{
    AllowlistRegistry, CompositeRegistry, ExcludingRegistry, ToolDef, ToolError, ToolOutcome,
    ToolRegistry, ToolServer,
};
pub use usage::{compact_tokens, CallSite, ModelUsage, StepUsage, UsageLedger, UsageReport};
