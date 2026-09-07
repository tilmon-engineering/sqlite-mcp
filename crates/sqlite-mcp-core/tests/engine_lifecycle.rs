use sqlite_mcp_core::{Cell, Config, Core};
use tempfile::tempdir;

#[tokio::test]
async fn request_handle_cancellation_is_token_scoped() {
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("db.sqlite")
        .canonicalize()
        .unwrap_or_else(|_| dir.path().join("db.sqlite"));
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core.create_database(path.to_str().unwrap()).await.unwrap();
    let handle = core.open_database(&path, false).await.unwrap();
    let schema = core.get_schema(&handle.id).await.unwrap();
    assert_eq!(schema.schema_version, 0);
    let token = sqlite_mcp_core::CancelToken::new();
    let request = sqlite_mcp_core::RequestHandle::new(token.clone());
    assert!(!request.is_cancelled());
    request.cancel();
    assert!(request.is_cancelled());
    core.shutdown().await;
    let _ = Cell::Null;
}

#[tokio::test]
async fn ordered_expiry_releases_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core.create_database(path.to_str().unwrap()).await.unwrap();
    let handle = core.open_database(&path, false).await.unwrap();
    core.get_schema(&handle.id).await.unwrap();
    core.begin_transaction(&handle.id, "deferred")
        .await
        .unwrap();
    assert!(core.expire_handle(&handle.id).await.unwrap());
    assert!(matches!(
        core.query(&handle.id, "SELECT 1", &[]).await,
        Err(sqlite_mcp_core::CoreError::TransactionExpired)
    ));
    core.shutdown().await;
}
