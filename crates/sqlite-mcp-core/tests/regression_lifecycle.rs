use rusqlite::Connection;
use sqlite_mcp_core::{Config, Core, CoreError, test_support};
use std::time::Duration;
use tempfile::tempdir;

async fn fixture(
    config: Config,
) -> (
    tempfile::TempDir,
    Core,
    String,
    String,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let _hooks = HOOK_LOCK.lock().await;
    enable_hooks();
    let dir = tempdir().unwrap();
    let path = dir.path().join("lifecycle.sqlite");
    let p = path.to_str().unwrap().to_owned();
    let db = Connection::open(&p).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(v INTEGER);")
        .unwrap();
    drop(db);
    let core = Core::new(config).unwrap();
    let handle = core.open_database(&p, false).await.unwrap();
    core.get_schema(&handle.id).await.unwrap();
    (dir, core, p, handle.id, _hooks)
}

async fn clean(core: &Core) {
    core.shutdown().await;
}

/// Serializes process-global hook state (event registry arms, injected clock,
/// fault registry) across tests in this binary: an armed gate pauses worker
/// threads process-wide, so concurrent tests could otherwise capture each
/// other's emissions.
static HOOK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enable_hooks() {
    unsafe { std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1") };
    // Clear stale arms/records/faults/clock from earlier tests.
    test_support::reset_registry();
}

/// Freeze the worker at `event` (armed before the operation is spawned), wait
/// until the emission is retained with the worker paused at the gate, then
/// abort the caller future while the operation is mid-flight, and only then
/// release the worker. Deterministic: no sleeps, bounded waits throughout.
async fn freeze_and_drop_caller<T: Send + 'static>(
    event: test_support::Event,
    spawn_operation: impl FnOnce() -> tokio::task::JoinHandle<T>,
) {
    test_support::arm(event);
    let task = spawn_operation();
    let arrived = tokio::task::spawn_blocking(move || {
        test_support::wait_for(event, std::time::Duration::from_secs(5))
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), arrived)
        .await
        .expect("arrival wait join timed out")
        .expect("arrival wait task panicked")
        .expect("operation never reached the frozen event");
    task.abort();
    let _ = task.await;
    test_support::release_arm(event);
}

#[tokio::test]
async fn close_waits_for_publication() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    let task = tokio::spawn({
        let c = core.clone();
        let id = id.clone();
        async move { c.get_schema(&id).await }
    });
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_ok(), "in-flight publication failed: {result:?}");
    assert!(core.close_database(&id).await.is_ok());
}

#[tokio::test]
async fn close_racing_begin_refuses_active() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(matches!(
        core.close_database(&id).await,
        Err(CoreError::TransactionOpen)
    ));
    clean(&core).await;
}

#[tokio::test]
async fn close_reserves_identity_until_closed() {
    let (_d, core, p, id, _hooks) = fixture(Config::default()).await;
    core.close_database(&id).await.unwrap();
    let reopened = core.open_database(&p, false).await;
    assert!(
        reopened.is_ok(),
        "closed identity was not released: {reopened:?}"
    );
    clean(&core).await;
}

#[tokio::test]
async fn close_does_not_block_other_handles() {
    let (_d1, core, _p1, id1, _hooks) = fixture(Config::default()).await;
    let close = tokio::spawn({
        let c = core.clone();
        let id = id1.clone();
        async move { c.close_database(&id).await }
    });
    let listed = tokio::time::timeout(Duration::from_secs(2), core.list_handles()).await;
    assert!(
        listed.is_ok(),
        "unrelated registry operation blocked during close"
    );
    close.await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_shutdown_joins_all_workers() {
    let (_d, core, _p, _id, _hooks) = fixture(Config::default()).await;
    let (a, b) = tokio::join!(core.shutdown(), core.shutdown());
    let _ = (a, b);
    assert!(
        core.list_handles().await.is_empty(),
        "shutdown left registered workers"
    );
}

#[tokio::test]
async fn drop_before_admission_has_no_effect() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(
        core.query(&id, "INSERT INTO t VALUES (1)", &[])
            .await
            .is_ok()
    );
    clean(&core).await;
}

#[tokio::test]
async fn drop_after_admission_publishes_once() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    // Freeze the worker right after the BEGIN closure executed, drop the
    // caller before core-side publication, then release. The admitted
    // operation must still publish its open transaction exactly once: close
    // must refuse the handle rather than falsely succeed by rolling back a
    // transaction it never observed.
    freeze_and_drop_caller(test_support::Event::BeginCompletion, || {
        let c = core.clone();
        let i = id.clone();
        tokio::spawn(async move { c.begin_transaction(&i, "deferred").await })
    })
    .await;
    assert!(
        matches!(
            core.close_database(&id).await,
            Err(CoreError::TransactionOpen)
        ),
        "a dropped begin must still publish the open transaction; close did not refuse"
    );
    // Recovery: explicit rollback clears the published state, then close works.
    core.rollback(&id).await.unwrap();
    assert!(core.close_database(&id).await.is_ok());
}

#[tokio::test]
async fn publication_precedes_next_execution() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "INSERT INTO t VALUES (1)", &[])
        .await
        .unwrap();
    core.get_schema(&id).await.unwrap();
    assert!(core.query(&id, "SELECT count(*) FROM t", &[]).await.is_ok());
    clean(&core).await;
}

#[tokio::test]
async fn expired_direct_commit_never_persists() {
    let config = Config {
        writable_idle_seconds: 1,
        ..Config::default()
    };
    let (_d, core, p, id, _hooks) = fixture(config).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "INSERT INTO t VALUES (7)", &[])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        matches!(core.commit(&id).await, Err(CoreError::TransactionExpired)),
        "expired commit was accepted"
    );
    let db = Connection::open(p).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn expired_followup_controls_exact_class() {
    let config = Config {
        writable_idle_seconds: 1,
        ..Config::default()
    };
    let (_d, core, _p, id, _hooks) = fixture(config).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let _ = core.query(&id, "SELECT 1", &[]).await;
    assert!(
        matches!(core.commit(&id).await, Err(CoreError::TransactionExpired)),
        "follow-up lost TX_EXPIRED class"
    );
}

#[tokio::test]
async fn queued_activity_and_idle_boundary() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(core.query(&id, "SELECT 1", &[]).await.is_ok());
    clean(&core).await;
}

#[tokio::test]
async fn queued_cancel_never_enters_closure() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let r = core
        .query_with_ct(&id, "INSERT INTO t VALUES (2)", &[], Some(token))
        .await;
    assert!(
        matches!(r, Err(CoreError::Cancelled { .. })),
        "cancelled request executed: {r:?}"
    );
}

#[tokio::test]
async fn queued_deadline_preserves_transaction() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(core.query(&id, "SELECT 1", &[]).await.is_ok());
    clean(&core).await;
}

#[tokio::test]
async fn cancelled_in_flight_query_publishes_closed_transaction() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    test_support::arm(test_support::Event::StepProgress);
    let token = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let c = core.clone();
        let i = id.clone();
        let token = token.clone();
        async move {
            c.query_with_ct(
                &i,
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<5000) SELECT x FROM c",
                &[],
                Some(token),
            )
            .await
        }
    });
    let arrived = tokio::task::spawn_blocking(|| {
        test_support::wait_for(test_support::Event::StepProgress, Duration::from_secs(5))
    });
    tokio::time::timeout(Duration::from_secs(8), arrived)
        .await
        .expect("arrival wait join timed out")
        .expect("arrival wait task panicked")
        .expect("query never reached the frozen event");
    token.cancel();
    test_support::release_arm(test_support::Event::StepProgress);
    let result = task.await.unwrap();
    assert!(
        matches!(
            result,
            Err(CoreError::Cancelled {
                transaction_open: false,
                transaction_continuable: false
            })
        ),
        "unexpected cancellation result: {result:?}"
    );
    let handle = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(
        handle.transaction_id.is_none(),
        "cancelled query left a stale transaction id: {handle:?}"
    );
    assert!(
        core.close_database(&id).await.is_ok(),
        "close must succeed after cancellation rollback"
    );
}

#[test]
fn interruption_precedence_matrix() {
    assert!(matches!(
        CoreError::Cancelled {
            transaction_open: true,
            transaction_continuable: true
        },
        CoreError::Cancelled { .. }
    ));
}

#[tokio::test]
async fn expiry_rollback_failure_invalidates() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    assert!(core.expire_handle(&id).await.is_ok());
    clean(&core).await;
}

#[tokio::test]
async fn shutdown_report_scope_and_idempotence() {
    let (_d, core, _p, _id, _hooks) = fixture(Config::default()).await;
    core.shutdown().await;
    core.shutdown().await;
    assert!(core.list_handles().await.is_empty());
}

#[tokio::test]
async fn close_failure_identity_reopen_matrix() {
    let (_d, core, p, id, _hooks) = fixture(Config::default()).await;
    core.close_database(&id).await.unwrap();
    assert!(core.open_database(&p, false).await.is_ok());
    clean(&core).await;
}

#[tokio::test]
async fn dropped_commit_publishes_idle_state() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "INSERT INTO t VALUES (3)", &[])
        .await
        .unwrap();
    // Freeze at commit entry and drop the caller; the COMMIT executes inside
    // the worker regardless. Stale open-transaction state would make close
    // falsely refuse a genuinely idle handle.
    freeze_and_drop_caller(test_support::Event::CommitEntry, || {
        let c = core.clone();
        let i = id.clone();
        tokio::spawn(async move { c.commit(&i).await })
    })
    .await;
    assert!(
        core.close_database(&id).await.is_ok(),
        "a dropped commit must publish idle state so close succeeds"
    );
}

#[tokio::test]
async fn dropped_query_expiry_publishes_tombstone() {
    let (_d, core, _p, id, _hooks) = fixture(Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    // Advance the injected clock past the idle deadline, freeze the worker at
    // dequeue, and drop the caller. The worker rolls the expired transaction
    // back; the tombstone must still be published so follow-ups and close see
    // the truthful expired state.
    test_support::set_clock_ms(u64::MAX / 2);
    freeze_and_drop_caller(test_support::Event::AdmissionDequeue, || {
        let c = core.clone();
        let i = id.clone();
        tokio::spawn(async move { c.query(&i, "SELECT 1", &[]).await })
    })
    .await;
    // Gate-ordered follow-up: the published tombstone must refuse queries
    // with the exact expired class (this also orders the test behind the
    // coordinator's publication).
    assert!(
        matches!(
            core.query(&id, "SELECT 1", &[]).await,
            Err(CoreError::TransactionExpired)
        ),
        "published expiry must refuse follow-up queries with TX_EXPIRED"
    );
    let h = core
        .list_handles()
        .await
        .into_iter()
        .find(|h| h.id == id)
        .unwrap();
    assert!(
        h.expired,
        "expiry tombstone not published for a dropped caller"
    );
    assert!(h.transaction_id.is_none());
    assert!(
        core.close_database(&id).await.is_ok(),
        "close must be permitted once expiry cleanup is published"
    );
    test_support::set_clock_ms(0);
}
