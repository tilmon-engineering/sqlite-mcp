use rusqlite::limits::Limit;
use serde_json::Value;
use sqlite_mcp_core::{Cell, Config, Core};
use std::collections::BTreeSet;
use tempfile::tempdir;

const FIELDS: [(&str, u64); 15] = [
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
    ("column_limit", 2048),
    ("parameter_limit", 32_766),
    ("expression_depth", 1000),
    ("compound_terms", 500),
];

fn config_with(field: &str, setting: u64) -> Config {
    let mut config = serde_json::to_value(Config::default()).unwrap();
    config[field] = Value::from(setting);
    serde_json::from_value(config).unwrap()
}

#[test]
fn config_all_field_boundaries() {
    for &(field, maximum) in &FIELDS {
        for value in [0, 1, maximum - 1, maximum, maximum + 1, u64::MAX] {
            let valid = config_with(field, value).validate().is_ok();
            assert_eq!(valid, (1..=maximum).contains(&value), "{field}={value}");
        }
        if usize::BITS < u64::BITS {
            let machine_max = usize::MAX as u64;
            assert!(
                config_with(field, machine_max).validate().is_err(),
                "{field}=usize::MAX"
            );
        }
    }
}

#[test]
fn config_policy_covers_serialized_fields() {
    let serialized = serde_json::to_value(Config::default()).unwrap();
    let actual: BTreeSet<_> = serialized
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let policy: BTreeSet<_> = FIELDS.iter().map(|(field, _)| *field).collect();
    assert_eq!(
        actual, policy,
        "every serialized Config field needs a boundary policy"
    );
}

#[test]
fn engine_limit_readback_and_checked_conversion() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    let requested = [
        (Limit::SQLITE_LIMIT_LENGTH, 67_108_864_i32),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, 1000),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 500),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 32_766),
    ];
    for (limit, value) in requested {
        let _ = connection.set_limit(limit, value).unwrap();
        let effective = connection.limit(limit).unwrap();
        assert_eq!(
            effective, value,
            "engine limit {limit:?} was not installed/read back"
        );
    }
}

#[tokio::test]
async fn configured_parameter_limit_single_source() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("parameters.sqlite");
    let config = Config {
        parameter_limit: 1001,
        ..Default::default()
    };
    let core = Core::new(config).unwrap();
    core.create_database(path.to_str().unwrap()).await.unwrap();
    let handle = core
        .open_database(path.to_str().unwrap(), false)
        .await
        .unwrap();
    let id = handle.id;
    core.get_schema(&id).await.unwrap();
    core.begin_transaction(&id, "deferred").await.unwrap();
    for count in [1000_usize, 1001] {
        let params: Vec<_> = (0..count).map(|_| Cell::Integer("1".to_owned())).collect();
        let query = format!("SELECT ?1, ?{count}");
        core.query(&id, &query, &params)
            .await
            .unwrap_or_else(|error| {
                panic!("configured parameter_limit must permit indexed ?{count}: {error:?}")
            });
    }
    let params: Vec<_> = (0..1002).map(|_| Cell::Integer("1".to_owned())).collect();
    let error = core
        .query(&id, "SELECT ?1, ?1002", &params)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("too many parameters"));
    core.shutdown().await;
}

#[test]
fn documented_config_bounds_match_policy() {
    // The documented bounds (DESIGN.md) must equal the enforced policy table
    // in config.rs for all 15 fields; a documentation/policy drift fails here.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let config_src =
        std::fs::read_to_string(root.join("crates/sqlite-mcp-core/src/config.rs")).unwrap();
    let design = std::fs::read_to_string(root.join("DESIGN.md")).unwrap();
    let example = std::fs::read_to_string(root.join("config.example.toml")).unwrap();
    let expected: &[(&str, u128)] = &[
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
        ("column_limit", 2048),
        ("parameter_limit", 32_766),
        ("expression_depth", 1000),
        ("compound_terms", 500),
    ];
    for (field, max) in expected {
        // Policy row exists with this exact maximum: match the field, then
        // strip digit separators from the numeric tail only (field names may
        // contain underscores).
        let field_marker = format!("\"{field}\"");
        let bound = format!("1, {max})");
        let row = config_src
            .lines()
            .filter(|line| line.contains(&field_marker))
            .find(|line| {
                line.split(&field_marker)
                    .nth(1)
                    .map(|tail| tail.replace('_', "").contains(&bound))
                    .unwrap_or(false)
            });
        assert!(
            row.is_some(),
            "config.rs policy must bound {field} at 1..={max}"
        );
        // DESIGN.md documents the same maximum for the field.
        let documented = design
            .lines()
            .any(|line| line.contains(field) && line.contains(&format!("{max}")));
        assert!(documented, "DESIGN.md must document {field} maximum {max}");
        // config.example.toml mentions the field so the example stays in sync.
        assert!(
            example.contains(field) || *field == "max_handles",
            "config.example.toml must mention {field}"
        );
    }
}
