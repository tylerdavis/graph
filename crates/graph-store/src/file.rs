//! File-backed `Store`: plain JSON/JSONL files under the data directory.
//!
//! Layout:
//!
//! ```text
//! <root>/threads/<id>/meta.json       thread metadata (atomic rename writes)
//! <root>/threads/<id>/entries.jsonl   one ThreadEntry per line (O_APPEND)
//! <root>/threads/<id>/.lock           advisory lock for append+meta updates
//! <root>/shapes/<tool>.json           one file per tool shape (atomic rename)
//! ```
//!
//! Concurrency model: whole files are written to a temp file and renamed
//! into place, so readers never observe partial writes. Message appends go
//! through `O_APPEND` as a single write, serialized across processes by an
//! exclusive flock on the thread's `.lock` file so `meta.json` stays
//! consistent with the log. Shape writes are last-writer-wins; `seen_count`
//! is advisory and may lose increments under contention. Advisory locks
//! assume a local filesystem (flock over NFS is unreliable).

use fs4::fs_std::FileExt;
use graph_core::store::{
    message_entries, EntryBody, NewEntry, Store, StoreError, ThreadEntry, ThreadMeta, ToolShape,
};
use graph_llm::types::ChatMessage;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const STORE_FORMAT: u32 = 1;

pub const STORE_FORMAT_OLDEST: u32 = 1;

const FORMAT_VERSION: u32 = STORE_FORMAT;

const FORMAT_MARKER: &str = "FORMAT";

const ENTRIES_FILE: &str = "entries.jsonl";

const LEGACY_MESSAGES_FILE: &str = "messages.jsonl";

const LEGACY_AGENT: &str = "chat";

pub struct FileStore {
    threads_dir: PathBuf,
    shapes_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct MetaFile {
    version: u32,
    id: String,
    title: String,
    created_at: i64,
    updated_at: i64,
    message_count: i64,
    #[serde(default)]
    entry_count: u64,
    #[serde(default = "legacy_agent")]
    owner: String,
    #[serde(default = "legacy_agent")]
    active: String,
}

fn legacy_agent() -> String {
    LEGACY_AGENT.to_string()
}

impl From<MetaFile> for ThreadMeta {
    fn from(m: MetaFile) -> Self {
        ThreadMeta {
            id: m.id,
            title: m.title,
            created_at: m.created_at,
            updated_at: m.updated_at,
            message_count: m.message_count,
            owner: m.owner,
            active: m.active,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ShapeFile {
    version: u32,
    tool: String,
    schema: Value,
    example: Value,
    seen_count: i64,
    updated_at: i64,
}

impl FileStore {
    /// Open (creating if needed) a file store rooted at the data directory.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let threads_dir = root.join("threads");
        let shapes_dir = root.join("shapes");
        for dir in [&threads_dir, &shapes_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError(format!("creating {}: {e}", dir.display())))?;
        }
        check_format_marker(root)?;
        Ok(Self {
            threads_dir,
            shapes_dir,
        })
    }

    fn thread_dir(&self, id: &str) -> PathBuf {
        self.threads_dir.join(id)
    }

    /// Run blocking filesystem work off the async runtime; lock waits and
    /// directory scans must not stall other tasks (map steps run buffered).
    async fn blocking<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|e| StoreError(format!("store task failed: {e}")))?
    }
}

pub fn marker_format(root: &Path) -> Result<Option<u32>, StoreError> {
    let path = root.join(FORMAT_MARKER);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError(format!("reading {}: {e}", path.display()))),
    };
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 1)
        .map(Some)
        .ok_or_else(|| {
            StoreError(format!(
                "{} does not hold a store version number (got {:?})",
                path.display(),
                raw.trim()
            ))
        })
}

fn check_format_marker(root: &Path) -> Result<(), StoreError> {
    match marker_format(root)? {
        Some(found) => match graph_config::window_problem(
            "store",
            &format_args!("data directory {}", root.display()),
            found,
            STORE_FORMAT_OLDEST,
            STORE_FORMAT,
        ) {
            Some(problem) => Err(StoreError(problem)),
            None => Ok(()),
        },
        None => match link_new(root, format!("{STORE_FORMAT}\n").as_bytes()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => check_format_marker(root),
            Err(e) => Err(StoreError(format!(
                "writing {}: {e}",
                root.join(FORMAT_MARKER).display()
            ))),
        },
    }
}

fn link_new(root: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(root)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    std::fs::hard_link(tmp.path(), root.join(FORMAT_MARKER))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn new_thread_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, then rename over the destination.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError(format!("no parent dir for {}", path.display())))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| StoreError(format!("creating temp file in {}: {e}", dir.display())))?;
    tmp.write_all(bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| StoreError(format!("writing {}: {e}", path.display())))?;
    tmp.persist(path)
        .map_err(|e| StoreError(format!("renaming into {}: {e}", path.display())))?;
    Ok(())
}

fn read_meta(dir: &Path) -> Result<Option<MetaFile>, StoreError> {
    let path = dir.join("meta.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError(format!("reading {}: {e}", path.display()))),
    };
    let meta: MetaFile = serde_json::from_str(&raw)
        .map_err(|e| StoreError(format!("corrupt {}: {e}", path.display())))?;
    Ok(Some(meta))
}

fn write_meta(dir: &Path, meta: &MetaFile) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|e| StoreError(format!("serializing thread meta: {e}")))?;
    write_atomic(&dir.join("meta.json"), &bytes)
}

/// Take the per-thread exclusive advisory lock. Released when the returned
/// file handle drops (flock releases on close).
fn lock_thread(dir: &Path) -> Result<File, StoreError> {
    let path = dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| StoreError(format!("opening {}: {e}", path.display())))?;
    file.lock_exclusive()
        .map_err(|e| StoreError(format!("locking {}: {e}", path.display())))?;
    Ok(file)
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Option<Vec<T>>, StoreError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError(format!("reading {}: {e}", path.display()))),
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut items = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<T>(line) {
            Ok(item) => items.push(item),
            // A torn final line is a partially flushed append; drop it.
            // Corruption anywhere else is a real error.
            Err(e) if i == lines.len() - 1 => {
                tracing::warn!("dropping torn final line in {}: {e}", path.display());
            }
            Err(e) => {
                return Err(StoreError(format!(
                    "corrupt line {} in {}: {e}",
                    i + 1,
                    path.display()
                )))
            }
        }
    }
    Ok(Some(items))
}

fn legacy_entries(dir: &Path, at: i64) -> Result<Option<Vec<ThreadEntry>>, StoreError> {
    let Some(messages) = read_jsonl::<ChatMessage>(&dir.join(LEGACY_MESSAGES_FILE))? else {
        return Ok(None);
    };
    Ok(Some(
        message_entries(LEGACY_AGENT, &messages)
            .into_iter()
            .enumerate()
            .map(|(seq, entry)| ThreadEntry {
                seq: seq as u64,
                at,
                author: entry.author,
                body: entry.body,
            })
            .collect(),
    ))
}

fn entry_lines(entries: &[ThreadEntry]) -> Result<Vec<u8>, StoreError> {
    let mut buf = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut buf, entry)
            .map_err(|e| StoreError(format!("serializing thread entry: {e}")))?;
        buf.push(b'\n');
    }
    Ok(buf)
}

fn upgrade_legacy_log(dir: &Path, meta: &mut MetaFile) -> Result<(), StoreError> {
    if dir.join(ENTRIES_FILE).exists() {
        return Ok(());
    }
    let Some(entries) = legacy_entries(dir, meta.created_at)? else {
        return Ok(());
    };
    write_atomic(&dir.join(ENTRIES_FILE), &entry_lines(&entries)?)?;
    meta.entry_count = entries.len() as u64;
    write_meta(dir, meta)?;
    std::fs::remove_file(dir.join(LEGACY_MESSAGES_FILE)).map_err(|e| {
        StoreError(format!(
            "removing {}: {e}",
            dir.join(LEGACY_MESSAGES_FILE).display()
        ))
    })
}

fn scan_threads(threads_dir: &Path) -> Result<Vec<ThreadMeta>, StoreError> {
    let entries = match std::fs::read_dir(threads_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(StoreError(format!(
                "reading {}: {e}",
                threads_dir.display()
            )))
        }
    };
    let mut threads: Vec<ThreadMeta> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.path().is_dir() {
            continue;
        }
        // A vanished or unreadable meta.json (mid-delete, mid-create) is
        // skipped rather than failing the whole listing.
        match read_meta(&entry.path()) {
            Ok(Some(meta)) => threads.push(meta.into()),
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping thread dir {}: {e}", entry.path().display()),
        }
    }
    threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    Ok(threads)
}

/// Encode a tool name into a filename: bytes outside `[A-Za-z0-9_.-]` are
/// percent-encoded. The authoritative name lives inside the file.
fn encode_tool_filename(tool: &str) -> String {
    let mut out = String::with_capacity(tool.len() + 5);
    for byte in tool.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out.push_str(".json");
    out
}

#[async_trait::async_trait]
impl Store for FileStore {
    async fn create_thread(&self, title: &str, owner: &str) -> Result<ThreadMeta, StoreError> {
        let title = title.to_string();
        let owner = owner.to_string();
        let threads_dir = self.threads_dir.clone();
        self.blocking(move || {
            let id = new_thread_id();
            let dir = threads_dir.join(&id);
            std::fs::create_dir_all(&dir)
                .map_err(|e| StoreError(format!("creating {}: {e}", dir.display())))?;
            let now = now_ms();
            let meta = MetaFile {
                version: FORMAT_VERSION,
                id,
                title,
                created_at: now,
                updated_at: now,
                message_count: 0,
                entry_count: 0,
                active: owner.clone(),
                owner,
            };
            write_meta(&dir, &meta)?;
            Ok(meta.into())
        })
        .await
    }

    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError> {
        let dir = self.thread_dir(id);
        self.blocking(move || Ok(read_meta(&dir)?.map(Into::into)))
            .await
    }

    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError> {
        let threads_dir = self.threads_dir.clone();
        self.blocking(move || Ok(scan_threads(&threads_dir)?.into_iter().next()))
            .await
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError> {
        let threads_dir = self.threads_dir.clone();
        self.blocking(move || scan_threads(&threads_dir)).await
    }

    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError> {
        let dir = self.thread_dir(id);
        self.blocking(move || match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StoreError(format!("deleting {}: {e}", dir.display()))),
        })
        .await
    }

    async fn append_entries(
        &self,
        thread_id: &str,
        entries: &[NewEntry],
    ) -> Result<(), StoreError> {
        let thread_id = thread_id.to_string();
        let dir = self.thread_dir(&thread_id);
        let entries = entries.to_vec();
        self.blocking(move || {
            read_meta(&dir)?.ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
            let _lock = lock_thread(&dir)?;
            let mut meta =
                read_meta(&dir)?.ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
            upgrade_legacy_log(&dir, &mut meta)?;
            let at = now_ms();
            let numbered: Vec<ThreadEntry> = entries
                .into_iter()
                .enumerate()
                .map(|(i, entry)| ThreadEntry {
                    seq: meta.entry_count + i as u64,
                    at,
                    author: entry.author,
                    body: entry.body,
                })
                .collect();
            let path = dir.join(ENTRIES_FILE);
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| StoreError(format!("opening {}: {e}", path.display())))?;
            file.write_all(&entry_lines(&numbered)?)
                .map_err(|e| StoreError(format!("appending to {}: {e}", path.display())))?;
            meta.entry_count += numbered.len() as u64;
            meta.message_count += numbered
                .iter()
                .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
                .count() as i64;
            meta.updated_at = meta.updated_at.max(at);
            write_meta(&dir, &meta)
        })
        .await
    }

    async fn load_entries(&self, thread_id: &str) -> Result<Vec<ThreadEntry>, StoreError> {
        let dir = self.thread_dir(thread_id);
        self.blocking(move || {
            if let Some(entries) = read_jsonl::<ThreadEntry>(&dir.join(ENTRIES_FILE))? {
                return Ok(entries);
            }
            let created_at = read_meta(&dir)?.map(|meta| meta.created_at).unwrap_or(0);
            Ok(legacy_entries(&dir, created_at)?.unwrap_or_default())
        })
        .await
    }

    async fn set_active_agent(&self, thread_id: &str, agent: &str) -> Result<(), StoreError> {
        let thread_id = thread_id.to_string();
        let agent = agent.to_string();
        let dir = self.thread_dir(&thread_id);
        self.blocking(move || {
            read_meta(&dir)?.ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
            let _lock = lock_thread(&dir)?;
            let mut meta =
                read_meta(&dir)?.ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
            meta.active = agent;
            meta.updated_at = meta.updated_at.max(now_ms());
            write_meta(&dir, &meta)
        })
        .await
    }

    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError> {
        let tool = tool.to_string();
        let schema = schema.clone();
        let example = example.clone();
        let path = self.shapes_dir.join(encode_tool_filename(&tool));
        self.blocking(move || {
            // Last-writer-wins by design; a corrupt or missing file just
            // starts the count over.
            let seen_count = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<ShapeFile>(&raw).ok())
                .map(|s| s.seen_count)
                .unwrap_or(0);
            let shape = ShapeFile {
                version: FORMAT_VERSION,
                tool,
                schema,
                example,
                seen_count: seen_count + 1,
                updated_at: now_ms(),
            };
            let bytes = serde_json::to_vec_pretty(&shape)
                .map_err(|e| StoreError(format!("serializing tool shape: {e}")))?;
            write_atomic(&path, &bytes)
        })
        .await
    }

    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
        let shapes_dir = self.shapes_dir.clone();
        self.blocking(move || {
            let entries = match std::fs::read_dir(&shapes_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(StoreError(format!("reading {}: {e}", shapes_dir.display()))),
            };
            let mut shapes = Vec::new();
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let raw = match std::fs::read_to_string(&path) {
                    Ok(raw) => raw,
                    Err(e) => {
                        tracing::warn!("skipping shape {}: {e}", path.display());
                        continue;
                    }
                };
                match serde_json::from_str::<ShapeFile>(&raw) {
                    Ok(shape) => shapes.push(ToolShape {
                        tool: shape.tool,
                        schema: shape.schema,
                        example: shape.example,
                        seen_count: shape.seen_count,
                    }),
                    Err(e) => tracing::warn!("skipping shape {}: {e}", path.display()),
                }
            }
            shapes.sort_by(|a, b| a.tool.cmp(&b.tool));
            Ok(shapes)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_writes_the_format_marker_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(marker_format(dir.path()).unwrap(), None);
        FileStore::open(dir.path()).unwrap();
        assert_eq!(marker_format(dir.path()).unwrap(), Some(STORE_FORMAT));
        let marker = dir.path().join(FORMAT_MARKER);
        let written = std::fs::read_to_string(&marker).unwrap();
        FileStore::open(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), written);
    }

    #[test]
    fn concurrent_first_opens_agree_on_one_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || FileStore::open(&root).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(marker_format(&root).unwrap(), Some(STORE_FORMAT));
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != FORMAT_MARKER && name != "threads" && name != "shapes")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn open_refuses_a_newer_store_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FORMAT_MARKER),
            format!("{}\n", STORE_FORMAT + 1),
        )
        .unwrap();
        let err = FileStore::open(dir.path())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("is store version {}", STORE_FORMAT + 1)),
            "{err}"
        );
        assert!(
            err.contains(&format!("reads store version {STORE_FORMAT}")),
            "{err}"
        );
        std::fs::write(dir.path().join(FORMAT_MARKER), "banana\n").unwrap();
        let err = FileStore::open(dir.path())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not hold a store version number"),
            "{err}"
        );
    }

    fn legacy_thread(root: &Path) -> String {
        let dir = root.join("threads").join("legacy00001");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"version":1,"id":"legacy00001","title":"old","created_at":5,"updated_at":6,"message_count":2}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(LEGACY_MESSAGES_FILE),
            "{\"kind\":\"user\",\"content\":\"hi\"}\n{\"kind\":\"assistant\",\"content\":\"hello\"}\n",
        )
        .unwrap();
        "legacy00001".to_string()
    }

    #[tokio::test]
    async fn a_legacy_message_log_reads_as_chat_entries_and_upgrades_on_append() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::open(root.path()).unwrap();
        let id = legacy_thread(root.path());

        let meta = store.get_thread(&id).await.unwrap().unwrap();
        assert_eq!(
            (meta.owner.as_str(), meta.active.as_str()),
            ("chat", "chat")
        );
        let entries = store.load_entries(&id).await.unwrap();
        let authors: Vec<&str> = entries.iter().map(|e| e.author.as_str()).collect();
        assert_eq!(authors, ["user", "chat"]);

        store
            .append_entries(
                &id,
                &[NewEntry::message(
                    "user",
                    ChatMessage::User {
                        content: "again".to_string(),
                    },
                )],
            )
            .await
            .unwrap();
        let dir = root.path().join("threads").join(&id);
        assert!(!dir.join(LEGACY_MESSAGES_FILE).exists());
        let entries = store.load_entries(&id).await.unwrap();
        let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [0, 1, 2]);
        assert_eq!(
            store.get_thread(&id).await.unwrap().unwrap().message_count,
            3
        );
    }

    #[tokio::test]
    async fn entries_keep_their_authors_order_and_kinds() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::open(root.path()).unwrap();
        let meta = store.create_thread("t", "orchestrator").await.unwrap();
        assert_eq!(
            (meta.owner.as_str(), meta.active.as_str()),
            ("orchestrator", "orchestrator")
        );
        store
            .append_entries(
                &meta.id,
                &[
                    NewEntry::message(
                        "user",
                        ChatMessage::User {
                            content: "go".to_string(),
                        },
                    ),
                    NewEntry {
                        author: "orchestrator".to_string(),
                        body: EntryBody::Handoff {
                            from: "orchestrator".to_string(),
                            to: "plan_author".to_string(),
                            message: "draft it".to_string(),
                            via: None,
                        },
                    },
                ],
            )
            .await
            .unwrap();
        store
            .set_active_agent(&meta.id, "plan_author")
            .await
            .unwrap();
        store
            .append_entries(
                &meta.id,
                &[NewEntry {
                    author: "plan_author".to_string(),
                    body: EntryBody::SubagentRun {
                        agent: "plan_refiner".to_string(),
                        caller: "plan_author".to_string(),
                        input: serde_json::json!({"goal": "g"}),
                        messages: Vec::new(),
                        output: serde_json::json!({"changes": []}),
                        final_: true,
                    },
                }],
            )
            .await
            .unwrap();

        let entries = store.load_entries(&meta.id).await.unwrap();
        let summary: Vec<(u64, &str)> =
            entries.iter().map(|e| (e.seq, e.author.as_str())).collect();
        assert_eq!(
            summary,
            [(0, "user"), (1, "orchestrator"), (2, "plan_author")]
        );
        assert!(matches!(entries[2].body, EntryBody::SubagentRun { .. }));
        let meta = store.get_thread(&meta.id).await.unwrap().unwrap();
        assert_eq!(meta.active, "plan_author");
        assert_eq!(meta.owner, "orchestrator");
        assert_eq!(meta.message_count, 1);
    }

    #[tokio::test]
    async fn concurrent_appends_number_entries_without_gaps() {
        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FileStore::open(root.path()).unwrap());
        let meta = store.create_thread("t", "chat").await.unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                let id = meta.id.clone();
                tokio::spawn(async move {
                    store
                        .append_entries(
                            &id,
                            &[NewEntry::message(
                                "user",
                                ChatMessage::User {
                                    content: format!("m{i}"),
                                },
                            )],
                        )
                        .await
                        .unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        let mut seqs: Vec<u64> = store
            .load_entries(&meta.id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        seqs.sort();
        assert_eq!(seqs, (0..8).collect::<Vec<u64>>());
    }

    #[test]
    fn tool_filename_encoding() {
        assert_eq!(encode_tool_filename("user__git_log"), "user__git_log.json");
        assert_eq!(encode_tool_filename("a/b:c"), "a%2Fb%3Ac.json");
        assert_eq!(encode_tool_filename("café"), "caf%C3%A9.json");
    }
}
