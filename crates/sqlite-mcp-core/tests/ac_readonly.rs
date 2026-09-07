use sqlite_mcp_core::{Cell, Config, Core, CoreError};
use tempfile::tempdir;

async fn fixture() -> (tempfile::TempDir, Core, String) {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    (dir, core, path)
}

#[tokio::test]
async fn open_existing_validation() {
    let (_dir, core, path) = fixture().await;
    let ro = core.open_database(&path, true).await.unwrap();
    core.get_schema(&ro.id).await.unwrap();
    core.begin_transaction(&ro.id, "deferred").await.unwrap();
    assert!(core.query(&ro.id, "CREATE TABLE t(a)", &[]).await.is_err());
    core.rollback(&ro.id).await.unwrap();
    let rw = core.open_database(&path, false).await.unwrap_err();
    assert!(matches!(rw, CoreError::AlreadyOpen));
    core.shutdown().await;
}

#[tokio::test]
async fn readonly_engine_enforcement() {
    let (_dir, core, path) = fixture().await;
    let h = core.open_database(&path, true).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    assert!(matches!(
        core.begin_transaction(&h.id, "immediate").await,
        Err(CoreError::ReadonlyImmediate)
    ));
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    for sql in [
        "INSERT INTO sqlite_schema VALUES ('table','t','t',1,'CREATE TABLE t(a)')",
        "CREATE TABLE t(a)",
        "WITH cte AS (SELECT 1) INSERT INTO t VALUES (1)",
        "CREATE TEMP TABLE t2(a)",
    ] {
        assert!(core.query(&h.id, sql, &[]).await.is_err(), "accepted {sql}");
    }
    core.commit(&h.id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn transaction_persistence_and_modes() {
    let (_dir, core, path) = fixture().await;
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    core.query(&h.id, "CREATE TABLE t(a)", &[]).await.unwrap();
    // Re-observe after local DDL before further statements.
    core.get_schema(&h.id).await.unwrap();
    core.query(
        &h.id,
        "INSERT INTO t VALUES (?)",
        &[Cell::Integer("1".into())],
    )
    .await
    .unwrap();
    core.commit(&h.id).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "immediate").await.unwrap();
    core.query(
        &h.id,
        "INSERT INTO t VALUES (?)",
        &[Cell::Integer("2".into())],
    )
    .await
    .unwrap();
    core.rollback(&h.id).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    let result = core
        .query(&h.id, "SELECT a FROM t ORDER BY a", &[])
        .await
        .unwrap();
    assert_eq!(result.rows_returned, 1);
    core.rollback(&h.id).await.unwrap();
    core.shutdown().await;
}
