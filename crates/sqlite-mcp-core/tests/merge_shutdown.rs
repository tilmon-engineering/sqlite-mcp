mod support;

use rusqlite::Connection;
use serde_json::json;
use std::fs;
use support::{Fixture, structured};

#[tokio::test]
async fn merge_call_after_shutdown_has_no_side_effects() {
    let fixture = Fixture::new().await;
    let base = fixture.dir.path().join("base.sqlite");
    let ours = fixture.dir.path().join("ours.sqlite");
    let theirs = fixture.dir.path().join("theirs.sqlite");
    for path in [&base, &ours, &theirs] {
        let c = Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t(v TEXT);").unwrap();
    }
    let merge_entries = || {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sqlite-mcp-merge-")
            })
            .map(|entry| entry.file_name())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let before = merge_entries();
    fixture.core.shutdown().await;
    let error = fixture
        .core
        .extract_sqlite_merge(
            base.to_str().unwrap(),
            ours.to_str().unwrap(),
            theirs.to_str().unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, sqlite_mcp_core::CoreError::ServerShutdown));
    assert_eq!(before, merge_entries());
    fixture.close().await;
}

#[tokio::test]
async fn pre_cancelled_merge_has_no_workspace_side_effect() {
    let fixture = Fixture::new().await;
    let paths = [
        fixture.dir.path().join("base.sqlite"),
        fixture.dir.path().join("ours.sqlite"),
        fixture.dir.path().join("theirs.sqlite"),
    ];
    for path in &paths {
        let c = Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t(v TEXT);").unwrap();
    }
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let error = fixture
        .core
        .extract_sqlite_merge_with_ct(
            paths[0].to_str().unwrap(),
            paths[1].to_str().unwrap(),
            paths[2].to_str().unwrap(),
            Some(token),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        sqlite_mcp_core::CoreError::Cancelled { .. }
    ));
    fixture.close().await;
}

#[tokio::test]
async fn extraction_rejects_present_sidecars_before_workspace_publication() {
    let fixture = Fixture::new().await;
    let paths = [
        fixture.dir.path().join("base.sqlite"),
        fixture.dir.path().join("ours.sqlite"),
        fixture.dir.path().join("theirs.sqlite"),
    ];
    for path in &paths {
        let c = Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t(v TEXT);").unwrap();
    }
    fs::write(format!("{}-wal", paths[1].display()), b"foreign sidecar").unwrap();
    let response = fixture
        .call(
            "extract_sqlite_merge",
            json!({
                "base_path": paths[0], "ours_path": paths[1], "theirs_path": paths[2]
            }),
        )
        .await;
    assert_eq!(
        structured(&response)["error"]["class"],
        "MERGE_INPUT_INVALID"
    );
    fixture.close().await;
}
