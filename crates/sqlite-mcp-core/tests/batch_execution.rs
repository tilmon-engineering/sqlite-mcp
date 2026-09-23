mod support;

use serde_json::json;
use sqlite_mcp_core::Config;
use support::{Fixture, assert_envelope, structured};

async fn prepared(config: Config) -> (Fixture, String) {
    let fixture = Fixture::with_config(config).await;
    fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    let opened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": false}),
        )
        .await;
    let id = structured(&opened)["result"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.call("get_schema", json!({"handle": id})).await;
    fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    (fixture, id)
}

#[tokio::test]
async fn batch_executes_native_tail_order_and_final_no_semicolon() {
    let (fixture, id) = prepared(Config::default()).await;
    let response = fixture
        .call(
            "query_batch",
            json!({
                "handle": id,
                "sql": "CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (7); SELECT v FROM t"
            }),
        )
        .await;
    let envelope = assert_envelope(&response, false);
    let results = envelope["result"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[1]["changes"], 1);
    assert_eq!(
        results[2]["rows"][0][0],
        json!({"type":"integer","value":"7"})
    );
    assert!(
        results
            .iter()
            .all(|result| result["execution_complete"] == true)
    );
    fixture.close().await;
}

#[tokio::test]
async fn inline_and_file_batches_have_equivalent_ordered_results() {
    let (fixture, id) = prepared(Config::default()).await;
    let sql = "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('a'); SELECT v FROM t";
    let inline = fixture
        .call("query_batch", json!({"handle": id, "sql": sql}))
        .await;
    assert_envelope(&inline, false);
    fixture.call("rollback", json!({"handle": id})).await;
    fixture.call("get_schema", json!({"handle": id})).await;
    fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let file = fixture.dir.path().join("batch.sql");
    std::fs::write(&file, sql).unwrap();
    let from_file = fixture
        .call(
            "execute_sql_file",
            json!({"handle": id, "sql_path": file.to_str().unwrap()}),
        )
        .await;
    let file_envelope = assert_envelope(&from_file, false);
    assert_eq!(
        file_envelope["result"]["results"],
        structured(&inline)["result"]["results"]
    );
    assert_eq!(
        file_envelope["result"]["sql_path"],
        file.canonicalize().unwrap().to_str().unwrap()
    );
    fixture.close().await;
}

#[tokio::test]
async fn file_batch_rejects_invalid_paths_encoding_nul_and_oversize() {
    let (fixture, id) = prepared(Config {
        batch_sql_byte_limit: 8,
        ..Default::default()
    })
    .await;
    let relative = fixture
        .call(
            "execute_sql_file",
            json!({"handle": id, "sql_path": "relative.sql"}),
        )
        .await;
    assert_envelope(&relative, true);
    let directory = fixture
        .call(
            "execute_sql_file",
            json!({"handle": id, "sql_path": fixture.dir.path().to_str().unwrap()}),
        )
        .await;
    assert_envelope(&directory, true);
    let nul = fixture.dir.path().join("nul.sql");
    std::fs::write(&nul, b"SELECT\0 1").unwrap();
    assert_envelope(
        &fixture
            .call(
                "execute_sql_file",
                json!({"handle": id, "sql_path": nul.to_str().unwrap()}),
            )
            .await,
        true,
    );
    let invalid = fixture.dir.path().join("invalid.sql");
    std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
    assert_envelope(
        &fixture
            .call(
                "execute_sql_file",
                json!({"handle": id, "sql_path": invalid.to_str().unwrap()}),
            )
            .await,
        true,
    );
    let large = fixture.dir.path().join("large.sql");
    std::fs::write(&large, b"SELECT 12345").unwrap();
    assert_envelope(
        &fixture
            .call(
                "execute_sql_file",
                json!({"handle": id, "sql_path": large.to_str().unwrap()}),
            )
            .await,
        true,
    );
    fixture.close().await;
}

#[tokio::test]
async fn batch_statement_limit_rejects_before_execution_of_extra_statement() {
    let (fixture, id) = prepared(Config {
        batch_statement_limit: 2,
        ..Default::default()
    })
    .await;
    let response = fixture
        .call(
            "query_batch",
            json!({"handle": id, "sql": "CREATE TABLE t (v); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)"}),
        )
        .await;
    let envelope = assert_envelope(&response, true);
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("batch statement limit exceeded")
    );
    let followup = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "SELECT v FROM t", "parameters": []}),
        )
        .await;
    assert_envelope(&followup, false);
    fixture.close().await;
}

#[tokio::test]
async fn failed_batch_after_ddl_keeps_transaction_schema_usable() {
    let (fixture, id) = prepared(Config::default()).await;
    let failed = fixture
        .call(
            "query_batch",
            json!({
                "handle": id,
                "sql": "CREATE TABLE t (v); INSERT INTO t VALUES (1); SELCT 3"
            }),
        )
        .await;
    assert_envelope(&failed, true);
    let followup = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "SELECT count(*) FROM t", "parameters": []}),
        )
        .await;
    let envelope = assert_envelope(&followup, false);
    assert_eq!(
        envelope["result"]["rows"][0][0],
        json!({"type":"integer","value":"1"})
    );
    fixture.close().await;
}

#[tokio::test]
async fn batch_tail_handles_strings_comments_compounds_and_returning() {
    let (fixture, id) = prepared(Config::default()).await;
    let response = fixture
        .call(
            "query_batch",
            json!({
                "handle": id,
                "sql": "/* ; */ CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('a;b') RETURNING v; WITH x(v) AS (SELECT 'c;d') SELECT v FROM x; -- final comment\n"
            }),
        )
        .await;
    let envelope = assert_envelope(&response, false);
    let results = envelope["result"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[1]["rows"][0][0],
        json!({"type":"text","value":"a;b"})
    );
    assert_eq!(
        results[2]["rows"][0][0],
        json!({"type":"text","value":"c;d"})
    );
    fixture.close().await;
}

#[tokio::test]
async fn legacy_query_rejects_second_statement_without_executing_first() {
    let (fixture, id) = prepared(Config::default()).await;
    let response = fixture
        .call(
            "query",
            json!({
                "handle": id,
                "sql": "CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1)",
                "parameters": []
            }),
        )
        .await;
    let envelope = assert_envelope(&response, true);
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("multiple SQL statements")
    );
    fixture.call("rollback", json!({"handle": id})).await;
    fixture.call("get_schema", json!({"handle": id})).await;
    fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let check = fixture
        .call(
            "query",
            json!({
                "handle": id,
                "sql": "SELECT name FROM sqlite_schema",
                "parameters": []
            }),
        )
        .await;
    let check_envelope = assert_envelope(&check, false);
    assert_eq!(check_envelope["result"]["rows_returned"], 0);
    fixture.close().await;
}
