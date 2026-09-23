mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn stdio_protocol_scenario() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("scenario.db");
    let mut server = support::ServerProcess::spawn();
    assert!(server.initialize().get("result").is_some());
    let listed = server.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools.iter().map(|v| v["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "begin_transaction",
            "close_database",
            "commit",
            "create_database",
            "execute_sql_file",
            "extract_sqlite_merge",
            "get_schema",
            "import_sqlite_text",
            "list_handles",
            "open_database",
            "query",
            "query_batch",
            "rollback"
        ]
    );
    let created = server.request(
        3,
        "tools/call",
        json!({"name":"create_database","arguments":{"path":db.to_str().unwrap()}}),
    );
    let structured = &created["result"]["structuredContent"];
    assert!(structured.is_object());
    let text: serde_json::Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, *structured);
    let opened = server.request(
        4,
        "tools/call",
        json!({"name":"open_database","arguments":{"path":db.to_str().unwrap(),"readonly":false}}),
    );
    let handle = opened["result"]["structuredContent"]["handle_state"]["handle"]
        .as_str()
        .or_else(|| opened["result"]["structuredContent"]["result"]["handle"].as_str())
        .unwrap_or_else(|| panic!("open response: {opened}"))
        .to_owned();
    server.request(
        5,
        "tools/call",
        json!({"name":"get_schema","arguments":{"handle":handle}}),
    );
    server.request(
        6,
        "tools/call",
        json!({"name":"begin_transaction","arguments":{"handle":handle,"mode":"deferred"}}),
    );
    let ddl = server.request(7, "tools/call", json!({"name":"query","arguments":{"handle":handle,"sql":"CREATE TABLE t (id INTEGER, name TEXT)","parameters":[]}}));
    support::assert_ok(&ddl);
    // Transaction-local DDL invalidates the schema observation; the
    // documented workflow re-reads the schema before further statements.
    let reobserve = server.request(
        8,
        "tools/call",
        json!({"name":"get_schema","arguments":{"handle":handle}}),
    );
    support::assert_ok(&reobserve);
    let batch_file = dir.path().join("batch.sql");
    std::fs::write(
        &batch_file,
        "INSERT INTO t VALUES (1, 'one'); SELECT id, name FROM t",
    )
    .unwrap();
    let insert = server.request(
        9,
        "tools/call",
        json!({"name":"execute_sql_file","arguments":{"handle":handle,"sql_path":batch_file.to_str().unwrap()}}),
    );
    support::assert_ok(&insert);
    assert_eq!(
        insert["result"]["structuredContent"]["result"]["results"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    server.request(
        10,
        "tools/call",
        json!({"name":"commit","arguments":{"handle":handle}}),
    );
    server.request(
        11,
        "tools/call",
        json!({"name":"get_schema","arguments":{"handle":handle}}),
    );
    server.request(
        12,
        "tools/call",
        json!({"name":"begin_transaction","arguments":{"handle":handle}}),
    );
    let read = server.request(13, "tools/call", json!({"name":"query","arguments":{"handle":handle,"sql":"SELECT id, name FROM t","parameters":[]}}));
    support::assert_ok(&read);
    assert_eq!(
        read["result"]["structuredContent"]["result"]["rows_returned"]
            .as_i64()
            .or_else(|| read["result"]["structuredContent"]["rows_returned"].as_i64()),
        Some(1),
        "committed row must be visible after reopen: {read}"
    );
    server.request(
        14,
        "tools/call",
        json!({"name":"rollback","arguments":{"handle":handle}}),
    );
    server.close_stdin();
    let _ = server.wait_bounded(Duration::from_secs(5));
    assert!(!server.stderr().contains("CREATE TABLE"));
}
