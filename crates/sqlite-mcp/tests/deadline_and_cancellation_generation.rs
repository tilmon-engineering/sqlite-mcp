mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn deadline_and_cancellation_do_not_poison_next_generation() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("deadline.db");
    let path = p.to_str().unwrap();
    let mut c = support::ServerProcess::spawn();
    c.initialize();
    let mut ci = 2;
    support::request_tool(&mut c, &mut ci, "create_database", json!({"path":path}));
    c.close_stdin();
    c.wait_bounded(Duration::from_secs(5));
    let cfg = support::config_file(&dir, "query_timeout_ms = 25\n");
    let mut s = support::ServerProcess::spawn_with_args(&["--config", &cfg]);
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
    let expensive = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100000000) SELECT sum(x) FROM c";
    let r = support::request_tool(
        &mut s,
        &mut id,
        "query",
        json!({"handle":h,"sql":expensive,"parameters":[]}),
    );
    assert_eq!(
        support::error_class(&r),
        "DEADLINE_EXCEEDED",
        "deadline expiry must be reported distinctly from client cancellation: {r}"
    );
    // The deadline interrupt aborts the outer transaction; the handle is
    // usable only after re-observation and a fresh begin.
    let after_deadline = support::request_tool(&mut s, &mut id, "commit", json!({"handle":h}));
    assert_eq!(
        support::error_class(&after_deadline),
        "NO_TX_OPEN",
        "{after_deadline}"
    );
    support::request_tool(&mut s, &mut id, "get_schema", json!({"handle":h}));
    support::request_tool(
        &mut s,
        &mut id,
        "begin_transaction",
        json!({"handle":h,"mode":"deferred"}),
    );
    let simple = support::request_tool(
        &mut s,
        &mut id,
        "query",
        json!({"handle":h,"sql":"SELECT 1","parameters":[]}),
    );
    support::assert_ok(&simple);
    // Cancellation segment: roll back this fresh transaction first, then
    // issue the expensive query, cancel it, and verify the truthful outcome.
    support::request_tool(&mut s, &mut id, "rollback", json!({"handle":h}));
    support::request_tool(&mut s, &mut id, "get_schema", json!({"handle":h}));
    support::request_tool(
        &mut s,
        &mut id,
        "begin_transaction",
        json!({"handle":h,"mode":"deferred"}),
    );
    // Cancellation segment: issue the expensive query, cancel it, and verify
    // the truthful outcome plus next-call survival on the same handle.
    let request_id = id;
    s.send_json(json!({"jsonrpc":"2.0","id":request_id,"method":"tools/call","params":{"name":"query","arguments":{"handle":h,"sql":expensive,"parameters":[]}}}));
    s.send_json(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":request_id}}));
    let cancelled = s
        .receive_timeout(Duration::from_secs(10))
        .expect("cancelled request response");
    assert_eq!(
        cancelled["id"],
        serde_json::json!(request_id),
        "{cancelled}"
    );
    assert_eq!(
        support::error_class(&cancelled),
        "CANCELLED",
        "client cancellation must report the CANCELLED class: {cancelled}"
    );
    // The cancelled request's envelope reports the authoritative transaction
    // state; the commit outcome must be consistent with it. When the cancel
    // landed before dispatch (or the interrupt aborted the outer
    // transaction), the reported state and the commit outcome agree.
    let reported_open = cancelled["result"]["structuredContent"]["error"]["transaction_open"]
        .as_bool()
        .expect("cancelled envelope carries transaction state");
    let after_cancel = support::request_tool(&mut s, &mut id, "commit", json!({"handle":h}));
    if reported_open {
        // The cancelled statement never executed and the untouched outer
        // transaction survived: commit truthfully succeeds.
        support::assert_ok(&after_cancel);
    } else {
        assert_eq!(
            support::error_class(&after_cancel),
            "NO_TX_OPEN",
            "{after_cancel}"
        );
    }
    // Same handle remains usable after re-observation and a fresh begin.
    support::request_tool(&mut s, &mut id, "get_schema", json!({"handle":h}));
    support::request_tool(
        &mut s,
        &mut id,
        "begin_transaction",
        json!({"handle":h,"mode":"deferred"}),
    );
    let third = support::request_tool(
        &mut s,
        &mut id,
        "query",
        json!({"handle":h,"sql":"SELECT 2","parameters":[]}),
    );
    support::assert_ok(&third);
    s.close_stdin();
    s.wait_bounded(Duration::from_secs(5));
}
