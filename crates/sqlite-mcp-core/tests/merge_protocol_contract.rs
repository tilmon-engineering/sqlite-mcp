mod support;

use rusqlite::Connection;
use serde_json::json;
use std::fs;
use support::{Fixture, structured};

fn init(path: &std::path::Path) {
    let c = Connection::open(path).unwrap();
    c.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes(body) VALUES ('base');").unwrap();
}

#[tokio::test]
async fn merge_tools_have_closed_schemas_and_path_guidance() {
    let fixture = Fixture::new().await;
    let tools = fixture.tools().await;
    let extract = tools
        .iter()
        .find(|tool| tool.name == "extract_sqlite_merge")
        .unwrap();
    let import = tools
        .iter()
        .find(|tool| tool.name == "import_sqlite_text")
        .unwrap();
    assert_eq!(extract.input_schema["additionalProperties"], json!(false));
    assert_eq!(import.input_schema["additionalProperties"], json!(false));
    fixture.close().await;
}

#[tokio::test]
async fn extraction_result_and_next_moves_are_closed_and_actionable() {
    let fixture = Fixture::new().await;
    let base = fixture.dir.path().join("base.sqlite");
    let ours = fixture.dir.path().join("ours.sqlite");
    let theirs = fixture.dir.path().join("theirs.sqlite");
    init(&base);
    init(&ours);
    init(&theirs);
    let response = fixture
        .call(
            "extract_sqlite_merge",
            json!({
                "base_path": base,
                "ours_path": ours,
                "theirs_path": theirs,
            }),
        )
        .await;
    let value = structured(&response);
    assert_eq!(response.is_error, Some(false));
    assert!(value["result"]["workspace_path"].is_string());
    assert!(value["result"]["files"]["resolved_sql"].is_string());
    assert_eq!(value["result"]["workspace_state"], "Retained");
    let manifest_path = value["result"]["manifest_path"].as_str().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest["file_byte_counts"]["manifest"],
        value["result"]["file_byte_counts"]["manifest"]
    );
    let moves = value["next_moves"].as_array().unwrap();
    assert!(
        moves
            .iter()
            .any(|item| item.as_str().unwrap().contains("resolved.sql"))
    );
    assert!(
        moves
            .iter()
            .all(|item| !item.as_str().unwrap().chars().any(char::is_control))
    );
    fixture.close().await;
}

#[tokio::test]
async fn importer_denies_temp_virtual_and_pragma_body_sql() {
    let fixture = Fixture::new().await;
    for (name, sql) in [
        (
            "temp.sql",
            "-- sqlite-mcp merge-format: 1\n-- sqlite-mcp schema-baseline: v1 count=1\n-- sqlite-mcp schema-hash: v1 kind=table name_b64=dA sql_sha256=bad sql_bytes=bad\n-- sqlite-mcp schema-baseline-end: v1\nCREATE TEMP TABLE t(x);\n",
        ),
        (
            "pragma.sql",
            "-- sqlite-mcp merge-format: 1\n-- sqlite-mcp schema-baseline: v1 count=1\n-- sqlite-mcp schema-hash: v1 kind=view name_b64=dg sql_sha256=bad sql_bytes=bad\n-- sqlite-mcp schema-baseline-end: v1\nCREATE VIEW v AS SELECT * FROM pragma_table_info('x');\n",
        ),
    ] {
        let sql_path = fixture.dir.path().join(name);
        fs::write(&sql_path, sql).unwrap();
        let output = fixture.dir.path().join(format!("{name}.sqlite"));
        let response = fixture
            .call(
                "import_sqlite_text",
                json!({"sql_path": sql_path, "output_path": output}),
            )
            .await;
        assert!(matches!(
            structured(&response)["error"]["class"].as_str(),
            Some("MERGE_FORMAT_INVALID") | Some("MERGE_POLICY_DENIED")
        ));
    }
    fixture.close().await;
}

#[tokio::test]
async fn import_rejects_existing_output_and_malformed_header() {
    let fixture = Fixture::new().await;
    let sql = fixture.dir.path().join("bad.sql");
    fs::write(&sql, "not a merge file").unwrap();
    let output = fixture.dir.path().join("output.sqlite");
    let response = fixture
        .call(
            "import_sqlite_text",
            json!({
                "sql_path": sql,
                "output_path": output,
            }),
        )
        .await;
    let value = structured(&response);
    assert_eq!(response.is_error, Some(true));
    assert_eq!(value["error"]["class"], "MERGE_FORMAT_INVALID");
    assert!(value["error"]["details"].is_object());
    fs::write(
        &sql,
        "-- sqlite-mcp merge-format: 1\n-- sqlite-mcp schema-baseline: v1 count=0\n-- sqlite-mcp schema-baseline-end: v1\nPRAGMA journal_mode=DELETE;\n",
    )
    .unwrap();
    let response = fixture
        .call(
            "import_sqlite_text",
            json!({
                "sql_path": sql,
                "output_path": output,
            }),
        )
        .await;
    assert!(matches!(
        structured(&response)["error"]["class"].as_str(),
        Some("MERGE_FORMAT_INVALID") | Some("MERGE_POLICY_DENIED")
    ));
    for pragma in ["PRAGMA foreign_keys;", "PRAGMA recursive_triggers;"] {
        fs::write(
            &sql,
            format!(
                "-- sqlite-mcp merge-format: 1\n-- sqlite-mcp schema-baseline: v1 count=0\n-- sqlite-mcp schema-baseline-end: v1\n{pragma}\n"
            ),
        )
        .unwrap();
        let response = fixture
            .call(
                "import_sqlite_text",
                json!({
                    "sql_path": sql,
                    "output_path": output,
                }),
            )
            .await;
        assert!(
            matches!(
                structured(&response)["error"]["class"].as_str(),
                Some("MERGE_FORMAT_INVALID") | Some("MERGE_POLICY_DENIED")
            ),
            "merge must blanket-deny {pragma}: {}",
            structured(&response)
        );
    }
    fs::write(&output, []).unwrap();
    let response = fixture
        .call(
            "import_sqlite_text",
            json!({
                "sql_path": sql,
                "output_path": output,
            }),
        )
        .await;
    assert_eq!(
        structured(&response)["error"]["class"],
        "MERGE_OUTPUT_EXISTS"
    );
    fixture.close().await;
}
