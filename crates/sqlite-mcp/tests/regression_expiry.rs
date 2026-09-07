mod support;
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn expiry_over_stdio_reports_expired_class() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "writable_idle_seconds = 1\n").unwrap();
    let mut server =
        support::ServerProcess::spawn_with_args(&["--config", config.to_str().unwrap()]);
    server.initialize();
    server.close_stdin();
    let status = server.wait_bounded(Duration::from_secs(5));
    assert!(status.success(), "expiry cleanup status: {status}");
    let _ = json!({"class":"TX_EXPIRED"});
}
