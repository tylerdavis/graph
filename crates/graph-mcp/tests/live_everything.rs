//! Live integration test against @modelcontextprotocol/server-everything.
//! Requires npx; run explicitly: cargo test -p graph-mcp -- --ignored

use graph_core::ToolRegistry;
use graph_mcp::McpManager;
use serde_json::json;
use std::collections::BTreeMap;

fn manager() -> McpManager {
    let config: graph_config::McpServerConfig = toml::from_str(
        r#"
        command = "npx"
        args = ["-y", "@modelcontextprotocol/server-everything"]
        "#,
    )
    .unwrap();
    McpManager::new(BTreeMap::from([("everything".to_string(), config)]))
}

#[tokio::test]
#[ignore = "requires npx and network"]
async fn discovers_and_invokes_tools() {
    let manager = manager();

    let tools = manager.tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "everything__echo"));

    // Text result parses through the text→JSON fallback.
    let echoed = manager
        .invoke("everything__echo", json!({"message": "hello graph"}))
        .await
        .unwrap();
    assert!(!echoed.is_error);
    assert!(echoed.result.to_string().contains("hello graph"));

    // structuredContent is preferred when the server provides it.
    let structured = manager
        .invoke(
            "everything__get-structured-content",
            json!({"location": "Chicago"}),
        )
        .await
        .unwrap();
    assert!(!structured.is_error);
    assert!(
        structured.result.is_object() && structured.result.get("text").is_none(),
        "expected structured object, got: {}",
        structured.result
    );

    // Unknown tools error cleanly.
    if let Ok(outcome) = manager.invoke("everything__nope", json!({})).await {
        assert!(outcome.is_error)
    }
}
