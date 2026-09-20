mod support;
use support::{Fixture, NAMES};

#[tokio::test]
async fn tool_contract_and_bootstrap() {
    let fixture = Fixture::new().await;
    let tools = fixture.tools().await;
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let mut expected = NAMES.to_vec();
    names.sort_unstable();
    expected.sort_unstable();
    assert_eq!(names, expected);

    for tool in &tools {
        assert!(
            !tool.description.as_deref().unwrap_or_default().is_empty(),
            "{}",
            tool.name.as_ref()
        );
        let schema = &tool.input_schema;
        let required = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        match tool.name.as_ref() {
            "create_database" => assert!(required.iter().any(|v| v == "path")),
            "open_database" => {
                assert!(required.iter().any(|v| v == "path"));
                assert!(required.iter().any(|v| v == "readonly"));
                assert_eq!(properties["readonly"]["type"], "boolean");
            }
            "query" => {
                for field in ["handle", "sql"] {
                    assert!(required.iter().any(|v| v == field));
                }
                assert!(properties.contains_key("parameters"));
            }
            "begin_transaction" => {
                assert!(required.iter().any(|v| v == "handle"));
                // rmcp 1.7 / schemars 1.2 do not advertise field defaults in
                // the generated schema; the deferred default behavior is
                // asserted by the workflow and mode tests.
                assert_eq!(properties["mode"]["type"], "string");
                assert!(!required.iter().any(|v| v == "mode"));
            }
            "list_handles" => assert!(required.is_empty()),
            "extract_sqlite_merge" => {
                for field in ["base_path", "ours_path", "theirs_path"] {
                    assert!(required.iter().any(|v| v == field));
                }
                assert_eq!(properties.len(), 3);
            }
            "import_sqlite_text" => {
                for field in ["sql_path", "output_path"] {
                    assert!(required.iter().any(|v| v == field));
                }
                assert_eq!(properties.len(), 2);
            }
            _ => assert!(required.iter().any(|v| v == "handle")),
        }
        let description = tool
            .description
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            description.contains("schema")
                || description.contains("transaction")
                || description.contains("handle")
        );
    }
    fixture.close().await;
}
