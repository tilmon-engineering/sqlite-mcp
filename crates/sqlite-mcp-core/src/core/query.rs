use super::{Core, CoreError};
use crate::core::error::classify_worker_error;
use crate::sql::{Cell, cell, validate_with_limit};
use crate::test_support;
use crate::worker::WorkerError;

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
struct QueryExecution {
    result: QueryResult,
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
        let this = self.clone();
        let owned_id = id.to_owned();
        let statement = sql.to_owned();
        self.coordinate(async move { this.query_admitted(&owned_id, statement, values, ct).await })
            .await
    }

    async fn query_admitted(
        &self,
        id: &str,
        statement: String,
        values: Vec<rusqlite::types::Value>,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<QueryResult, CoreError> {
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
        let limit = self.config.result_row_limit;
        let result_byte_limit = self.config.result_byte_limit;
        let column_limit = self.config.column_limit;
        let w = self.worker(id).await?;
        // The marker is still drained for every admitted request. It classifies
        // policy activity only; schema freshness is decided by cookie deltas.
        let mutation_signal = w.mutation_seen.clone();
        let out = w
            .run_with_optional_token(
                std::time::Duration::from_millis(self.config.query_timeout_ms),
                ct,
                move |c, _ctx| {
                    if c.is_autocommit() {
                        return Err(WorkerError::Message("transaction required".into()));
                    }
                    let before: i64 = crate::policy::trusted(|| {
                        c.query_row("PRAGMA schema_version", [], |r| r.get(0))
                    })?;
                    if before != expected_version {
                        return Err(WorkerError::Message("SCHEMA_STALE".into()));
                    }
                    // SQLite is the authoritative parser and statement-boundary
                    // detector. This first pass is trusted only so policy
                    // callbacks cannot hide a parser/multiple-statement result;
                    // no bytecode is executed. The real untrusted prepare below
                    // remains authorizer-protected through step and reprepare.
                    match crate::policy::trusted(|| crate::policy::prepare_exact(c, &statement)) {
                        Err(crate::policy::PolicyError::Multiple) => {
                            return Err(WorkerError::Message(
                                "multiple SQL statements are not allowed".into(),
                            ));
                        }
                        Err(crate::policy::PolicyError::Empty) => {
                            return Err(WorkerError::Message("SQL is empty".into()));
                        }
                        Err(error) => {
                            return Err(WorkerError::Message(error.to_string()));
                        }
                        Ok(()) => {}
                    }
                    // Source provenance is command-local and remains installed
                    // through the real prepare, bind, step, and SQLite automatic
                    // reprepare.
                    mutation_signal.classify_source(&statement);
                    crate::policy::check_stored_body(&statement, &mutation_signal).map_err(
                        |e| WorkerError::Message(format!("statement is denied by SQL policy: {e}")),
                    )?;
                    crate::policy::check_maintenance(&statement, &mutation_signal).map_err(
                        |e| WorkerError::Message(format!("statement is denied by SQL policy: {e}")),
                    )?;
                    crate::policy::trusted(|| c.execute_batch("SAVEPOINT agent_stmt"))?;
                    let result = (|| {
                        let mut st = c.prepare(&statement)?;
                        let column_count = st.column_count();
                        if column_count > column_limit {
                            return Err(rusqlite::Error::InvalidParameterName(
                                "column limit exceeded".into(),
                            ));
                        }
                        let columns = (0..column_count)
                            .map(|i| st.column_name(i).unwrap_or("").to_owned())
                            .collect();
                        // Snapshot the connection-wide DML counter before
                        // execution: only statements that actually modified
                        // rows report a change count, so DDL and SELECT (and
                        // zero-row DML) report zero instead of inheriting the
                        // previous statement's count.
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
                            if data.len() < limit {
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
                                    c.execute_batch("ROLLBACK TO agent_stmt; RELEASE agent_stmt")
                                });
                                if restored.is_err() {
                                    return Err(WorkerError::Message(
                                        "savepoint restoration failed; handle invalidated".into(),
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
                            Ok(serde_json::json!({
                                "columns": x.columns,
                                "rows": x.rows,
                                "rows_returned": x.rows_returned,
                                "truncated": x.truncated,
                                "execution_complete": x.execution_complete,
                                "changes": x.changes,
                                "schema_before": before,
                                "schema_after": after,
                                "statement_succeeded": true,
                                "transaction_open": !c.is_autocommit(),
                                "transaction_continuable": !c.is_autocommit(),
                                "cleanup_uncertain": false
                            }))
                        }
                        Err(e) => {
                            // SQLite may invoke the authorizer before a later
                            // parser/type error wins. Only SQLITE_AUTH may
                            // retain callback-derived denial provenance;
                            // malformed and unrelated SQLite failures keep
                            // their native class.
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
                            Err(WorkerError::Sqlite(e))
                        }
                    }
                },
            )
            .await;
        // Always drain both authorizer classifications, including pre-dispatch
        // policy failures and failed statements, so queued requests cannot
        // inherit another request's marker.
        let _attempted = w.mutation_seen.take_after_request();
        let _ddl_attempted = w.mutation_seen.take_ddl_after_request();
        let out = match out {
            Ok(v) => v,
            Err(e) => {
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
        if out == serde_json::json!("SCHEMA_STALE") {
            return Err(CoreError::SchemaStale);
        }
        let value = out.as_object().ok_or_else(|| {
            CoreError::Worker(WorkerError::Message("invalid query outcome".into()))
        })?;
        let execution = QueryExecution {
            result: QueryResult {
                columns: serde_json::from_value(value.get("columns").cloned().unwrap_or_default())
                    .map_err(|e| CoreError::Worker(WorkerError::Message(e.to_string())))?,
                rows: serde_json::from_value(value.get("rows").cloned().unwrap_or_default())
                    .map_err(|e| CoreError::Worker(WorkerError::Message(e.to_string())))?,
                rows_returned: value
                    .get("rows_returned")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                truncated: value
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                execution_complete: value
                    .get("execution_complete")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                changes: value
                    .get("changes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            },
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
        Ok(execution.result)
    }
}
