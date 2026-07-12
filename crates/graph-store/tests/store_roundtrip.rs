//! Store conformance suite: every backend must satisfy the same semantics.
//! Each case runs against `MemoryStore` and `FileStore`.

use graph_core::store::Store;
use graph_llm::types::{ChatMessage, ToolCall};
use graph_store::{FileStore, MemoryStore};
use serde_json::json;
use std::sync::Arc;

fn backends(dir: &tempfile::TempDir) -> Vec<(&'static str, Arc<dyn Store>)> {
    vec![
        ("memory", Arc::new(MemoryStore::new())),
        ("file", Arc::new(FileStore::open(dir.path()).unwrap())),
    ]
}

#[tokio::test]
async fn thread_lifecycle_and_message_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    for (name, store) in backends(&dir) {
        let thread = store.create_thread("first thread").await.unwrap();
        let messages = vec![
            ChatMessage::User {
                content: "list my PRs".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "github__list_prs".into(),
                    arguments: json!({"state": "open"}),
                }],
            },
            ChatMessage::ToolResult {
                tool_call_id: "c1".into(),
                content: json!({"values": [{"id": 1}]}),
                is_error: false,
            },
            ChatMessage::Assistant {
                content: Some("You have 1 PR.".into()),
                tool_calls: vec![],
            },
        ];
        store.append_messages(&thread.id, &messages).await.unwrap();

        // Round-trip preserves order and structure exactly.
        let loaded = store.load_messages(&thread.id).await.unwrap();
        assert_eq!(loaded.len(), 4, "{name}");
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&messages).unwrap(),
            "{name}"
        );

        // Appending continues the sequence.
        store
            .append_messages(
                &thread.id,
                &[ChatMessage::User {
                    content: "thanks".into(),
                }],
            )
            .await
            .unwrap();
        let loaded = store.load_messages(&thread.id).await.unwrap();
        assert_eq!(loaded.len(), 5, "{name}");
        assert!(matches!(&loaded[4], ChatMessage::User { content } if content == "thanks"));

        let meta = store.get_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(meta.message_count, 5, "{name}");
        assert!(meta.updated_at >= meta.created_at, "{name}");

        assert!(store.delete_thread(&thread.id).await.unwrap(), "{name}");
        assert!(
            !store.delete_thread(&thread.id).await.unwrap(),
            "{name}: second delete is a no-op"
        );
        assert!(
            store.get_thread(&thread.id).await.unwrap().is_none(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn append_to_missing_thread_errors() {
    let dir = tempfile::tempdir().unwrap();
    for (name, store) in backends(&dir) {
        let err = store
            .append_messages(
                "nonexistent",
                &[ChatMessage::User {
                    content: "hi".into(),
                }],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nonexistent"), "{name}: {err}");
    }
}

#[tokio::test]
async fn latest_and_list_order_by_recency() {
    let dir = tempfile::tempdir().unwrap();
    for (name, store) in backends(&dir) {
        let a = store.create_thread("a").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let b = store.create_thread("b").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Touching A with a message makes it the most recent.
        store
            .append_messages(
                &a.id,
                &[ChatMessage::User {
                    content: "hi".into(),
                }],
            )
            .await
            .unwrap();

        let latest = store.latest_thread().await.unwrap().unwrap();
        assert_eq!(latest.id, a.id, "{name}");
        let ids: Vec<String> = store
            .list_threads()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![a.id, b.id], "{name}");
    }
}

#[tokio::test]
async fn tool_shapes_upsert_and_count() {
    let dir = tempfile::tempdir().unwrap();
    for (name, store) in backends(&dir) {
        let first = json!({"values": [{"id": "x"}]});
        store
            .record_tool_shape("linear__search", &json!({"type": "object"}), &first)
            .await
            .unwrap();
        store
            .record_tool_shape("linear__search", &json!({"type": "object"}), &first)
            .await
            .unwrap();

        let shapes = store.tool_shapes().await.unwrap();
        assert_eq!(shapes.len(), 1, "{name}");
        assert_eq!(shapes[0].tool, "linear__search", "{name}");
        assert_eq!(shapes[0].seen_count, 2, "{name}");
        assert_eq!(shapes[0].example, first, "{name}");
    }
}
