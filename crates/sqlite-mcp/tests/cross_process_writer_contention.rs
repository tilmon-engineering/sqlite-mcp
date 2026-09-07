mod support;
use rusqlite::Connection;
use serde_json::json;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn cross_process_writer_contention() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("contention.db");
    let path = db.to_str().unwrap();
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    let mut id = 2;
    support::request_tool(
        &mut server,
        &mut id,
        "create_database",
        json!({"path":path}),
    );
    let schema_connection = Connection::open(path).unwrap();
    schema_connection
        .execute("CREATE TABLE t (v TEXT)", [])
        .unwrap();
    drop(schema_connection);
    let handle = support::open_handle(&mut server, &mut id, path);
    support::request_tool(&mut server, &mut id, "get_schema", json!({"handle":handle}));
    support::request_tool(
        &mut server,
        &mut id,
        "begin_transaction",
        json!({"handle":handle,"mode":"immediate"}),
    );
    let inserted = support::request_tool(
        &mut server,
        &mut id,
        "query",
        json!({"handle":handle,"sql":"INSERT INTO t VALUES ('from-a')","parameters":[]}),
    );
    assert!(
        !inserted["result"]["isError"].as_bool().unwrap_or(true),
        "{inserted}"
    );

    let competing = Connection::open(path).unwrap();
    let started = Instant::now();
    let result = competing.execute_batch("PRAGMA busy_timeout=25; BEGIN IMMEDIATE");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        result.is_err(),
        "competing writer unexpectedly acquired lock"
    );
    drop(competing);

    let committed = support::request_tool(&mut server, &mut id, "commit", json!({"handle":handle}));
    support::assert_ok(&committed);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut value: Option<String> = None;
    while Instant::now() < deadline {
        let connection = Connection::open(path).unwrap();
        value = connection
            .query_row("SELECT v FROM t", [], |row| row.get(0))
            .ok();
        if value.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(value.as_deref(), Some("from-a"));
    server.close_stdin();
    server.wait_bounded(Duration::from_secs(5));
}
