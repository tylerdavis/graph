//! LadybugDB-backed `Store` implementation.
//!
//! lbug is synchronous and `Connection` borrows the `Database`, so the
//! store owns an `Arc<Database>` and runs every operation on a blocking
//! thread with a fresh short-lived connection.

use graph_core::store::{Store, StoreError, ThreadMeta, ToolShape};
use graph_llm::types::ChatMessage;
use lbug::{Connection, Database, SystemConfig, Value as DbValue};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

const DDL: &str = "
    CREATE NODE TABLE IF NOT EXISTS Thread(
        id STRING PRIMARY KEY, title STRING, created_at INT64, updated_at INT64);
    CREATE NODE TABLE IF NOT EXISTS Message(
        id STRING PRIMARY KEY, idx INT64, payload STRING, created_at INT64);
    CREATE NODE TABLE IF NOT EXISTS ToolShape(
        tool STRING PRIMARY KEY, schema STRING, example STRING,
        seen_count INT64, updated_at INT64);
    CREATE REL TABLE IF NOT EXISTS HAS_MESSAGE(FROM Thread TO Message);
";

pub struct GraphStore {
    db: Arc<Database>,
}

impl GraphStore {
    /// Open (creating if needed) the database at `dir` and apply DDL.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError(e.to_string()))?;
        }
        let db = Database::new(dir, SystemConfig::default()).map_err(|e| {
            StoreError(format!(
                "opening database at {}: {e} (another graph process may hold the lock)",
                dir.display()
            ))
        })?;
        let store = Self { db: Arc::new(db) };
        store.exec_blocking(|conn| {
            conn.query(DDL)?;
            Ok(())
        })?;
        Ok(store)
    }

    /// Run `f` with a fresh connection on the current thread (open/DDL path).
    fn exec_blocking<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, lbug::Error>,
    ) -> Result<T, StoreError> {
        let conn = Connection::new(&self.db).map_err(|e| StoreError(e.to_string()))?;
        f(&conn).map_err(|e| StoreError(e.to_string()))
    }

    /// Run `f` with a fresh connection on a blocking thread.
    async fn exec<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, lbug::Error> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::new(&db).map_err(|e| StoreError(e.to_string()))?;
            f(&conn).map_err(|e| StoreError(e.to_string()))
        })
        .await
        .map_err(|e| StoreError(format!("blocking task panicked: {e}")))?
    }
}

impl GraphStore {
    /// Run a raw Cypher query and return stringified rows (debugging surface
    /// for `graph db query`).
    pub async fn raw_query(&self, cypher: &str) -> Result<Vec<Vec<String>>, StoreError> {
        let cypher = cypher.to_string();
        self.exec(move |conn| {
            let result = conn.query(&cypher)?;
            Ok(result
                .into_iter()
                .map(|row| row.iter().map(|v| v.to_string()).collect())
                .collect())
        })
        .await
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn as_string(value: &DbValue) -> String {
    match value {
        DbValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn as_i64(value: &DbValue) -> i64 {
    match value {
        DbValue::Int64(n) => *n,
        other => other.to_string().parse().unwrap_or(0),
    }
}

fn thread_from_row(row: &[DbValue]) -> ThreadMeta {
    ThreadMeta {
        id: as_string(&row[0]),
        title: as_string(&row[1]),
        created_at: as_i64(&row[2]),
        updated_at: as_i64(&row[3]),
        message_count: row.get(4).map(as_i64).unwrap_or(0),
    }
}

const THREAD_COLUMNS: &str = "t.id, t.title, t.created_at, t.updated_at, count(m) AS message_count";

#[async_trait::async_trait]
impl Store for GraphStore {
    async fn create_thread(&self, title: &str) -> Result<ThreadMeta, StoreError> {
        let id = short_id();
        let now = now_ms();
        let title = title.to_string();
        let meta = ThreadMeta {
            id: id.clone(),
            title: title.clone(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        self.exec(move |conn| {
            let mut stmt = conn.prepare(
                "CREATE (:Thread {id: $id, title: $title, created_at: $now, updated_at: $now});",
            )?;
            conn.execute(
                &mut stmt,
                vec![
                    ("id", DbValue::String(id)),
                    ("title", DbValue::String(title)),
                    ("now", DbValue::Int64(now)),
                ],
            )?;
            Ok(())
        })
        .await?;
        Ok(meta)
    }

    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError> {
        let id = id.to_string();
        self.exec(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "MATCH (t:Thread {{id: $id}})
                 OPTIONAL MATCH (t)-[:HAS_MESSAGE]->(m:Message)
                 RETURN {THREAD_COLUMNS};"
            ))?;
            let result = conn.execute(&mut stmt, vec![("id", DbValue::String(id))])?;
            Ok(result.into_iter().next().map(|row| thread_from_row(&row)))
        })
        .await
    }

    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError> {
        self.exec(move |conn| {
            let result = conn.query(&format!(
                "MATCH (t:Thread)
                 OPTIONAL MATCH (t)-[:HAS_MESSAGE]->(m:Message)
                 RETURN {THREAD_COLUMNS} ORDER BY t.updated_at DESC LIMIT 1;"
            ))?;
            Ok(result.into_iter().next().map(|row| thread_from_row(&row)))
        })
        .await
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError> {
        self.exec(move |conn| {
            let result = conn.query(&format!(
                "MATCH (t:Thread)
                 OPTIONAL MATCH (t)-[:HAS_MESSAGE]->(m:Message)
                 RETURN {THREAD_COLUMNS} ORDER BY t.updated_at DESC;"
            ))?;
            Ok(result
                .into_iter()
                .map(|row| thread_from_row(&row))
                .collect())
        })
        .await
    }

    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.exec(move |conn| {
            let mut count_stmt = conn.prepare("MATCH (t:Thread {id: $id}) RETURN count(t);")?;
            let existing = conn
                .execute(&mut count_stmt, vec![("id", DbValue::String(id.clone()))])?
                .next()
                .map(|row| as_i64(&row[0]))
                .unwrap_or(0);
            if existing == 0 {
                return Ok(false);
            }
            let mut messages = conn.prepare(
                "MATCH (t:Thread {id: $id})-[:HAS_MESSAGE]->(m:Message) DETACH DELETE m;",
            )?;
            conn.execute(&mut messages, vec![("id", DbValue::String(id.clone()))])?;
            let mut thread = conn.prepare("MATCH (t:Thread {id: $id}) DETACH DELETE t;")?;
            conn.execute(&mut thread, vec![("id", DbValue::String(id))])?;
            Ok(true)
        })
        .await
    }

    async fn set_title(&self, id: &str, title: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        let title = title.to_string();
        self.exec(move |conn| {
            let mut stmt = conn.prepare("MATCH (t:Thread {id: $id}) SET t.title = $title;")?;
            conn.execute(
                &mut stmt,
                vec![
                    ("id", DbValue::String(id)),
                    ("title", DbValue::String(title)),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn append_messages(
        &self,
        thread_id: &str,
        messages: &[ChatMessage],
    ) -> Result<(), StoreError> {
        let thread_id = thread_id.to_string();
        let payloads: Vec<String> = messages
            .iter()
            .map(|m| serde_json::to_string(m).map_err(|e| StoreError(e.to_string())))
            .collect::<Result<_, _>>()?;
        let now = now_ms();
        self.exec(move |conn| {
            let mut count_stmt = conn.prepare(
                "MATCH (t:Thread {id: $id})-[:HAS_MESSAGE]->(m:Message) RETURN count(m);",
            )?;
            let mut idx = conn
                .execute(
                    &mut count_stmt,
                    vec![("id", DbValue::String(thread_id.clone()))],
                )?
                .next()
                .map(|row| as_i64(&row[0]))
                .unwrap_or(0);

            let mut insert = conn.prepare(
                "MATCH (t:Thread {id: $thread})
                 CREATE (t)-[:HAS_MESSAGE]->(:Message {
                     id: $id, idx: $idx, payload: $payload, created_at: $now});",
            )?;
            for payload in payloads {
                conn.execute(
                    &mut insert,
                    vec![
                        ("thread", DbValue::String(thread_id.clone())),
                        ("id", DbValue::String(short_id())),
                        ("idx", DbValue::Int64(idx)),
                        ("payload", DbValue::String(payload)),
                        ("now", DbValue::Int64(now)),
                    ],
                )?;
                idx += 1;
            }
            let mut touch = conn.prepare("MATCH (t:Thread {id: $id}) SET t.updated_at = $now;")?;
            conn.execute(
                &mut touch,
                vec![
                    ("id", DbValue::String(thread_id)),
                    ("now", DbValue::Int64(now)),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Vec<ChatMessage>, StoreError> {
        let thread_id = thread_id.to_string();
        let payloads: Vec<String> = self
            .exec(move |conn| {
                let mut stmt = conn.prepare(
                    "MATCH (t:Thread {id: $id})-[:HAS_MESSAGE]->(m:Message)
                     RETURN m.payload ORDER BY m.idx;",
                )?;
                let result = conn.execute(&mut stmt, vec![("id", DbValue::String(thread_id))])?;
                Ok(result.into_iter().map(|row| as_string(&row[0])).collect())
            })
            .await?;
        payloads
            .iter()
            .map(|p| {
                serde_json::from_str(p).map_err(|e| StoreError(format!("corrupt message: {e}")))
            })
            .collect()
    }

    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError> {
        let tool = tool.to_string();
        let schema = schema.to_string();
        let example = example.to_string();
        let now = now_ms();
        self.exec(move |conn| {
            let mut stmt = conn.prepare(
                "MERGE (s:ToolShape {tool: $tool})
                 ON CREATE SET s.schema = $schema, s.example = $example,
                               s.seen_count = 1, s.updated_at = $now
                 ON MATCH SET s.schema = $schema, s.example = $example,
                              s.seen_count = s.seen_count + 1, s.updated_at = $now;",
            )?;
            conn.execute(
                &mut stmt,
                vec![
                    ("tool", DbValue::String(tool)),
                    ("schema", DbValue::String(schema)),
                    ("example", DbValue::String(example)),
                    ("now", DbValue::Int64(now)),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
        let rows: Vec<(String, String, String, i64)> = self
            .exec(move |conn| {
                let result = conn.query(
                    "MATCH (s:ToolShape) RETURN s.tool, s.schema, s.example, s.seen_count;",
                )?;
                Ok(result
                    .into_iter()
                    .map(|row| {
                        (
                            as_string(&row[0]),
                            as_string(&row[1]),
                            as_string(&row[2]),
                            as_i64(&row[3]),
                        )
                    })
                    .collect())
            })
            .await?;
        rows.into_iter()
            .map(|(tool, schema, example, seen_count)| {
                Ok(ToolShape {
                    tool,
                    schema: serde_json::from_str(&schema).map_err(|e| StoreError(e.to_string()))?,
                    example: serde_json::from_str(&example)
                        .map_err(|e| StoreError(e.to_string()))?,
                    seen_count,
                })
            })
            .collect()
    }
}
