//! The UI-backed execution gate: each tool call parks on a oneshot until
//! the user decides proceed / skip / abort.

use super::app::Msg;
use async_trait::async_trait;
use graph_core::pipeline::{ExecutionGate, GateContext, GateDecision};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

pub struct UiGate {
    pub tx: UnboundedSender<Msg>,
}

#[async_trait]
impl ExecutionGate for UiGate {
    async fn before_tool(&self, ctx: GateContext<'_>) -> GateDecision {
        let (reply, receiver) = oneshot::channel();
        let path = if ctx.call_stack.is_empty() {
            ctx.path.to_string()
        } else {
            format!("{}→{}", ctx.call_stack.join("→"), ctx.path)
        };
        let sent = self.tx.send(Msg::GateAsk {
            path,
            tool: ctx.tool_name.to_string(),
            input: ctx.rendered_input.clone(),
            reply,
        });
        if sent.is_err() {
            // UI is gone — end the run rather than free-running.
            return GateDecision::Abort;
        }
        receiver.await.unwrap_or(GateDecision::Abort)
    }
}
