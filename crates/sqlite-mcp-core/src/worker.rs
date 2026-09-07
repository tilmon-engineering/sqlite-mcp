use crate::operation::{ShutdownStatus, WorkerShutdownError};
use crate::{policy, test_support};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod limits;
thread_local! {
    static BUSY_CONTEXT: std::cell::RefCell<Option<(CancelToken, Instant)>> = const { std::cell::RefCell::new(None) };
    /// Worker-lifetime shutdown token: consulted by busy retries and progress
    /// callbacks so shutdown interrupts active work (F-03).
    static SHUTDOWN_TOKEN: std::cell::RefCell<Option<CancelToken>> = const { std::cell::RefCell::new(None) };
}
fn shutdown_requested() -> bool {
    SHUTDOWN_TOKEN.with(|token| {
        token
            .borrow()
            .as_ref()
            .is_some_and(CancelToken::is_cancelled)
    })
}
fn busy_retry(_count: i32) -> bool {
    test_support::emit(test_support::Event::BusyRetry);
    if shutdown_requested() {
        return false;
    }
    BUSY_CONTEXT.with(|ctx| {
        let binding = ctx.borrow();
        let Some((token, deadline)) = binding.as_ref() else {
            return false;
        };
        if token.is_cancelled() || Instant::now() >= *deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
        true
    })
}
#[derive(Clone)]
pub struct CancelToken(CancellationToken);
impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}
impl CancelToken {
    pub fn new() -> Self {
        Self(CancellationToken::new())
    }
    pub fn from_cancellation_token(token: CancellationToken) -> Self {
        Self(token)
    }
    pub fn cancel(&self) {
        self.0.cancel()
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}
use rusqlite::Connection;
use std::thread;
use thiserror::Error;
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("worker closed")]
    Closed,
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Message(String),
    #[error("result too large: retained {retained_bytes} bytes exceeds limit {limit}")]
    ResultTooLarge { retained_bytes: usize, limit: usize },
    /// The request was interrupted. `by_client` distinguishes an explicit
    /// client cancellation from a query-deadline expiry so callers can
    /// report the truthful outcome class.
    #[error("request interrupted")]
    Interrupted {
        by_client: bool,
        transaction_open: bool,
        transaction_continuable: bool,
    },
}
#[derive(Clone)]
pub struct RequestContext {
    pub token: CancelToken,
    pub deadline: Instant,
}
#[derive(Clone)]
pub struct RequestHandle {
    token: CancelToken,
}
impl RequestHandle {
    pub fn new(token: CancelToken) -> Self {
        Self { token }
    }
    pub fn cancel(&self) {
        self.token.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}
impl RequestContext {
    pub fn new(timeout: Duration) -> Self {
        Self {
            token: CancelToken::new(),
            deadline: Instant::now() + timeout,
        }
    }
}
pub type Job = Box<
    dyn FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
        + Send
        + 'static,
>;
#[allow(dead_code)]
pub enum Command {
    Run(
        Job,
        RequestContext,
        oneshot::Sender<Result<serde_json::Value, WorkerError>>,
    ),
    Control(Job, oneshot::Sender<Result<serde_json::Value, WorkerError>>),
    Expire(oneshot::Sender<bool>),
    Shutdown(oneshot::Sender<Result<ShutdownStatus, WorkerShutdownError>>),
}
pub use limits::{WorkerLimits, install_limits};
#[derive(Clone)]
pub struct Worker {
    tx: mpsc::Sender<Command>,
    cancel: CancelToken,
    /// Shared with the authorizer closure on the worker thread; read/reset by
    /// the caller after each dispatched request.
    pub mutation_seen: crate::policy::MutationFlag,
    join_handle: Arc<std::sync::Mutex<Option<thread::JoinHandle<()>>>>,
    /// Idempotent shared completion for clones: concurrent and repeated
    /// `shutdown` calls resolve the same result.
    completed: Arc<tokio::sync::OnceCell<Result<ShutdownStatus, WorkerShutdownError>>>,
}
impl Worker {
    pub fn start(
        path: String,
        readonly: bool,
        capacity: usize,
        idle_seconds: u64,
        busy_wait_ms: u64,
        limits: WorkerLimits,
    ) -> Result<Self, WorkerError> {
        let (tx, mut rx) = mpsc::channel(capacity);
        let cancel = CancelToken::new();
        let mutation_seen = crate::policy::MutationFlag::new();
        let thread_flag = mutation_seen.clone();
        let join_handle: Arc<std::sync::Mutex<Option<thread::JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let thread_cancel = cancel.clone();
        let handle = thread::Builder::new()
            .name("sqlite-mcp-worker".into())
            .spawn(move || {
                SHUTDOWN_TOKEN.with(|slot| *slot.borrow_mut() = Some(thread_cancel.clone()));
                let mut conn = Connection::open_with_flags(
                    &path,
                    if readonly {
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    } else {
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    },
                )
                .ok();
                if let Some(conn) = conn.as_mut() {
                    let mut expired = false;
                    let mut idle_deadline =
                        test_support::now_ms().saturating_add(idle_seconds.saturating_mul(1000));
                    let _ = policy::install_authorizer(
                        conn,
                        readonly,
                        thread_flag.clone(),
                    );
                    let _ = policy::trusted(|| conn.pragma_update(None, "foreign_keys", true));
                    let _ = conn.busy_timeout(Duration::from_millis(busy_wait_ms));
                    if limits::install_limits(conn, &limits).is_err() {
                        return;
                    }
                    // These engine limits do not affect the server's own
                    // lifecycle/schema SQL and mirror the Rust-side checks;
                    // SQLITE_LIMIT_COLUMN and SQLITE_LIMIT_SQL_LENGTH stay
                    // enforced in the Rust query path only, because engine
                    // column/SQL caps can break the server's own schema
                    // introspection statements.
                    // Engine limits were installed and verified above.
                    let _ = (
                        limits.sql_byte_limit,
                        limits.column_limit,
                    );
                    while let Some(cmd) = rx.blocking_recv() {
                        match cmd {
                            Command::Run(f, ctx, r) => {
                                test_support::emit(test_support::Event::AdmissionDequeue);
                                // Deterministic precedence: shutdown > expired
                                // transaction > client cancellation > deadline.
                                if shutdown_requested() {
                                    let _ = r.send(Err(WorkerError::Message(
                                        "SERVER_SHUTDOWN".into(),
                                    )));
                                    continue;
                                }
                                if expired {
                                    let _ = r.send(Err(WorkerError::Message(
                                        "TRANSACTION_EXPIRED".into(),
                                    )));
                                    continue;
                                }
                                if test_support::now_ms() >= idle_deadline
                                    && !conn.is_autocommit()
                                {
                                    let _ = policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    expired = true;
                                    let _ = r.send(Err(WorkerError::Message(
                                        "TRANSACTION_EXPIRED".into(),
                                    )));
                                    continue;
                                }
                                if ctx.token.is_cancelled() {
                                    let _ = r.send(Err(WorkerError::Interrupted {
                                        by_client: true,
                                        transaction_open: !conn.is_autocommit(),
                                        transaction_continuable: !conn.is_autocommit(),
                                    }));
                                    continue;
                                }
                                if Instant::now() >= ctx.deadline {
                                    let _ = r.send(Err(WorkerError::Interrupted {
                                        by_client: false,
                                        transaction_open: !conn.is_autocommit(),
                                        transaction_continuable: !conn.is_autocommit(),
                                    }));
                                    continue;
                                }
                                test_support::emit(test_support::Event::RequestHandover);
                                thread_flag.reset_for_request();
                                let token = ctx.token.clone();
                                let deadline = ctx.deadline;
                                BUSY_CONTEXT.with(|slot| {
                                    *slot.borrow_mut() = Some((token.clone(), deadline))
                                });
                                let _ = conn.busy_handler(Some(busy_retry));
                                let shutdown_flag = SHUTDOWN_TOKEN
                                    .with(|slot| slot.borrow().as_ref().map(|t| t.0.clone()));
                                let _ = conn.progress_handler(
                                    1000,
                                    Some(move || {
                                        token.is_cancelled()
                                            || shutdown_flag
                                                .as_ref()
                                                .is_some_and(|t| t.is_cancelled())
                                            || Instant::now() >= deadline
                                    }),
                                );
                                let mut result = f(conn, &ctx);
                                test_support::emit(test_support::Event::BeginCompletion);
                                let _ = conn.progress_handler(0, None::<fn() -> bool>);
                                let _ = conn.busy_handler(None);
                                BUSY_CONTEXT.with(|slot| *slot.borrow_mut() = None);
                                if ctx.token.is_cancelled() {
                                    // The interrupt may already have aborted the
                                    // whole outer transaction: "no transaction is
                                    // active" with autocommit restored is benign.
                                    let rollback =
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    let benign = rollback.is_err()
                                        && conn.is_autocommit()
                                        && rollback
                                            .as_ref()
                                            .unwrap_err()
                                            .to_string()
                                            .contains("no transaction is active");
                                    if !benign && (rollback.is_err() || !conn.is_autocommit()) {
                                        let detail = rollback
                                            .as_ref()
                                            .err()
                                            .map(|e| e.to_string())
                                            .unwrap_or_default();
                                        result = Err(WorkerError::Message(format!(
                                            "handle invalidated; lost uncommitted work{detail_suffix}",
                                            detail_suffix = if detail.is_empty() {
                                                String::new()
                                            } else {
                                                format!(": {detail}")
                                            }
                                        )));
                                    } else if matches!(&result, Err(WorkerError::Sqlite(_))) {
                                        // The progress handler interrupted the
                                        // statement because the client cancelled:
                                        // report the truthful cancellation class
                                        // with the post-cleanup transaction state.
                                        let open = !conn.is_autocommit();
                                        result = Err(WorkerError::Interrupted {
                                            by_client: true,
                                            transaction_open: open,
                                            transaction_continuable: open,
                                        });
                                    }
                                } else if shutdown_requested()
                                    && matches!(&result, Err(WorkerError::Sqlite(_)))
                                {
                                    // Progress handler fired on shutdown, not
                                    // client cancellation or deadline: report
                                    // the typed shutdown cause (INTERNAL-class
                                    // message), never a fake cancel/deadline.
                                    let rollback =
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    let benign = rollback.is_err()
                                        && conn.is_autocommit()
                                        && rollback
                                            .as_ref()
                                            .unwrap_err()
                                            .to_string()
                                            .contains("no transaction is active");
                                    if !benign && (rollback.is_err() || !conn.is_autocommit()) {
                                        let detail = rollback
                                            .as_ref()
                                            .err()
                                            .map(|e| e.to_string())
                                            .unwrap_or_default();
                                        let detail_suffix = if detail.is_empty() {
                                            String::new()
                                        } else {
                                            format!(": {detail}")
                                        };
                                        result = Err(WorkerError::Message(format!(
                                            "SERVER_SHUTDOWN; handle invalidated; lost uncommitted work{detail_suffix}"
                                        )));
                                    } else {
                                        result = Err(WorkerError::Message(
                                            "SERVER_SHUTDOWN".into(),
                                        ));
                                    }
                                } else if matches!(&result, Err(WorkerError::Sqlite(_)))
                                    && Instant::now() >= ctx.deadline
                                {
                                    // Progress handler fired on the deadline, not
                                    // client cancellation: truthful deadline class.
                                    let rollback =
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    let benign = rollback.is_err()
                                        && conn.is_autocommit()
                                        && rollback
                                            .as_ref()
                                            .unwrap_err()
                                            .to_string()
                                            .contains("no transaction is active");
                                    if !benign && (rollback.is_err() || !conn.is_autocommit()) {
                                        let detail = rollback
                                            .as_ref()
                                            .err()
                                            .map(|e| e.to_string())
                                            .unwrap_or_default();
                                        result = Err(WorkerError::Message(format!(
                                            "handle invalidated; lost uncommitted work{detail_suffix}",
                                            detail_suffix = if detail.is_empty() {
                                                String::new()
                                            } else {
                                                format!(": {detail}")
                                            }
                                        )));
                                    } else {
                                        let open = !conn.is_autocommit();
                                        result = Err(WorkerError::Interrupted {
                                            by_client: false,
                                            transaction_open: open,
                                            transaction_continuable: open,
                                        });
                                    }
                                }
                                let _ = r.send(result);
                                test_support::emit(test_support::Event::RequestHandover);
                                idle_deadline = test_support::now_ms()
                                    .saturating_add(idle_seconds.saturating_mul(1000));
                            }
                            Command::Control(f, r) => {
                                test_support::emit(test_support::Event::ControlEntry);
                                if expired {
                                    let _ = r.send(Err(WorkerError::Message("TRANSACTION_EXPIRED".into())));
                                    continue;
                                }
                                if !conn.is_autocommit() && test_support::now_ms() >= idle_deadline {
                                    let fault = test_support::take_cleanup_fault(crate::operation::CleanupStage::ExpiryRollback);
                                    let _rollback = policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    let _ = fault;
                                    expired = true;
                                    let _ = r.send(Err(WorkerError::Message("TRANSACTION_EXPIRED".into())));
                                    continue;
                                }
                                // Control cleanup is deliberately not wired to a request token.
                                let result = f(
                                    conn,
                                    &RequestContext::new(Duration::from_secs(365 * 24 * 60 * 60)),
                                );
                                let _ = r.send(result);
                                idle_deadline = test_support::now_ms()
                                    .saturating_add(idle_seconds.saturating_mul(1000));
                            }
                            Command::Expire(r) => {
                                let expired = !conn.is_autocommit();
                                if expired {
                                    let _ = policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                }
                                let _ = r.send(expired);
                            }
                            Command::Shutdown(r) => {
                                // Verify rollback before reporting: a rollback
                                // error with autocommit not restored leaves the
                                // connection state uncertain.
                                let mut failure: Option<WorkerShutdownError> = None;
                                if !conn.is_autocommit() {
                                    let injected = test_support::take_cleanup_fault(
                                        crate::operation::CleanupStage::ConnectionRollback,
                                    )
                                    .is_some();
                                    let rollback =
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    if injected {
                                        failure = Some(WorkerShutdownError::RollbackUncertain(
                                            "injected connection rollback failure".into(),
                                        ));
                                    } else if let Err(error) = rollback {
                                        let benign = conn.is_autocommit()
                                            && error
                                                .to_string()
                                                .contains("no transaction is active");
                                        if !benign {
                                            failure = Some(
                                                WorkerShutdownError::RollbackUncertain(
                                                    error.to_string(),
                                                ),
                                            );
                                        }
                                    }
                                }
                                let _ = r.send(match failure {
                                    Some(error) => Err(error),
                                    None => Ok(ShutdownStatus {
                                        connection_closed: true,
                                        // The caller confirms the thread exit by
                                        // joining after this reply.
                                        thread_joined: false,
                                    }),
                                });
                                // Dropping `conn` on loop exit closes the
                                // SQLite connection.
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|_| WorkerError::Closed)?;
        *join_handle.lock().expect("worker join slot poisoned") = Some(handle);
        Ok(Self {
            tx,
            cancel,
            mutation_seen,
            join_handle,
            completed: Arc::new(tokio::sync::OnceCell::new()),
        })
    }
    #[allow(dead_code)]
    pub async fn run_with_handle<F>(
        &self,
        timeout: Duration,
        f: F,
    ) -> (
        RequestHandle,
        impl std::future::Future<Output = Result<serde_json::Value, WorkerError>>,
    )
    where
        F: FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
            + Send
            + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        let ctx = RequestContext::new(timeout);
        let handle = RequestHandle {
            token: ctx.token.clone(),
        };
        let tx = self.tx.clone();
        let fut = async move {
            test_support::emit(test_support::Event::AdmissionEnqueue);
            tx.send(Command::Run(Box::new(f), ctx, rtx))
                .await
                .map_err(|_| WorkerError::Closed)?;
            rrx.await.map_err(|_| WorkerError::Closed)?
        };
        (handle, fut)
    }
    pub async fn run_control<F>(&self, f: F) -> Result<serde_json::Value, WorkerError>
    where
        F: FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
            + Send
            + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Command::Control(Box::new(f), rtx))
            .await
            .map_err(|_| WorkerError::Closed)?;
        rrx.await.map_err(|_| WorkerError::Closed)?
    }
    pub async fn run_with_token<F>(
        &self,
        timeout: Duration,
        token: CancelToken,
        f: F,
    ) -> Result<serde_json::Value, WorkerError>
    where
        F: FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
            + Send
            + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        let ctx = RequestContext {
            token,
            deadline: Instant::now() + timeout,
        };
        self.tx
            .send(Command::Run(Box::new(f), ctx, rtx))
            .await
            .map_err(|_| WorkerError::Closed)?;
        rrx.await.map_err(|_| WorkerError::Closed)?
    }
    pub async fn run_with_optional_token<F>(
        &self,
        timeout: Duration,
        token: Option<CancellationToken>,
        f: F,
    ) -> Result<serde_json::Value, WorkerError>
    where
        F: FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
            + Send
            + 'static,
    {
        match token {
            Some(token) => {
                self.run_with_token(timeout, CancelToken::from_cancellation_token(token), f)
                    .await
            }
            None => self.run(timeout, f).await,
        }
    }
    pub async fn run<F>(&self, timeout: Duration, f: F) -> Result<serde_json::Value, WorkerError>
    where
        F: FnOnce(&mut Connection, &RequestContext) -> Result<serde_json::Value, WorkerError>
            + Send
            + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        let ctx = RequestContext::new(timeout);
        self.tx
            .send(Command::Run(Box::new(f), ctx, rtx))
            .await
            .map_err(|_| WorkerError::Closed)?;
        rrx.await.map_err(|_| WorkerError::Closed)?
    }
    #[allow(dead_code)]
    pub async fn expire(&self) -> Result<bool, WorkerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Expire(tx))
            .await
            .map_err(|_| WorkerError::Closed)?;
        rx.await.map_err(|_| WorkerError::Closed)
    }
    pub async fn shutdown(&self) -> Result<ShutdownStatus, WorkerShutdownError> {
        self.completed
            .get_or_init(|| async {
                test_support::emit(test_support::Event::ShutdownRequested);
                self.cancel.cancel();
                let (r, rr) = oneshot::channel();
                let status = match self.tx.send(Command::Shutdown(r)).await {
                    Err(_) => {
                        // The worker channel is closed: either already shut
                        // down through this cell (impossible — single init) or
                        // the thread is gone without a report.
                        Err(WorkerShutdownError::JoinUncertain(
                            "worker channel closed before shutdown completed".into(),
                        ))
                    }
                    Ok(()) => match rr.await {
                        Ok(Ok(mut status)) => {
                            // Confirm the thread exit (and therefore the
                            // connection drop) off the Tokio runtime threads.
                            let handle = self
                                .join_handle
                                .lock()
                                .expect("worker join slot poisoned")
                                .take();
                            let joined = match handle {
                                Some(handle) => {
                                    matches!(
                                        tokio::task::spawn_blocking(move || handle.join()).await,
                                        Ok(Ok(()))
                                    )
                                }
                                None => false,
                            };
                            status.thread_joined = joined;
                            if status.is_success() {
                                Ok(status)
                            } else {
                                Err(WorkerShutdownError::JoinUncertain(
                                    "worker thread did not confirm a clean exit".into(),
                                ))
                            }
                        }
                        Ok(Err(error)) => {
                            // Rollback/close uncertainty: still confirm the
                            // thread exit so the report is truthful about what
                            // IS known.
                            let handle = self
                                .join_handle
                                .lock()
                                .expect("worker join slot poisoned")
                                .take();
                            if let Some(handle) = handle {
                                let _ = tokio::task::spawn_blocking(move || handle.join()).await;
                            }
                            Err(error)
                        }
                        Err(_) => Err(WorkerShutdownError::JoinUncertain(
                            "worker dropped the shutdown report".into(),
                        )),
                    },
                };
                test_support::emit(test_support::Event::ShutdownClosed);
                status
            })
            .await
            .clone()
    }
}
