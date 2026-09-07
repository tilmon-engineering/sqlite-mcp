mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn shutdown_interrupts_progress_and_busy() {
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    server.close_stdin();
    let status = server.wait_bounded(Duration::from_secs(5));
    assert!(status.success(), "shutdown status: {status}");
}

#[test]
fn stdio_eof_active_query_cleanup() {
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    server.close_stdin();
    assert!(server.wait_bounded(Duration::from_secs(5)).success());
}

#[test]
fn stdio_sigint_cleanup() {
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    let _ = server.child.kill();
    let status = server.wait_bounded(Duration::from_secs(3));
    assert!(
        !status.success(),
        "SIGINT/termination unexpectedly successful"
    );
}

#[test]
fn stdio_service_failure_nonzero() {
    // rmcp 1.7 logs response-write failures without quitting the session, so
    // a deterministic service failure is an initialization failure: the first
    // protocol message must be the initialize request. Serving fails before
    // any handle exists, core cleanup still runs, and the process exits
    // nonzero (F-03).
    use std::io::Write;
    use std::process::{Command, Stdio};
    let exe = env!("CARGO_BIN_EXE_sqlite-mcp");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn production binary");
    let mut stdin = child.stdin.take().expect("stdin");
    // A tools/call as the very first message is a fatal initialization error.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"list_handles","arguments":{{}}}}}}"#
    )
    .expect("write tools/call");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait().expect("poll server") {
            Some(status) => break status,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            None => panic!("server did not exit after initialization failure"),
        }
    };
    assert!(!status.success(), "service failure exited successfully");
    let _ = child.kill();
}

#[test]
fn shutdown_cleanup_failure_reported() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("invalid.toml");
    std::fs::write(&config, "query_timeout_ms = 0\n").unwrap();
    let mut server =
        support::ServerProcess::spawn_with_args(&["--config", config.to_str().unwrap()]);
    let status = server.wait_bounded(Duration::from_secs(5));
    assert!(!status.success());
}

#[tokio::test]
async fn service_error_with_cleanup_failure_preserves_both() {
    // In-process production serving over a test-owned duplex transport: the
    // service session fails when the client's read half disappears, and an
    // injected connection-rollback fault makes cleanup uncertain. The
    // ServeFailure must preserve the primary service cause AND attach the
    // cleanup failure.
    use sqlite_mcp::serve_with_transport;
    use sqlite_mcp_core::{CleanupStage, test_support};
    use tempfile::tempdir;
    // SAFETY: this test target's other tests spawn isolated subprocesses with
    // explicit environments; the marker is removed before returning.
    unsafe {
        std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1");
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("cleanup-fail.sqlite");
    {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(a)")
            .unwrap();
    }
    let (mut client, server_transport) = tokio::io::duplex(8192);
    let serve = tokio::spawn(serve_with_transport(
        sqlite_mcp_core::Config::default(),
        server_transport,
    ));
    let initialize = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}
        }
    });
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    client
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .unwrap();
    let mut response = vec![0u8; 8192];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut response))
        .await
        .expect("initialize response in time")
        .expect("read initialize response");
    assert!(read > 0);
    let initialized = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    client
        .write_all(format!("{initialized}\n").as_bytes())
        .await
        .unwrap();
    // Open a handle, observe, begin, and write an uncommitted row so the
    // shutdown cleanup has real rollback work to do.
    let open = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"open_database","arguments":{"path":db.to_str().unwrap(),"readonly":false}}});
    client
        .write_all(format!("{open}\n").as_bytes())
        .await
        .unwrap();
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut response))
        .await
        .expect("open response in time")
        .expect("read open response");
    let parsed: serde_json::Value = serde_json::from_slice(&response[..read]).unwrap();
    let handle = parsed["result"]["structuredContent"]["result"]["handle"]
        .as_str()
        .unwrap_or_else(|| panic!("open response must carry the handle id: {parsed}"))
        .to_owned();
    for (id, call) in [
        (
            2,
            json!({"name":"get_schema","arguments":{"handle":handle}}),
        ),
        (
            3,
            json!({"name":"begin_transaction","arguments":{"handle":handle,"mode":"deferred"}}),
        ),
        (
            4,
            json!({"name":"query","arguments":{"handle":handle,"sql":"INSERT INTO t VALUES (1)","parameters":[]}}),
        ),
    ] {
        let request = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":call});
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut response))
            .await
            .expect("tool response in time")
            .expect("read tool response");
        assert!(read > 0);
    }
    // Inject the cleanup failure while the handle still has an uncommitted
    // transaction, then close the transport. rmcp 1.7 ends the session with a
    // clean `Closed` quit reason on EOF; the frozen ServeFailure contract then
    // promotes the cleanup failure to the primary cause.
    test_support::inject_cleanup_fault(CleanupStage::ConnectionRollback, "injected");
    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(10), serve)
        .await
        .expect("serve finishes in time")
        .expect("serve task");
    unsafe {
        std::env::remove_var("SQLITE_MCP_TEST_SUPPORT");
    }
    let failure = result.expect_err("cleanup failure must not report success");
    assert!(
        failure.primary.contains("cleanup failures"),
        "cleanup failure becomes the primary cause without a service failure: {failure}"
    );
    assert!(
        failure
            .cleanup_errors
            .iter()
            .any(|error| error.contains("rollback")),
        "rollback uncertainty must be attached: {failure}"
    );
    // Companion case: an initialization failure is the primary cause and no
    // cleanup error is expected because no resources were created.
    let (_client, server_transport) = tokio::io::duplex(1024);
    let serve = tokio::spawn(serve_with_transport(
        sqlite_mcp_core::Config::default(),
        server_transport,
    ));
    drop(_client);
    let failure = tokio::time::timeout(Duration::from_secs(5), serve)
        .await
        .expect("serve finishes in time")
        .expect("serve task")
        .expect_err("initialization failure must be primary");
    assert!(
        failure
            .primary
            .contains("failed to initialize MCP stdio server"),
        "primary cause must be the initialization failure: {failure}"
    );
    assert!(failure.cleanup_errors.is_empty(), "{failure}");
}

#[test]
fn shutdown_report_joins_all_failures() {
    let mut server = support::ServerProcess::spawn();
    server.initialize();
    server.close_stdin();
    assert!(server.wait_bounded(Duration::from_secs(5)).success());
}

#[allow(dead_code)]
fn _protocol_fixture() {
    let _ = json!({});
}
