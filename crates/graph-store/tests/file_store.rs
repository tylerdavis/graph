//! FileStore-specific behavior: durability, corruption handling, and
//! concurrent access through independent store instances on one directory
//! (each instance holds its own file descriptors, so flock contention is
//! real even in-process).

mod common;

use common::MessageLog;
use graph_core::store::Store;
use graph_llm::types::ChatMessage;
use graph_store::FileStore;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;

fn user(content: &str) -> ChatMessage {
    ChatMessage::User {
        content: content.into(),
    }
}

#[tokio::test]
async fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let thread_id = {
        let store = FileStore::open(dir.path()).unwrap();
        let thread = store.create_thread("durable", "chat").await.unwrap();
        store
            .append_messages(&thread.id, &[user("hello")])
            .await
            .unwrap();
        store
            .record_tool_shape("t__x", &json!({"type": "object"}), &json!({}))
            .await
            .unwrap();
        thread.id
    };
    let store = FileStore::open(dir.path()).unwrap();
    assert_eq!(store.load_messages(&thread_id).await.unwrap().len(), 1);
    let meta = store.get_thread(&thread_id).await.unwrap().unwrap();
    assert_eq!(meta.message_count, 1);
    assert_eq!(store.tool_shapes().await.unwrap()[0].tool, "t__x");
}

#[tokio::test]
async fn torn_final_line_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let thread = store.create_thread("torn", "chat").await.unwrap();
    store
        .append_messages(&thread.id, &[user("one"), user("two")])
        .await
        .unwrap();

    // Simulate a partially flushed append: garbage with no closing newline.
    let path = dir
        .path()
        .join("threads")
        .join(&thread.id)
        .join("entries.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"kind\":\"user\",\"content\":\"tru")
        .unwrap();
    drop(file);

    let loaded = store.load_messages(&thread.id).await.unwrap();
    assert_eq!(loaded.len(), 2);
}

#[tokio::test]
async fn mid_file_corruption_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let thread = store.create_thread("corrupt", "chat").await.unwrap();
    store
        .append_messages(&thread.id, &[user("one")])
        .await
        .unwrap();

    let path = dir
        .path()
        .join("threads")
        .join(&thread.id)
        .join("entries.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"not json\n").unwrap();
    drop(file);
    store
        .append_messages(&thread.id, &[user("three")])
        .await
        .unwrap();

    let err = store.load_messages(&thread.id).await.unwrap_err();
    assert!(err.to_string().contains("corrupt line"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_appends_from_separate_instances() {
    const WRITERS: usize = 8;
    const MESSAGES_EACH: usize = 25;

    let dir = tempfile::tempdir().unwrap();
    let thread_id = FileStore::open(dir.path())
        .unwrap()
        .create_thread("contended", "chat")
        .await
        .unwrap()
        .id;

    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let root = dir.path().to_path_buf();
        let thread_id = thread_id.clone();
        handles.push(tokio::spawn(async move {
            let store = FileStore::open(&root).unwrap();
            for m in 0..MESSAGES_EACH {
                store
                    .append_messages(&thread_id, &[user(&format!("w{w} m{m}"))])
                    .await
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let store = FileStore::open(dir.path()).unwrap();
    let loaded = store.load_messages(&thread_id).await.unwrap();
    assert_eq!(loaded.len(), WRITERS * MESSAGES_EACH);
    let meta = store.get_thread(&thread_id).await.unwrap().unwrap();
    assert_eq!(meta.message_count, (WRITERS * MESSAGES_EACH) as i64);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_shape_writes_never_corrupt() {
    const WRITERS: usize = 8;

    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let root = dir.path().to_path_buf();
        handles.push(tokio::spawn(async move {
            let store = FileStore::open(&root).unwrap();
            for _ in 0..10 {
                store
                    .record_tool_shape("hot__tool", &json!({"type": "object"}), &json!({"w": w}))
                    .await
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    // Last-writer-wins: the file must parse and hold a sane count, even if
    // some increments were lost under contention.
    let shapes = FileStore::open(dir.path())
        .unwrap()
        .tool_shapes()
        .await
        .unwrap();
    assert_eq!(shapes.len(), 1);
    assert_eq!(shapes[0].tool, "hot__tool");
    assert!(shapes[0].seen_count >= 1);
    assert!(shapes[0].seen_count <= (WRITERS * 10) as i64);
}

#[tokio::test]
async fn tool_name_encoding_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(FileStore::open(dir.path()).unwrap());
    store
        .record_tool_shape("weird/tool:name", &json!({}), &json!({}))
        .await
        .unwrap();
    let shapes = store.tool_shapes().await.unwrap();
    assert_eq!(shapes.len(), 1);
    assert_eq!(shapes[0].tool, "weird/tool:name");
}

#[tokio::test]
async fn scan_skips_incomplete_thread_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    store.create_thread("real", "chat").await.unwrap();
    // A directory without meta.json (mid-create or mid-delete) is skipped.
    std::fs::create_dir_all(dir.path().join("threads").join("halfmade")).unwrap();
    assert_eq!(store.list_threads().await.unwrap().len(), 1);
}

fn fixture_dir(version: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(version)
}

fn data_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().unwrap() != ".lock" {
                files.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    files.sort();
    files
}

fn copy_fixture(version: &str) -> tempfile::TempDir {
    let from = fixture_dir(version);
    let dir = tempfile::tempdir().unwrap();
    for file in data_files(&from) {
        let to = dir.path().join(&file);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from.join(&file), to).unwrap();
    }
    dir
}

fn contents(path: &std::path::Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).unwrap();
    if path.extension().is_some_and(|ext| ext == "jsonl") {
        return raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    }
    vec![serde_json::from_str(&raw).unwrap_or_else(|_| json!(raw.trim()))]
}

#[test]
fn a_version_1_data_directory_migrates_to_the_frozen_version_2_layout() {
    let dir = copy_fixture("v1");
    FileStore::open(dir.path()).unwrap();
    let expected = fixture_dir("v2");
    assert_eq!(data_files(dir.path()), data_files(&expected));
    for file in data_files(&expected) {
        assert_eq!(
            contents(&dir.path().join(&file)),
            contents(&expected.join(&file)),
            "{}",
            file.display()
        );
    }
}

#[tokio::test]
async fn a_frozen_version_2_data_directory_opens_as_is() {
    let dir = copy_fixture("v2");
    let store = FileStore::open(dir.path()).unwrap();
    let thread = store.get_thread("a1b2c3d4e5f6").await.unwrap().unwrap();
    assert_eq!((thread.owner.as_str(), thread.message_count), ("chat", 4));
    let authors: Vec<String> = store
        .load_entries("a1b2c3d4e5f6")
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.author)
        .collect();
    assert_eq!(authors, ["user", "chat", "chat", "chat"]);
    assert_eq!(store.tool_shapes().await.unwrap().len(), 1);
    assert_eq!(
        data_files(dir.path()),
        data_files(&fixture_dir("v2")),
        "opening a current store writes nothing new"
    );
}
