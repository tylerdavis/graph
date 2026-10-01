//! LLM provider abstraction: chat with native tool use, structured output,
//! and streaming across Anthropic, OpenAI, OpenAI-compatible, and Bedrock.

pub mod decision;
mod error;
mod failover;
mod metering;
mod provider;
pub mod providers;
mod retry;
mod roles;
mod structured;
pub mod types;

pub use decision::DecisionProvider;
pub use error::LlmError;
pub use metering::{MeteredProvider, ModelCall, UsageMeter};
pub use provider::ChatProvider;
pub use roles::ModelRouter;
