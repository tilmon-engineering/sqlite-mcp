use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::tempdir;

#[test]
fn invalid_config_fails_before_serving() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid.toml");
    std::fs::write(&path, "unknown_key = true\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sqlite-mcp"))
        .args(["--config", path.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "invalid config child did not exit"
        );
        std::thread::yield_now();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "invalid config wrote protocol output"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid"), "diagnostic missing: {stderr}");
}
