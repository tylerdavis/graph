mod chat;

pub const TOOL_FORMAT: &str = include_str!("tool_format.md");

pub const AGENT_FORMAT: &str = include_str!("agent_format.md");

pub use chat::{chat_system_prompt, user_context_section, DEFAULT_CHAT_PROMPT};
