use sqlite_mcp_core::{Cell, Config, Core, CoreError};
use tempfile::{TempDir, tempdir};

fn enable_hooks() {
    unsafe { std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1") };
    // Clear stale arms/records from earlier tests: an armed gate that outlived
    // its test would otherwise park unrelated workers' emits here.
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
    core.get_schema(&id).await.unwrap();
    (dir, core, id)
}

/// Serializes tests that use the process-global hook state (fault registry,
/// marker, event registry) so parallel tests cannot steal each other's
/// injected faults or arms.
static HOOK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn failed_ddl_invalidates_immediately() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    assert!(
        core.query(&id, "CREATE TABLE t(x TEXT)", &[])
            .await
            .is_err()
    );
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(
        !h.schema_observed,
        "failed DDL must invalidate before the next call"
    );
    assert!(matches!(
        core.query(&id, "SELECT 1", &[]).await,
        Err(CoreError::SchemaRequired | CoreError::SchemaStale)
    ));
    core.shutdown().await;
}

#[tokio::test]
async fn denied_stored_ddl_invalidates() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    assert!(
        core.query(
            &id,
            "CREATE VIEW v AS SELECT * FROM pragma_table_info('t')",
            &[]
        )
        .await
        .is_err()
    );
    assert!(
        !core
            .list_handles()
            .await
            .into_iter()
            .find(|h| h.id == id)
            .unwrap()
            .schema_observed
    );
    core.shutdown().await;
}

#[tokio::test]
async fn queued_mutation_markers_isolated() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let first = core.query(&id, "CREATE TABLE q(x)", &[]).await;
    assert!(first.is_ok());
    let second = core.query(&id, "SELECT 1", &[]).await;
    assert!(second.is_err());
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(!h.schema_observed);
    core.shutdown().await;
}

#[tokio::test]
async fn dropped_caller_publishes_mutation_state() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    // Freeze the worker right after the CREATE TABLE closure completed
    // (statement executed, attempted-DDL marker set), then drop the caller
    // future before core-side drain/invalidation runs. The admitted operation
    // must still publish its invalidation exactly once.
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
    // Any gate-ordered operation runs strictly after the coordinator's
    // publication: the stale observation must refuse the follow-up query.
    assert!(matches!(
        core.query(&id, "SELECT 1", &[]).await,
        Err(CoreError::SchemaRequired | CoreError::SchemaStale)
    ));
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(
        !h.schema_observed,
        "dropped caller skipped attempted-DDL invalidation"
    );
    // The DDL itself executed inside the worker despite the dropped caller.
    core.get_schema(&id).await.unwrap();
    assert!(
        core.query(
            &id,
            "SELECT count(*) FROM sqlite_schema WHERE name = 'dropped'",
            &[]
        )
        .await
        .is_ok()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn dropped_observe_publishes_observation() {
    let _hooks = HOOK_LOCK.lock().await;
    enable_hooks();
    let (dir, core, _path, _id) = setup(Config::default()).await;
    // A second handle that has never been observed.
    let (second, _) = core
        .create_database(dir.path().join("second.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let handle = core.open_database(&second, false).await.unwrap();
    assert!(!handle.schema_observed);
    let h2 = handle.id.clone();
    // Freeze the managed read right after the schema version was read, then
    // drop the caller before publication. The completed observation must
    // still be published exactly once: begin relies on that freshness.
    sqlite_mcp_core::test_support::arm(sqlite_mcp_core::test_support::Event::SchemaVersionRead);
    let task = tokio::spawn({
        let c = core.clone();
        async move { c.get_schema(&h2).await }
    });
    let arrived = tokio::task::spawn_blocking(move || {
        sqlite_mcp_core::test_support::wait_for(
            sqlite_mcp_core::test_support::Event::SchemaVersionRead,
            std::time::Duration::from_secs(5),
        )
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), arrived)
        .await
        .expect("arrival wait join timed out")
        .expect("arrival wait task panicked")
        .expect("observation never reached the frozen event");
    task.abort();
    let _ = task.await;
    sqlite_mcp_core::test_support::release_arm(
        sqlite_mcp_core::test_support::Event::SchemaVersionRead,
    );
    assert!(
        core.begin_transaction(&handle.id, "deferred").await.is_ok(),
        "dropped observation was not published; begin wrongly refused"
    );
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
    assert_eq!(after.observation_generation, before.observation_generation);
    assert!(after.schema_observed);
    core.shutdown().await;
}

#[tokio::test]
async fn schema_snapshot_external_ddl_interleaving() {
    enable_hooks();
    let _hooks = HOOK_LOCK.lock().await;
    let (dir, core, id) = table_setup().await;
    let path = dir.path().join("db.sqlite");
    core.rollback(&id).await.unwrap();
    let before = core.get_schema(&id).await.unwrap();
    let barrier =
        sqlite_mcp_core::test_support::arm(sqlite_mcp_core::test_support::Event::SchemaVersionRead);
    let task = tokio::spawn({
        let c = core.clone();
        let i = id.clone();
        async move { c.get_schema(&i).await }
    });
    // The armed emission parks the runtime thread inside the managed read, so
    // the observe-then-release sequence must run in a blocking task.
    let wait_event = sqlite_mcp_core::test_support::Event::SchemaVersionRead;
    tokio::task::spawn_blocking(move || {
        sqlite_mcp_core::test_support::wait_for(wait_event, std::time::Duration::from_secs(2))
            .expect("schema version read observed");
        barrier.release();
    })
    .await
    .expect("waiter task");
    let external = rusqlite::Connection::open(&path).unwrap();
    external
        .execute_batch("CREATE TABLE external_after_read(x INTEGER)")
        .unwrap();
    let observed = task.await.unwrap().unwrap();
    assert_eq!(observed.schema_version, before.schema_version);
    assert!(
        observed
            .objects
            .iter()
            .all(|o| o.name != "external_after_read"),
        "managed read must retain one pre-DDL snapshot"
    );
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(h.schema_observed);
    assert!(
        core.begin_transaction(&id, "deferred").await.is_err(),
        "post-DDL begin requires re-observation"
    );
    core.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn readonly_schema_snapshot_interleaving() {
    enable_hooks();
    let _hooks = HOOK_LOCK.lock().await;
    let (dir, path) = {
        let d = tempdir().unwrap();
        let p = d.path().join("ro.sqlite");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x)")
            .unwrap();
        (d, p.to_string_lossy().into_owned())
    };
    let core = Core::new(Config::default()).unwrap();
    let h = core.open_database(&path, true).await.unwrap();
    let before = core.get_schema(&h.id).await.unwrap();
    let barrier =
        sqlite_mcp_core::test_support::arm(sqlite_mcp_core::test_support::Event::SchemaVersionRead);
    let task = tokio::spawn({
        let c = core.clone();
        let i = h.id.clone();
        async move { c.get_schema(&i).await }
    });
    let wait_event = sqlite_mcp_core::test_support::Event::SchemaVersionRead;
    tokio::task::spawn_blocking(move || {
        sqlite_mcp_core::test_support::wait_for(wait_event, std::time::Duration::from_secs(2))
            .expect("schema version read observed");
        // Release inside the blocking task: the armed emission parks the
        // runtime thread inside the managed read.
        barrier.release();
    })
    .await
    .expect("waiter task");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE external_after_read(x INTEGER)")
        .unwrap();
    let observed = task.await.unwrap().unwrap();
    assert_eq!(observed.schema_version, before.schema_version);
    assert!(
        observed
            .objects
            .iter()
            .all(|o| o.name != "external_after_read")
    );
    assert!(core.begin_transaction(&h.id, "deferred").await.is_err());
    core.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn schema_read_preserves_caller_transaction() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    core.get_schema(&id).await.unwrap();
    let after = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert_eq!(before.transaction_id, after.transaction_id);
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn schema_failure_cleanup_and_gate() {
    // A cleanup failure on a SUCCESSFUL managed read is uncertain: the
    // observation must not publish freshness and the handle is invalidated.
    let _hooks = HOOK_LOCK.lock().await;
    let (_dir, core, id) = table_setup().await;
    // Close the caller transaction so the observation runs on the managed
    // path (caller transactions retain ownership and never consume cleanup).
    core.rollback(&id).await.unwrap();
    let before = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    sqlite_mcp_core::test_support::inject_cleanup_fault(
        sqlite_mcp_core::CleanupStage::ManagedSchemaCleanup,
        "schema cleanup",
    );
    let result = core.get_schema(&id).await;
    assert!(result.is_err(), "cleanup failure must surface");
    let after = core.list_handles().await.into_iter().find(|h| h.id == id);
    // Uncertain cleanup invalidates: the handle cannot be used again.
    assert!(
        after.is_none() || !after.unwrap().schema_observed,
        "failed managed observation must not publish freshness"
    );
    let _ = before;
    core.shutdown().await;
}

#[tokio::test]
async fn schema_cleanup_failure_invalidates() {
    let _hooks = HOOK_LOCK.lock().await;
    let (dir, core, id) = table_setup().await;
    core.rollback(&id).await.unwrap();
    let path = dir.path().join("db.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql='malformed' WHERE name='t'; PRAGMA writable_schema=OFF;")
        .unwrap();
    sqlite_mcp_core::test_support::inject_cleanup_fault(
        sqlite_mcp_core::CleanupStage::ManagedSchemaCleanup,
        "schema cleanup",
    );
    let _ = core.get_schema(&id).await;
    assert!(
        core.list_handles().await.into_iter().all(|h| h.id != id),
        "uncertain managed cleanup invalidates handle"
    );
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
    assert!(
        !e.contains("SQL exceeds configured byte limit"),
        "parser error was misclassified: {e}"
    );
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
    core.shutdown().await;
}

#[tokio::test]
async fn numeric_select_values_execute() {
    let _hooks = HOOK_LOCK.lock().await;
    let (_d, core, id) = table_setup().await;
    let r = core.query(&id, "SELECT 123456789", &[]).await.unwrap();
    assert_eq!(r.rows[0][0], Cell::Integer("123456789".into()));
    core.shutdown().await;
}
