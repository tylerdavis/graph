//! `graph mcp serve` — graph as an MCP server over stdio.
//!
//! graph is already an MCP *client* (`graph-mcp`). This is the other
//! direction: it exposes the plan pipeline to somebody else's agent, as two
//! populations of tools (see [`catalog`]) — the authoring commands, so an
//! agent can build and check a plan without a shell, and every plan in the
//! catalog as a directly callable tool.
//!
//! ## Why this lives in the binary crate
//!
//! The roadmap called for a separate `graph-mcp-server` crate. It isn't one,
//! because the thing being served *is* the CLI's command layer — `Runtime`,
//! `commands::plan_edit`, `commands::outcome` — and none of that is
//! extractable without turning `graph-cli` into a library first. Serving from
//! here keeps one implementation of every command; a second crate would have
//! meant a second one to keep in sync, which is the exact failure this
//! refactor exists to avoid.
//!
//! ## The stdout rule
//!
//! stdout is the JSON-RPC channel. Nothing in this module may print to it,
//! which is why every command it calls returns an [`Outcome`] instead of
//! writing. Diagnostics go to stderr, where MCP clients conventionally
//! collect them as server logs.

mod catalog;
mod run;

use crate::cli::{PlanAttribute, StepAttribute};
use crate::commands::outcome::Outcome;
use crate::commands::plan_edit;
use crate::runtime::Runtime;
use anyhow::{anyhow, Result};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Serve MCP on stdin/stdout until the client disconnects.
pub async fn serve(dir: Option<PathBuf>) -> Result<()> {
    // MCP clients launch servers with an arbitrary working directory, so the
    // project config layer (`./.graph/`) and the plan catalog would resolve
    // against wherever the client happened to be. `--dir` is how a client
    // says which project it means.
    if let Some(dir) = dir {
        std::env::set_current_dir(&dir)
            .map_err(|e| anyhow!("cannot serve from {}: {e}", dir.display()))?;
    }
    // Fail before the transport opens rather than answering `initialize` and
    // then erroring on every call.
    let _ = Runtime::init()?;
    tracing::info!("graph mcp server ready");
    let service = GraphServer::default()
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone, Default)]
pub struct GraphServer {
    /// Serializes plan writes.
    ///
    /// MCP request handling is concurrent, but plan authoring is
    /// read-modify-write: two `step_add` calls on one plan that interleave
    /// would let the second overwrite the first's step, silently. Clients
    /// normally await each call, so this contends approximately never — it is
    /// here because the failure mode is lost work rather than an error.
    writes: Arc<Mutex<()>>,
}

/// Turn an [`Outcome`] into a tool result.
///
/// A domain rejection becomes `isError: true` *with its body intact* rather
/// than an MCP protocol error: the problem list is the useful part, and a
/// protocol error would reduce it to a string the model has to parse back.
fn outcome_result(outcome: Outcome) -> CallToolResult {
    if outcome.rejected {
        CallToolResult::structured_error(outcome.body)
    } else {
        CallToolResult::structured(outcome.body)
    }
}

/// An argument error — the caller mis-typed. Distinct from a rejection.
fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn args(request: &CallToolRequestParams) -> Map<String, Value> {
    request.arguments.clone().unwrap_or_default()
}

fn required_str(args: &Map<String, Value>, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("`{key}` is required and must be a string")))
}

fn optional_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `set`/`step update` take their value as a string on the command line but
/// as real JSON here — an agent should not have to serialize an object into a
/// string to pass it through. Objects and arrays are re-serialized because
/// that is the form the shared authoring layer parses.
fn value_args(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect(),
        other => vec![other.to_string()],
    }
}

fn plan_attribute(name: &str) -> Result<PlanAttribute, McpError> {
    Ok(match name {
        "name" => PlanAttribute::Name,
        "description" => PlanAttribute::Description,
        "identifier" => PlanAttribute::Identifier,
        "exemplars" => PlanAttribute::Exemplars,
        "requires_servers" => PlanAttribute::RequiresServers,
        "input_schema" => PlanAttribute::InputSchema,
        "solver" => PlanAttribute::Solver,
        "output" => PlanAttribute::Output,
        other => return Err(invalid(format!("unknown plan attribute '{other}'"))),
    })
}

fn step_attribute(name: &str) -> Result<StepAttribute, McpError> {
    Ok(match name {
        "tool" | "tool_name" => StepAttribute::Tool,
        "input" => StepAttribute::Input,
        "reasoning" => StepAttribute::Reasoning,
        other => return Err(invalid(format!("unknown step attribute '{other}'"))),
    })
}

impl GraphServer {
    /// Every tool visible right now: the static authoring verbs, plus one per
    /// plan in the catalog.
    ///
    /// Read fresh on each `tools/list` rather than cached at startup, because
    /// an agent that just wrote a plan with `graph_plan_new` should see it as
    /// a callable tool without restarting the server.
    fn tools(&self) -> Vec<rmcp::model::Tool> {
        let mut tools = catalog::authoring_tools();
        if let Ok(runtime) = Runtime::init() {
            for doc in &runtime.plan_docs().docs {
                tools.push(catalog::plan_tool(doc));
            }
        }
        tools
    }

    async fn dispatch(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.to_string();
        let args = args(&request);
        // Anything that writes a plan file also changes the tool list, since
        // every plan is a tool.
        let mutates = MUTATING.contains(&name.as_str());
        let _guard = if mutates {
            Some(self.writes.lock().await)
        } else {
            None
        };

        // Execution tools first: a plan named after a verb is still a plan.
        if let Some(identifier) = name.strip_prefix(catalog::PLAN_PREFIX) {
            let input = Value::Object(args);
            return run::run_plan(identifier, input)
                .await
                .map(outcome_result)
                .map_err(|error| McpError::internal_error(format!("{error:#}"), None));
        }

        let outcome = match name.as_str() {
            "graph_plan_list" => run::plan_list(),
            "graph_plan_show" => run::plan_show(&required_str(&args, "target")?),
            "graph_plan_validate" => run::plan_validate(&required_str(&args, "target")?),
            "graph_plan_new" => plan_edit::new_plan(
                &required_str(&args, "identifier")?,
                optional_str(&args, "name").as_deref(),
                optional_str(&args, "description").as_deref(),
                None,
            ),
            "graph_plan_draft" => {
                plan_edit::draft(
                    &required_str(&args, "goal")?,
                    optional_str(&args, "from").as_deref(),
                    None,
                    false,
                )
                .await
            }
            "graph_plan_set" => {
                let value = args
                    .get("value")
                    .ok_or_else(|| invalid("`value` is required"))?;
                plan_edit::set(
                    &required_str(&args, "target")?,
                    plan_attribute(&required_str(&args, "attribute")?)?,
                    &value_args(value),
                )
            }
            "graph_plan_unset" => plan_edit::unset(
                &required_str(&args, "target")?,
                plan_attribute(&required_str(&args, "attribute")?)?,
            ),
            "graph_plan_step_add" => run::step_add(&args),
            "graph_plan_step_update" => run::step_update(&args),
            "graph_plan_step_rename" => run::step_rename(&args),
            "graph_plan_step_rm" => run::step_rm(&args),
            "graph_tools_list" => run::tools_list().await,
            "graph_tools_show" => run::tools_show(&required_str(&args, "name")?).await,
            "graph_tools_test" => {
                run::tools_test(
                    &required_str(&args, "name")?,
                    args.get("input").cloned().unwrap_or_else(|| json!({})),
                )
                .await
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool '{other}'"),
                    None,
                ))
            }
        };

        let result = outcome
            .map(outcome_result)
            .map_err(|error| McpError::internal_error(format!("{error:#}"), None))?;

        // A plan the agent just created is a tool it can call next, but only
        // if the client re-reads the list. Fire-and-forget: a client that
        // does not support the notification is not a reason to fail the edit
        // that already succeeded.
        if mutates && !result.is_error.unwrap_or(false) {
            let peer = context.peer.clone();
            tokio::spawn(async move {
                if let Err(error) = peer.notify_tool_list_changed().await {
                    tracing::debug!(%error, "could not send tools/list_changed");
                }
            });
        }
        Ok(result)
    }
}

/// Tools that write a plan file. They take the write lock, and a successful
/// one triggers a `tools/list_changed`.
const MUTATING: &[&str] = &[
    "graph_plan_new",
    "graph_plan_draft",
    "graph_plan_set",
    "graph_plan_unset",
    "graph_plan_step_add",
    "graph_plan_step_update",
    "graph_plan_step_rename",
    "graph_plan_step_rm",
];

impl ServerHandler for GraphServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            // The plan tools change as plans are authored, in this session.
            .enable_tool_list_changed()
            .build();
        info.server_info.name = "graph".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "graph runs plans: validated, reviewable pipelines of tool calls.\n\n\
                 Each `plan_<name>` tool runs one of this machine's plans; call it the way \
                 you would call any tool, using its own input schema.\n\n\
                 The `graph_*` tools author plans. The loop that works: \
                 graph_plan_draft once to get a statically valid plan grounded in the real \
                 tool catalog, then graph_plan_set and graph_plan_step_* to correct it, then \
                 graph_plan_validate for the verdict. Never redraft to fix a plan — drafting \
                 replaces every step, so it discards whatever was already right."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch(request, &context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_values_reach_the_authoring_layer_without_being_stringified() {
        // An agent passes `exemplars` as a real array and `input_schema` as a
        // real object. The CLI takes both as strings, so the bridge has to
        // convert — without wrapping an object in quotes, which would make it
        // parse as a string and fail validation.
        assert_eq!(
            value_args(&json!(["a", "b"])),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(value_args(&json!("plain")), vec!["plain".to_string()]);
        let schema = value_args(&json!({"type": "object"}));
        assert_eq!(schema.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&schema[0]).unwrap(),
            json!({"type": "object"})
        );
    }

    #[test]
    fn a_rejection_is_an_error_result_that_keeps_its_body() {
        // The problem list is the useful part of a rejection. Turning it into
        // an McpError would flatten it to a string the model has to re-parse.
        let result = outcome_result(Outcome::rejected(
            json!({"error": "edit rejected", "problemsIntroduced": ["step E2 references E7"]}),
        ));
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("body survives");
        assert_eq!(
            structured["problemsIntroduced"][0],
            json!("step E2 references E7")
        );
    }

    #[test]
    fn an_ok_outcome_is_not_flagged_as_an_error() {
        let result = outcome_result(Outcome::ok(json!({"ok": true})));
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn unknown_attributes_are_argument_errors() {
        assert!(plan_attribute("nope").is_err());
        assert!(step_attribute("nope").is_err());
        // The YAML spelling and the CLI spelling must both work.
        assert!(plan_attribute("requires_servers").is_ok());
        assert!(step_attribute("tool_name").is_ok());
    }
}
