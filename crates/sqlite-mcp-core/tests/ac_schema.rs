use rusqlite::Connection;
use sqlite_mcp_core::{Config, Core, CoreError};
use tempfile::{TempDir, tempdir};

fn fixture() -> (TempDir, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("schema.sqlite");
    let c = Connection::open(&path).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE);
         CREATE TABLE ordinary(a INTEGER, b TEXT, generated_v AS (a * 2) VIRTUAL, generated_s TEXT GENERATED ALWAYS AS (b || '!') STORED);
         CREATE TABLE wr(a INTEGER NOT NULL, b TEXT, PRIMARY KEY(a,b)) WITHOUT ROWID;
         CREATE TABLE strict_t(a INTEGER PRIMARY KEY, b TEXT) STRICT;
         CREATE INDEX plain_idx ON ordinary(a);
         CREATE INDEX partial_idx ON ordinary(b) WHERE a > 0;
         CREATE INDEX expression_idx ON ordinary(lower(b));
         CREATE VIEW ordinary_view AS SELECT a,b FROM ordinary;
         CREATE TRIGGER child_cleanup AFTER DELETE ON parent BEGIN DELETE FROM child WHERE parent_id = old.id; END;
         INSERT INTO parent VALUES (1);
         INSERT INTO child VALUES (1,1);",
    )
    .unwrap();
    drop(c);
    (dir, path.to_string_lossy().into_owned())
}

async fn opened(path: &str) -> (Core, String) {
    let core = Core::new(Config::default()).unwrap();
    let h = core.open_database(path, false).await.unwrap();
    (core, h.id)
}

#[tokio::test]
async fn schema_gate_and_metadata() {
    let (_dir, path) = fixture();
    let (core, id) = opened(&path).await;
    let first = core.get_schema(&id).await.unwrap();
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(!first.identity.is_empty());
    assert!(first.schema_version >= 1);
    assert!(
        first
            .objects
            .iter()
            .any(|o| o.name == "ordinary_view" && o.object_type == "view")
    );
    assert!(
        first
            .objects
            .iter()
            .any(|o| o.name == "child_cleanup" && o.object_type == "trigger")
    );
    let ordinary = first.tables.iter().find(|t| t.name == "ordinary").unwrap();
    assert!(
        ordinary
            .columns
            .iter()
            .any(|c| c.name == "generated_v" && c.hidden == 2)
    );
    assert!(
        ordinary
            .columns
            .iter()
            .any(|c| c.name == "generated_s" && c.hidden == 3)
    );
    assert!(ordinary.indexes.iter().any(|i| i.name == "plain_idx"));
    assert!(ordinary.indexes.iter().any(|i| i.name == "partial_idx"));
    assert!(ordinary.indexes.iter().any(|i| i.name == "expression_idx"));
    let wr = first.tables.iter().find(|t| t.name == "wr").unwrap();
    assert!(wr.without_rowid);
    assert!(
        first
            .tables
            .iter()
            .find(|t| t.name == "strict_t")
            .unwrap()
            .strict
    );
    let child = first.tables.iter().find(|t| t.name == "child").unwrap();
    assert!(
        child
            .foreign_keys
            .iter()
            .any(|f| f.table == "parent" && f.from == "parent_id" && f.to == "id")
    );
    let second = core.get_schema(&id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first.objects).unwrap(),
        serde_json::to_value(&second.objects).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&first.tables).unwrap(),
        serde_json::to_value(&second.tables).unwrap()
    );
    let handle2 = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert_eq!(
        handle2.observation_generation,
        handle.observation_generation + 1
    );
    assert_eq!(handle2.schema_version, first.schema_version);
    core.shutdown().await;
}

#[tokio::test]
async fn schema_snapshot_freshness_and_gate() {
    let (dir, path) = fixture();
    let (core, id) = opened(&path).await;
    core.get_schema(&id).await.unwrap();
    let external = Connection::open(&path).unwrap();
    external
        .execute_batch("CREATE TABLE external_table(x INTEGER);")
        .unwrap();
    assert!(matches!(
        core.begin_transaction(&id, "deferred").await,
        Err(CoreError::SchemaStale)
    ));
    assert!(
        core.list_handles()
            .await
            .iter()
            .all(|h| h.transaction_id.is_none())
    );
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.rollback(&id).await.unwrap();
    external
        .execute("INSERT INTO parent VALUES (2)", [])
        .unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    let inside = core
        .query(&id, "SELECT count(*) FROM parent", &[])
        .await
        .unwrap();
    assert!(matches!(&inside.rows[0][0], sqlite_mcp_core::Cell::Integer(v) if v == "2"));
    core.rollback(&id).await.unwrap();
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    let fresh = core
        .query(&id, "SELECT count(*) FROM parent", &[])
        .await
        .unwrap();
    assert!(matches!(&fresh.rows[0][0], sqlite_mcp_core::Cell::Integer(v) if v == "2"));
    core.query(&id, "CREATE TABLE local_rollback(x)", &[])
        .await
        .unwrap();
    // F-06 contract: attempted local DDL invalidates the observation, so the
    // gate rejects the next query with SCHEMA_REQUIRED (previously the stale
    // class; invalidation replaces version-bump-only semantics).
    assert!(matches!(
        core.query(&id, "SELECT 1", &[]).await,
        Err(CoreError::SchemaRequired)
    ));
    core.rollback(&id).await.unwrap();
    // Attempted local DDL invalidated the observation; re-observe before the
    // next begin.
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    let rolled_back = core
        .query(
            &id,
            "SELECT name FROM sqlite_schema WHERE name = 'local_rollback'",
            &[],
        )
        .await
        .unwrap();
    assert!(rolled_back.rows.is_empty());
    core.rollback(&id).await.unwrap();
    core.get_schema(&id).await.unwrap();
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(h.observation_generation >= 4);
    core.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn schema_overflow_does_not_unlock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("large.sqlite");
    let c = Connection::open(&path).unwrap();
    for i in 0..40 {
        c.execute_batch(&format!("CREATE TABLE t{i}(a TEXT, b TEXT, c TEXT);"))
            .unwrap();
    }
    drop(c);
    let small = Config {
        schema_byte_limit: 512,
        ..Config::default()
    };
    let core = Core::new(small).unwrap();
    let h = core
        .open_database(path.to_str().unwrap(), false)
        .await
        .unwrap();
    assert!(matches!(
        core.get_schema(&h.id).await,
        Err(CoreError::SchemaTooLarge)
    ));
    assert!(matches!(
        core.begin_transaction(&h.id, "deferred").await,
        Err(CoreError::SchemaRequired)
    ));
    core.shutdown().await;
    let large = Core::new(Config::default()).unwrap();
    let h2 = large
        .open_database(path.to_str().unwrap(), false)
        .await
        .unwrap();
    assert!(large.get_schema(&h2.id).await.is_ok());
    large.shutdown().await;
}
