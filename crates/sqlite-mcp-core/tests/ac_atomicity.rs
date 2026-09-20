use sqlite_mcp_core::{Cell, Config, Core, CoreError};
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, Core, String) {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    core.query(&h.id, "CREATE TABLE t(a INTEGER UNIQUE)", &[])
        .await
        .unwrap();
    core.query(&h.id, "INSERT INTO t VALUES (1),(2),(3)", &[])
        .await
        .unwrap();
    core.commit(&h.id).await.unwrap();
    (dir, core, h.id)
}

#[tokio::test]
async fn statement_savepoint_atomicity() {
    let (_dir, core, id) = setup().await;
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(
        core.query(&id, "UPDATE OR FAIL t SET a=a+1", &[])
            .await
            .is_err()
    );
    assert!(core.query(&id, "SELECT 1", &[]).await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn outer_rollback_reporting_and_ddl_rollback() {
    let (_dir, core, id) = setup().await;
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "CREATE TABLE t2(x)", &[]).await.unwrap();
    core.query(&id, "ALTER TABLE t2 ADD COLUMN y", &[])
        .await
        .unwrap();
    core.rollback(&id).await.unwrap();
    let schema = core.get_schema(&id).await.unwrap();
    assert!(!schema.objects.iter().any(|o| o.name == "t2"));
    core.begin_transaction(&id, "deferred").await.unwrap();
    let err = core
        .query(&id, "INSERT OR ROLLBACK INTO t VALUES (1)", &[])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Worker(_)) || matches!(err, CoreError::Sqlite(_)));
    core.shutdown().await;
}

#[tokio::test]
async fn returning_completion_under_caps() {
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
    core.query(&h.id, "CREATE TABLE t(a)", &[]).await.unwrap();
    let r = core
        .query(&h.id, "INSERT INTO t VALUES (1),(2),(3) RETURNING a", &[])
        .await
        .unwrap();
    assert_eq!(r.rows_returned, 2);
    assert!(r.execution_complete);
    core.commit(&h.id).await.unwrap();
    assert!(r.truncated, "result cap did not report truncation");
    core.shutdown().await;
}

#[allow(dead_code)]
fn _cell_use() {
    let _ = Cell::Null;
}
