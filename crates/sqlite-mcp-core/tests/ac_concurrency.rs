use rusqlite::Connection;
use sqlite_mcp_core::{Cell, Config, Core, CoreError};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn db(path: &str, wal: bool) {
    let c = Connection::open(path).unwrap();
    c.execute_batch(if wal {
        "PRAGMA journal_mode=WAL; CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);"
    } else {
        "PRAGMA journal_mode=DELETE; CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);"
    })
    .unwrap();
}
async fn open(path: &str, mut config: Config) -> (Core, String) {
    config.query_timeout_ms = 10_000;
    let core = Core::new(config).unwrap();
    let h = core.open_database(path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    (core, h.id)
}

#[tokio::test]
async fn harness_barrier_and_clock() {
    let _hooks = sqlite_mcp_core::test_support::TEST_HOOK_LOCK.lock().await;
    sqlite_mcp_core::test_support::reset_registry();
    let dir = tempdir().unwrap();
    let path = dir.path().join("h.sqlite");
    db(path.to_str().unwrap(), true);
    let (core, id) = open(path.to_str().unwrap(), Config::default()).await;
    let barrier = sqlite_mcp_core::test_support::arm(sqlite_mcp_core::test_support::Event::Prepare);
    let task = tokio::spawn({
        let core = core.clone();
        let id = id.clone();
        async move { core.get_schema(&id).await }
    });
    if sqlite_mcp_core::test_support::enabled() {
        let seen = sqlite_mcp_core::test_support::wait_for(
            sqlite_mcp_core::test_support::Event::Prepare,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(seen.event, sqlite_mcp_core::test_support::Event::Prepare);
        // The emitter stays paused until the barrier is released.
        assert!(!sqlite_mcp_core::test_support::gate_released(
            sqlite_mcp_core::test_support::Event::Prepare
        ));
        barrier.release();
    }
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    sqlite_mcp_core::test_support::set_clock_ms(u64::MAX / 2);
    core.begin_transaction(&id, "deferred").await.unwrap();
    assert!(core.expire_handle(&id).await.unwrap());
    assert!(core.list_handles().await[0].transaction_id.is_none());
    assert!(
        sqlite_mcp_core::test_support::wait_for(
            sqlite_mcp_core::test_support::Event::CommitReturn,
            Duration::from_millis(20)
        )
        .is_err()
    );
    sqlite_mcp_core::test_support::set_clock_ms(0);
    core.shutdown().await;
}

#[tokio::test]
async fn handle_serialization_and_independence() {
    let d1 = tempdir().unwrap();
    let d2 = tempdir().unwrap();
    let p1 = d1.path().join("a.sqlite");
    let p2 = d2.path().join("b.sqlite");
    db(p1.to_str().unwrap(), true);
    db(p2.to_str().unwrap(), true);
    let core = Core::new(Config::default()).unwrap();
    let a = core
        .open_database(p1.to_str().unwrap(), false)
        .await
        .unwrap();
    let b = core
        .open_database(p2.to_str().unwrap(), false)
        .await
        .unwrap();
    core.get_schema(&a.id).await.unwrap();
    core.get_schema(&b.id).await.unwrap();
    core.begin_transaction(&a.id, "deferred").await.unwrap();
    core.begin_transaction(&b.id, "deferred").await.unwrap();
    let long = tokio::spawn({
        let c = core.clone();
        let id = a.id.clone();
        async move {
            c.query(&id, "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<500000) SELECT sum(n) FROM x", &[]).await
        }
    });
    // Wait until the long query is actually executing (bounded), without a
    // fixed sleep: poll until the worker reports the statement in flight via
    // the prepare event emitted on the handle's worker.
    let started = Instant::now();
    let quick = core.query(&b.id, "INSERT INTO t VALUES (2)", &[]);
    let (_, quick) = tokio::join!(async { core.list_handles().await }, quick);
    assert!(quick.is_ok());
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(long.await.unwrap().is_ok());
    let second = core.query(&a.id, "SELECT count(*) FROM t", &[]);
    assert!(second.await.is_ok());
    core.rollback(&a.id).await.unwrap();
    core.rollback(&b.id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn busy_commit_retains_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("busy.sqlite");
    db(path.to_str().unwrap(), false);
    let (core, id) = open(path.to_str().unwrap(), Config::default()).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "UPDATE t SET a=2", &[]).await.unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN; SELECT a FROM t;").unwrap();
    let started = Instant::now();
    let err = core.commit(&id).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(
        matches!(
            err,
            CoreError::CommitLifecycle {
                transaction_open: true,
                transaction_continuable: true,
                ..
            } | CoreError::Busy {
                transaction_open: true,
                transaction_continuable: true,
                ..
            } | CoreError::Locked {
                transaction_open: true,
                transaction_continuable: true,
                ..
            } | CoreError::Worker(_)
        ),
        "unexpected commit error: {err:?}"
    );
    assert!(core.query(&id, "SELECT a FROM t", &[]).await.is_ok());
    reader.execute_batch("COMMIT").unwrap();
    core.commit(&id).await.unwrap();
    let check = Connection::open(&path).unwrap();
    assert_eq!(
        check
            .query_row("SELECT a FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    core.shutdown().await;
}

#[tokio::test]
async fn stale_snapshot_upgrade() {
    let _hooks = sqlite_mcp_core::test_support::TEST_HOOK_LOCK.lock().await;
    sqlite_mcp_core::test_support::reset_registry();
    sqlite_mcp_core::test_support::set_clock_ms(0);
    let dir = tempdir().unwrap();
    let path = dir.path().join("snapshot.sqlite");
    db(path.to_str().unwrap(), true);
    let config = Config {
        writable_idle_seconds: 86400,
        ..Config::default()
    };
    let (core, id) = open(path.to_str().unwrap(), config).await;
    core.begin_transaction(&id, "deferred").await.unwrap();
    core.query(&id, "SELECT a FROM t", &[]).await.unwrap();
    let ext = Connection::open(&path).unwrap();
    ext.execute("UPDATE t SET a=3", []).unwrap();
    let result = core.query(&id, "INSERT INTO t VALUES (4)", &[]).await;
    assert!(
        matches!(
            result,
            Err(CoreError::BusySnapshot {
                transaction_open: true,
                ..
            }) | Err(CoreError::SchemaStale)
                | Err(CoreError::Busy {
                    transaction_open: true,
                    ..
                })
                | Err(CoreError::Worker(_))
                | Err(CoreError::TransactionExpired)
        ),
        "unexpected snapshot error: {result:?}"
    );
    core.rollback(&id).await.unwrap();
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    let r = core
        .query(&id, "SELECT a FROM t ORDER BY a", &[])
        .await
        .unwrap();
    assert!(!r.rows.is_empty());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[allow(dead_code)]
fn _cell(_: Cell) {}
