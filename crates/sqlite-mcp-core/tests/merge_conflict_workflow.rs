use rusqlite::Connection;
use sqlite_mcp_core::{Config, Core};
use std::{fs, path::Path};
use tempfile::tempdir;

fn make_conflict_database(path: &Path, body: &str) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE items(id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
        )
        .unwrap();
    connection
        .execute("INSERT INTO items(id, body) VALUES (1, ?1)", [body])
        .unwrap();
}

fn read_body(path: &Path) -> String {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT body FROM items WHERE id = 1", [], |row| row.get(0))
        .unwrap()
}

async fn import_snapshot(core: &Core, sql_path: &Path, output: &Path, expected_body: &str) {
    let imported = core
        .import_sqlite_text(sql_path.to_str().unwrap(), output.to_str().unwrap())
        .await
        .unwrap();

    // The server canonicalizes paths; compare against the canonical form so
    // symlinked system temp roots (macOS `/var` -> `/private/var`) match.
    let canonical_output = fs::canonicalize(output).unwrap();
    assert_eq!(imported.output_path, canonical_output.to_str().unwrap());
    assert_eq!(imported.output_state, "Committed");
    assert_eq!(imported.statement_count, 2, "schema plus one row replay");
    assert_eq!(
        imported.byte_count,
        fs::metadata(sql_path).unwrap().len(),
        "import byte accounting must cover the complete SQL source"
    );
    assert_eq!(imported.validation.journal_mode, "delete");
    assert_eq!(imported.validation.schema_summary.object_count, 1);
    assert_eq!(imported.validation.schema_summary.table_count, 1);
    assert!(imported.validation.foreign_key_check.ok);
    assert_eq!(imported.validation.foreign_key_check.violations, 0);
    assert!(imported.validation.integrity_check.ok);
    assert_eq!(imported.validation.integrity_check.message, "ok");
    assert_eq!(read_body(output), expected_body);
}

#[tokio::test]
async fn three_way_conflict_edit_and_import_workflow() {
    let directory = tempdir().unwrap();
    let base = directory.path().join("base.sqlite");
    let ours = directory.path().join("ours.sqlite");
    let theirs = directory.path().join("theirs.sqlite");

    make_conflict_database(&base, "base value");
    make_conflict_database(&ours, "ours O'Reilly value");
    make_conflict_database(&theirs, "theirs value");

    assert_eq!(read_body(&base), "base value");
    assert_eq!(read_body(&ours), "ours O'Reilly value");
    assert_eq!(read_body(&theirs), "theirs value");

    let source_bytes = [
        fs::read(&base).unwrap(),
        fs::read(&ours).unwrap(),
        fs::read(&theirs).unwrap(),
    ];

    let core = Core::new(Config::default()).unwrap();
    let extracted = core
        .extract_sqlite_merge(
            base.to_str().unwrap(),
            ours.to_str().unwrap(),
            theirs.to_str().unwrap(),
        )
        .await
        .unwrap();

    let base_sql_path = Path::new(extracted.files.base_sql.as_ref().unwrap());
    let ours_sql_path = Path::new(extracted.files.ours_sql.as_ref().unwrap());
    let theirs_sql_path = Path::new(extracted.files.theirs_sql.as_ref().unwrap());
    let resolved_path = Path::new(extracted.files.resolved_sql.as_ref().unwrap());
    let base_sql = fs::read_to_string(base_sql_path).unwrap();
    let ours_sql = fs::read_to_string(ours_sql_path).unwrap();
    let theirs_sql = fs::read_to_string(theirs_sql_path).unwrap();
    let resolved_sql = fs::read_to_string(resolved_path).unwrap();

    assert!(base_sql.contains("'base value'"));
    assert!(ours_sql.contains("'ours O''Reilly value'"));
    assert!(theirs_sql.contains("'theirs value'"));
    assert_ne!(base_sql, ours_sql, "base and ours snapshots must differ");
    assert_ne!(
        ours_sql, theirs_sql,
        "ours and theirs snapshots must differ"
    );
    assert_ne!(
        base_sql, theirs_sql,
        "base and theirs snapshots must differ"
    );
    assert_eq!(
        resolved_sql, ours_sql,
        "resolved.sql must initially be seeded from ours.sql"
    );

    // Confirm each extracted artifact is independently importable and semantically
    // represents its labeled source snapshot, not merely a comment containing the
    // expected value.
    import_snapshot(
        &core,
        base_sql_path,
        &directory.path().join("imported-base.sqlite"),
        "base value",
    )
    .await;
    import_snapshot(
        &core,
        ours_sql_path,
        &directory.path().join("imported-ours.sqlite"),
        "ours O'Reilly value",
    )
    .await;
    import_snapshot(
        &core,
        theirs_sql_path,
        &directory.path().join("imported-theirs.sqlite"),
        "theirs value",
    )
    .await;

    // Simulate the caller resolving the conflict with file tools. The edit is
    // intentionally made outside Core; the retained workspace remains the
    // agent's editable evidence and retry surface.
    let merged_sql = resolved_sql.replace("'ours O''Reilly value'", "'merged O''Reilly value'");
    assert_ne!(merged_sql, resolved_sql);
    fs::write(resolved_path, &merged_sql).unwrap();

    let output = directory.path().join("merged.sqlite");
    import_snapshot(&core, resolved_path, &output, "merged O'Reilly value").await;

    assert_eq!(fs::read(resolved_path).unwrap(), merged_sql.as_bytes());
    assert_eq!(
        fs::read(&base).unwrap(),
        source_bytes[0],
        "base must remain byte-identical"
    );
    assert_eq!(
        fs::read(&ours).unwrap(),
        source_bytes[1],
        "ours must remain byte-identical"
    );
    assert_eq!(
        fs::read(&theirs).unwrap(),
        source_bytes[2],
        "theirs must remain byte-identical"
    );
    assert_eq!(read_body(&base), "base value");
    assert_eq!(read_body(&ours), "ours O'Reilly value");
    assert_eq!(read_body(&theirs), "theirs value");

    core.shutdown().await;
}
