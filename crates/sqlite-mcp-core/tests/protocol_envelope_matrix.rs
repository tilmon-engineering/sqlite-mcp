mod support;
use serde_json::json;
use support::{Fixture, assert_envelope};

fn handle(value: &serde_json::Value) -> String {
    value["result"]["handle"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn envelope_state_matrix() {
    let fixture = Fixture::new().await;
    let created = fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    assert_envelope(&created, false);
    let opened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": false}),
        )
        .await;
    let opened_value = assert_envelope(&opened, false);
    let id = handle(opened_value);
    assert_eq!(opened_value["handle_state"]["id"], id);
    assert_eq!(opened_value["handle_state"]["readonly"], false);

    let schema = fixture.call("get_schema", json!({"handle": id})).await;
    let schema_value = assert_envelope(&schema, false);
    assert!(
        schema_value["handle_state"]["schema_observed"]
            .as_bool()
            .unwrap_or(true)
    );
    let begun = fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let begun_value = assert_envelope(&begun, false);
    assert!(begun_value["handle_state"]["transaction_id"].is_string());
    assert_eq!(begun_value["handle_state"]["transaction_mode"], "deferred");
    let ddl = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)"}),
        )
        .await;
    assert_envelope(&ddl, false);
    // Actual DDL remains usable for subsequent statements in the same
    // transaction; handle_state still reports the committed observation.
    let follow_up = fixture.call("query", json!({"handle": id, "sql": "INSERT INTO items (name) VALUES (?)", "parameters": [{"type":"text","value":"one"}]})).await;
    let follow_up_value = assert_envelope(&follow_up, false);
    assert_eq!(follow_up_value["handle_state"]["schema_observed"], true);
    let dml = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "SELECT count(*) FROM items"}),
        )
        .await;
    assert_envelope(&dml, false);
    let committed = fixture.call("commit", json!({"handle": id})).await;
    assert_envelope(&committed, false);
    let no_tx = fixture.call("commit", json!({"handle": id})).await;
    assert_eq!(
        assert_envelope(&no_tx, true)["error"]["class"],
        "NO_TX_OPEN"
    );
    let rollback = fixture.call("rollback", json!({"handle": id})).await;
    assert_envelope(&rollback, false);
    let closed = fixture.call("close_database", json!({"handle": id})).await;
    assert_envelope(&closed, false);

    let reopened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": true}),
        )
        .await;
    let reopened_id = handle(assert_envelope(&reopened, false));
    let missing_schema = fixture
        .call("begin_transaction", json!({"handle": reopened_id}))
        .await;
    assert_eq!(
        assert_envelope(&missing_schema, true)["error"]["class"],
        "SCHEMA_REQUIRED"
    );
    let schema = fixture
        .call("get_schema", json!({"handle": reopened_id}))
        .await;
    assert_envelope(&schema, false);
    let begun = fixture
        .call("begin_transaction", json!({"handle": reopened_id}))
        .await;
    assert_envelope(&begun, false);
    let selected = fixture
        .call(
            "query",
            json!({"handle": reopened_id, "sql": "SELECT name FROM items"}),
        )
        .await;
    assert_envelope(&selected, false);
    let _ = fixture
        .call("rollback", json!({"handle": reopened_id}))
        .await;
    let unknown = fixture
        .call("get_schema", json!({"handle": "not-a-handle"}))
        .await;
    assert_eq!(
        assert_envelope(&unknown, true)["error"]["class"],
        "HANDLE_UNKNOWN"
    );
    fixture.close().await;
}

#[tokio::test]
async fn result_too_large_class_reported() {
    // A result payload exceeding the configured byte cap reports the
    // RESULT_TOO_LARGE class (not INTERNAL) with a truthful, usable
    // transaction state.
    let config = sqlite_mcp_core::Config {
        result_byte_limit: 512,
        ..sqlite_mcp_core::Config::default()
    };
    let fixture = Fixture::with_config(config).await;
    let created = fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    assert_envelope(&created, false);
    let opened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": false}),
        )
        .await;
    let id = handle(assert_envelope(&opened, false));
    fixture.call("get_schema", json!({"handle": id})).await;
    fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let oversized = fixture
        .call(
            "query",
            json!({
                "handle": id,
                "sql": "SELECT randomblob(1024)",
                "parameters": []
            }),
        )
        .await;
    let error = assert_envelope(&oversized, true)["error"].clone();
    assert_eq!(error["class"], "RESULT_TOO_LARGE", "{error}");
    assert_eq!(error["transaction_open"], true, "{error}");
    assert_eq!(error["transaction_continuable"], true, "{error}");
    // The transaction survives truthfully: a subsequent query and rollback
    // both work.
    let followup = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "SELECT 1", "parameters": []}),
        )
        .await;
    assert_envelope(&followup, false);
    let rollback = fixture.call("rollback", json!({"handle": id})).await;
    assert_envelope(&rollback, false);
    fixture.close().await;
}

#[tokio::test]
async fn second_begin_reports_tx_already_open() {
    let fixture = Fixture::new().await;
    let created = fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    assert_envelope(&created, false);
    let opened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": false}),
        )
        .await;
    let id = handle(assert_envelope(&opened, false));
    fixture.call("get_schema", json!({"handle": id})).await;
    let begun = fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let begun_value = assert_envelope(&begun, false);
    let transaction_id = begun_value["handle_state"]["transaction_id"].clone();
    assert!(transaction_id.is_string());
    let second = fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let error = assert_envelope(&second, true)["error"].clone();
    assert_eq!(error["class"], "TX_ALREADY_OPEN", "{error}");
    assert_eq!(error["transaction_open"], true, "{error}");
    assert_eq!(error["transaction_continuable"], true, "{error}");
    assert_eq!(
        assert_envelope(&second, true)["handle_state"]["transaction_id"],
        transaction_id,
        "the original transaction ID must be unchanged"
    );
    // The original transaction remains usable.
    let query = fixture
        .call(
            "query",
            json!({"handle": id, "sql": "SELECT 1", "parameters": []}),
        )
        .await;
    assert_envelope(&query, false);
    let rollback = fixture.call("rollback", json!({"handle": id})).await;
    assert_envelope(&rollback, false);
    fixture.close().await;
}
