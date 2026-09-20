use crate::handles::TransactionSchemaState;
use crate::test_support;
use crate::worker::RequestHandle;
use crate::{
    admission::{AdmissionRegistry, ReservationKind, SharedAdmission},
    config::Config,
    handles::Handle,
    operation::ShutdownReport,
    paths,
    worker::{Worker, WorkerError},
};
use rusqlite::{Connection, OpenFlags};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;

mod create;
mod error;
mod query;
mod schema_ops;
pub use error::CoreError;
use error::classify_worker_error;
pub use query::QueryResult;
#[derive(Clone)]
pub struct Core {
    inner: Arc<Mutex<State>>,
    pub config: Config,
    /// Shared idempotent completion for global shutdown; repeated and
    /// concurrent callers resolve the identical report.
    shutdown_report: Arc<tokio::sync::OnceCell<ShutdownReport>>,
    admission: SharedAdmission,
    admission_notify: Arc<tokio::sync::Notify>,
}
struct State {
    handles: HashMap<String, (Handle, Worker)>,
    transaction_schema: HashMap<String, TransactionSchemaState>,
    /// (device, inode) of every open database file for duplicate-identity
    /// detection across symlinks and hardlinks.
    identities: HashMap<(u64, u64), String>,
    /// Per-handle operation gates: public operations on one handle
    /// (begin/commit/rollback/close/query/observe) serialize on this so
    /// authoritative state publication is linearized per handle and close
    /// cannot race a concurrently publishing begin.
    gates: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}
impl Core {
    pub fn new(config: Config) -> Result<Self, CoreError> {
        config.validate().map_err(|e| {
            CoreError::Path(paths::PathError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            )))
        })?;
        // Engine-capability probe on a disposable in-memory connection:
        // unsupported configured ceilings are rejected before any protocol
        // bytes, never silently inside a worker (F-04).
        {
            let probe = rusqlite::Connection::open_in_memory()
                .map_err(|e| CoreError::Worker(WorkerError::Sqlite(e)))?;
            let limits = crate::worker::WorkerLimits {
                cell_byte_limit: config.cell_byte_limit,
                sql_byte_limit: config.sql_byte_limit,
                column_limit: config.column_limit,
                expression_depth: config.expression_depth,
                compound_terms: config.compound_terms,
                parameter_limit: config.parameter_limit,
            };
            crate::worker::install_limits(&probe, &limits).map_err(|e| {
                CoreError::Path(paths::PathError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("engine limit probe failed: {e}"),
                )))
            })?;
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(State {
                handles: HashMap::new(),
                transaction_schema: HashMap::new(),
                identities: HashMap::new(),
                gates: HashMap::new(),
            })),
            config,
            shutdown_report: Arc::new(tokio::sync::OnceCell::new()),
            admission: Arc::new(Mutex::new(AdmissionRegistry::default())),
            admission_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Per-handle operation gate plus a cloned worker. The gate linearizes
    /// authoritative state publication for public operations on one handle.
    async fn handle_gate(
        &self,
        id: &str,
    ) -> Result<(Arc<tokio::sync::Mutex<()>>, Worker), CoreError> {
        let s = self.inner.lock().await;
        let (_, w) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
        let g = s.gates.get(id).ok_or(CoreError::UnknownHandle)?;
        Ok((g.clone(), w.clone()))
    }

    /// Admit one operation to the core-owned coordinator wrapper. The wrapper
    /// keeps the admitted future alive after the caller drops its reply; each
    /// operation still serializes through the per-handle gate, and the
    /// operation-specific method performs authoritative publication before
    /// returning its result.
    fn coordinate<T, F>(&self, operation: F) -> impl Future<Output = Result<T, CoreError>> + Send
    where
        T: Send + 'static,
        F: Future<Output = Result<T, CoreError>> + Send + 'static,
    {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = reply.send(operation.await);
        });
        async move {
            match receiver.await {
                Ok(outcome) => outcome,
                Err(_) => Err(CoreError::Worker(WorkerError::Message(
                    "operation coordinator terminated without a reply".into(),
                ))),
            }
        }
    }

    pub async fn open_database(&self, path: &str, readonly: bool) -> Result<Handle, CoreError> {
        self.open_database_with_ct(path, readonly, None).await
    }
    pub async fn open_database_with_ct(
        &self,
        path: &str,
        readonly: bool,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Handle, CoreError> {
        if ct
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(CoreError::Cancelled {
                transaction_open: false,
                transaction_continuable: false,
            });
        }
        let p = paths::existing(path)?;
        // Reject empty files and files that do not carry an initialized
        // SQLite header BEFORE opening: SQLite would happily treat a 0-byte
        // file as a brand-new database, which open must never do.
        let meta = std::fs::metadata(&p)?;
        if meta.len() == 0 {
            return Err(CoreError::InvalidDatabase);
        }
        let mut header = [0u8; 16];
        {
            use std::io::Read;
            let mut f = std::fs::File::open(&p)?;
            f.read_exact(&mut header)?;
        }
        if &header != b"SQLite format 3\x00" {
            return Err(CoreError::InvalidDatabase);
        }
        let flags = if readonly {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let c = Connection::open_with_flags(&p, flags)?;
        // The schema must be queryable through this connection before the
        // handle is registered; otherwise the file is not a usable database.
        crate::policy::trusted(|| {
            let _count: i64 =
                c.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            Ok::<(), rusqlite::Error>(())
        })?;
        // Confirm actual access against SQLite; never silently downgrade.
        // PRAGMA db_readonly yields no row for a writable database and a 1
        // row for read-only, so a missing row means writable.
        use rusqlite::OptionalExtension;
        let db_readonly: Option<i64> = crate::policy::trusted(|| {
            c.query_row("PRAGMA db_readonly", [], |r| r.get(0))
                .optional()
        })?;
        if !readonly && db_readonly.unwrap_or(0) != 0 {
            return Err(CoreError::InvalidDatabase);
        }
        let mode: String =
            crate::policy::trusted(|| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))?;
        use std::os::unix::fs::MetadataExt;
        let identity = (meta.dev(), meta.ino());
        let mut s = self.inner.lock().await;
        if s.handles
            .values()
            .any(|(h, _)| h.path == p.to_string_lossy())
        {
            return Err(CoreError::AlreadyOpen);
        }
        if let Some(existing) = s.identities.get(&identity) {
            let _ = existing;
            return Err(CoreError::AlreadyOpen);
        }
        if s.handles.len() >= self.config.max_handles {
            return Err(CoreError::HandleLimitReached);
        }
        let h = Handle::new(p.to_string_lossy().into_owned(), readonly, mode);
        let idle_seconds = if readonly {
            self.config.readonly_idle_seconds
        } else {
            self.config.writable_idle_seconds
        };
        let w = Worker::start(
            h.path.clone(),
            readonly,
            self.config.queue_capacity,
            idle_seconds,
            self.config.busy_wait_ms,
            crate::worker::WorkerLimits {
                cell_byte_limit: self.config.cell_byte_limit,
                sql_byte_limit: self.config.sql_byte_limit,
                column_limit: self.config.column_limit,
                expression_depth: self.config.expression_depth,
                compound_terms: self.config.compound_terms,
                parameter_limit: self.config.parameter_limit,
            },
        )?;
        s.identities.insert(identity, h.id.clone());
        s.handles.insert(h.id.clone(), (h.clone(), w));
        s.gates.insert(h.id.clone(), Arc::new(Mutex::new(())));
        Ok(h)
    }
    pub async fn close_database(&self, id: &str) -> Result<(), CoreError> {
        let (gate, _w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let w = {
            let mut s = self.inner.lock().await;
            let (h, w) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            if h.transaction_id.is_some() {
                return Err(CoreError::TransactionOpen);
            }
            let w = w.clone();
            if let Some(identity_key) = s
                .identities
                .iter()
                .find(|(_, v)| v.as_str() == id)
                .map(|(k, _)| *k)
            {
                s.identities.remove(&identity_key);
            }
            s.handles.remove(id);
            s.gates.remove(id);
            s.transaction_schema.remove(id);
            w
        };
        let shutdown = w.shutdown().await;
        match shutdown {
            Ok(status) if status.is_success() => Ok(()),
            Ok(status) => Err(CoreError::Worker(WorkerError::Message(format!(
                "close cleanup incomplete (connection_closed={}, thread_joined={})",
                status.connection_closed, status.thread_joined
            )))),
            Err(error) => Err(CoreError::Worker(WorkerError::Message(error.to_string()))),
        }
    }
    pub async fn shutdown(&self) -> ShutdownReport {
        // Idempotent: repeated and concurrent callers resolve the same
        // completed report rather than enqueueing duplicate controls.
        self.shutdown_report
            .get_or_init(|| async {
                let mut report = ShutdownReport::new();
                let _transient = self.admission.lock().await.begin_shutdown();
                loop {
                    if self.admission.lock().await.active_len() == 0 {
                        break;
                    }
                    self.admission_notify.notified().await;
                }
                self.admission.lock().await.complete_shutdown();
                let workers = {
                    let mut s = self.inner.lock().await;
                    let drained = s.handles.drain().collect::<Vec<_>>();
                    s.transaction_schema.clear();
                    s.gates.clear();
                    drained
                        .into_iter()
                        .map(|(id, (_, w))| (id, w))
                        .collect::<Vec<_>>()
                };
                let mut joins = Vec::with_capacity(workers.len());
                for (id, worker) in workers {
                    joins.push(tokio::spawn(async move { (id, worker.shutdown().await) }));
                }
                for join in joins {
                    match join.await {
                        Ok((id, Ok(status))) => report.record(id, Ok(status)),
                        Ok((id, Err(error))) => report.record(id, Err(error)),
                        Err(error) => report
                            .registry_cleanup_errors
                            .push(format!("shutdown join failed: {error}")),
                    }
                }
                report
            })
            .await
            .clone()
    }
    pub async fn expire_handle(&self, id: &str) -> Result<bool, CoreError> {
        let this = self.clone();
        let owned_id = id.to_owned();
        self.coordinate(async move { this.expire_admitted(&owned_id).await })
            .await
    }

    async fn expire_admitted(&self, id: &str) -> Result<bool, CoreError> {
        let (gate, w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let expired = w.expire().await.map_err(CoreError::Worker)?;
        if expired {
            let mut s = self.inner.lock().await;
            if s.handles.contains_key(id) {
                let tx = s.transaction_schema.remove(id);
                if let Some((h, _)) = s.handles.get_mut(id) {
                    if let Some(tx) = tx {
                        h.schema_observed = tx.pre_observed;
                        h.schema_version = tx.pre_version;
                        h.observation_generation = tx.pre_generation;
                    }
                    h.expired = true;
                    h.transaction_id = None;
                    h.transaction_mode = None;
                }
            }
        }
        Ok(expired)
    }
    pub async fn list_handles(&self) -> Vec<Handle> {
        self.inner
            .lock()
            .await
            .handles
            .values()
            .map(|(h, _)| h.clone())
            .collect()
    }

    async fn admit_merge(
        &self,
        ct: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Arc<crate::admission::Reservation>, CoreError> {
        let reservation = self
            .admission
            .lock()
            .await
            .admit(ReservationKind::TransientMerge)
            .map_err(|_| CoreError::ServerShutdown)?;
        if ct.is_some_and(|token| token.is_cancelled()) {
            self.admission.lock().await.finish(reservation.id);
            self.admission_notify.notify_one();
            return Err(CoreError::Cancelled {
                transaction_open: false,
                transaction_continuable: false,
            });
        }
        Ok(reservation)
    }

    async fn finish_merge(&self, reservation: &crate::admission::Reservation) {
        self.admission.lock().await.finish(reservation.id);
        self.admission_notify.notify_one();
    }

    pub async fn extract_sqlite_merge(
        &self,
        base_path: &str,
        ours_path: &str,
        theirs_path: &str,
    ) -> Result<crate::ExtractionResult, CoreError> {
        self.extract_sqlite_merge_with_ct(base_path, ours_path, theirs_path, None)
            .await
    }

    pub async fn extract_sqlite_merge_with_ct(
        &self,
        base_path: &str,
        ours_path: &str,
        theirs_path: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ExtractionResult, CoreError> {
        let reservation = self.admit_merge(ct.as_ref()).await?;
        let this = self.clone();
        let base = base_path.to_owned();
        let ours = ours_path.to_owned();
        let theirs = theirs_path.to_owned();
        self.coordinate(async move {
            if ct
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                this.finish_merge(&reservation).await;
                return Err(CoreError::Cancelled {
                    transaction_open: false,
                    transaction_continuable: false,
                });
            }
            let config = this.config.clone();
            let operation_cancel = reservation.cancel.clone();
            let task = tokio::task::spawn_blocking(move || {
                crate::merge::extract(
                    &base,
                    &ours,
                    &theirs,
                    config.merge_text_byte_limit,
                    config.merge_statement_limit,
                    config.merge_source_observation_byte_limit,
                    Some(operation_cancel),
                )
            });
            let result = match task.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(CoreError::Merge(error)),
                Err(error) => Err(CoreError::Worker(WorkerError::Message(format!(
                    "merge worker join failed: {error}"
                )))),
            };
            this.finish_merge(&reservation).await;
            result
        })
        .await
    }

    pub async fn import_sqlite_text(
        &self,
        sql_path: &str,
        output_path: &str,
    ) -> Result<crate::ImportResult, CoreError> {
        self.import_sqlite_text_with_ct(sql_path, output_path, None)
            .await
    }

    pub async fn import_sqlite_text_with_ct(
        &self,
        sql_path: &str,
        output_path: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ImportResult, CoreError> {
        let reservation = self.admit_merge(ct.as_ref()).await?;
        let this = self.clone();
        let sql = sql_path.to_owned();
        let output = output_path.to_owned();
        self.coordinate(async move {
            if ct
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                this.finish_merge(&reservation).await;
                return Err(CoreError::Cancelled {
                    transaction_open: false,
                    transaction_continuable: false,
                });
            }
            let config = this.config.clone();
            let operation_cancel = reservation.cancel.clone();
            let task = tokio::task::spawn_blocking(move || {
                crate::merge::import(
                    &sql,
                    &output,
                    config.merge_text_byte_limit,
                    config.merge_statement_limit,
                    config.merge_image_byte_limit,
                    Some(operation_cancel),
                )
            });
            let result = match task.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(CoreError::Merge(error)),
                Err(error) => Err(CoreError::Worker(WorkerError::Message(format!(
                    "merge worker join failed: {error}"
                )))),
            };
            this.finish_merge(&reservation).await;
            result
        })
        .await
    }
}

#[allow(dead_code)]
pub struct CoreRequest<T> {
    pub handle: RequestHandle,
    pub completion: Pin<Box<dyn Future<Output = Result<T, CoreError>> + Send>>,
}
#[allow(dead_code)]
impl CoreRequest<serde_json::Value> {
    pub fn cancel(&self) {
        self.handle.cancel();
    }
}
#[allow(dead_code)]
impl Core {
    async fn worker(&self, id: &str) -> Result<Worker, CoreError> {
        self.inner
            .lock()
            .await
            .handles
            .get(id)
            .map(|(_, w)| w.clone())
            .ok_or(CoreError::UnknownHandle)
    }

    async fn invalidate_handle(&self, id: &str) {
        let worker = {
            let mut s = self.inner.lock().await;
            if let Some(identity_key) = s
                .identities
                .iter()
                .find(|(_, value)| value.as_str() == id)
                .map(|(key, _)| *key)
            {
                s.identities.remove(&identity_key);
            }
            s.gates.remove(id);
            s.transaction_schema.remove(id);
            s.handles.remove(id).map(|(_, worker)| worker)
        };
        if let Some(worker) = worker {
            let _ = worker.shutdown().await;
        }
    }

    async fn transaction_open(&self, id: &str) -> Result<bool, CoreError> {
        self.inner
            .lock()
            .await
            .handles
            .get(id)
            .map(|(h, _)| h.transaction_id.is_some())
            .ok_or(CoreError::UnknownHandle)
    }
    pub async fn begin_transaction(&self, id: &str, mode: &str) -> Result<Handle, CoreError> {
        self.begin_transaction_with_ct(id, mode, None).await
    }
    pub async fn begin_transaction_with_ct(
        &self,
        id: &str,
        mode: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Handle, CoreError> {
        // Mode parsing is pre-admission input validation: an invalid mode is
        // refused without touching registry or worker state.
        let parsed_mode = crate::operation::TransactionMode::parse(mode)
            .filter(|parsed| {
                matches!(
                    parsed,
                    crate::operation::TransactionMode::Deferred
                        | crate::operation::TransactionMode::Immediate
                )
            })
            .ok_or(CoreError::InvalidTransactionMode)?;
        let immediate = matches!(parsed_mode, crate::operation::TransactionMode::Immediate);
        let this = self.clone();
        let owned_id = id.to_owned();
        let owned_mode = mode.to_ascii_lowercase();
        self.coordinate(async move {
            this.begin_admitted(&owned_id, owned_mode, immediate, ct)
                .await
        })
        .await
    }
    async fn begin_admitted(
        &self,
        id: &str,
        mode: String,
        immediate: bool,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Handle, CoreError> {
        let (gate, _w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let (readonly, observed, expected_version, expired, transaction_open) = {
            let s = self.inner.lock().await;
            let (h, _) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            (
                h.readonly,
                h.schema_observed,
                h.schema_version,
                h.expired,
                h.transaction_id.is_some(),
            )
        };
        if expired {
            return Err(CoreError::TransactionExpired);
        }
        if transaction_open {
            // Mirror close_database's open-transaction refusal: the worker
            // BEGIN is never reached, so the existing transaction's state
            // stays truthful and the public envelope reports
            // TX_ALREADY_OPEN.
            return Err(CoreError::TransactionOpen);
        }
        if !observed {
            return Err(CoreError::SchemaRequired);
        }
        if immediate && readonly {
            return Err(CoreError::ReadonlyImmediate);
        }
        let w = self.worker(id).await?;
        let sql = if immediate {
            "BEGIN IMMEDIATE"
        } else {
            "BEGIN DEFERRED"
        };
        let begin_result = w
            .run_with_optional_token(
                std::time::Duration::from_millis(self.config.query_timeout_ms),
                ct,
                move |c, _ctx| {
                    crate::policy::trusted(|| c.execute_batch(sql))?;
                    let current: i64 = crate::policy::trusted(|| {
                        c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                    })?;
                    if current != expected_version {
                        let rollback = crate::policy::trusted(|| c.execute_batch("ROLLBACK"));
                        if rollback.is_err() || !c.is_autocommit() {
                            return Err(WorkerError::Message(
                                "begin cleanup failed; handle invalidated".into(),
                            ));
                        }
                        return Err(WorkerError::Message("SCHEMA_STALE".into()));
                    }
                    Ok(serde_json::json!(current))
                },
            )
            .await;
        let live_version = match begin_result {
            Ok(value) => serde_json::from_value::<i64>(value)
                .map_err(|e| CoreError::Worker(WorkerError::Message(e.to_string())))?,
            Err(e) => {
                if e.to_string().contains("SCHEMA_STALE") {
                    return Err(CoreError::SchemaStale);
                }
                if e.to_string().contains("handle invalidated") {
                    self.invalidate_handle(id).await;
                }
                return Err(classify_worker_error(e, false));
            }
        };
        let mut s = self.inner.lock().await;
        let (pre_observed, pre_version, pre_generation) = {
            let Some((h, _)) = s.handles.get(id) else {
                return Err(CoreError::UnknownHandle);
            };
            (
                h.schema_observed,
                h.schema_version,
                h.observation_generation,
            )
        };
        s.transaction_schema.insert(
            id.to_owned(),
            TransactionSchemaState {
                pre_observed,
                pre_version,
                pre_generation,
                expected_version: live_version,
                pending_schema_change: false,
                overlay: None,
            },
        );
        let Some((h, _)) = s.handles.get_mut(id) else {
            return Err(CoreError::UnknownHandle);
        };
        h.transaction_id = Some(uuid::Uuid::new_v4().to_string());
        h.transaction_mode = Some(mode);
        Ok(h.clone())
    }
    pub async fn rollback(&self, id: &str) -> Result<Handle, CoreError> {
        let this = self.clone();
        let owned_id = id.to_owned();
        self.coordinate(async move { this.rollback_admitted(&owned_id).await })
            .await
    }
    async fn rollback_admitted(&self, id: &str) -> Result<Handle, CoreError> {
        let (gate, w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let rollback_result = w
            .run_control(|c, _ctx| {
                test_support::emit(test_support::Event::RollbackCompletion);
                if !c.is_autocommit() {
                    if test_support::take_cleanup_fault(
                        crate::operation::CleanupStage::ConnectionRollback,
                    )
                    .is_some()
                    {
                        return Err(WorkerError::Message(
                            "rollback cleanup failed; handle invalidated".into(),
                        ));
                    }
                    crate::policy::trusted(|| c.execute_batch("ROLLBACK"))?
                };
                if !c.is_autocommit() {
                    return Err(WorkerError::Message(
                        "rollback cleanup failed; handle invalidated".into(),
                    ));
                }
                Ok(serde_json::json!(true))
            })
            .await;
        if let Err(e) = rollback_result {
            let message = e.to_string();
            if message.contains("TRANSACTION_EXPIRED")
                && let Ok(mut s) = self.inner.try_lock()
            {
                let tx = s.transaction_schema.remove(id);
                if let Some((h, _)) = s.handles.get_mut(id) {
                    if let Some(tx) = tx {
                        h.schema_observed = tx.pre_observed;
                        h.schema_version = tx.pre_version;
                        h.observation_generation = tx.pre_generation;
                    }
                    h.expired = true;
                    h.transaction_id = None;
                    h.transaction_mode = None;
                }
            }
            if message.contains("handle invalidated") {
                self.invalidate_handle(id).await;
                return Err(CoreError::Worker(e));
            }
            return Err(classify_worker_error(e, true));
        }
        let mut s = self.inner.lock().await;
        let tx = s.transaction_schema.remove(id);
        let Some((h, _)) = s.handles.get_mut(id) else {
            return Err(CoreError::UnknownHandle);
        };
        if let Some(tx) = tx {
            h.schema_observed = tx.pre_observed;
            h.schema_version = tx.pre_version;
            h.observation_generation = tx.pre_generation;
        }
        h.transaction_id = None;
        h.transaction_mode = None;
        Ok(h.clone())
    }
    pub async fn commit(&self, id: &str) -> Result<Handle, CoreError> {
        let this = self.clone();
        let owned_id = id.to_owned();
        self.coordinate(async move { this.commit_admitted(&owned_id).await })
            .await
    }
    async fn commit_admitted(&self, id: &str) -> Result<Handle, CoreError> {
        let (gate, w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        {
            let s = self.inner.lock().await;
            let (h, _) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            if h.expired {
                return Err(CoreError::TransactionExpired);
            }
        }
        let outcome = w
            .run_commit()
            .await
            .map_err(|error| CoreError::CommitLifecycle {
                error: Box::new(error),
                commit_confirmed: false,
                transaction_open: false,
                transaction_continuable: false,
                expired: false,
                uncertain_or_invalidated: true,
            })?;
        let commit_confirmed = outcome.commit_confirmed;
        let transaction_open = outcome.transaction_open;
        let transaction_continuable = outcome.transaction_continuable;
        let expired = outcome.expired;
        let uncertain_or_invalidated = outcome.uncertain_or_invalidated;
        let (committed_result, committed_schema_version) = match outcome.result {
            Ok(value) => {
                let committed = value
                    .get("committed")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let schema_version = value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_i64);
                (Ok(committed), schema_version)
            }
            Err(error) => (Err(error), None),
        };
        if uncertain_or_invalidated {
            self.invalidate_handle(id).await;
            return Err(CoreError::CommitLifecycle {
                error: Box::new(
                    committed_result
                        .err()
                        .unwrap_or_else(|| WorkerError::Message("commit state uncertain".into())),
                ),
                commit_confirmed,
                transaction_open,
                transaction_continuable,
                expired,
                uncertain_or_invalidated: true,
            });
        }
        if !commit_confirmed {
            if expired {
                let mut s = self.inner.lock().await;
                let tx = s.transaction_schema.remove(id);
                if let Some((h, _)) = s.handles.get_mut(id) {
                    if let Some(tx) = tx {
                        h.schema_observed = tx.pre_observed;
                        h.schema_version = tx.pre_version;
                        h.observation_generation = tx.pre_generation;
                    }
                    h.expired = true;
                    h.transaction_id = None;
                    h.transaction_mode = None;
                }
            } else if !transaction_open {
                let mut s = self.inner.lock().await;
                let tx = s.transaction_schema.remove(id);
                if let Some((h, _)) = s.handles.get_mut(id) {
                    if let Some(tx) = tx {
                        h.schema_observed = tx.pre_observed;
                        h.schema_version = tx.pre_version;
                        h.observation_generation = tx.pre_generation;
                    }
                    h.transaction_id = None;
                    h.transaction_mode = None;
                }
            }
            if expired {
                return Err(CoreError::TransactionExpired);
            }
            return Err(CoreError::CommitLifecycle {
                error: Box::new(
                    committed_result
                        .err()
                        .unwrap_or_else(|| WorkerError::Message("commit was not confirmed".into())),
                ),
                commit_confirmed,
                transaction_open,
                transaction_continuable,
                expired,
                uncertain_or_invalidated,
            });
        }
        let mut s = self.inner.lock().await;
        let tx = s.transaction_schema.remove(id);
        let pending_schema_change = tx.as_ref().is_some_and(|tx| tx.pending_schema_change);
        let Some((h, _)) = s.handles.get_mut(id) else {
            return Err(CoreError::UnknownHandle);
        };
        h.transaction_id = None;
        h.transaction_mode = None;
        if pending_schema_change {
            h.schema_observed = false;
            h.observation_generation = h.observation_generation.saturating_add(1);
        }
        match committed_result {
            Ok(_) => {
                if !pending_schema_change
                    && let Some(version) = committed_schema_version
                    && h.schema_observed
                    && version == h.schema_version
                {
                    // The full committed observation remains valid. No new
                    // generation is published by commit.
                }
                Ok(h.clone())
            }
            Err(error) => Err(CoreError::CommitLifecycle {
                error: Box::new(error),
                commit_confirmed,
                transaction_open,
                transaction_continuable,
                expired,
                uncertain_or_invalidated,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-crate pin: a second begin while a transaction is open fails fast
    /// with `CoreError::TransactionOpen` before the worker BEGIN is ever
    /// dispatched (an external integration test cannot name the private
    /// error path, and the protocol envelope is pinned separately).
    #[tokio::test]
    async fn second_begin_returns_transaction_open() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(Config::default()).unwrap();
        let (path, _) = core
            .create_database(dir.path().join("begin-pin.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let handle = core.open_database(&path, false).await.unwrap();
        core.get_schema(&handle.id).await.unwrap();
        core.begin_transaction(&handle.id, "deferred")
            .await
            .unwrap();
        let second = core.begin_transaction(&handle.id, "deferred").await;
        assert!(
            matches!(second, Err(CoreError::TransactionOpen)),
            "second begin must fail with TransactionOpen, got {second:?}"
        );
        // The original transaction remains usable.
        core.query(&handle.id, "SELECT 1", &[]).await.unwrap();
        core.rollback(&handle.id).await.unwrap();
        core.shutdown().await;
    }
}
