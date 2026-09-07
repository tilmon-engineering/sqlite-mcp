mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn process_kill_releases_writer_and_rolls_back() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("kill.db");
    let path = p.to_str().unwrap();
    let mut creator = support::ServerProcess::spawn();
    creator.initialize();
    let mut id = 2;
    support::request_tool(
        &mut creator,
        &mut id,
        "create_database",
        json!({"path":path}),
    );
    creator.close_stdin();
    creator.wait_bounded(Duration::from_secs(5));
    let mut s = support::ServerProcess::spawn();
    s.initialize();
    let mut id = 2;
    let h = support::open_handle(&mut s, &mut id, path);
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
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .unwrap();
}
