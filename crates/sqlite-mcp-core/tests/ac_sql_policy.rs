use sqlite_mcp_core::{Cell, Config, Core};
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, Core, String, String) {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    core.query(&h.id, "CREATE TABLE t(a)", &[]).await.unwrap();
    // Re-observe after local DDL before further statements.
    core.get_schema(&h.id).await.unwrap();
    (dir, core, path, h.id)
}

#[tokio::test]
async fn sql_policy_escape_matrix() {
    let (_dir, core, path, id) = setup().await;
    for sql in [
        "ATTACH DATABASE '/tmp/x' AS x",
        "DETACH x",
        "PRAGMA journal_mode",
        "PRAGMA writable_schema=1",
        "SELECT * FROM pragma_table_info('t')",
        "VACUUM",
        "VACUUM INTO '/tmp/v.db'",
        "COMMIT",
        "BEGIN",
        "ROLLBACK",
        "SAVEPOINT foo",
        "RELEASE foo",
        "CREATE VIRTUAL TABLE vt USING fts5(a)",
        "CREATE TEMP TABLE tmp(a)",
        "SELECT load_extension('x')",
        "EXPLAIN PRAGMA journal_mode",
        "CREATE VIEW pv AS SELECT * FROM pragma_table_info('t')",
        "CREATE TRIGGER pt AFTER INSERT ON t BEGIN PRAGMA user_version; END",
    ] {
        assert!(
            core.query(&id, sql, &[]).await.is_err(),
            "policy accepted {sql}"
        );
    }
    // Attempted (denied) agent DDL conservatively invalidates the
    // observation even when the statement never executes; re-observe.
    core.get_schema(&id).await.unwrap();
    let ok = core.query(&id, "EXPLAIN QUERY PLAN SELECT 1", &[]).await;
    assert!(ok.is_ok());
    assert!(
        core.query(&id, "SELECT 1; DROP TABLE t", &[])
            .await
            .is_err()
    );
    assert!(core.query(&id, "", &[]).await.is_err());
    assert!(core.query(&id, "SELECT\0 1", &[]).await.is_err());
    core.rollback(&id).await.unwrap();
    assert!(!std::path::Path::new("/tmp/v.db").exists());
    assert!(path.ends_with("db.sqlite"));
    core.shutdown().await;
}

#[tokio::test]
async fn parameter_validation_and_schema_recheck() {
    let (_dir, core, _path, id) = setup().await;
    assert!(
        core.query(&id, "INSERT INTO t VALUES (?)", &[])
            .await
            .is_err()
    );
    assert!(
        core.query(
            &id,
            "INSERT INTO t VALUES (?)",
            &[Cell::Integer("1".into()), Cell::Integer("2".into())]
        )
        .await
        .is_err()
    );
    assert!(
        core.query(&id, "INSERT INTO t VALUES (?)", &[Cell::Real("NaN".into())])
            .await
            .is_err()
    );
    core.query(&id, "CREATE VIEW ok AS SELECT 1", &[])
        .await
        .unwrap();
    // Re-observe after local DDL before further statements.
    core.get_schema(&id).await.unwrap();
    assert!(core.query(&id, "SELECT * FROM ok", &[]).await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}
