//! Phase-0 risk spike for LadybugDB (`lbug` crate).
//!
//! Validates the capabilities the storage design depends on:
//! idempotent DDL, MERGE upserts, prepared statements with parameters,
//! JSON-blob round-trips (checkpoints), FTS + vector extensions, and
//! reopening a database from disk.

use graph_store::extensions::{materialize, Extension};
use lbug::{Connection, Database, SystemConfig, Value};

/// Load a bundled extension into a raw connection by file path — no
/// `INSTALL`, no network, no shared `~/.lbdb/` cache to race on (the
/// download-at-test-time approach flaked whenever the extension CDN did).
fn load_extension(conn: &Connection, ext: Extension, dir: &std::path::Path) {
    let path = materialize(ext, dir).expect("materialize extension");
    conn.query(&format!("LOAD EXTENSION '{}'", path.display()))
        .expect("load extension");
}

fn open(dir: &std::path::Path) -> Database {
    Database::new(dir.join("spike.db"), SystemConfig::default()).expect("open database")
}

#[test]
fn ddl_is_idempotent_and_data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();

    let ddl = "
        CREATE NODE TABLE IF NOT EXISTS Thread(id STRING, title STRING, PRIMARY KEY(id));
        CREATE NODE TABLE IF NOT EXISTS Message(id STRING, role STRING, content STRING, idx INT64, PRIMARY KEY(id));
        CREATE REL TABLE IF NOT EXISTS HAS_MESSAGE(FROM Thread TO Message);
    ";

    {
        let db = open(dir.path());
        let conn = Connection::new(&db).unwrap();
        conn.query(ddl).expect("first DDL");
        conn.query(ddl).expect("repeated DDL (IF NOT EXISTS)");

        conn.query("CREATE (:Thread {id: 't1', title: 'first thread'});")
            .unwrap();
        conn.query("CREATE (:Message {id: 'm1', role: 'user', content: 'hello', idx: 0});")
            .unwrap();
        conn.query(
            "MATCH (t:Thread {id: 't1'}), (m:Message {id: 'm1'}) CREATE (t)-[:HAS_MESSAGE]->(m);",
        )
        .unwrap();
    }

    // Reopen from disk: schema and data must persist.
    let db = open(dir.path());
    let conn = Connection::new(&db).unwrap();
    conn.query(ddl).expect("DDL after reopen");

    let result = conn
        .query("MATCH (t:Thread)-[:HAS_MESSAGE]->(m:Message) RETURN t.title, m.content;")
        .unwrap();
    let rows: Vec<Vec<Value>> = result.into_iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].to_string(), "first thread");
    assert_eq!(rows[0][1].to_string(), "hello");
}

#[test]
fn merge_upserts_and_prepared_statements_with_params() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let conn = Connection::new(&db).unwrap();

    conn.query(
        "CREATE NODE TABLE IF NOT EXISTS Repo(name STRING, stars INT64, PRIMARY KEY(name));",
    )
    .unwrap();

    let mut upsert = conn
        .prepare("MERGE (r:Repo {name: $name}) ON CREATE SET r.stars = $stars ON MATCH SET r.stars = $stars;")
        .expect("prepare MERGE");

    for stars in [1i64, 2, 3] {
        conn.execute(
            &mut upsert,
            vec![
                ("name", Value::String("graph".to_string())),
                ("stars", Value::Int64(stars)),
            ],
        )
        .expect("execute MERGE");
    }

    let result = conn
        .query("MATCH (r:Repo) RETURN r.name, r.stars;")
        .unwrap();
    let rows: Vec<Vec<Value>> = result.into_iter().collect();
    assert_eq!(rows.len(), 1, "MERGE must upsert, not duplicate");
    assert_eq!(rows[0][1], Value::Int64(3));
}

#[test]
fn json_blob_round_trip_for_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let conn = Connection::new(&db).unwrap();

    conn.query(
        "CREATE NODE TABLE IF NOT EXISTS Checkpoint(id STRING, state STRING, PRIMARY KEY(id));",
    )
    .unwrap();

    let state = serde_json::json!({
        "plan": [{"id": "E0", "toolName": "linear__search_teams", "input": {"query": "Platform"}}],
        "results": {"E0": {"values": [{"id": "abc", "name": "Platform"}]}},
        "planAttempts": 1,
    });
    let mut stmt = conn
        .prepare("CREATE (:Checkpoint {id: $id, state: $state});")
        .unwrap();
    conn.execute(
        &mut stmt,
        vec![
            ("id", Value::String("c1".to_string())),
            ("state", Value::String(state.to_string())),
        ],
    )
    .unwrap();

    let result = conn
        .query("MATCH (c:Checkpoint {id: 'c1'}) RETURN c.state;")
        .unwrap();
    let rows: Vec<Vec<Value>> = result.into_iter().collect();
    let restored: serde_json::Value = serde_json::from_str(&rows[0][0].to_string()).unwrap();
    assert_eq!(restored, state);
}

#[test]
fn fts_extension_loads_and_searches() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let conn = Connection::new(&db).unwrap();

    load_extension(&conn, Extension::Fts, dir.path());

    conn.query("CREATE NODE TABLE IF NOT EXISTS Doc(id STRING, body STRING, PRIMARY KEY(id));")
        .unwrap();
    conn.query("CREATE (:Doc {id: 'd1', body: 'how is my sprint going'});")
        .unwrap();
    conn.query("CREATE (:Doc {id: 'd2', body: 'release notes for last week'});")
        .unwrap();
    conn.query("CALL CREATE_FTS_INDEX('Doc', 'doc_fts', ['body']);")
        .unwrap();

    let result = conn
        .query("CALL QUERY_FTS_INDEX('Doc', 'doc_fts', 'sprint') RETURN node.id, score;")
        .unwrap();
    let rows: Vec<Vec<Value>> = result.into_iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].to_string(), "d1");
}

#[test]
fn vector_extension_loads_and_finds_nearest() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let conn = Connection::new(&db).unwrap();

    load_extension(&conn, Extension::Vector, dir.path());

    conn.query(
        "CREATE NODE TABLE IF NOT EXISTS Exemplar(id STRING, embedding FLOAT[4], PRIMARY KEY(id));",
    )
    .unwrap();
    conn.query("CREATE (:Exemplar {id: 'e1', embedding: [1.0, 0.0, 0.0, 0.0]});")
        .unwrap();
    conn.query("CREATE (:Exemplar {id: 'e2', embedding: [0.0, 1.0, 0.0, 0.0]});")
        .unwrap();
    conn.query("CALL CREATE_VECTOR_INDEX('Exemplar', 'exemplar_idx', 'embedding');")
        .unwrap();

    let result = conn
        .query(
            "CALL QUERY_VECTOR_INDEX('Exemplar', 'exemplar_idx', CAST([0.9, 0.1, 0.0, 0.0] AS FLOAT[4]), 1)
             RETURN node.id, distance;",
        )
        .unwrap();
    let rows: Vec<Vec<Value>> = result.into_iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].to_string(), "e1");
}

#[test]
fn database_is_send_and_sync_for_concurrent_use() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Database>();
    // Connections borrow the Database; the Store will own the Database and
    // create short-lived connections per operation.
}
