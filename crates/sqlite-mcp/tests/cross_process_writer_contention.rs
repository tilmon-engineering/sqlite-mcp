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

#[test]
fn busy_wait_ms_bounds_reported_busy() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("busy-bound.db");
    let path = db.to_str().unwrap().to_owned();
    {
        let connection = Connection::open(&db).unwrap();
        connection.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
    }
    let config = support::config_file(&dir, "busy_wait_ms = 400\nquery_timeout_ms = 15000\n");
    let mut server = support::ServerProcess::spawn_with_args(&["--config", &config]);
    server.initialize();
    let mut id = 2;
    let handle = support::open_handle(&mut server, &mut id, &path);
    support::request_tool(&mut server, &mut id, "get_schema", json!({"handle":handle}));
    // An external process (relative to the server) holds the write lock.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let begin = support::request_tool(
        &mut server,
        &mut id,
        "begin_transaction",
        json!({"handle":handle,"mode":"immediate"}),
    );
    let elapsed = started.elapsed();
    assert_eq!(support::error_class(&begin), "BUSY", "{begin}");
    let error = &begin["result"]["structuredContent"]["error"];
    assert_eq!(
        error["transaction_open"].as_bool(),
        Some(false),
        "failed begin must report a closed transaction: {begin}"
    );
    // The 15s deadline must not be the bound: the busy budget is.
    assert!(
        elapsed >= Duration::from_millis(350),
        "BEGIN IMMEDIATE gave up before the busy budget: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "BEGIN IMMEDIATE waited past the busy budget: {elapsed:?}"
    );
    drop(holder);
    // After release the retry acquires the lock.
    let retry = support::request_tool(
        &mut server,
        &mut id,
        "begin_transaction",
        json!({"handle":handle,"mode":"immediate"}),
    );
    support::assert_ok(&retry);
    let rollback =
        support::request_tool(&mut server, &mut id, "rollback", json!({"handle":handle}));
    support::assert_ok(&rollback);
    server.close_stdin();
    server.wait_bounded(Duration::from_secs(5));
}

#[test]
fn commit_under_contention_reports_busy_within_bound() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("commit-contention.db");
    let path = db.to_str().unwrap().to_owned();
    {
        // Rollback-journal mode: a reader's SHARED lock blocks COMMIT's
        // EXCLUSIVE acquisition (a server writer in WAL mode owns the write
        // lock and cannot be blocked by a reader).
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE t (v TEXT)")
            .unwrap();
    }
    let config = support::config_file(&dir, "busy_wait_ms = 400\nquery_timeout_ms = 15000\n");
    let mut server = support::ServerProcess::spawn_with_args(&["--config", &config]);
    server.initialize();
    let mut id = 2;
    let handle = support::open_handle(&mut server, &mut id, &path);
    support::request_tool(&mut server, &mut id, "get_schema", json!({"handle":handle}));
    support::request_tool(
        &mut server,
        &mut id,
        "begin_transaction",
        json!({"handle":handle,"mode":"deferred"}),
    );
    let insert = support::request_tool(
        &mut server,
        &mut id,
        "query",
        json!({"handle":handle,"sql":"INSERT INTO t VALUES ('uncommitted')","parameters":[]}),
    );
    support::assert_ok(&insert);
    // The external reader holds a SHARED lock so the server COMMIT needs the
    // blocked EXCLUSIVE lock.
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let read: i64 = reader
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(read, 0);
    let started = Instant::now();
    let commit = support::request_tool(&mut server, &mut id, "commit", json!({"handle":handle}));
    let elapsed = started.elapsed();
    assert_eq!(support::error_class(&commit), "BUSY", "{commit}");
    let error = &commit["result"]["structuredContent"]["error"];
    assert_eq!(
        error["transaction_open"].as_bool(),
        Some(true),
        "failed commit must preserve the transaction: {commit}"
    );
    assert_eq!(
        error["transaction_continuable"].as_bool(),
        Some(true),
        "failed commit must keep the transaction continuable: {commit}"
    );
    assert!(
        elapsed >= Duration::from_millis(350),
        "COMMIT gave up before the busy budget: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "COMMIT waited past the busy budget: {elapsed:?}"
    );
    drop(reader);
    let retry = support::request_tool(&mut server, &mut id, "commit", json!({"handle":handle}));
    support::assert_ok(&retry);
    let connection = Connection::open(&path).unwrap();
    let value: String = connection
        .query_row("SELECT v FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "uncommitted");
    server.close_stdin();
    server.wait_bounded(Duration::from_secs(5));
}
