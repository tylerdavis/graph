//! Core runtime: the ReAct agent loop and the plan-based execution pipeline.
//!
//! Defines the `ToolRegistry` and `Store` traits implemented by graph-mcp
//! and graph-store respectively.

pub mod tools;

pub use tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
