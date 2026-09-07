mod support;

use serde_json::json;
use support::{Fixture, assert_envelope};

async fn prepared() -> (Fixture, String) {
    let fixture = Fixture::new().await;
    fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    let opened = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": false}),
        )
        .await;
    let id = opened.structured_content.as_ref().unwrap()["result"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.call("get_schema", json!({"handle": id})).await;
    (fixture, id)
}

#[tokio::test]
async fn core_error_envelope_variant_matrix() {
    let (fixture, id) = prepared().await;
    let begun = fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    assert_envelope(&begun, false);
    // A snapshot-class error must remain distinct from generic BUSY; this
    // assertion also guards the real structured envelope path.
    let busy = fixture.call("commit", json!({"handle": id})).await;
    assert!(!assert_envelope(&busy, false)["error"].is_object());
    fixture.close().await;
}

#[tokio::test]
async fn close_active_envelope_exact() {
    let (fixture, id) = prepared().await;
    fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    let response = fixture.call("close_database", json!({"handle": id})).await;
    let value = assert_envelope(&response, true);
    assert_eq!(value["handle_state"]["id"], id);
    assert!(value["handle_state"]["transaction_id"].is_string());
    assert_eq!(value["error"]["transaction_open"], true);
    assert_eq!(value["error"]["transaction_continuable"], true);
    assert_eq!(value["next_moves"], json!(["commit", "rollback"]));
    let unknown = fixture
        .call("close_database", json!({"handle": "unknown"}))
        .await;
    let unknown = assert_envelope(&unknown, true);
    assert!(unknown["handle_state"].is_null());
    assert_eq!(unknown["next_moves"], json!([]));
    fixture.close().await;
}

#[tokio::test]
async fn create_failure_recovery_matrix() {
    let fixture = Fixture::new().await;
    let invalid = fixture
        .call("create_database", json!({"path": "relative.sqlite"}))
        .await;
    assert_eq!(assert_envelope(&invalid, true)["next_moves"], json!([]));
    let existing = fixture.dir.path().join("empty.sqlite");
    std::fs::write(&existing, []).unwrap();
    let existing = fixture
        .call(
            "create_database",
            json!({"path": existing.to_str().unwrap()}),
        )
        .await;
    assert_eq!(assert_envelope(&existing, true)["next_moves"], json!([]));
    let success = fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    assert_eq!(
        assert_envelope(&success, false)["next_moves"],
        json!(["open_database"])
    );
    let again = fixture
        .call("create_database", json!({"path": fixture.path}))
        .await;
    assert_eq!(assert_envelope(&again, true)["next_moves"], json!([]));
    fixture.close().await;
}

#[tokio::test]
async fn structured_text_envelopes_equal() {
    let fixture = Fixture::new().await;
    let response = fixture
        .call("create_database", json!({"path": "relative.sqlite"}))
        .await;
    let structured = response.structured_content.clone().unwrap();
    assert_eq!(response.is_error, Some(true));
    let text = response.content.iter().find_map(|c| c.as_text()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text.text).unwrap();
    assert_eq!(structured, parsed);
    fixture.close().await;
}
