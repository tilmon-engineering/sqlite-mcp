mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn stdio_shutdown_rolls_back_and_releases_lock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("shutdown.db");
    let p = path.to_str().unwrap();
    let mut s = support::ServerProcess::spawn();
    s.initialize();
    let mut id = 2;
    support::request_tool(&mut s, &mut id, "create_database", json!({"path":p}));
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
    support::request_tool(
        &mut s,
        &mut id,
        "query",
        json!({"handle":h,"sql":"INSERT INTO t VALUES ('gone')","parameters":[]}),
    );
    s.close_stdin();
    s.wait_bounded(Duration::from_secs(5));
    let connection = rusqlite::Connection::open(&path).unwrap();
    let table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='t'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 0);
    drop(connection);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .unwrap();
}
