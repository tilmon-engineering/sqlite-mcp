// Shared harness compiled once per integration-test crate; helpers that are
// unused by a given test binary are expected.
#![allow(dead_code)]

use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientInfo, Tool},
};
use serde_json::Value;
use sqlite_mcp_core::{Config, Core, McpServer};
use tempfile::TempDir;

#[derive(Clone, Default)]
pub struct TestClient;
impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

pub struct Fixture {
    pub dir: TempDir,
    pub path: String,
    pub core: Core,
    pub client: rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    pub server: Option<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with_config(Config::default()).await
    }

    /// Test-support-only constructor with an explicit configuration (for
    /// example a small `result_byte_limit`); never MCP/config surface area.
    pub async fn with_config(config: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sqlite").to_str().unwrap().to_owned();
        let core = Core::new(config).unwrap();
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let server_core = core.clone();
        let server = tokio::spawn(async move {
            if let Ok(running) = McpServer::new(server_core).serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        let client = TestClient.serve(client_io).await.unwrap();
        Self {
            dir,
            path,
            core,
            client,
            server: Some(server),
        }
    }

    pub async fn try_call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, rmcp::service::ServiceError> {
        self.client
            .peer()
            .call_tool(
                CallToolRequestParams::new(name.to_owned())
                    .with_arguments(arguments.as_object().cloned().unwrap_or_default()),
            )
            .await
    }

    pub async fn call(&self, name: &str, arguments: Value) -> CallToolResult {
        self.try_call(name, arguments).await.unwrap()
    }

    pub async fn tools(&self) -> Vec<Tool> {
        self.client.peer().list_all_tools().await.unwrap()
    }

    pub async fn close(mut self) {
        let _ = self.client.cancel().await;
        if let Some(server) = self.server.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
        }
        self.core.shutdown().await;
    }
}

pub fn structured(response: &CallToolResult) -> &Value {
    response
        .structured_content
        .as_ref()
        .expect("structured response")
}

pub fn assert_envelope(response: &CallToolResult, error: bool) -> &Value {
    assert_eq!(
        response.is_error,
        Some(error),
        "unexpected isError (expected {error}) in envelope: {:?}",
        response.structured_content
    );
    let value = structured(response);
    assert_eq!(value.get("envelope_version"), Some(&Value::from(1)));
    assert!(value.get("result").is_some() ^ value.get("error").is_some());
    let moves = value.get("next_moves").and_then(Value::as_array).unwrap();
    for item in moves {
        let text = item.as_str().unwrap();
        assert!(!text.is_empty());
        assert!(
            !text.chars().any(char::is_control),
            "next_moves entries must be one-line guidance"
        );
    }
    value
}

pub const NAMES: [&str; 11] = [
    "create_database",
    "open_database",
    "list_handles",
    "get_schema",
    "begin_transaction",
    "query",
    "commit",
    "rollback",
    "close_database",
    "extract_sqlite_merge",
    "import_sqlite_text",
];
