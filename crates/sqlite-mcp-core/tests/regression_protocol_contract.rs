mod support;

use serde_json::json;
use sqlite_mcp_core::{Config, Core};
use support::{Fixture, NAMES};

#[tokio::test]
async fn closed_tool_schema_matrix() {
    let fixture = Fixture::new().await;
    let tools = fixture.tools().await;
    assert_eq!(tools.len(), 9);
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    let mut expected = NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected);
    for tool in tools {
        let schema = &tool.input_schema;
        assert_eq!(schema["type"], "object", "{}", tool.name);
        assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
        if tool.name == "begin_transaction" {
            assert_eq!(
                schema["properties"]["mode"]["enum"],
                json!(["deferred", "immediate"])
            );
            // rmcp 1.7 / schemars 1.2 do not advertise field defaults in the
            // generated schema; the deferred default is a deserialization
            // behavior verified by valid_mode_default_and_readonly.
            assert_eq!(schema["properties"]["mode"]["type"], "string");
            assert!(
                !schema["required"]
                    .as_array()
                    .expect("required list")
                    .iter()
                    .any(|value| value == "mode")
            );
        }
        if tool.name == "list_handles" {
            let empty_properties = schema
                .get("properties")
                .map(|properties| {
                    properties
                        .as_object()
                        .expect("properties object")
                        .is_empty()
                })
                .unwrap_or(true);
            assert!(empty_properties, "list_handles must declare no parameters");
            assert!(
                schema
                    .get("required")
                    .map(|required| required.as_array().expect("required list").is_empty())
                    .unwrap_or(true),
                "list_handles must require no parameters"
            );
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn unknown_argument_dispatch_no_effects() {
    let fixture = Fixture::new().await;
    let before = fixture.core.list_handles().await;
    let err = fixture
        .try_call(
            "create_database",
            json!({"path": fixture.path, "unexpected": true}),
        )
        .await
        .expect_err("unknown argument must be rejected");
    let rmcp::service::ServiceError::McpError(data) = err else {
        panic!("expected MCP error")
    };
    assert_eq!(data.code.0, -32602);
    assert_eq!(fixture.core.list_handles().await.len(), before.len());
    fixture.close().await;
}

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
async fn invalid_mode_core_and_protocol() {
    let core = Core::new(Config::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core.sqlite");
    core.create_database(path.to_str().unwrap()).await.unwrap();
    let h = core
        .open_database(path.to_str().unwrap(), false)
        .await
        .unwrap();
    core.get_schema(&h.id).await.unwrap();
    for mode in ["banana", "DEFERRED", "Immediate", "exclusive"] {
        assert!(
            core.begin_transaction(&h.id, mode).await.is_err(),
            "invalid mode {mode} must be rejected before BEGIN"
        );
    }
    core.shutdown().await;

    let (fixture, id) = prepared().await;
    let err = fixture
        .try_call("begin_transaction", json!({"handle": id, "mode": "banana"}))
        .await
        .expect_err("invalid mode");
    let rmcp::service::ServiceError::McpError(data) = err else {
        panic!("expected MCP error")
    };
    assert_eq!(data.code.0, -32602);
    assert!(
        fixture.core.list_handles().await[0]
            .transaction_id
            .is_none()
    );
    fixture.close().await;
}

#[tokio::test]
async fn valid_mode_default_and_readonly() {
    let (fixture, id) = prepared().await;
    let begun = fixture
        .call("begin_transaction", json!({"handle": id}))
        .await;
    assert_eq!(
        begun.structured_content.as_ref().unwrap()["result"]["mode"],
        "deferred"
    );
    fixture.call("rollback", json!({"handle": id})).await;
    fixture.call("close_database", json!({"handle": id})).await;
    let ro = fixture
        .call(
            "open_database",
            json!({"path": fixture.path, "readonly": true}),
        )
        .await;
    let rid = ro.structured_content.as_ref().unwrap()["result"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.call("get_schema", json!({"handle": rid})).await;
    let begun = fixture
        .call(
            "begin_transaction",
            json!({"handle": rid, "mode": "deferred"}),
        )
        .await;
    assert_eq!(
        begun.structured_content.as_ref().unwrap()["result"]["mode"],
        "deferred"
    );
    fixture.close().await;
}
