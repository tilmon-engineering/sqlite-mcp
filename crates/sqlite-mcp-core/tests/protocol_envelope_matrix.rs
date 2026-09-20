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
