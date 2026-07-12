//! File-backed `Store`: plain JSON/JSONL files under the data directory.
//!
//! Layout:
//!
//! ```text
//! <root>/threads/<id>/meta.json       thread metadata (atomic rename writes)
//! <root>/threads/<id>/messages.jsonl  one ChatMessage per line (O_APPEND)
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
use graph_core::store::{Store, StoreError, ThreadMeta, ToolShape};
use graph_llm::types::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u32 = 1;

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
}

impl From<MetaFile> for ThreadMeta {
    fn from(m: MetaFile) -> Self {
        ThreadMeta {
            id: m.id,
            title: m.title,
            created_at: m.created_at,
            updated_at: m.updated_at,
            message_count: m.message_count,
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
    async fn create_thread(&self, title: &str) -> Result<ThreadMeta, StoreError> {
        let title = title.to_string();
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

    async fn append_messages(
        &self,
        thread_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), StoreError> {
        let thread_id = thread_id.to_string();
        let dir = self.thread_dir(&thread_id);
        // Serialize the whole batch up front: one buffer, one append write.
        let mut buf = Vec::new();
        for message in messages {
            serde_json::to_writer(&mut buf, message)
                .map_err(|e| StoreError(format!("serializing message: {e}")))?;
            buf.push(b'\n');
        }
        let count = messages.len() as i64;
        self.blocking(move || {
            let mut meta =
                read_meta(&dir)?.ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
            let _lock = lock_thread(&dir)?;
            // Re-read under the lock: another process may have appended
            // between the existence check and lock acquisition.
            meta = read_meta(&dir)?.unwrap_or(meta);
            let path = dir.join("messages.jsonl");
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| StoreError(format!("opening {}: {e}", path.display())))?;
            file.write_all(&buf)
                .map_err(|e| StoreError(format!("appending to {}: {e}", path.display())))?;
            meta.message_count += count;
            meta.updated_at = meta.updated_at.max(now_ms());
            write_meta(&dir, &meta)
        })
        .await
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Vec<ChatMessage>, StoreError> {
        let path = self.thread_dir(thread_id).join("messages.jsonl");
        self.blocking(move || {
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(StoreError(format!("reading {}: {e}", path.display()))),
            };
            let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
            let mut messages = Vec::with_capacity(lines.len());
            for (i, line) in lines.iter().enumerate() {
                match serde_json::from_str::<ChatMessage>(line) {
                    Ok(message) => messages.push(message),
                    // A torn final line is a partially flushed append; drop
                    // it. Corruption anywhere else is a real error.
                    Err(e) if i == lines.len() - 1 => {
                        tracing::warn!("dropping torn final line in {}: {e}", path.display());
                    }
                    Err(e) => {
                        return Err(StoreError(format!(
                            "corrupt message (line {}) in {}: {e}",
                            i + 1,
                            path.display()
                        )))
                    }
                }
            }
            Ok(messages)
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
    fn tool_filename_encoding() {
        assert_eq!(encode_tool_filename("user__git_log"), "user__git_log.json");
        assert_eq!(encode_tool_filename("a/b:c"), "a%2Fb%3Ac.json");
        assert_eq!(encode_tool_filename("café"), "caf%C3%A9.json");
    }
}
