use rusqlite::Connection;
use sqlite_mcp_core::{Config, Core};
use tempfile::tempdir;

fn db(path: &std::path::Path, body: &str) {
    let c = Connection::open(path).unwrap();
    c.execute_batch(body).unwrap();
}

#[tokio::test]
async fn extract_and_import_round_trip_smoke() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base.sqlite");
    let ours = dir.path().join("ours.sqlite");
    let theirs = dir.path().join("theirs.sqlite");
    for path in [&base, &ours, &theirs] {
        db(
            path,
            "PRAGMA journal_mode=DELETE; CREATE TABLE items(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO items(body) VALUES ('hello');",
        );
    }
    let core = Core::new(Config::default()).unwrap();
    let extracted = core
        .extract_sqlite_merge(
            base.to_str().unwrap(),
            ours.to_str().unwrap(),
            theirs.to_str().unwrap(),
        )
        .await
        .unwrap();
    assert!(std::path::Path::new(extracted.files.resolved_sql.as_ref().unwrap()).exists());
    let output = dir.path().join("merged.sqlite");
    let imported = core
        .import_sqlite_text(
            extracted.files.resolved_sql.as_ref().unwrap(),
            output.to_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(imported.output_state, "Committed");
    let c = Connection::open(output).unwrap();
    assert_eq!(
        c.query_row("SELECT body FROM items", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "hello"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn empty_snapshot_round_trip_smoke() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("empty.sqlite");
    db(
        &source,
        "PRAGMA journal_mode=DELETE; CREATE TABLE transient(value INTEGER); DROP TABLE transient;",
    );
    let core = Core::new(Config::default()).unwrap();
    let extracted = core
        .extract_sqlite_merge(
            source.to_str().unwrap(),
            source.to_str().unwrap(),
            source.to_str().unwrap(),
        )
        .await
        .unwrap();
    let output = dir.path().join("empty-output.sqlite");
    let imported = core
        .import_sqlite_text(
            extracted.files.resolved_sql.as_ref().unwrap(),
            output.to_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(imported.validation.schema_summary.object_count, 0);
    core.shutdown().await;
}
