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
    // Denied DDL does not change the schema cookie or the committed
    // observation, so the transaction remains usable.
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
async fn schema_qualified_temp_objects_denied() {
    let (_dir, core, _path, id) = setup().await;
    for sql in [
        "CREATE TABLE temp.t2(a)",
        "CREATE VIEW temp.v AS SELECT 1",
        // `CREATE INDEX temp.i` never reaches the authorizer: SQLite
        // structurally rejects a TEMP index on a non-TEMP table. The
        // authorizer-level temp denial for CreateIndex is pinned by the
        // policy unit decision table.
        "CREATE INDEX temp.i ON t(a)",
    ] {
        let err = core
            .query(&id, sql, &[])
            .await
            .err()
            .unwrap_or_else(|| panic!("temp-schema create accepted: {sql}"));
        assert!(
            err.to_string().contains("not authorized")
                || err.to_string().to_lowercase().contains("temp index"),
            "expected denial for {sql}, got: {err}"
        );
    }
    // Controls: unqualified and `main.`-qualified creates succeed on the
    // writable handle.
    core.query(&id, "CREATE TABLE plain_t2(a)", &[])
        .await
        .expect("unqualified create");
    core.query(&id, "CREATE TABLE main.t2(a)", &[])
        .await
        .expect("main-qualified create");
    core.query(&id, "CREATE INDEX main.i ON t(a)", &[])
        .await
        .expect("main-qualified index");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn analyze_and_reindex_denied() {
    let (_dir, core, _path, id) = setup().await;
    // REINDEX only fires the authorizer when an index exists to reindex.
    core.query(&id, "CREATE INDEX i ON t(a)", &[])
        .await
        .expect("setup index");
    for sql in [
        "ANALYZE",
        "REINDEX main.t",
        "REINDEX",
        // Adjacent-comment spellings are valid SQLite and must be denied
        // before execution (the authorizer cannot catch REINDEX).
        "REINDEX/**/i",
        "REINDEX/*c*/i",
        "REINDEX/**/",
        "ANALYZE/**/",
        "ANALYZE/*c*/main.t",
        // EXPLAIN is a diagnostic prefix: the underlying maintenance
        // statement is still denied (R2-2).
        "EXPLAIN REINDEX",
        "EXPLAIN/**/REINDEX",
        "EXPLAIN QUERY PLAN REINDEX",
        "EXPLAIN QUERY PLAN ANALYZE",
    ] {
        let err = core
            .query(&id, sql, &[])
            .await
            .err()
            .unwrap_or_else(|| panic!("maintenance operation accepted: {sql}"));
        assert!(
            err.to_string().contains("maintenance operations"),
            "expected maintenance policy denial for {sql}, got: {err}"
        );
    }
    // No effect: the ordinary read path still works and the transaction
    // remains usable.
    core.query(&id, "SELECT count(*) FROM t", &[])
        .await
        .expect("read after denied maintenance");
    core.query(&id, "EXPLAIN QUERY PLAN SELECT 1", &[])
        .await
        .expect("EXPLAIN QUERY PLAN of an allowed statement stays allowed");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn stored_body_guard_accepts_pragma_named_identifiers() {
    // Quoted identifiers (and string literals) containing `pragma_` cannot
    // trigger the stored-body structural denial (R2-1).
    let (_dir, core, _path, id) = setup().await;
    core.query(&id, "CREATE VIEW \"a-b-pragma_x\" AS SELECT 1", &[])
        .await
        .expect("quoted view name containing pragma_ must be allowed");
    core.query(&id, "CREATE VIEW \"weird-name\" AS SELECT 1", &[])
        .await
        .expect("quoted view name with punctuation must be allowed");
    core.query(
        &id,
        "CREATE TRIGGER \"t-pragma_x\" AFTER INSERT ON t BEGIN SELECT 1; END",
        &[],
    )
    .await
    .expect("quoted trigger name containing pragma_ must be allowed");
    core.query(&id, "SELECT * FROM \"a-b-pragma_x\"", &[])
        .await
        .expect("the created view is readable");
    // A real unquoted pragma table-valued reference in a stored body is
    // still rejected.
    let result = core
        .query(
            &id,
            "CREATE VIEW bad AS SELECT * FROM pragma_table_info('t')",
            &[],
        )
        .await;
    let err = result.expect_err("pragma reference in stored body must be denied");
    assert!(
        err.to_string().contains("denied by SQL policy"),
        "unexpected denial shape: {err}"
    );
    core.rollback(&id).await.unwrap();
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
    assert!(core.query(&id, "SELECT * FROM ok", &[]).await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}
