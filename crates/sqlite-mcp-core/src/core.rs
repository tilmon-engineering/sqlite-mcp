use crate::test_support;
use crate::worker::RequestHandle;
use crate::{
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
}
struct State {
    handles: HashMap<String, (Handle, Worker)>,
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
                identities: HashMap::new(),
                gates: HashMap::new(),
            })),
            config,
            shutdown_report: Arc::new(tokio::sync::OnceCell::new()),
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

    /// Admit one operation to a core-owned coordinator: the coordinator task
    /// owns execution and authoritative publication, so a dropped caller
    /// abandons only its reply — never the publication. Worker-side effects
    /// (mutation-marker drains, transaction publication/clearing, observation
    /// freshness, expiry tombstones) are applied exactly once per admitted
    /// operation even when the reply receiver vanishes.
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
                let workers = {
                    let mut s = self.inner.lock().await;
                    let drained = s.handles.drain().collect::<Vec<_>>();
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
        let (gate, w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let expired = w.expire().await?;
        if expired {
            let mut s = self.inner.lock().await;
            if let Some((h, _)) = s.handles.get_mut(id) {
                h.expired = true;
                h.transaction_id = None;
                h.transaction_mode = None;
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
        let (readonly, observed, expected_version, expired) = {
            let s = self.inner.lock().await;
            let (h, _) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            (h.readonly, h.schema_observed, h.schema_version, h.expired)
        };
        if expired {
            return Err(CoreError::TransactionExpired);
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
        w.run_with_optional_token(
            std::time::Duration::from_millis(self.config.query_timeout_ms),
            ct,
            move |c, _ctx| {
                crate::policy::trusted(|| c.execute_batch(sql))?;
                let current: i64 = crate::policy::trusted(|| {
                    c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                })?;
                if current != expected_version {
                    let _ = crate::policy::trusted(|| c.execute_batch("ROLLBACK"));
                    return Err(WorkerError::Message("SCHEMA_STALE".into()));
                }
                Ok(serde_json::json!(true))
            },
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("SCHEMA_STALE") {
                CoreError::SchemaStale
            } else {
                classify_worker_error(e, false)
            }
        })?;
        let mut s = self.inner.lock().await;
        let Some((h, _)) = s.handles.get_mut(id) else {
            // The registry entry vanished (global shutdown or invalidation
            // raced this publication): there is nothing left to publish onto.
            // Report unknown rather than panicking.
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
        w.run_control(|c, _ctx| {
            test_support::emit(test_support::Event::RollbackCompletion);
            if !c.is_autocommit() {
                crate::policy::trusted(|| c.execute_batch("ROLLBACK"))?
            };
            Ok(serde_json::json!(true))
        })
        .await
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("TRANSACTION_EXPIRED")
                && let Ok(mut s) = self.inner.try_lock()
                && let Some((h, _)) = s.handles.get_mut(id)
            {
                // Worker-internal idle expiry: publish the tombstone.
                h.expired = true;
                h.transaction_id = None;
                h.transaction_mode = None;
            }
            classify_worker_error(e, true)
        })?;
        let mut s = self.inner.lock().await;
        let Some((h, _)) = s.handles.get_mut(id) else {
            // The registry entry vanished (global shutdown or invalidation
            // raced this publication): there is nothing left to publish onto.
            // Report unknown rather than panicking.
            return Err(CoreError::UnknownHandle);
        };
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
        let (gate, _w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        {
            // Expired tombstone precedes the transaction check: follow-ups on
            // an expired handle report TX_EXPIRED, never NO_TX_OPEN (AC.4).
            let s = self.inner.lock().await;
            let (h, _) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            if h.expired {
                return Err(CoreError::TransactionExpired);
            }
        }
        let w = self.worker(id).await?;
        w.run_control(|c, _ctx| {
            test_support::emit(test_support::Event::CommitEntry);
            if c.is_autocommit() {
                return Err(WorkerError::Message("no transaction open".into()));
            }
            crate::policy::trusted(|| c.execute_batch("COMMIT"))?;
            test_support::emit(test_support::Event::CommitReturn);
            Ok(serde_json::json!(true))
        })
        .await
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("TRANSACTION_EXPIRED")
                && let Ok(mut s) = self.inner.try_lock()
                && let Some((h, _)) = s.handles.get_mut(id)
            {
                // Worker-internal idle expiry: publish the tombstone.
                h.expired = true;
                h.transaction_id = None;
                h.transaction_mode = None;
            }
            classify_worker_error(e, true)
        })?;
        let mut s = self.inner.lock().await;
        let Some((h, _)) = s.handles.get_mut(id) else {
            // The registry entry vanished (global shutdown or invalidation
            // raced this publication): there is nothing left to publish onto.
            // Report unknown rather than panicking.
            return Err(CoreError::UnknownHandle);
        };
        h.transaction_id = None;
        h.transaction_mode = None;
        Ok(h.clone())
    }
}
