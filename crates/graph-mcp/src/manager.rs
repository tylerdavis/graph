//! MCP server lifecycle: lazy connection, tool discovery, invocation.

use graph_config::McpServerConfig;
use graph_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;

/// Separator between server name and tool name in the planner-visible name.
pub const NAMESPACE_SEPARATOR: &str = "__";

type Client = RunningService<RoleClient, ClientInfo>;

pub struct McpManager {
    servers: BTreeMap<String, ServerHandle>,
}

struct ServerHandle {
    name: String,
    config: McpServerConfig,
    connection: Mutex<Option<Connection>>,
}

struct Connection {
    client: Client,
    tools: Vec<ToolDef>,
}

impl McpManager {
    pub fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            servers: servers
                .into_iter()
                .map(|(name, config)| {
                    let handle = ServerHandle {
                        name: name.clone(),
                        config,
                        connection: Mutex::new(None),
                    };
                    (name, handle)
                })
                .collect(),
        }
    }

    pub fn server_names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// Connect to one server (idempotent) and return its tool list.
    pub async fn connect(&self, server: &str) -> Result<Vec<ToolDef>, ToolError> {
        let handle = self
            .servers
            .get(server)
            .ok_or_else(|| ToolError::Unknown(format!("mcp server '{server}'")))?;
        handle.tools().await
    }

    fn split(name: &str) -> Option<(&str, &str)> {
        name.split_once(NAMESPACE_SEPARATOR)
    }
}

#[async_trait::async_trait]
impl ToolRegistry for McpManager {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut all = Vec::new();
        for handle in self.servers.values() {
            match handle.tools().await {
                Ok(tools) => all.extend(tools),
                Err(e) => {
                    tracing::warn!(server = handle.name, error = %e, "skipping unreachable MCP server");
                }
            }
        }
        Ok(all)
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let (server, tool) =
            Self::split(name).ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        let handle = self
            .servers
            .get(server)
            .ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        handle.call(tool, input).await
    }
}

impl ServerHandle {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let mut guard = self.connection.lock().await;
        if guard.is_none() {
            *guard = Some(self.establish().await?);
        }
        Ok(guard.as_ref().unwrap().tools.clone())
    }

    async fn call(&self, tool: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        {
            let mut guard = self.connection.lock().await;
            if guard.is_none() {
                *guard = Some(self.establish().await?);
            }
        }
        let guard = self.connection.lock().await;
        let connection = guard.as_ref().unwrap();
        let arguments = match input {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(ToolError::Transport(format!(
                    "tool input must be a JSON object, got: {other}"
                )))
            }
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let result = connection
            .client
            .call_tool(params)
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;

        Ok(ToolOutcome {
            result: extract_result(
                result.structured_content.clone(),
                result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect(),
            ),
            is_error: result.is_error.unwrap_or(false),
        })
    }

    async fn establish(&self) -> Result<Connection, ToolError> {
        let info = client_info();
        let client: Client = if let Some(command) = &self.config.command {
            let cmd = tokio::process::Command::new(command);
            let transport = TokioChildProcess::new(cmd.configure(|cmd| {
                cmd.args(&self.config.args);
                for (key, value) in &self.config.env {
                    cmd.env(key, value);
                }
            }))
            .map_err(|e| ToolError::Transport(format!("spawn '{command}': {e}")))?;
            info.serve(transport)
                .await
                .map_err(|e| ToolError::Transport(e.to_string()))?
        } else if let Some(url) = &self.config.url {
            let mut headers = HashMap::new();
            for (key, value) in &self.config.headers {
                let name: http::HeaderName = key
                    .parse()
                    .map_err(|_| ToolError::Transport(format!("invalid header name: {key}")))?;
                let value: http::HeaderValue = value
                    .parse()
                    .map_err(|_| ToolError::Transport(format!("invalid value for header {key}")))?;
                headers.insert(name, value);
            }
            let config =
                StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(headers);
            let transport = StreamableHttpClientTransport::from_config(config);
            info.serve(transport)
                .await
                .map_err(|e| ToolError::Transport(e.to_string()))?
        } else {
            return Err(ToolError::Transport(format!(
                "mcp server '{}' has neither command nor url",
                self.name
            )));
        };

        let mut tools = Vec::new();
        let listed = client
            .list_tools(Default::default())
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;
        for tool in listed.tools {
            let bare_name = tool.name.to_string();
            if !self.exposes(&bare_name) {
                continue;
            }
            let overrides = self.config.tool_overrides.get(&bare_name);
            tools.push(ToolDef {
                name: format!("{}{NAMESPACE_SEPARATOR}{bare_name}", self.name),
                description: overrides
                    .and_then(|o| o.description.clone())
                    .or_else(|| tool.description.as_ref().map(ToString::to_string))
                    .unwrap_or_default(),
                input_schema: Value::Object((*tool.input_schema).clone()),
                output_schema: overrides.and_then(|o| o.output_schema.clone()).or_else(|| {
                    tool.output_schema
                        .as_ref()
                        .map(|s| Value::Object((**s).clone()))
                }),
                output_example: overrides.and_then(|o| o.output_example.clone()),
                read_only: tool.annotations.as_ref().and_then(|a| a.read_only_hint),
            });
        }
        tracing::info!(
            server = self.name,
            tools = tools.len(),
            "connected to MCP server"
        );
        Ok(Connection { client, tools })
    }

    fn exposes(&self, tool: &str) -> bool {
        if let Some(include) = &self.config.include_tools {
            if !include.iter().any(|t| t == tool) {
                return false;
            }
        }
        !self.config.exclude_tools.iter().any(|t| t == tool)
    }
}

/// Prefer structured content; fall back to text parsed as JSON; wrap plain
/// text; join multiple text blocks.
fn extract_result(structured: Option<Value>, texts: Vec<String>) -> Value {
    if let Some(structured) = structured {
        return structured;
    }
    match texts.len() {
        0 => json!({}),
        1 => serde_json::from_str(&texts[0]).unwrap_or_else(|_| json!({ "text": texts[0] })),
        _ => json!({ "text": texts.join("\n") }),
    }
}

fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("graph", env!("CARGO_PKG_VERSION")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prefers_structured_then_json_text_then_wraps() {
        assert_eq!(
            extract_result(Some(json!({"a": 1})), vec!["ignored".into()]),
            json!({"a": 1})
        );
        assert_eq!(
            extract_result(None, vec![r#"{"b": 2}"#.into()]),
            json!({"b": 2})
        );
        assert_eq!(
            extract_result(None, vec!["plain".into()]),
            json!({"text": "plain"})
        );
        assert_eq!(extract_result(None, vec![]), json!({}));
    }

    #[test]
    fn include_exclude_filters_apply() {
        let config = McpServerConfig {
            command: Some("true".into()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            include_tools: Some(vec!["a".into(), "b".into()]),
            exclude_tools: vec!["b".into()],
            tool_overrides: Default::default(),
        };
        let handle = ServerHandle {
            name: "test".into(),
            config,
            connection: Mutex::new(None),
        };
        assert!(handle.exposes("a"));
        assert!(!handle.exposes("b"), "exclude wins over include");
        assert!(!handle.exposes("c"), "not in include list");
    }
}
