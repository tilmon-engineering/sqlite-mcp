use crate::operation::{ShutdownStatus, WorkerShutdownError};
use crate::{policy, test_support};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod limits;
/// State of the currently active bounded busy window. `None` in the
/// thread-local slot means no window is active and SQLITE_BUSY surfaces
/// immediately.
#[derive(Clone)]
struct BusyState {
    /// Request cancellation token; `None` for non-cancellable cleanup
    /// windows.
    token: Option<CancelToken>,
    /// Request deadline; `None` for non-cancellable cleanup windows.
    deadline: Option<Instant>,
    /// Configured per-request busy budget (`busy_wait_ms`).
    budget: Duration,
    /// Instant of the first retry in this window; the budget is measured
    /// from here so each request gets a full, fresh budget.
    first_retry: Option<Instant>,
}
thread_local! {
    static BUSY_CONTEXT: std::cell::RefCell<Option<BusyState>> = const { std::cell::RefCell::new(None) };
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
        let mut binding = ctx.borrow_mut();
        let Some(state) = binding.as_mut() else {
            return false;
        };
        if state.token.as_ref().is_some_and(CancelToken::is_cancelled) {
            return false;
        }
        if let Some(deadline) = state.deadline
            && Instant::now() >= deadline
        {
            return false;
        }
        let now = Instant::now();
        let first = *state.first_retry.get_or_insert(now);
        // The bounded busy wait is capped at `busy_wait_ms` per request;
        // exhaustion surfaces SQLITE_BUSY (DESIGN §4).
        if now >= first + state.budget {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
        true
    })
}

/// Monotonic id for guard/first-operation event keys so each emission is
/// individually addressable by the deterministic worker fixture.
fn guard_event_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn guard_event_key(command: &'static str, branch: &'static str) -> test_support::EventKey {
    test_support::EventKey {
        operation_id: guard_event_seq(),
        generation: 0,
        worker_id: Some(format!("{command}:{branch}")),
        creation_id: None,
    }
}

fn emit_guard_installed(command: &'static str, branch: &'static str) {
    test_support::emit_keyed(
        test_support::Event::GuardInstalled,
        Some(guard_event_key(command, branch)),
    );
}

fn emit_first_sqlite_operation(command: &'static str, branch: &'static str) {
    test_support::emit_keyed(
        test_support::Event::FirstSqliteOperation,
        Some(guard_event_key(command, branch)),
    );
}

/// RAII bounded busy window for one worker command arm. Installing swaps a
/// fresh [`BusyState`] into the thread-local slot (the `busy_retry` handler
/// itself is installed once for the worker lifetime); dropping clears the
/// slot unconditionally on every exit path of the scoped arm, so no context
/// leaks across commands and no command path runs without a bounded window.
///
/// The arm-level guard starts as a non-cancellable budget-only window (it
/// covers the arm's pre-checks and expiry rollbacks); the Run main path
/// swaps in the request-scoped window (request token + deadline) around the
/// job, and swaps back to a fresh non-cancellable window for post-request
/// cleanup, so cleanup never inherits an expired or cancelled request
/// context.
struct BusyGuard {
    command: &'static str,
    branch: &'static str,
}

impl BusyGuard {
    fn install(command: &'static str, branch: &'static str, budget: Duration) -> Self {
        Self::set_window(command, branch, None, None, budget);
        Self { command, branch }
    }
    fn enter_request(&self, token: CancelToken, deadline: Instant, budget: Duration) {
        Self::set_window(self.command, "request", Some(token), Some(deadline), budget);
    }
    fn enter_cleanup(&self, budget: Duration) {
        Self::set_window(self.command, "cleanup", None, None, budget);
    }
    fn set_window(
        command: &'static str,
        branch: &'static str,
        token: Option<CancelToken>,
        deadline: Option<Instant>,
        budget: Duration,
    ) {
        BUSY_CONTEXT.with(|slot| {
            *slot.borrow_mut() = Some(BusyState {
                token,
                deadline,
                budget,
                first_retry: None,
            })
        });
        emit_guard_installed(command, branch);
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY_CONTEXT.with(|slot| *slot.borrow_mut() = None);
        test_support::emit_keyed(
            test_support::Event::GuardCleared,
            Some(guard_event_key(self.command, self.branch)),
        );
    }
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
#[derive(Debug)]
pub(crate) struct CommitOutcome {
    pub(crate) result: Result<serde_json::Value, WorkerError>,
    pub(crate) commit_confirmed: bool,
    pub(crate) transaction_open: bool,
    pub(crate) transaction_continuable: bool,
    pub(crate) expired: bool,
    pub(crate) uncertain_or_invalidated: bool,
}

impl CommitOutcome {
    fn success(value: serde_json::Value, schema_version: i64) -> Self {
        Self {
            result: Ok(serde_json::json!({"committed": value, "schema_version": schema_version})),
            commit_confirmed: true,
            transaction_open: false,
            transaction_continuable: false,
            expired: false,
            uncertain_or_invalidated: false,
        }
    }
    fn failure(
        result: Result<serde_json::Value, WorkerError>,
        open: bool,
        continuable: bool,
        uncertain: bool,
    ) -> Self {
        Self {
            result,
            commit_confirmed: false,
            transaction_open: open,
            transaction_continuable: open && continuable,
            expired: false,
            uncertain_or_invalidated: uncertain,
        }
    }
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
    Commit(oneshot::Sender<CommitOutcome>),
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
                    // The bounded busy handler is installed once for the
                    // worker lifetime; per-request windows are managed by
                    // BusyGuard in each command arm. With no window active a
                    // lock conflict surfaces SQLITE_BUSY immediately.
                    let _ = conn.busy_handler(Some(busy_retry));
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
                        let busy_budget = Duration::from_millis(busy_wait_ms);
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
                                let _busy_guard = BusyGuard::install("run", "arm", busy_budget);
                                if test_support::now_ms() >= idle_deadline
                                    && !conn.is_autocommit()
                                {
                                    emit_first_sqlite_operation("run", "idle-expiry");
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
                                _busy_guard.enter_request(token.clone(), deadline, busy_budget);
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
                                emit_first_sqlite_operation("run", "request");
                                let mut result = f(conn, &ctx);
                                test_support::emit(test_support::Event::BeginCompletion);
                                let _ = conn.progress_handler(0, None::<fn() -> bool>);
                                // Post-request cleanup (rollback verification,
                                // expiry accounting) runs under a fresh
                                // non-cancellable bounded window, never the
                                // drained request context.
                                _busy_guard.enter_cleanup(busy_budget);
                                emit_first_sqlite_operation("run", "cleanup");
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
                                idle_deadline = test_support::now_ms()
                                    .saturating_add(idle_seconds.saturating_mul(1000));
                                let _ = r.send(result);
                                test_support::emit(test_support::Event::RequestHandover);
                            }
                            Command::Control(f, r) => {
                                test_support::emit(test_support::Event::ControlEntry);
                                let _busy_guard =
                                    BusyGuard::install("control", "arm", busy_budget);
                                if expired {
                                    let _ = r.send(Err(WorkerError::Message("TRANSACTION_EXPIRED".into())));
                                    continue;
                                }
                                if !conn.is_autocommit() && test_support::now_ms() >= idle_deadline {
                                    emit_first_sqlite_operation("control", "expiry");
                                    let injected = test_support::take_cleanup_fault(crate::operation::CleanupStage::ExpiryRollback).is_some();
                                    let rollback = if injected {
                                        Err(rusqlite::Error::InvalidQuery)
                                    } else {
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"))
                                    };
                                    if rollback.is_ok() && conn.is_autocommit() {
                                        expired = true;
                                        let _ = r.send(Err(WorkerError::Message("TRANSACTION_EXPIRED".into())));
                                    } else {
                                        let _ = r.send(Err(WorkerError::Message("handle invalidated; expiry rollback uncertain".into())));
                                    }
                                    continue;
                                }
                                // Control cleanup is deliberately not wired to a request token.
                                emit_first_sqlite_operation("control", "normal");
                                let result = f(
                                    conn,
                                    &RequestContext::new(Duration::from_secs(365 * 24 * 60 * 60)),
                                );
                                let _ = r.send(result);
                                idle_deadline = test_support::now_ms()
                                    .saturating_add(idle_seconds.saturating_mul(1000));
                            }
                            Command::Commit(r) => {
                                test_support::emit(test_support::Event::CommitEntry);
                                let _busy_guard = BusyGuard::install("commit", "arm", busy_budget);
                                if !expired && !conn.is_autocommit() && test_support::now_ms() >= idle_deadline {
                                    emit_first_sqlite_operation("commit", "expiry");
                                    let rollback = policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                    if rollback.is_ok() && conn.is_autocommit() {
                                        expired = true;
                                        let _ = r.send(CommitOutcome { result: Err(WorkerError::Message("TRANSACTION_EXPIRED".into())), commit_confirmed: false, transaction_open: false, transaction_continuable: false, expired: true, uncertain_or_invalidated: false });
                                    } else {
                                        let _ = r.send(CommitOutcome::failure(Err(WorkerError::Message("handle invalidated; expiry rollback uncertain".into())), !conn.is_autocommit(), false, true));
                                    }
                                    continue;
                                }
                                if expired {
                                    let _ = r.send(CommitOutcome { result: Err(WorkerError::Message("TRANSACTION_EXPIRED".into())), commit_confirmed: false, transaction_open: false, transaction_continuable: false, expired: true, uncertain_or_invalidated: false });
                                    continue;
                                }
                                if conn.is_autocommit() {
                                    emit_first_sqlite_operation("commit", "no-transaction");
                                    let _ = r.send(CommitOutcome::failure(Err(WorkerError::Message("no transaction open".into())), false, false, false));
                                    continue;
                                }
                                emit_first_sqlite_operation("commit", "normal");
                                let fault = test_support::take_commit_fault();
                                let commit = match fault {
                                    Some(test_support::CommitFault::OpenContinuable) => Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), Some("injected open continuable commit fault".into()))),
                                    Some(test_support::CommitFault::AutocommitRestoredUnconfirmed) | Some(test_support::CommitFault::Uncertain) => {
                                        let rollback = policy::trusted(|| conn.execute_batch("ROLLBACK"));
                                        if rollback.is_ok() && conn.is_autocommit() { Err(rusqlite::Error::InvalidQuery) } else { Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), Some("injected uncertain commit fault".into()))) }
                                    }
                                    Some(test_support::CommitFault::AutocommitRestored) => {
                                        let result = policy::trusted(|| conn.execute_batch("COMMIT"));
                                        if result.is_ok() && conn.is_autocommit() { Err(rusqlite::Error::InvalidQuery) } else { result }
                                    }
                                    None => policy::trusted(|| conn.execute_batch("COMMIT")),
                                };
                                let autocommit = conn.is_autocommit();
                                test_support::emit(test_support::Event::CommitReturn);
                                match (fault, commit) {
                                    (Some(test_support::CommitFault::AutocommitRestored), Err(error)) if autocommit => {
                                        let _ = r.send(CommitOutcome { result: Err(WorkerError::Sqlite(error)), commit_confirmed: true, transaction_open: false, transaction_continuable: false, expired: false, uncertain_or_invalidated: false });
                                    }
                                    (Some(test_support::CommitFault::AutocommitRestoredUnconfirmed), Err(error)) if autocommit => {
                                        let _ = r.send(CommitOutcome::failure(Err(WorkerError::Sqlite(error)), false, false, false));
                                    }
                                    (Some(test_support::CommitFault::Uncertain), Err(error)) => {
                                        let _ = r.send(CommitOutcome::failure(Err(WorkerError::Sqlite(error)), !autocommit, false, true));
                                    }
                                    (Some(test_support::CommitFault::OpenContinuable), Err(error)) => {
                                        let _ = r.send(CommitOutcome::failure(Err(WorkerError::Sqlite(error)), !autocommit, !autocommit, false));
                                    }
                                    (None, Ok(())) if autocommit => {
                                        match policy::trusted(|| conn.query_row("PRAGMA schema_version", [], |row| row.get(0))) {
                                            Ok(version) => { let _ = r.send(CommitOutcome::success(serde_json::json!(true), version)); }
                                            Err(error) => { let _ = r.send(CommitOutcome { result: Err(WorkerError::Sqlite(error)), commit_confirmed: true, transaction_open: false, transaction_continuable: false, expired: false, uncertain_or_invalidated: true }); }
                                        }
                                    }
                                    (_, Ok(())) => { let _ = r.send(CommitOutcome::failure(Err(WorkerError::Message("commit completed without restoring autocommit".into())), true, true, true)); }
                                    (_, Err(error)) if !autocommit => { let _ = r.send(CommitOutcome::failure(Err(WorkerError::Sqlite(error)), true, true, false)); }
                                    (_, Err(error)) => { let _ = r.send(CommitOutcome::failure(Err(WorkerError::Sqlite(error)), false, false, true)); }
                                }
                            }
                            Command::Expire(r) => {
                                let _busy_guard = BusyGuard::install("expire", "arm", busy_budget);
                                emit_first_sqlite_operation("expire", "normal");
                                let expired = !conn.is_autocommit();
                                if expired {
                                    let injected = test_support::take_cleanup_fault(crate::operation::CleanupStage::ExpiryRollback).is_some();
                                    let rollback = if injected {
                                        Err(rusqlite::Error::InvalidQuery)
                                    } else {
                                        policy::trusted(|| conn.execute_batch("ROLLBACK"))
                                    };
                                    if rollback.is_err() || !conn.is_autocommit() {
                                        let _ = r.send(false);
                                        continue;
                                    }
                                }
                                let _ = r.send(expired);
                            }
                            Command::Shutdown(r) => {
                                let _busy_guard =
                                    BusyGuard::install("shutdown", "arm", busy_budget);
                                emit_first_sqlite_operation("shutdown", "cleanup");
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
    pub(crate) async fn run_commit(&self) -> Result<CommitOutcome, WorkerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Commit(tx))
            .await
            .map_err(|_| WorkerError::Closed)?;
        rx.await.map_err(|_| WorkerError::Closed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy;
    use crate::test_support::{self, Event};
    use std::sync::atomic::Ordering;

    /// Deterministic bounded-busy-window fixture: drives the worker command
    /// arms directly under the SQLITE_MCP_TEST_SUPPORT gate with the injected
    /// clock and asserts the guard ordering invariants — every command arm
    /// installs a bounded busy window before its first SQLite operation,
    /// clears it exactly once on every exit path, and leaves no window
    /// behind for the next command.
    /// Env-var manipulation is process-global and other lib tests (for
    /// example core lifecycle pins) also run workers whose emissions the
    /// fixture counts, so every hook-touching test serializes on the shared
    /// test-support lock.
    struct SupportGuard(#[allow(dead_code)] tokio::sync::MutexGuard<'static, ()>);
    impl SupportGuard {
        async fn enable() -> Self {
            let guard = test_support::TEST_HOOK_LOCK.lock().await;
            // SAFETY: the shared lock serializes every test that touches the
            // marker, and the marker is removed before the guard releases.
            unsafe {
                std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1");
            }
            test_support::reset_registry();
            // Take control of the injected clock (0 means "real time").
            test_support::set_clock_ms(1);
            SupportGuard(guard)
        }
    }
    impl Drop for SupportGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("SQLITE_MCP_TEST_SUPPORT");
            }
            test_support::reset_registry();
        }
    }

    fn seqs(event: Event, label: &str) -> Vec<u64> {
        test_support::retained_records(event)
            .into_iter()
            .filter(|record| {
                record.key.as_ref().and_then(|key| key.worker_id.as_deref()) == Some(label)
            })
            .map(|record| record.order)
            .collect()
    }

    fn one_seq(event: Event, label: &str) -> u64 {
        let seqs = seqs(event, label);
        assert_eq!(
            seqs.len(),
            1,
            "expected exactly one {event:?} for {label:?}, got {seqs:?}"
        );
        seqs[0]
    }

    /// Assert the arm invariant across every observed command of this kind:
    /// each command installs its guard before any branch's first SQLite
    /// operation, clears it exactly once before the next command's guard,
    /// and windows never overlap.
    fn assert_arm_window(command: &str, branches: &[&str]) {
        let arm = format!("{command}:arm");
        let mut installs = seqs(Event::GuardInstalled, &arm);
        let mut cleared = seqs(Event::GuardCleared, &arm);
        installs.sort_unstable();
        cleared.sort_unstable();
        assert!(
            !installs.is_empty(),
            "{command}: no guard window observed at all"
        );
        assert_eq!(
            installs.len(),
            cleared.len(),
            "{command}: guard install/clear count imbalance (installs={installs:?}, cleared={cleared:?})"
        );
        for (install, clear) in installs.iter().zip(&cleared) {
            assert!(
                install < clear,
                "{command}: guard cleared before installed ({install} >= {clear})"
            );
        }
        for (clear, next_install) in cleared.iter().zip(installs.iter().skip(1)) {
            assert!(
                clear < next_install,
                "{command}: guard windows overlap ({clear} >= {next_install})"
            );
        }
        for branch in branches {
            let label = format!("{command}:{branch}");
            let firsts = seqs(Event::FirstSqliteOperation, &label);
            assert!(
                !firsts.is_empty(),
                "{command}: no first SQLite operation observed for {branch:?}"
            );
            for first in firsts {
                let inside = installs
                    .iter()
                    .zip(&cleared)
                    .any(|(install, clear)| install < &first && &first < clear);
                assert!(
                    inside,
                    "{command}: first operation for {branch:?} at order {first} is outside every \
                     guard window (installs={installs:?}, cleared={cleared:?})"
                );
            }
        }
    }

    fn start_test_worker(idle_seconds: u64) -> (tempfile::TempDir, Worker) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker-fixture.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t(a)").unwrap();
        }
        let worker = Worker::start(
            path.to_str().unwrap().to_owned(),
            false,
            8,
            idle_seconds,
            50,
            limits::WorkerLimits {
                cell_byte_limit: 1_048_576,
                sql_byte_limit: 102_400,
                column_limit: 256,
                expression_depth: 100,
                compound_terms: 50,
                parameter_limit: 1000,
            },
        )
        .expect("start worker");
        (dir, worker)
    }

    fn trusted_begin() -> Job {
        Box::new(|c: &mut Connection, _ctx| {
            policy::trusted(|| c.execute_batch("BEGIN")).map_err(WorkerError::Sqlite)?;
            Ok(serde_json::json!({"begun": true}))
        })
    }

    #[tokio::test]
    async fn run_normal_guard_window() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        let job: Job = Box::new(|c: &mut Connection, _ctx: &RequestContext| {
            policy::trusted(|| c.execute_batch("SELECT 1")).map_err(WorkerError::Sqlite)?;
            Ok(serde_json::json!({"ok": true}))
        });
        worker
            .run(Duration::from_secs(5), job)
            .await
            .expect("run job");
        assert_arm_window("run", &["request", "cleanup"]);
        // The request window is installed before the job's first operation.
        let arm = one_seq(Event::GuardInstalled, "run:arm");
        let request_install = one_seq(Event::GuardInstalled, "run:request");
        let request_first = one_seq(Event::FirstSqliteOperation, "run:request");
        let cleanup_install = one_seq(Event::GuardInstalled, "run:cleanup");
        let cleanup_first = one_seq(Event::FirstSqliteOperation, "run:cleanup");
        assert!(arm < request_install);
        assert!(request_install < request_first);
        assert!(request_first < cleanup_install);
        assert!(cleanup_install < cleanup_first);
        let _ = worker.shutdown().await;
    }

    #[tokio::test]
    async fn run_idle_expiry_guard_window() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        // Open a transaction, then move the injected clock past the idle
        // deadline so the next Run takes the idle-expiry rollback branch.
        worker
            .run(Duration::from_secs(5), trusted_begin())
            .await
            .unwrap();
        test_support::set_clock_ms(61_000);
        let job: Job = Box::new(|_c: &mut Connection, _ctx: &RequestContext| {
            Ok(serde_json::json!({"unreachable": true}))
        });
        let expired = worker.run(Duration::from_secs(5), job).await;
        assert!(expired.is_err(), "expired run must fail: {expired:?}");
        assert_arm_window("run", &["idle-expiry"]);
        let _ = worker.shutdown().await;
    }

    #[tokio::test]
    async fn run_client_cancel_cleanup_guard_window() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        // The job cancels its own request token and then fails with a SQLite
        // error, so the post-request client-cancellation cleanup path
        // converts the outcome to the truthful interruption class.
        let job: Job = Box::new(|_c: &mut Connection, ctx: &RequestContext| {
            ctx.token.cancel();
            Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery))
        });
        let cancelled = worker.run(Duration::from_secs(5), job).await;
        assert!(
            cancelled.is_err(),
            "self-cancelled request must report interruption: {cancelled:?}"
        );
        assert_arm_window("run", &["request", "cleanup"]);
        let _ = worker.shutdown().await;
    }

    #[tokio::test]
    async fn run_deadline_cleanup_guard_window() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        // Deadline expires while the job runs and the job returns a SQLite
        // error, entering the truthful deadline cleanup path.
        let deadline = Instant::now() + Duration::from_millis(100);
        let ctx = RequestContext {
            token: CancelToken::new(),
            deadline,
        };
        let (rtx, rrx) = oneshot::channel();
        let job: Job = Box::new(|_c: &mut Connection, _ctx: &RequestContext| {
            std::thread::sleep(Duration::from_millis(200));
            Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery))
        });
        worker.tx.send(Command::Run(job, ctx, rtx)).await.unwrap();
        let outcome = rrx.await.expect("run result");
        assert!(outcome.is_err(), "deadline run must fail: {outcome:?}");
        assert_arm_window("run", &["request", "cleanup"]);
        let _ = worker.shutdown().await;
    }

    #[tokio::test]
    async fn run_shutdown_cleanup_guard_window() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        // Cancel the worker's shutdown token inside the job and return a
        // SQLite error, entering the shutdown cleanup path.
        let cancel = worker.cancel.clone();
        let job: Job = Box::new(move |_c: &mut Connection, _ctx: &RequestContext| {
            cancel.cancel();
            Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery))
        });
        let shutdown_run = worker.run(Duration::from_secs(5), job).await;
        assert!(shutdown_run.is_err(), "shutdown run must fail");
        assert_arm_window("run", &["request", "cleanup"]);
        let _ = worker.shutdown().await;
    }

    #[tokio::test]
    async fn control_and_commit_guard_windows() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        // Control normal.
        let control_job: Job = Box::new(|c: &mut Connection, _ctx: &RequestContext| {
            policy::trusted(|| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)))
                .map_err(WorkerError::Sqlite)?;
            Ok(serde_json::json!({"control": true}))
        });
        worker.run_control(control_job).await.expect("control job");
        assert_arm_window("control", &["normal"]);
        // Commit with no transaction open.
        let no_tx = worker.run_commit().await.expect("commit outcome");
        assert!(no_tx.result.is_err());
        assert_arm_window("commit", &["no-transaction"]);
        // Open a transaction and expire it so Control takes the expiry branch.
        worker
            .run(Duration::from_secs(5), trusted_begin())
            .await
            .unwrap();
        test_support::set_clock_ms(61_000);
        let unreachable: Job = Box::new(|_c: &mut Connection, _ctx: &RequestContext| {
            Ok(serde_json::json!({"unreachable": true}))
        });
        let expired_control = worker.run_control(unreachable).await;
        assert!(expired_control.is_err(), "control must observe expiry");
        assert_arm_window("control", &["normal", "expiry"]);
        // Commit expiry requires another open transaction on a fresh worker;
        // its idle deadline is computed from the already-advanced clock, so
        // advance past it.
        let (_dir2, worker2) = start_test_worker(60);
        worker2
            .run(Duration::from_secs(5), trusted_begin())
            .await
            .unwrap();
        test_support::set_clock_ms(122_000);
        let expired_commit = worker2.run_commit().await.expect("commit outcome");
        assert!(expired_commit.result.is_err());
        assert_arm_window("commit", &["expiry"]);
        let _ = worker.shutdown().await;
        let _ = worker2.shutdown().await;
    }

    #[tokio::test]
    async fn expire_and_shutdown_guard_windows() {
        let _support = SupportGuard::enable().await;
        let (_dir, worker) = start_test_worker(60);
        worker.expire().await.expect("expire result");
        assert_arm_window("expire", &["normal"]);
        worker.shutdown().await.expect("clean shutdown");
        assert_arm_window("shutdown", &["cleanup"]);
        // After shutdown the busy window is gone; the guard cleared exactly
        // once per command already asserted by assert_arm_window.
        let cleared = test_support::retained_records(Event::GuardCleared)
            .into_iter()
            .filter(|record| {
                record
                    .key
                    .as_ref()
                    .and_then(|key| key.worker_id.as_deref())
                    .is_some_and(|label| label.ends_with(":arm"))
            })
            .count();
        let installed = test_support::retained_records(Event::GuardInstalled)
            .into_iter()
            .filter(|record| {
                record
                    .key
                    .as_ref()
                    .and_then(|key| key.worker_id.as_deref())
                    .is_some_and(|label| label.ends_with(":arm"))
            })
            .count();
        assert_eq!(
            cleared, installed,
            "every installed arm guard must be cleared exactly once"
        );
        let _ = Ordering::SeqCst;
    }
}
