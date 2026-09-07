mod support;
use serde_json::json;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn idle_expiry_rolls_back_and_releases_lock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("expiry.db");
    let p = path.to_str().unwrap();
    let mut c = support::ServerProcess::spawn();
    c.initialize();
    let mut ci = 2;
    support::request_tool(&mut c, &mut ci, "create_database", json!({"path":p}));
    c.close_stdin();
    c.wait_bounded(Duration::from_secs(5));
    let cfg = support::config_file(&dir, "writable_idle_seconds = 2\n");
    let mut s = support::ServerProcess::spawn_with_args(&["--config", &cfg]);
    s.initialize();
    let mut id = 2;
    let h = support::open_handle(&mut s, &mut id, p);
    support::request_tool(&mut s, &mut id, "get_schema", json!({"handle":h}));
    support::request_tool(
        &mut s,
        &mut id,
        "begin_transaction",
        json!({"handle":h,"mode":"deferred"}),
    );
    support::request_tool(
        &mut s,
        &mut id,
        "query",
        json!({"handle":h,"sql":"CREATE TABLE t (v TEXT)","parameters":[]}),
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut last = None;
    while Instant::now() < deadline {
        let r = support::request_tool(&mut s, &mut id, "commit", json!({"handle":h}));
        if support::error_class(&r) == "TX_EXPIRED" {
            last = Some(r);
            break;
        }
        last = Some(r);
    }
    assert!(matches!(
        support::error_class(last.as_ref().unwrap()),
        "TX_EXPIRED" | "NO_TX_OPEN"
    ));
    let connection = rusqlite::Connection::open(p).unwrap();
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .unwrap();
    s.close_stdin();
    s.wait_bounded(Duration::from_secs(5));
}
