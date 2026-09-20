//! Shared operation seam declarations (Phase 0).
//!
//! Historical typed coordination scaffolding retained for compatibility with
//! the original design seams. The current implementation uses `Core::coordinate`
//! plus each handle's mutex gate as its active coordinator; it does not enqueue
//! [`OperationRecord`] values or consume [`OperationOutcome`] directly.
//!
//! The active lifecycle invariant is still the same: operation-specific core
//! methods publish authoritative worker facts while holding the per-handle gate,
//! and a dropped caller abandons only its reply, not the admitted future.
use std::fmt;

use crate::worker::{Job, WorkerError};

/// The operation a record carries. `Begin` carries the typed transaction mode;
/// exact lowercase parse/render lives on [`TransactionMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Schema,
    Begin(TransactionMode),
    Commit,
    Rollback,
    Expire,
    Close,
}

/// Typed interruption cause. `Shutdown` must surface as INTERNAL with a
/// shutdown message, never as a fake client cancellation or deadline miss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptionCause {
    Client,
    Deadline,
    Shutdown,
}

impl fmt::Display for InterruptionCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InterruptionCause::Client => "client cancellation",
            InterruptionCause::Deadline => "request deadline",
            InterruptionCause::Shutdown => "server shutdown",
        })
    }
}

/// Typed transaction mode with exact lowercase parsing and rendering. No
/// uppercase aliases are accepted; wiring into `begin_transaction` lands with
/// the protocol owner's RED/GREEN cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransactionMode {
    #[default]
    Deferred,
    Immediate,
    Exclusive,
}

impl TransactionMode {
    /// Exact lowercase parse. Returns `None` for anything else, including
    /// uppercase variants.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deferred" => Some(TransactionMode::Deferred),
            "immediate" => Some(TransactionMode::Immediate),
            "exclusive" => Some(TransactionMode::Exclusive),
            _ => None,
        }
    }
    /// Exact lowercase rendering used in responses and schemas.
    pub fn render(self) -> &'static str {
        match self {
            TransactionMode::Deferred => "deferred",
            TransactionMode::Immediate => "immediate",
            TransactionMode::Exclusive => "exclusive",
        }
    }
}

impl fmt::Display for TransactionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.render())
    }
}

/// The executable unit of an admitted operation: identical to the existing
/// worker job shape so compatibility adapters can move between representations
/// without conversion.
pub type WorkerJob = Job;

/// Which cleanup stage failed. Injectable via Rust-only test APIs; never
/// through MCP or configuration input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupStage {
    ExpiryRollback,
    SavepointRestore,
    ManagedSchemaCleanup,
    ConnectionRollback,
    ConnectionClose,
}

impl fmt::Display for CleanupStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CleanupStage::ExpiryRollback => "expiry rollback",
            CleanupStage::SavepointRestore => "savepoint restoration",
            CleanupStage::ManagedSchemaCleanup => "managed schema cleanup",
            CleanupStage::ConnectionRollback => "connection rollback",
            CleanupStage::ConnectionClose => "connection close",
        })
    }
}

/// A cleanup failure retained alongside the original operation result so it is
/// never silently overwritten. The booleans record whatever lifecycle state is
/// *known* after the failure; uncertainty is expressed by `false` plus a
/// non-empty `detail`, never by claiming success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupFailure {
    pub stage: CleanupStage,
    pub known_transaction_open: bool,
    pub known_invalidated: bool,
    pub detail: String,
}

/// Authoritative outcome of one operation. Populated by the worker/coordinator
/// before publication, even when the response receiver was already dropped.
#[derive(Debug)]
pub struct OperationOutcome<T> {
    pub result: Result<T, WorkerError>,
    /// Authoritative post-operation autocommit state as observed inside the
    /// worker (`Connection::is_autocommit`).
    pub transaction_open: bool,
    /// False when the transaction cannot continue (invalidated, expired, or
    /// rolled back by cleanup). Always false when `transaction_open` is false.
    pub transaction_continuable: bool,
    pub expired: bool,
    pub invalidated: bool,
    /// Whether the authorizer/structural policy observed a mutation attempt
    /// during this request, regardless of success or failure.
    pub mutation_attempted: bool,
    /// Retained cleanup failure; never overwrites `result`. A successful
    /// operation carries `None`.
    pub cleanup_error: Option<CleanupFailure>,
}

impl<T> OperationOutcome<T> {
    /// Construct an outcome with authoritative lifecycle booleans. When the
    /// connection is in autocommit (`open == false`), both lifecycle booleans
    /// are forced false; an invalidated outcome is forced non-continuable.
    pub fn new(
        result: Result<T, WorkerError>,
        transaction_open: bool,
        transaction_continuable: bool,
    ) -> Self {
        let open = transaction_open && transaction_continuable;
        OperationOutcome {
            result,
            transaction_open: open,
            transaction_continuable: transaction_continuable && open,
            expired: false,
            invalidated: false,
            mutation_attempted: false,
            cleanup_error: None,
        }
    }

    /// Convenience constructor for the all-false lifecycle shape used by
    /// autocommit (read) operations.
    pub fn autocommit(result: Result<T, WorkerError>) -> Self {
        Self::new(result, false, false)
    }

    pub fn with_expired(mut self, expired: bool) -> Self {
        self.expired = expired;
        if expired {
            self.transaction_continuable = false;
        }
        self
    }

    pub fn with_invalidated(mut self, invalidated: bool) -> Self {
        self.invalidated = invalidated;
        if invalidated {
            self.transaction_continuable = false;
        }
        self
    }

    pub fn with_mutation_attempted(mut self, attempted: bool) -> Self {
        self.mutation_attempted = attempted;
        self
    }

    pub fn with_cleanup_error(mut self, failure: CleanupFailure) -> Self {
        self.cleanup_error = Some(failure);
        self
    }
}

/// Operation-specific publication step, run synchronously while the registry
/// entry is retained. The coordinator first applies the common authoritative
/// lifecycle/expiry/invalidation metadata, then invokes this action once
/// (schema success, begin id/mode, observation marker). It must not mutate an
/// invalidated entry back to live, must not run SQL, and must not await.
pub type PublishAction = Box<
    dyn FnOnce(
            &mut crate::Handle,
            &OperationOutcome<serde_json::Value>,
        ) -> Result<(), crate::CoreError>
        + Send
        + 'static,
>;

/// One admitted operation. The queue storage owns its capacity permit; the
/// processor owns the execution permit on the record's behalf. Close uses the
/// reserved lifecycle slot rather than a user-capacity permit.
pub struct OperationRecord {
    pub sequence: u64,
    /// Acceptance timestamp in test-clock milliseconds, assigned at insertion.
    pub accepted_ms: u64,
    pub kind: OperationKind,
    /// Token and deadline captured at public invocation/poll, before any
    /// capacity wait.
    pub request: crate::worker::RequestContext,
    pub job: WorkerJob,
    pub publish: PublishAction,
    /// Oneshot sender carrying the published outcome to the caller.
    pub reply: tokio::sync::oneshot::Sender<OperationOutcome<serde_json::Value>>,
}

/// Verified worker shutdown result. Success requires both booleans true; any
/// false value must be paired with a [`WorkerShutdownError`] from the reporter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownStatus {
    pub connection_closed: bool,
    pub thread_joined: bool,
}

impl ShutdownStatus {
    pub fn is_success(self) -> bool {
        self.connection_closed && self.thread_joined
    }
}

/// Concrete shutdown/cleanup uncertainty, always accompanied by a [`ShutdownStatus`]
/// that does not claim success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerShutdownError {
    /// Rollback could not be verified (transaction state unknown).
    RollbackUncertain(String),
    /// Connection close could not be confirmed.
    CloseUncertain(String),
    /// The worker thread did not finish within the bounded wait.
    JoinUncertain(String),
}

impl fmt::Display for WorkerShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerShutdownError::RollbackUncertain(detail) => {
                write!(f, "rollback verification failed: {detail}")
            }
            WorkerShutdownError::CloseUncertain(detail) => {
                write!(f, "connection close unconfirmed: {detail}")
            }
            WorkerShutdownError::JoinUncertain(detail) => {
                write!(f, "worker join unconfirmed: {detail}")
            }
        }
    }
}

/// Aggregated core shutdown report. Entries are stable-sorted by handle id and
/// identical for repeated callers of the same completed shutdown. Overall
/// success is derived, never stored, so it cannot contradict the entries.
#[derive(Clone, Debug, Default)]
pub struct ShutdownReport {
    pub entries: Vec<(String, Result<ShutdownStatus, WorkerShutdownError>)>,
    pub registry_cleanup_errors: Vec<String>,
}

impl ShutdownReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one worker result, keeping entries sorted by handle id for
    /// stable, identical reports.
    pub fn record(
        &mut self,
        handle_id: impl Into<String>,
        result: Result<ShutdownStatus, WorkerShutdownError>,
    ) {
        let id = handle_id.into();
        match self
            .entries
            .binary_search_by(|(existing, _)| existing.as_str().cmp(id.as_str()))
        {
            Ok(_) => {} // duplicate ids are not expected; keep first
            Err(pos) => self.entries.insert(pos, (id, result)),
        }
    }

    pub fn is_success(&self) -> bool {
        self.entries
            .iter()
            .all(|(_, r)| matches!(r, Ok(status) if status.is_success()))
            && self.registry_cleanup_errors.is_empty()
    }
}

/// Effective engine limits read back from SQLite after installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveLimits {
    pub cell_byte_limit: i32,
    pub expression_depth: i32,
    pub compound_terms: i32,
    pub parameter_limit: i32,
}

/// Why a configured engine limit could not be installed as requested.
#[derive(Debug)]
pub enum LimitInstallError {
    /// The configured usize does not fit the engine's i32 limit parameter.
    Conversion {
        field: &'static str,
        requested: usize,
    },
    /// SQLite refused the value (e.g. above its compiled hard maximum).
    Unsupported {
        field: &'static str,
        requested: usize,
        effective: Option<i32>,
    },
    /// The read-back verification failed at the SQLite layer.
    Sqlite {
        field: &'static str,
        source: rusqlite::Error,
    },
}

impl fmt::Display for LimitInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitInstallError::Conversion { field, requested } => {
                write!(
                    f,
                    "limit {field}: configured value {requested} does not fit the engine limit type"
                )
            }
            LimitInstallError::Unsupported {
                field,
                requested,
                effective,
            } => match effective {
                Some(effective) => write!(
                    f,
                    "limit {field}: engine refused {requested}; effective value is {effective}"
                ),
                None => write!(f, "limit {field}: engine refused {requested}"),
            },
            LimitInstallError::Sqlite { field, source } => {
                write!(f, "limit {field}: read-back failed: {source}")
            }
        }
    }
}

impl std::error::Error for LimitInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LimitInstallError::Sqlite { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod shared_seams_compile {
    use super::*;

    /// Representative consumers of every shared type must compile and honor
    /// the documented invariants. Behavioral wiring lands with owner GREEN
    /// changes; this test only pins the frozen shapes.
    #[test]
    fn shared_seams_compile() {
        // TransactionMode parse/render: exact lowercase, no aliases.
        assert_eq!(
            TransactionMode::parse("deferred"),
            Some(TransactionMode::Deferred)
        );
        assert_eq!(
            TransactionMode::parse("immediate"),
            Some(TransactionMode::Immediate)
        );
        assert_eq!(
            TransactionMode::parse("exclusive"),
            Some(TransactionMode::Exclusive)
        );
        assert_eq!(TransactionMode::parse("DEFERRED"), None);
        assert_eq!(TransactionMode::Deferred.render(), "deferred");
        assert_eq!(TransactionMode::default(), TransactionMode::Deferred);

        // OperationOutcome invariants: autocommit forces lifecycle false;
        // invalidated implies non-continuable.
        let outcome: OperationOutcome<serde_json::Value> =
            OperationOutcome::new(Ok(serde_json::Value::Null), false, true);
        assert!(!outcome.transaction_open);
        assert!(!outcome.transaction_continuable);
        let outcome = outcome
            .with_invalidated(true)
            .with_mutation_attempted(true)
            .with_expired(true);
        assert!(outcome.invalidated && !outcome.transaction_continuable && outcome.expired);
        assert!(outcome.mutation_attempted);
        let failed: OperationOutcome<serde_json::Value> =
            OperationOutcome::new(Err(WorkerError::Closed), true, true).with_cleanup_error(
                CleanupFailure {
                    stage: CleanupStage::ExpiryRollback,
                    known_transaction_open: true,
                    known_invalidated: false,
                    detail: "injected".to_owned(),
                },
            );
        assert!(failed.cleanup_error.is_some());

        // Shutdown reporting: derived success, stable sorted entries.
        let mut report = ShutdownReport::new();
        report.record(
            "h-b",
            Ok(ShutdownStatus {
                connection_closed: true,
                thread_joined: true,
            }),
        );
        report.record(
            "h-a",
            Err(WorkerShutdownError::RollbackUncertain(
                "uncertain".to_owned(),
            )),
        );
        assert!(!report.is_success());
        assert_eq!(
            report
                .entries
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            vec!["h-a".to_owned(), "h-b".to_owned()]
        );

        // Limit install seam shapes.
        let error = LimitInstallError::Unsupported {
            field: "expression_depth",
            requested: 2000,
            effective: Some(1000),
        };
        assert!(error.to_string().contains("expression_depth"));

        // Interruption cause rendering is typed.
        assert_eq!(InterruptionCause::Shutdown.to_string(), "server shutdown");
    }
}
