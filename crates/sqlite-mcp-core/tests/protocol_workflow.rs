mod support;
use serde_json::json;
use support::{Fixture, assert_envelope};

#[tokio::test]
async fn documented_workflow_scenario() {
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
    let id = assert_envelope(&opened, false)["result"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_envelope(
        &fixture.call("get_schema", json!({"handle": id})).await,
        false,
    );
    assert_envelope(
        &fixture
            .call(
                "begin_transaction",
                json!({"handle": id, "mode": "deferred"}),
            )
            .await,
        false,
    );
    assert_envelope(
        &fixture
            .call(
                "query",
                json!({
                    "handle": id,
                    "sql": "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
                }),
            )
            .await,
        false,
    );
    // Actual DDL remains usable for subsequent statements in the same
    // transaction; one schema reread is needed only after its commit.
    assert_envelope(
        &fixture
            .call(
                "query",
                json!({
                    "handle": id,
                    "sql": "INSERT INTO notes (body) VALUES (?)",
                    "parameters": [{"type": "text", "value": "hello"}]
                }),
            )
            .await,
        false,
    );
    let committed = fixture.call("commit", json!({"handle": id})).await;
    let committed_value = assert_envelope(&committed, false);
    assert_eq!(committed_value["handle_state"]["schema_observed"], false);
    assert_envelope(
        &fixture.call("get_schema", json!({"handle": id})).await,
        false,
    );

    // Failed-call recovery (F-13): close is refused while a transaction is
    // active, reporting the authoritative state and the exact recovery moves.
    // The actual schema change committed, so reread once before the next begin.
    assert_envelope(
        &fixture.call("get_schema", json!({"handle": id})).await,
        false,
    );
    assert_envelope(
        &fixture
            .call(
                "begin_transaction",
                json!({"handle": id, "mode": "deferred"}),
            )
            .await,
        false,
    );
    let refused = fixture.call("close_database", json!({"handle": id})).await;
    let refused_body = assert_envelope(&refused, true);
    assert_eq!(
        refused_body["error"]["transaction_open"], true,
        "close refusal reports the authoritative active state"
    );
    assert_eq!(
        refused_body["next_moves"],
        json!(["commit", "rollback"]),
        "close refusal names the exact recovery moves"
    );
    // The documented recovery: roll back, then close succeeds.
    assert_envelope(
        &fixture.call("rollback", json!({"handle": id})).await,
        false,
    );
    assert_envelope(
        &fixture.call("close_database", json!({"handle": id})).await,
        false,
    );

    // Invalid arguments are rejected by SDK-level schema validation before
    // dispatch; covered by invalid_typed_argument_is_rejected_at_dispatch.
    fixture.close().await;
}

#[tokio::test]
async fn invalid_typed_argument_is_rejected_at_dispatch() {
    let fixture = Fixture::new().await;
    // Missing required `readonly` must be rejected by SDK-level schema
    // validation before any handler dispatch (MCP validation semantics),
    // not by a custom tool envelope.
    // SDK-level parameter validation surfaces as an MCP invalid-params
    // error (-32602), not a custom tool envelope.
    let rmcp::service::ServiceError::McpError(data) = fixture
        .try_call("open_database", json!({"path": fixture.path}))
        .await
        .expect_err("missing readonly must be rejected before dispatch")
    else {
        panic!("expected MCP error variant");
    };
    assert_eq!(data.code.0, -32602, "expected invalid-params error");
    fixture.close().await;
}
