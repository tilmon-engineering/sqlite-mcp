//! Production serving orchestration for the `sqlite-mcp` binary.
//!
//! `serve_with_transport` is the single production service path: `main` passes
//! the real stdio transport, while deterministic integration-test children pass
//! a test-owned transport so tests exercise production orchestration rather
//! than a copied server.
//!
//! Core cleanup always runs after initialization and service outcomes. The
//! original initialization/service failure remains the primary cause of a
//! [`ServeFailure`], cleanup failures are attached separately, and with no
//! service failure any cleanup failure becomes the primary cause so the
//! process exits nonzero (F-03). Clean EOF with clean cleanup exits zero.
use rmcp::RoleServer;
use rmcp::transport::IntoTransport;
use rmcp::transport::io::stdio;
use sqlite_mcp_core::{Config, Core, McpServer, ShutdownReport};

/// Failure of serving with its causal separation preserved.
///
/// `primary` is the original initialization/service failure when one exists;
/// `cleanup_errors` are attached separately and never replace the primary
/// cause. Without a primary failure, any cleanup failure becomes the primary
/// cause so the process still exits nonzero.
#[derive(Debug)]
pub struct ServeFailure {
    pub primary: String,
    pub cleanup_errors: Vec<String>,
}

impl std::fmt::Display for ServeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.primary)?;
        for error in &self.cleanup_errors {
            write!(f, "; cleanup failure: {error}")?;
        }
        Ok(())
    }
}

fn summarize(report: &ShutdownReport) -> Vec<String> {
    let mut errors = Vec::new();
    for (id, result) in &report.entries {
        match result {
            Ok(status) if status.is_success() => {}
            Ok(status) => errors.push(format!(
                "handle {id}: shutdown incomplete (connection_closed={}, thread_joined={})",
                status.connection_closed, status.thread_joined
            )),
            Err(error) => errors.push(format!("handle {id}: {error}")),
        }
    }
    errors.extend(report.registry_cleanup_errors.iter().cloned());
    errors
}

/// Serve `config` over the supplied MCP transport using production
/// orchestration. Transport bounds mirror [`rmcp::service::serve_server`].
pub async fn serve_with_transport<T, E, A>(config: Config, transport: T) -> Result<(), ServeFailure>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    serve_with_transport_and_ct(config, transport, None).await
}

/// [`serve_with_transport`] with an optional cooperative shutdown token.
/// Cancelling the token ends the serving session and triggers the same
/// always-run cleanup path; a clean cancellation with clean cleanup yields
/// `Ok(())` so SIGINT can exit zero.
pub async fn serve_with_transport_and_ct<T, E, A>(
    config: Config,
    transport: T,
    shutdown_ct: Option<tokio_util::sync::CancellationToken>,
) -> Result<(), ServeFailure>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let core = Core::new(config).map_err(|error| ServeFailure {
        primary: error.to_string(),
        cleanup_errors: Vec::new(),
    })?;
    let server = McpServer::new(core.clone());
    let mut primary = match shutdown_ct {
        None => match rmcp::service::serve_server(server, transport).await {
            Ok(running) => running
                .waiting()
                .await
                .err()
                .map(|error| format!("MCP stdio server failed: {error}")),
            Err(error) => Some(format!("failed to initialize MCP stdio server: {error}")),
        },
        Some(shutdown_ct) => {
            match rmcp::service::serve_server_with_ct(server, transport, shutdown_ct.clone()).await
            {
                Ok(running) => {
                    // rmcp takes the token by value and its service loop
                    // retains a clone, so cancelling the retained token breaks
                    // that loop with a bounded response drain and a transport
                    // close. `waiting(self)` consumes the `RunningService`, so
                    // build the consuming future once, pin it, and race it
                    // against the retained token: whichever wins, the pinned
                    // future is then awaited to completion so the service task
                    // join (and with it transport teardown) is the barrier
                    // BEFORE `core.shutdown()` rolls back open handles.
                    let mut waiting = Box::pin(running.waiting());
                    let outcome = tokio::select! {
                        outcome = &mut waiting => outcome,
                        _ = shutdown_ct.cancelled() => waiting.await,
                    };
                    outcome
                        .err()
                        .map(|error| format!("MCP stdio server failed: {error}"))
                }
                Err(error) => {
                    // Cancelling during initialization is a clean shutdown
                    // (rmcp surfaces `ServerInitializeError::Cancelled`), not
                    // a service failure, so Ctrl-C still exits zero.
                    if shutdown_ct.is_cancelled()
                        && matches!(error, rmcp::service::ServerInitializeError::Cancelled)
                    {
                        None
                    } else {
                        Some(format!("failed to initialize MCP stdio server: {error}"))
                    }
                }
            }
        }
    };
    let report = core.shutdown().await;
    let cleanup_errors = summarize(&report);
    match (primary.take(), cleanup_errors.is_empty()) {
        (Some(primary), _) => Err(ServeFailure {
            primary,
            cleanup_errors,
        }),
        (None, false) => Err(ServeFailure {
            primary: "server stopped with cleanup failures".into(),
            cleanup_errors,
        }),
        (None, true) => Ok(()),
    }
}

/// Convenience wrapper used by production `main`: serve over stdio.
pub async fn serve_stdio(config: Config) -> Result<(), ServeFailure> {
    serve_with_transport(config, stdio()).await
}
