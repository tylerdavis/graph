//! Asking a human a question, abstracted away from any one host.
//!
//! This is the in-band counterpart to [`super::gate`]. A gate lets an
//! *outside observer* interrupt a run it did not plan; an [`Interlocutor`]
//! lets the plan itself request a value it cannot compute — "which of
//! these three projects did you mean", "approve this before I post it".
//! The [`ask`](super::ask) control step is the only caller.
//!
//! Hosts differ wildly in what "ask a human" means: a TTY prompt, a
//! workbench modal, an MCP `elicitation/create` round trip, or nothing at
//! all in CI. All of them implement this one trait, and a plan that runs
//! against one runs against the others — because the plan declares what
//! happens when nobody answers ([`super::ask::WhenUnanswered`]) rather
//! than assuming a human is present.

use super::gate::StepPath;
use async_trait::async_trait;
use serde_json::Value;

/// A question put to a human.
#[derive(Debug, Clone)]
pub struct AskRequest {
    /// Where the asking step sits in the plan, for display and logging.
    /// Displays with the bus-source syntax ("E3", "E3/do.2").
    pub path: StepPath,
    /// Plan-call nesting; empty at the top level.
    pub call_stack: Vec<String>,
    /// The rendered question. Templates are already resolved.
    pub prompt: String,
    /// JSON Schema the answer must conform to — always `type: object`
    /// with primitive-typed properties (see
    /// [`super::ask::elicitation_schema_problem`]). Hosts that can render
    /// a form should build it from this; hosts that take raw text should
    /// show it so the human knows the shape.
    pub schema: Value,
}

/// How a human responded — or didn't.
#[derive(Debug, Clone)]
pub enum AskOutcome {
    /// An answer. The pipeline validates it against the request's schema,
    /// so an implementation may return unvalidated input.
    Answered(Value),
    /// A human was reachable and refused to answer (an MCP client's
    /// `decline` or `cancel`, an empty TTY response). Distinct from
    /// `Unavailable` because it carries intent: someone said no.
    Declined,
    /// Nobody could be asked — no interlocutor installed, stdin is not a
    /// terminal, the MCP client did not advertise elicitation, or the
    /// transport failed. Carries a short reason for the run log.
    Unavailable(String),
}

/// A host's ability to put a question to a human.
///
/// **Implementations must serialize.** `map` with `concurrency` above 1
/// runs body steps in parallel, so several `ask` steps can land at once;
/// prompting concurrently would interleave two questions on one terminal
/// or clobber a single-slot UI. Hold an internal mutex across the whole
/// exchange, as [`super::gate::ExecutionGate`] implementations do.
///
/// Implementations must not panic and should map transport errors onto
/// [`AskOutcome::Unavailable`] — an unanswerable question is a normal,
/// plan-declared condition, not a pipeline failure.
#[async_trait]
pub trait Interlocutor: Send + Sync {
    async fn ask(&self, request: AskRequest) -> AskOutcome;
}

/// An interlocutor that always declines to be reachable. Installed
/// nowhere — it exists so tests and headless callers can be explicit
/// instead of relying on `None`, which means the same thing.
pub struct Unavailable;

#[async_trait]
impl Interlocutor for Unavailable {
    async fn ask(&self, _request: AskRequest) -> AskOutcome {
        AskOutcome::Unavailable("no interlocutor is installed".to_string())
    }
}
