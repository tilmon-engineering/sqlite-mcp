use sqlite_mcp_core::{Cell, Config, Core};
use tempfile::tempdir;

#[tokio::test]
async fn typed_value_roundtrip() {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    core.query(&h.id, "CREATE TABLE t(i, n, s, b)", &[])
        .await
        .unwrap();
    // Re-observe after local DDL before further statements.
    core.get_schema(&h.id).await.unwrap();
    let blob = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x80u8, 0xff]);
    core.query(
        &h.id,
        "INSERT INTO t VALUES (?,?,?,?)",
        &[
            Cell::Integer(i64::MIN.to_string()),
            Cell::Null,
            Cell::Text("héllo '世界'".into()),
            Cell::Blob(blob.clone()),
        ],
    )
    .await
    .unwrap();
    // Re-observe after the invalidating DML before further queries.
    core.get_schema(&h.id).await.unwrap();
    let r = core
        .query(
            &h.id,
            "SELECT i,n,s,b,CAST(x'80FF' AS TEXT),1e999,-1e999,1 AS x,2 AS x FROM t",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(r.rows_returned, 1);
    assert_eq!(r.rows[0][0], Cell::Integer(i64::MIN.to_string()));
    assert!(matches!(r.rows[0][1], Cell::Null));
    assert!(matches!(r.rows[0][2], Cell::Text(_)));
    assert_eq!(r.rows[0][3], Cell::Blob(blob));
    assert!(matches!(r.rows[0][4], Cell::TextBytes(_)));
    assert_eq!(r.rows[0][5], Cell::Real("Infinity".into()));
    assert_eq!(r.rows[0][6], Cell::Real("-Infinity".into()));
    assert_eq!(r.columns[7], "x");
    assert_eq!(r.columns[8], "x");
    assert!(
        core.query(&h.id, "SELECT ?", &[Cell::Real("NaN".into())])
            .await
            .is_err()
    );
    core.rollback(&h.id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn result_caps_and_changes() {
    let dir = tempdir().unwrap();
    let config = Config {
        result_row_limit: 2,
        ..Default::default()
    };
    let core = Core::new(config).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    let ddl = core.query(&h.id, "CREATE TABLE t(a)", &[]).await.unwrap();
    assert_eq!(ddl.changes, 0);
    // Re-observe after local DDL before further statements.
    core.get_schema(&h.id).await.unwrap();
    let ins = core
        .query(&h.id, "INSERT INTO t VALUES (1),(2),(3)", &[])
        .await
        .unwrap();
    assert_eq!(ins.changes, 3);
    // Re-observe after the invalidating DML before further queries.
    core.get_schema(&h.id).await.unwrap();
    let sel = core.query(&h.id, "SELECT a FROM t", &[]).await.unwrap();
    assert_eq!(sel.changes, 0);
    assert_eq!(sel.rows_returned, 2);
    assert!(sel.execution_complete);
    assert!(sel.truncated, "row cap should truncate");
    core.rollback(&h.id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn parameter_and_runtime_limits() {
    let dir = tempdir().unwrap();
    let config = Config {
        parameter_limit: 3,
        column_limit: 2,
        sql_byte_limit: 100,
        ..Default::default()
    };
    let core = Core::new(config).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    assert!(core.query(&h.id, "SELECT ?", &[]).await.is_err());
    assert!(
        core.query(
            &h.id,
            "SELECT ?",
            &[Cell::Integer("1".into()), Cell::Integer("2".into())]
        )
        .await
        .is_err()
    );
    assert!(core.query(&h.id, "SELECT 1,2,3", &[]).await.is_err());
    assert!(core.query(&h.id, "SELECT 123456789", &[]).await.is_ok());
    core.rollback(&h.id).await.unwrap();
    core.shutdown().await;
}
