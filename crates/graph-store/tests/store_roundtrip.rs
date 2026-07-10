//! Integration tests for the Ladybug-backed Store.

use graph_core::store::Store;
use graph_llm::types::{ChatMessage, ToolCall};
use graph_store::GraphStore;
use serde_json::json;

fn store(dir: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&dir.path().join("db")).unwrap()
}

#[tokio::test]
async fn thread_lifecycle_and_message_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

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
    assert_eq!(loaded.len(), 4);
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&messages).unwrap()
    );

    // Appending continues the index sequence.
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
    assert_eq!(loaded.len(), 5);
    assert!(matches!(&loaded[4], ChatMessage::User { content } if content == "thanks"));

    let meta = store.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(meta.message_count, 5);
    assert!(meta.updated_at >= meta.created_at);

    assert!(store.delete_thread(&thread.id).await.unwrap());
    assert!(
        !store.delete_thread(&thread.id).await.unwrap(),
        "second delete is a no-op"
    );
    assert!(store.get_thread(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
async fn latest_and_list_order_by_recency() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

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
    assert_eq!(latest.id, a.id);
    let ids: Vec<String> = store
        .list_threads()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![a.id, b.id]);
}

#[tokio::test]
async fn tool_shapes_upsert_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

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
    assert_eq!(shapes.len(), 1);
    assert_eq!(shapes[0].tool, "linear__search");
    assert_eq!(shapes[0].seen_count, 2);
    assert_eq!(shapes[0].example, first);
}

#[tokio::test]
async fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let thread_id = {
        let store = store(&dir);
        let thread = store.create_thread("durable").await.unwrap();
        store
            .append_messages(
                &thread.id,
                &[ChatMessage::User {
                    content: "hello".into(),
                }],
            )
            .await
            .unwrap();
        thread.id
    };
    let store = store(&dir);
    let loaded = store.load_messages(&thread_id).await.unwrap();
    assert_eq!(loaded.len(), 1);
}

#[tokio::test]
async fn bundled_extension_loads_and_works_offline() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    // Loading twice must be harmless (lbug ignores an already-loaded
    // extension; the materialized file is reused).
    store
        .load_extension(graph_store::Extension::Fts)
        .await
        .unwrap();
    store
        .load_extension(graph_store::Extension::Fts)
        .await
        .unwrap();

    store
        .raw_query("CREATE NODE TABLE IF NOT EXISTS Doc(id STRING, body STRING, PRIMARY KEY(id));")
        .await
        .unwrap();
    store
        .raw_query("CREATE (:Doc {id: 'd1', body: 'vendored extensions load by path'});")
        .await
        .unwrap();
    store
        .raw_query("CALL CREATE_FTS_INDEX('Doc', 'doc_fts', ['body']);")
        .await
        .unwrap();
    let rows = store
        .raw_query("CALL QUERY_FTS_INDEX('Doc', 'doc_fts', 'vendored') RETURN node.id;")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "d1");
}
