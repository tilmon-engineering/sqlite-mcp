use base64::Engine;
use rusqlite::Connection;
use serde_json::{Value, json};
use sqlite_mcp_core::{Cell, Config, Core, CoreError};
use tempfile::{TempDir, tempdir};

/// Serializes tests that touch the savepoint boundary: the cleanup-fault
/// registry is process-global, so parallel tests could steal each other's
/// injected faults.
static FAULT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture(config: Config) -> (TempDir, Core, String, String) {
    unsafe { std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1") };
    sqlite_mcp_core::test_support::reset_registry();
    let dir = tempdir().unwrap();
    let core = Core::new(config).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE t(x)")
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    (dir, core, path, h.id)
}

// Deliberately independent from the implementation's QueryResult serializer:
// this is the documented selected payload, columns plus rows only.
fn expected_payload(columns: &[&str], rows: &[Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({ "columns": columns, "rows": rows })).unwrap()
}
fn text_row(s: &str) -> Value {
    json!([{ "type": "text", "value": s }])
}

#[tokio::test]
async fn returning_byte_overflow_atomic() {
    let _faults = FAULT_LOCK.lock().await;
    let (dir, core, path, id) = fixture(Config {
        result_byte_limit: 1,
        ..Default::default()
    })
    .await;
    let err = core
        .query(&id, "INSERT INTO t VALUES ('persist?') RETURNING x", &[])
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::Worker(_)) && err.to_string().contains("result"),
        "expected result overflow, got {err}"
    );
    core.commit(&id).await.unwrap();
    let check = Connection::open(&path).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    core.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn result_payload_exact_boundaries() {
    let _faults = FAULT_LOCK.lock().await;
    let payload = expected_payload(&["x"], &[text_row("é")]);
    let n = payload.len();
    for (budget, succeeds) in [(n - 1, false), (n, true), (n + 1, true)] {
        let (_d, core, _p, id) = fixture(Config {
            result_byte_limit: budget,
            ..Default::default()
        })
        .await;
        let result = core.query(&id, "SELECT 'é' AS x", &[]).await;
        assert_eq!(
            result.is_ok(),
            succeeds,
            "budget={budget}, expected {succeeds}, oracle N={n}, result={result:?}"
        );
        if result.is_ok() {
            core.rollback(&id).await.unwrap();
        }
        core.shutdown().await;
    }
}

#[tokio::test]
async fn result_encoding_boundary_matrix() {
    let _faults = FAULT_LOCK.lock().await;
    let blob = base64::engine::general_purpose::STANDARD.encode([0x80u8, 0xff]);
    let cases = [
        (vec!["x"], vec![text_row(r#"quote \" and \\slash"#)]),
        (vec!["x"], vec![text_row("héllo 世界")]),
        (
            vec!["x"],
            vec![json!([{ "type": "text_bytes", "value": blob }])],
        ),
    ];
    for (columns, rows) in cases {
        let n = expected_payload(&columns, &rows).len();
        let (_d, core, _p, id) = fixture(Config {
            result_byte_limit: n,
            ..Default::default()
        })
        .await;
        let sql = if rows[0][0]["type"] == "text_bytes" {
            "SELECT CAST(x'80FF' AS TEXT) AS x"
        } else if rows[0][0]["value"].as_str().unwrap().contains("世界") {
            "SELECT 'héllo 世界' AS x"
        } else {
            r#"SELECT 'quote \" and \\slash' AS x"#
        };
        assert!(
            core.query(&id, sql, &[]).await.is_ok(),
            "exact independent payload boundary N={n} should succeed"
        );
        core.rollback(&id).await.unwrap();
        core.shutdown().await;
    }
}

#[tokio::test]
async fn returning_row_cap_still_drains() {
    let _faults = FAULT_LOCK.lock().await;
    let (dir, core, path, id) = fixture(Config {
        result_row_limit: 1,
        ..Default::default()
    })
    .await;
    let r = core
        .query(&id, "INSERT INTO t VALUES (1),(2),(3) RETURNING x", &[])
        .await
        .unwrap();
    assert_eq!(r.rows_returned, 1);
    assert!(r.truncated);
    assert!(r.execution_complete);
    core.commit(&id).await.unwrap();
    let check = Connection::open(&path).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM t", [], |x| x.get::<_, i64>(0))
            .unwrap(),
        3
    );
    core.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn overflow_cleanup_failure_invalidates() {
    let _faults = FAULT_LOCK.lock().await;
    let (dir, core, path, id) = fixture(Config {
        result_byte_limit: 1,
        ..Default::default()
    })
    .await;
    sqlite_mcp_core::test_support::inject_cleanup_fault(
        sqlite_mcp_core::CleanupStage::SavepointRestore,
        "injected savepoint restore failure",
    );
    let err = core
        .query(&id, "INSERT INTO t VALUES ('overflow') RETURNING x", &[])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("invalidated"),
        "cleanup failure must propagate: {err}"
    );
    assert!(
        core.list_handles().await.into_iter().all(|h| h.id != id),
        "failed savepoint restore must invalidate the handle"
    );
    assert!(
        sqlite_mcp_core::test_support::take_cleanup_fault(
            sqlite_mcp_core::CleanupStage::SavepointRestore
        )
        .is_none(),
        "fault must be consumed exactly once"
    );
    let check = Connection::open(&path).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    core.shutdown().await;
    drop(dir);
}

#[allow(dead_code)]
fn _cell_oracle_marker() {
    let _ = Cell::Null;
}
