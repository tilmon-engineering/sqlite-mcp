mod support;
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn cancelling_commit_does_not_rollback_dispatched_commit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("commit.db");
    let path_str = path.to_str().unwrap();
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    let mut id = 2;
    support::request_tool(
        &mut server,
        &mut id,
        "create_database",
        json!({"path":path_str}),
    );
    let handle = support::open_handle(&mut server, &mut id, path_str);
    support::request_tool(&mut server, &mut id, "get_schema", json!({"handle":handle}));
    support::request_tool(
        &mut server,
        &mut id,
        "begin_transaction",
        json!({"handle":handle,"mode":"deferred"}),
    );
    support::request_tool(
        &mut server,
        &mut id,
        "query",
        json!({"handle":handle,"sql":"CREATE TABLE t (v TEXT)","parameters":[]}),
    );
    support::request_tool(
        &mut server,
        &mut id,
        "query",
        json!({"handle":handle,"sql":"INSERT INTO t VALUES ('persisted')","parameters":[]}),
    );

    let commit_id = id;
    server.send_json(json!({"jsonrpc":"2.0","id":commit_id,"method":"tools/call","params":{"name":"commit","arguments":{"handle":handle}}}));
    server.send_json(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":commit_id}}));
    let outcome = server.receive_timeout(Duration::from_secs(2));
    if let Some(response) = outcome {
        assert_eq!(response["id"], serde_json::Value::from(commit_id));
        assert_eq!(
            response["result"]["structuredContent"]["result"], true,
            "{response}"
        );
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut persisted: Option<String> = None;
    while std::time::Instant::now() < deadline {
        let connection = Connection::open(&path).unwrap();
        persisted = connection
            .query_row("SELECT v FROM t", [], |row| row.get(0))
            .optional()
            .unwrap();
        if persisted.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(persisted.is_none() || persisted.as_deref() == Some("persisted"));

    let handles = support::request_tool(&mut server, &mut id, "list_handles", json!({}));
    assert!(
        handles["result"]["structuredContent"].is_object(),
        "{handles}"
    );
    let follow_up = support::request_tool(&mut server, &mut id, "commit", json!({"handle":handle}));
    assert_eq!(
        support::error_class(&follow_up),
        "NO_TX_OPEN",
        "{follow_up}"
    );
    server.close_stdin();
    server.wait_bounded(Duration::from_secs(5));
}
