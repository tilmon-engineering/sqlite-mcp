use super::{Core, CoreError};
use crate::core::error::classify_worker_error;
use crate::policy::{PolicyError, TailCursor};
use crate::sql::{Cell, cell, validate_with_limit};
use crate::test_support;
use crate::worker::WorkerError;
use std::io::Read;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub rows_returned: usize,
    pub truncated: bool,
    pub execution_complete: bool,
    pub changes: u64,
}

#[derive(Debug)]
struct BatchExecution {
    results: Vec<QueryResult>,
    schema_before: i64,
    schema_after: i64,
    statement_succeeded: bool,
    transaction_open: bool,
    transaction_continuable: bool,
    cleanup_uncertain: bool,
}

fn serialized_selected_payload(result: &QueryResult) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&serde_json::json!({ "columns": &result.columns, "rows": &result.rows }))
}

fn policy_worker_error(error: PolicyError) -> WorkerError {
    WorkerError::Message(error.to_string())
}

impl Core {
    pub async fn query(
        &self,
        id: &str,
        sql: &str,
        params: &[Cell],
    ) -> Result<QueryResult, CoreError> {
        self.query_with_ct(id, sql, params, None).await
    }

    pub async fn query_with_ct(
        &self,
        id: &str,
        sql: &str,
        params: &[Cell],
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<QueryResult, CoreError> {
        if sql.len() > self.config.sql_byte_limit {
            return Err(CoreError::Worker(WorkerError::Message(
                "SQL exceeds configured byte limit".into(),
            )));
        }
        if params.len() > self.config.parameter_limit {
            return Err(CoreError::Worker(WorkerError::Message(
                "too many parameters".into(),
            )));
        }
        let values = validate_with_limit(sql, params, self.config.parameter_limit)
            .map_err(|e| CoreError::Worker(WorkerError::Message(e)))?;
        let results = self
            .execute_batch_with_ct(id, sql.to_owned(), values, true, ct)
            .await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| CoreError::Worker(WorkerError::Message("SQL is empty".into())))
    }

    pub async fn query_batch(&self, id: &str, sql: &str) -> Result<Vec<QueryResult>, CoreError> {
        self.query_batch_with_ct(id, sql, None).await
    }

    pub async fn query_batch_with_ct(
        &self,
        id: &str,
        sql: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<QueryResult>, CoreError> {
        self.validate_batch_sql(sql)?;
        self.execute_batch_with_ct(id, sql.to_owned(), Vec::new(), false, ct)
            .await
    }

    pub async fn execute_sql_file(
        &self,
        id: &str,
        sql_path: &str,
    ) -> Result<(String, Vec<QueryResult>), CoreError> {
        self.execute_sql_file_with_ct(id, sql_path, None).await
    }

    pub async fn execute_sql_file_with_ct(
        &self,
        id: &str,
        sql_path: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(String, Vec<QueryResult>), CoreError> {
        let path = crate::paths::existing(sql_path)?;
        let normalized = path.to_str().ok_or_else(|| {
            CoreError::Worker(WorkerError::Message("SQL path is not valid UTF-8".into()))
        })?;
        let mut file = std::fs::File::open(&path)?;
        let read_limit = self.config.batch_sql_byte_limit.saturating_add(1);
        let mut bytes = Vec::with_capacity(read_limit.min(8192));
        file.by_ref()
            .take(read_limit as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > self.config.batch_sql_byte_limit {
            return Err(CoreError::Worker(WorkerError::Message(
                "batch SQL exceeds configured byte limit".into(),
            )));
        }
        let sql = String::from_utf8(bytes).map_err(|_| {
            CoreError::Worker(WorkerError::Message(
                "batch SQL file is not valid UTF-8".into(),
            ))
        })?;
        self.validate_batch_sql(&sql)?;
        let results = self
            .execute_batch_with_ct(id, sql, Vec::new(), false, ct)
            .await?;
        Ok((normalized.to_owned(), results))
    }

    fn validate_batch_sql(&self, sql: &str) -> Result<(), CoreError> {
        if sql.len() > self.config.batch_sql_byte_limit {
            return Err(CoreError::Worker(WorkerError::Message(
                "batch SQL exceeds configured byte limit".into(),
            )));
        }
        if sql.as_bytes().contains(&0) {
            return Err(CoreError::Worker(WorkerError::Message(
                "batch SQL contains NUL".into(),
            )));
        }
        Ok(())
    }

    async fn execute_batch_with_ct(
        &self,
        id: &str,
        sql: String,
        values: Vec<rusqlite::types::Value>,
        single_statement: bool,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<QueryResult>, CoreError> {
        let this = self.clone();
        let owned_id = id.to_owned();
        self.coordinate(async move {
            this.execute_batch_admitted(&owned_id, sql, values, single_statement, ct)
                .await
        })
        .await
    }

    async fn execute_batch_admitted(
        &self,
        id: &str,
        sql: String,
        values: Vec<rusqlite::types::Value>,
        single_statement: bool,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<QueryResult>, CoreError> {
        let (gate, _w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let (expected_version, observed, expired) = {
            let s = self.inner.lock().await;
            let (h, _) = s.handles.get(id).ok_or(CoreError::UnknownHandle)?;
            let expected = s
                .transaction_schema
                .get(id)
                .map(|tx| tx.expected_version)
                .unwrap_or(h.schema_version);
            (expected, h.schema_observed, h.expired)
        };
        if !observed {
            return Err(CoreError::SchemaRequired);
        }
        if expired {
            return Err(CoreError::TransactionExpired);
        }
        let row_limit = self.config.result_row_limit;
        let result_byte_limit = self.config.result_byte_limit;
        let column_limit = self.config.column_limit;
        let statement_limit = self.config.batch_statement_limit;
        let w = self.worker(id).await?;
        let mutation_signal = w.mutation_seen.clone();
        let out = w
            .run_with_optional_token(
                std::time::Duration::from_millis(self.config.query_timeout_ms),
                ct,
                move |c, _ctx| {
                    if c.is_autocommit() {
                        return Err(WorkerError::Message("transaction required".into()));
                    }
                    let schema_before: i64 = crate::policy::trusted(|| {
                        c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                    })?;
                    if schema_before != expected_version {
                        return Err(WorkerError::Message("SCHEMA_STALE".into()));
                    }
                    let mut expected = schema_before;
                    if single_statement {
                        let mut validation = TailCursor::new(&sql).map_err(policy_worker_error)?;
                        let first = crate::policy::trusted(|| validation.next(c))
                            .map_err(policy_worker_error)?;
                        if first.is_none() {
                            return Err(WorkerError::Message("SQL is empty".into()));
                        }
                        match crate::policy::trusted(|| validation.next(c)) {
                            Ok(Some(_)) | Err(_) => {
                                return Err(WorkerError::Message(
                                    "multiple SQL statements are not allowed".into(),
                                ));
                            }
                            Ok(None) => {}
                        }
                    }
                    let mut cursor = TailCursor::new(&sql).map_err(policy_worker_error)?;
                    let mut results = Vec::new();
                    let mut statement_count = 0usize;

                    loop {
                        let range = crate::policy::trusted(|| cursor.next(c))
                            .map_err(policy_worker_error)?;
                        let Some(range) = range else { break };
                        statement_count = statement_count.saturating_add(1);
                        if single_statement && statement_count > 1 {
                            return Err(WorkerError::Message(
                                "multiple SQL statements are not allowed".into(),
                            ));
                        }
                        if !single_statement && statement_count > statement_limit {
                            return Err(WorkerError::Message(format!(
                                "batch statement limit exceeded (limit {statement_limit})"
                            )));
                        }
                        let current = cursor.sql(&range).to_owned();
                        let before: i64 = crate::policy::trusted(|| {
                            c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                        })?;
                        if before != expected {
                            return Err(WorkerError::Message("SCHEMA_STALE".into()));
                        }
                        mutation_signal.classify_source(&current);
                        crate::policy::check_stored_body(&current, &mutation_signal).map_err(
                            |e| {
                                WorkerError::Message(format!(
                                    "statement is denied by SQL policy: {e}"
                                ))
                            },
                        )?;
                        crate::policy::check_maintenance(&current, &mutation_signal).map_err(
                            |e| {
                                WorkerError::Message(format!(
                                    "statement is denied by SQL policy: {e}"
                                ))
                            },
                        )?;
                        crate::policy::trusted(|| c.execute_batch("SAVEPOINT agent_stmt"))?;
                        let result = (|| {
                            test_support::emit(test_support::Event::Prepare);
                            let mut st = c.prepare(&current)?;
                            let column_count = st.column_count();
                            if column_count > column_limit {
                                return Err(rusqlite::Error::InvalidParameterName(
                                    "column limit exceeded".into(),
                                ));
                            }
                            let columns = (0..column_count)
                                .map(|i| st.column_name(i).unwrap_or("").to_owned())
                                .collect();
                            let total_changes_before =
                                unsafe { rusqlite::ffi::sqlite3_total_changes(c.handle()) };
                            let mut rows = st.query(rusqlite::params_from_iter(values.iter()))?;
                            let mut data = Vec::new();
                            let mut truncated = false;
                            while let Some(r) = rows.next()? {
                                test_support::emit(test_support::Event::StepProgress);
                                let mut row = Vec::new();
                                for i in 0..r.as_ref().column_count() {
                                    row.push(cell(r.get_ref(i)?));
                                }
                                if data.len() < row_limit {
                                    data.push(row);
                                } else {
                                    truncated = true;
                                }
                            }
                            drop(rows);
                            let total_changes_after =
                                unsafe { rusqlite::ffi::sqlite3_total_changes(c.handle()) };
                            let changes = if total_changes_after > total_changes_before {
                                c.changes()
                            } else {
                                0
                            };
                            Ok(QueryResult {
                                columns,
                                rows_returned: data.len(),
                                rows: data,
                                truncated,
                                execution_complete: true,
                                changes,
                            })
                        })();
                        match result {
                            Ok(x) => {
                                let payload = serialized_selected_payload(&x)
                                    .map_err(|e| WorkerError::Message(e.to_string()))?;
                                if payload.len() > result_byte_limit {
                                    let restored = crate::policy::trusted(|| {
                                        if crate::test_support::take_cleanup_fault(
                                            crate::operation::CleanupStage::SavepointRestore,
                                        )
                                        .is_some()
                                        {
                                            return Err(rusqlite::Error::InvalidQuery);
                                        }
                                        c.execute_batch(
                                            "ROLLBACK TO agent_stmt; RELEASE agent_stmt",
                                        )
                                    });
                                    if restored.is_err() {
                                        return Err(WorkerError::Message(
                                            "savepoint restoration failed; handle invalidated"
                                                .into(),
                                        ));
                                    }
                                    return Err(WorkerError::ResultTooLarge {
                                        retained_bytes: payload.len(),
                                        limit: result_byte_limit,
                                    });
                                }
                                crate::policy::trusted(|| c.execute_batch("RELEASE agent_stmt"))?;
                                let after: i64 = crate::policy::trusted(|| {
                                    c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                                })?;
                                expected = after;
                                results.push(x);
                            }
                            Err(e) => {
                                let authorization_failure = e
                                    .sqlite_extended_error_code()
                                    .is_some_and(|code| code & 0xff == rusqlite::ffi::SQLITE_AUTH);
                                if !authorization_failure {
                                    mutation_signal.clear_policy_denied();
                                }
                                if c.is_autocommit() {
                                    return Err(WorkerError::Message(format!(
                                        "outer transaction aborted: {e}"
                                    )));
                                }
                                if crate::policy::trusted(|| {
                                    c.execute_batch("ROLLBACK TO agent_stmt; RELEASE agent_stmt")
                                })
                                .is_err()
                                {
                                    return Err(WorkerError::Message(
                                        "savepoint restoration failed; handle invalidated".into(),
                                    ));
                                }
                                return Err(WorkerError::Sqlite(e));
                            }
                        }
                    }
                    if results.is_empty() {
                        return Err(WorkerError::Message("SQL is empty".into()));
                    }
                    Ok(serde_json::json!({
                        "results": results,
                        "schema_before": schema_before,
                        "schema_after": expected,
                        "statement_succeeded": true,
                        "transaction_open": !c.is_autocommit(),
                        "transaction_continuable": !c.is_autocommit(),
                        "cleanup_uncertain": false
                    }))
                },
            )
            .await;
        let _attempted = w.mutation_seen.take_after_request();
        let ddl_attempted = w.mutation_seen.take_ddl_after_request();
        let out = match out {
            Ok(v) => v,
            Err(e) => {
                // A batch may have released one or more successful DDL
                // savepoints before a later statement failed. The worker keeps
                // the outer transaction open, but the closure-local schema
                // cookie is otherwise lost with the error. Re-read it before
                // classifying the original failure so the next statement can
                // use the same transaction-local schema snapshot.
                let message = e.to_string();
                let recover_schema = ddl_attempted
                    && !matches!(e, WorkerError::Interrupted { .. })
                    && !message.contains("TRANSACTION_EXPIRED")
                    && !message.contains("handle invalidated")
                    && !message.contains("outer transaction aborted");
                if recover_schema {
                    if let Ok(probe) = w
                        .run_with_optional_token(
                            std::time::Duration::from_millis(self.config.query_timeout_ms),
                            None,
                            |c, _ctx| {
                                if c.is_autocommit() {
                                    return Ok(serde_json::Value::Null);
                                }
                                let version: i64 = crate::policy::trusted(|| {
                                    c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                                })?;
                                Ok(serde_json::json!(version))
                            },
                        )
                        .await
                        && let Some(version) = probe.as_i64()
                        && version != expected_version
                    {
                        let mut s = self.inner.lock().await;
                        if let Some(tx) = s.transaction_schema.get_mut(id) {
                            tx.expected_version = version;
                            tx.pending_schema_change = true;
                        }
                    }
                    let _ = w.mutation_seen.take_after_request();
                    let _ = w.mutation_seen.take_ddl_after_request();
                }
                if let WorkerError::Interrupted {
                    transaction_open: false,
                    ..
                } = e
                {
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
                let message = e.to_string();
                if message.contains("TRANSACTION_EXPIRED") {
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
                    return Err(CoreError::TransactionExpired);
                }
                if message.contains("handle invalidated") {
                    self.invalidate_handle(id).await;
                }
                if message.contains("SCHEMA_STALE") {
                    return Err(CoreError::SchemaStale);
                }
                return Err(classify_worker_error(e, true));
            }
        };
        let value = out.as_object().ok_or_else(|| {
            CoreError::Worker(WorkerError::Message("invalid batch outcome".into()))
        })?;
        let execution = BatchExecution {
            results: serde_json::from_value(value.get("results").cloned().unwrap_or_default())
                .map_err(|e| CoreError::Worker(WorkerError::Message(e.to_string())))?,
            schema_before: value
                .get("schema_before")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(expected_version),
            schema_after: value
                .get("schema_after")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(expected_version),
            statement_succeeded: value
                .get("statement_succeeded")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            transaction_open: value
                .get("transaction_open")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            transaction_continuable: value
                .get("transaction_continuable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            cleanup_uncertain: value
                .get("cleanup_uncertain")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        };
        if execution.cleanup_uncertain {
            return Err(CoreError::Worker(WorkerError::Message(
                "query cleanup state uncertain; handle invalidated".into(),
            )));
        }
        if !execution.transaction_open || !execution.transaction_continuable {
            return Err(CoreError::Worker(WorkerError::Message(
                "outer transaction aborted".into(),
            )));
        }
        if execution.statement_succeeded && execution.schema_after != execution.schema_before {
            let mut s = self.inner.lock().await;
            let Some(tx) = s.transaction_schema.get_mut(id) else {
                return Err(CoreError::Worker(WorkerError::Message(
                    "transaction schema state missing after query".into(),
                )));
            };
            tx.expected_version = execution.schema_after;
            tx.pending_schema_change = true;
        }
        Ok(execution.results)
    }
}
