//! The `exit` escape hatch: a step that ends the plan early with a
//! success or error state, gated by a logical condition (`when`) or an
//! inferred verdict (`infer`). Intercepted by the executor — never
//! dispatched to a tool registry, and skips the solver entirely (an
//! escape hatch is a zero-inference exit, except the verdict call itself).

use super::condition::evaluate_gate;
pub use super::condition::{Condition, Op, Verdict};
use graph_llm::ModelRouter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Reserved step tool name.
pub const EXIT_TOOL: &str = "exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Success,
    Error,
}

/// A triggered exit, carried through the pipeline outcome.
#[derive(Debug, Clone, Serialize)]
pub struct PlanExit {
    pub status: ExitStatus,
    pub message: String,
    /// The model's reasoning, for inferred exits.
    pub reason: Option<String>,
    /// Rendered output map, when the step declared one.
    pub output: Option<Map<String, Value>>,
    /// The step id that exited.
    pub step: String,
}

/// The exit step's input, parsed *after* template rendering.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitSpec {
    /// Logical gate. Mutually exclusive with `infer`; omit both for an
    /// unconditional exit.
    pub when: Option<Condition>,
    /// Inferred gate: a yes/no question answered by the `judge` model
    /// role with a structured verdict.
    pub infer: Option<String>,
    pub status: ExitStatus,
    #[serde(default)]
    pub message: Option<String>,
    /// Emitted as the plan's structured output when the exit triggers.
    #[serde(default)]
    pub output: Option<Map<String, Value>>,
}

/// The exit step as described to the planner.
pub fn exit_tool_def() -> crate::tools::ToolDef {
    crate::tools::ToolDef {
        name: EXIT_TOOL.to_string(),
        description: "End the plan early with a success or error state. Use it instead of \
                      fabricating results: exit success when there is legitimately nothing to \
                      do (e.g. a search returned nothing actionable), exit error to assert a \
                      failure condition. Gate it with `when` (a logical comparison) or `infer` \
                      (a yes/no question judged against prior results); omit both to exit \
                      unconditionally. When the gate does not fire, the plan continues."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["status"],
            "properties": {
                "status": {"type": "string", "enum": ["success", "error"]},
                "message": {"type": "string", "description": "Shown as the plan's answer / error message. May use templates."},
                "when": {
                    "type": "object",
                    "required": ["value", "op"],
                    "properties": {
                        "value": {"description": "Usually a template like {{E0.issues.length}}"},
                        "op": {"type": "string", "enum": ["eq","ne","gt","lt","gte","lte","empty","not_empty","contains"]},
                        "to": {"description": "Comparison operand (omit for empty/not_empty)"}
                    }
                },
                "infer": {"type": "string", "description": "A yes/no question about prior results; exits when the answer is yes."}
            }
        }),
        output_schema: None,
        output_example: Some(json!({"passed": true, "verdict": false, "reason": "…"})),
        read_only: Some(true),
    }
}

/// How an evaluated exit step resolved.
pub enum ExitEval {
    /// Gate did not fire; the plan continues. The value becomes the step's
    /// result (so later steps can reference e.g. `{{Ex.reason}}`).
    Passed(Value),
    /// Gate fired; the plan ends.
    Exited(PlanExit),
}

pub async fn evaluate(
    step_id: &str,
    rendered_input: &Value,
    router: &ModelRouter,
) -> Result<ExitEval, String> {
    let spec: ExitSpec = serde_json::from_value(rendered_input.clone())
        .map_err(|e| format!("invalid exit step input: {e}"))?;

    let (triggered, reason) = match (&spec.when, &spec.infer) {
        (None, None) => (true, None), // unconditional
        (when, infer) => evaluate_gate(when.as_ref(), infer.as_deref(), router)
            .await
            .map_err(|e| format!("exit step: {e}"))?,
    };

    if !triggered {
        return Ok(ExitEval::Passed(json!({
            "passed": true,
            "verdict": false,
            "reason": reason,
        })));
    }

    let mut message = spec.message.unwrap_or_else(|| match spec.status {
        ExitStatus::Success => "plan exited early".to_string(),
        ExitStatus::Error => "plan asserted failure".to_string(),
    });
    if let Some(reason) = &reason {
        message = format!("{message} ({reason})");
    }
    Ok(ExitEval::Exited(PlanExit {
        status: spec.status,
        message,
        reason,
        output: spec.output,
        step: step_id.to_string(),
    }))
}
