//! LLM provider abstraction: chat with native tool use, structured output,
//! and streaming across Anthropic, OpenAI, OpenAI-compatible, and Bedrock.

mod error;
mod failover;
mod provider;
pub mod providers;
mod retry;
mod roles;
mod structured;
pub mod types;

pub use error::LlmError;
pub use provider::ChatProvider;
pub use roles::ModelRouter;
