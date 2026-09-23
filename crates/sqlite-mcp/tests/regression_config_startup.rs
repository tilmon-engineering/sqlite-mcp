use std::{
    process::Command,
    time::{Duration, Instant},
};
use tempfile::tempdir;

const FIELDS: [(&str, u64); 17] = [
    ("max_handles", 1024),
    ("queue_capacity", 4096),
    ("query_timeout_ms", 300_000),
    ("writable_idle_seconds", 86_400),
    ("readonly_idle_seconds", 604_800),
    ("result_row_limit", 100_000),
    ("result_byte_limit", 67_108_864),
    ("schema_byte_limit", 67_108_864),
    ("cell_byte_limit", 67_108_864),
    ("busy_wait_ms", 60_000),
    ("sql_byte_limit", 1_048_576),
    ("batch_sql_byte_limit", 16_777_216),
    ("batch_statement_limit", 100_000),
    ("column_limit", 2048),
    ("parameter_limit", 32_766),
    ("expression_depth", 1000),
    ("compound_terms", 500),
];

fn run_config(contents: &str) -> std::process::Output {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid.toml");
    std::fs::write(&path, contents).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sqlite-mcp"))
        .args(["--config", path.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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
    child.wait_with_output().unwrap()
}

fn assert_rejected(contents: &str, label: &str) {
    let output = run_config(contents);
    assert!(!output.status.success(), "{label} unexpectedly accepted");
    assert!(output.stdout.is_empty(), "{label} emitted protocol bytes");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid") || stderr.contains("unknown"),
        "{label} lacks useful stderr: {stderr}"
    );
}

#[test]
fn invalid_config_matrix_before_serving() {
    for &(field, maximum) in &FIELDS {
        assert_rejected(&format!("{field} = 0\n"), &format!("{field}=0"));
        assert_rejected(
            &format!("{field} = {}\n", maximum + 1),
            &format!("{field}=max+1"),
        );
        assert_rejected(
            &format!("{field} = 18446744073709551615\n"),
            &format!("{field}=u64::MAX"),
        );
    }
    let all_zero = FIELDS
        .iter()
        .map(|(field, _)| format!("{field} = 0"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_rejected(&all_zero, "all-zero");
    let all_huge = FIELDS
        .iter()
        .map(|(field, _)| format!("{field} = 18446744073709551615"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_rejected(&all_huge, "all-huge");
    assert_rejected("unknown_config_key = true\n", "unknown-key");

    // example_config loads; historical enormous-clock fixtures must migrate to
    // valid bounded values plus injected clock in the later GREEN change.
    let example = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config.example.toml"
    ))
    .unwrap();
    assert!(
        toml::from_str::<sqlite_mcp_core::Config>(&example)
            .unwrap()
            .validate()
            .is_ok()
    );
}
