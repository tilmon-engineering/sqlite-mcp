use sqlite_mcp_core::{Cell, Config, Core};
use tempfile::{TempDir, tempdir};

fn enable_hooks() {
    unsafe { std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1") };
    sqlite_mcp_core::test_support::reset_registry();
}

async fn setup(config: Config) -> (TempDir, Core, String, String) {
    let dir = tempdir().unwrap();
    let core = Core::new(config).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    (dir, core, path, h.id)
}

async fn table_setup() -> (TempDir, Core, String) {
    enable_hooks();
    let (dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE t(x TEXT)", &[])
        .await
        .unwrap();
    (dir, core, id)
}

/// Serializes tests using the process-global event/fault hooks.
static HOOK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn multiple_in_transaction_schema_and_data_queries() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE first(value INTEGER)", &[])
        .await
        .unwrap();
    core.query(&id, "CREATE TABLE second(value INTEGER)", &[])
        .await
        .unwrap();
    core.query(&id, "INSERT INTO first VALUES (1)", &[])
        .await
        .unwrap();
    let selected = core
        .query(&id, "SELECT value FROM first", &[])
        .await
        .unwrap();
    assert_eq!(selected.rows[0][0], Cell::Integer("1".into()));
    core.commit(&id).await.unwrap();
    assert!(!core.list_handles().await[0].schema_observed);
    let schema = core.get_schema(&id).await.unwrap();
    assert!(schema.objects.iter().any(|object| object.name == "first"));
    assert!(schema.objects.iter().any(|object| object.name == "second"));
    core.shutdown().await;
}

#[tokio::test]
async fn successful_dml_does_not_invalidate_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.query(&id, "INSERT INTO t VALUES ('one')", &[])
        .await
        .unwrap();
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(after.schema_observed);
    assert_eq!(after.schema_version, before.schema_version);
    assert_eq!(after.observation_generation, before.observation_generation);
    assert!(core.query(&id, "SELECT count(*) FROM t", &[]).await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn ddl_and_select_do_not_inherit_dml_changes() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    let insert = core
        .query(
            &id,
            "INSERT INTO t VALUES ('a'),('b'),('c'),('d'),('e')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(insert.changes, 5, "{insert:?}");
    let ddl = core
        .query(&id, "CREATE TABLE ddl_only(x)", &[])
        .await
        .unwrap();
    assert_eq!(ddl.changes, 0, "{ddl:?}");
    let select = core
        .query(&id, "SELECT count(*) FROM t", &[])
        .await
        .unwrap();
    assert_eq!(select.changes, 0, "{select:?}");
    let zero_row = core
        .query(&id, "UPDATE t SET x = 'a' WHERE x = 'missing'", &[])
        .await
        .unwrap();
    assert_eq!(
        zero_row.changes, 0,
        "zero-match DML after a 5-row DML must not inherit its count: {zero_row:?}"
    );
    let real = core
        .query(&id, "UPDATE t SET x = 'z' WHERE x in ('a','b','c')", &[])
        .await
        .unwrap();
    assert_eq!(real.changes, 3, "{real:?}");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn successful_noop_ddl_retains_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE baseline(x TEXT)", &[])
        .await
        .unwrap();
    core.commit(&id).await.unwrap();
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.query(&id, "CREATE TABLE IF NOT EXISTS baseline(x TEXT)", &[])
        .await
        .unwrap();
    core.query(&id, "DROP TABLE IF EXISTS missing", &[])
        .await
        .unwrap();
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(after.schema_observed);
    assert_eq!(after.schema_version, before.schema_version);
    assert_eq!(after.observation_generation, before.observation_generation);
    core.commit(&id).await.unwrap();
    let begin = core.begin_transaction(&id, "deferred").await;
    assert!(begin.is_ok(), "no-op DDL begin failed: {begin:?}");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn active_transaction_schema_result_does_not_promote_handle_state() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.query(&id, "CREATE TABLE uncommitted(value TEXT)", &[])
        .await
        .unwrap();
    let schema = core.get_schema(&id).await.unwrap();
    assert!(
        schema
            .objects
            .iter()
            .any(|object| object.name == "uncommitted")
    );
    let during = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert_eq!(during.schema_observed, before.schema_observed);
    assert_eq!(during.schema_version, before.schema_version);
    assert_eq!(during.observation_generation, before.observation_generation);
    core.rollback(&id).await.unwrap();
    assert!(core.begin_transaction(&id, "deferred").await.is_ok());
    let absent = core
        .query(
            &id,
            "SELECT name FROM sqlite_schema WHERE name = 'uncommitted'",
            &[],
        )
        .await
        .unwrap();
    assert!(absent.rows.is_empty());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn actual_ddl_commit_requires_one_post_commit_schema_read() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.query(&id, "CREATE TABLE committed(value INTEGER)", &[])
        .await
        .unwrap();
    core.commit(&id).await.unwrap();
    let invalidated = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(!invalidated.schema_observed);
    assert_eq!(
        invalidated.observation_generation,
        before.observation_generation + 1
    );
    assert!(core.begin_transaction(&id, "deferred").await.is_err());
    let schema = core.get_schema(&id).await.unwrap();
    assert!(
        schema
            .objects
            .iter()
            .any(|object| object.name == "committed")
    );
    let observed = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(observed.schema_observed);
    assert_eq!(
        observed.observation_generation,
        invalidated.observation_generation + 1
    );
    assert!(core.begin_transaction(&id, "deferred").await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn rollback_of_local_ddl_keeps_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.query(&id, "CREATE TABLE rolled_back(value INTEGER)", &[])
        .await
        .unwrap();
    core.rollback(&id).await.unwrap();
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(after.schema_observed);
    assert_eq!(after.schema_version, before.schema_version);
    assert_eq!(after.observation_generation, before.observation_generation);
    assert!(core.begin_transaction(&id, "deferred").await.is_ok());
    let absent = core
        .query(
            &id,
            "SELECT name FROM sqlite_schema WHERE name = 'rolled_back'",
            &[],
        )
        .await
        .unwrap();
    assert!(absent.rows.is_empty());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn failed_or_denied_mutations_retain_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(
        core.query(&id, "CREATE TABLE t(x TEXT)", &[])
            .await
            .is_err()
    );
    assert!(
        core.query(
            &id,
            "CREATE VIEW v AS SELECT * FROM pragma_table_info('t')",
            &[],
        )
        .await
        .is_err()
    );
    assert!(
        core.query(&id, "UPDATE t SET x = 'ok' WHERE 0", &[])
            .await
            .is_ok()
    );
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(after.schema_observed);
    assert_eq!(after.observation_generation, before.observation_generation);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn dropped_caller_publishes_mutation_state() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    sqlite_mcp_core::test_support::arm(sqlite_mcp_core::test_support::Event::BeginCompletion);
    let task = tokio::spawn({
        let c = core.clone();
        let i = id.clone();
        async move { c.query(&i, "CREATE TABLE dropped(x)", &[]).await }
    });
    let arrived = tokio::task::spawn_blocking(move || {
        sqlite_mcp_core::test_support::wait_for(
            sqlite_mcp_core::test_support::Event::BeginCompletion,
            std::time::Duration::from_secs(5),
        )
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), arrived)
        .await
        .expect("arrival wait join timed out")
        .expect("arrival wait task panicked")
        .expect("query never reached the frozen event");
    task.abort();
    let _ = task.await;
    sqlite_mcp_core::test_support::release_arm(
        sqlite_mcp_core::test_support::Event::BeginCompletion,
    );
    assert!(core.query(&id, "SELECT 1", &[]).await.is_ok());
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(handle.schema_observed);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn schema_cookie_external_ddl_interleaving() {
    enable_hooks();
    let _hooks = HOOK_LOCK.lock().await;
    let (dir, core, id) = table_setup().await;
    core.rollback(&id).await.unwrap();
    let path = dir.path().join("db.sqlite");
    let external = rusqlite::Connection::open(path).unwrap();
    external
        .execute_batch("CREATE TABLE external_cookie(x INTEGER)")
        .unwrap();
    let result = core.begin_transaction(&id, "deferred").await;
    assert!(matches!(
        result,
        Err(sqlite_mcp_core::CoreError::SchemaStale)
    ));
    core.shutdown().await;
}

#[tokio::test]
async fn commit_fault_open_continuable_preserves_transaction() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE pending_commit(x)", &[])
        .await
        .unwrap();
    sqlite_mcp_core::test_support::inject_commit_fault(
        sqlite_mcp_core::test_support::CommitFault::OpenContinuable,
    );
    let error = core.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("commit lifecycle"));
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(handle.transaction_id.is_some());
    assert!(handle.schema_observed);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn commit_fault_autocommit_restored_discards_pending_state() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE discarded_commit(x)", &[])
        .await
        .unwrap();
    sqlite_mcp_core::test_support::inject_commit_fault(
        sqlite_mcp_core::test_support::CommitFault::AutocommitRestoredUnconfirmed,
    );
    let error = core.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("commit lifecycle"));
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(handle.transaction_id.is_none());
    assert!(handle.schema_observed);
    core.shutdown().await;
}

#[tokio::test]
async fn post_commit_fault_publishes_schema_invalidation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE durable_commit(x)", &[])
        .await
        .unwrap();
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    sqlite_mcp_core::test_support::inject_commit_fault(
        sqlite_mcp_core::test_support::CommitFault::AutocommitRestored,
    );
    let error = core.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("commit lifecycle"));
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(handle.transaction_id.is_none());
    assert!(!handle.schema_observed);
    assert_eq!(
        handle.observation_generation,
        before.observation_generation + 1
    );
    assert!(
        core.get_schema(&id)
            .await
            .unwrap()
            .objects
            .iter()
            .any(|o| o.name == "durable_commit")
    );
    core.shutdown().await;
}

#[tokio::test]
async fn commit_fault_uncertain_invalidates_handle() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, _path, id) = setup(Config::default()).await;
    core.query(&id, "CREATE TABLE uncertain_commit(x)", &[])
        .await
        .unwrap();
    sqlite_mcp_core::test_support::inject_commit_fault(
        sqlite_mcp_core::test_support::CommitFault::Uncertain,
    );
    let error = core.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("commit lifecycle"));
    assert!(core.list_handles().await.into_iter().all(|h| h.id != id));
    core.shutdown().await;
}

#[tokio::test]
async fn predispatch_rejection_keeps_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(core.query(&id, &"x".repeat(2_000_000), &[]).await.is_err());
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(after.schema_observed);
    assert_eq!(after.observation_generation, before.observation_generation);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn schema_read_preserves_caller_transaction() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    let schema = core.get_schema(&id).await.unwrap();
    assert!(schema.objects.iter().any(|object| object.name == "t"));
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert_eq!(before.transaction_id, after.transaction_id);
    assert_eq!(before.schema_observed, after.schema_observed);
    assert_eq!(before.schema_version, after.schema_version);
    assert_eq!(before.observation_generation, after.observation_generation);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn bare_digits_are_parser_errors() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let e = core
        .query(&id, "123456789", &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(!e.contains("SQL exceeds configured byte limit"));
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn sql_utf8_byte_boundaries() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, _path, id) = setup(Config {
        sql_byte_limit: 12,
        ..Default::default()
    })
    .await;
    assert!(core.query(&id, "SELECT 'é'", &[]).await.is_ok());
    assert!(core.query(&id, "SELECT 'éééééé'", &[]).await.is_err());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn numeric_select_values_execute() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let r = core.query(&id, "SELECT 123456789", &[]).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Integer("123456789".into()));
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}
