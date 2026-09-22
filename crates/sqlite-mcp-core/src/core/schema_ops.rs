use super::{Core, CoreError};
use crate::core::error::classify_worker_error;
use crate::schema::{
    SchemaColumn, SchemaForeignKey, SchemaIndex, SchemaInfo, SchemaObject, SchemaTable,
};
use crate::test_support;
use crate::worker::WorkerError;
impl Core {
    pub async fn get_schema(&self, id: &str) -> Result<SchemaInfo, CoreError> {
        self.get_schema_with_ct(id, None).await
    }
    pub async fn get_schema_with_ct(
        &self,
        id: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<SchemaInfo, CoreError> {
        let this = self.clone();
        let owned_id = id.to_owned();
        self.coordinate(async move { this.get_schema_admitted(&owned_id, ct).await })
            .await
    }
    async fn get_schema_admitted(
        &self,
        id: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<SchemaInfo, CoreError> {
        let (gate, w) = self.handle_gate(id).await?;
        let _operation = gate.lock().await;
        let tx_open = self.transaction_open(id).await?;
        let out = w
            .run_with_optional_token(
                std::time::Duration::from_millis(self.config.query_timeout_ms),
                ct,
                |c, _ctx| {
                    test_support::emit(test_support::Event::Prepare);
                    let managed = c.is_autocommit();
                    if managed {
                        crate::policy::trusted(|| c.execute_batch("BEGIN DEFERRED"))?;
                    }
                    let result: Result<serde_json::Value, WorkerError> =
                        crate::policy::trusted(|| {
                            // Establish the SQLite read snapshot before consulting the
                            // schema cookie; PRAGMA schema_version alone is not a
                            // snapshot-establishing table read on all SQLite builds.
                            let _: i64 =
                                c.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
                                    r.get(0)
                                })?;
                            let version: i64 =
                                c.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
                            let user_version: i64 = {
                                let mut statement = c.prepare("PRAGMA user_version")?;
                                if statement.column_count() != 1 {
                                    return Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery));
                                }
                                let mut rows = statement.query([])?;
                                let Some(row) = rows.next()? else {
                                    return Err(WorkerError::Sqlite(
                                        rusqlite::Error::QueryReturnedNoRows,
                                    ));
                                };
                                let value = row.get::<_, i64>(0)?;
                                if rows.next()?.is_some() {
                                    return Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery));
                                }
                                value
                            };
                            test_support::emit(test_support::Event::SchemaVersionRead);
                            let identity = c.path().unwrap_or("").to_string();
                            let mut st = c.prepare(
                                "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY name",
                            )?;
                            let rows = st.query_map([], |r| {
                                Ok(SchemaObject {
                                    object_type: r.get(0)?,
                                    name: r.get(1)?,
                                    table_name: r.get(2)?,
                                    sql: r.get(3)?,
                                })
                            })?;
                            let objects = rows.collect::<Result<Vec<_>, _>>()?;
                            let mut tables = Vec::new();
                            let mut tl = c.prepare("PRAGMA table_list")?;
                            let table_rows = tl.query_map([], |r| {
                                Ok((
                                    r.get::<_, String>(1)?,
                                    r.get::<_, i64>(4)?,
                                    r.get::<_, i64>(5)?,
                                ))
                            })?;
                            for tr in table_rows {
                                let (name, wr, strict) = tr?;
                                let mut columns = Vec::new();
                                let mut x = c.prepare(&format!(
                                    "PRAGMA table_xinfo('{}')",
                                    name.replace("'", "''")
                                ))?;
                                for r in x.query_map([], |r| {
                                    Ok(SchemaColumn {
                                        name: r.get(1)?,
                                        declared_type: r.get(2)?,
                                        not_null: r.get::<_, i64>(3)? != 0,
                                        default_value: r.get(4)?,
                                        primary_key_position: r.get(5)?,
                                        hidden: r.get(6)?,
                                    })
                                })? {
                                    columns.push(r?);
                                }
                                let mut indexes = Vec::new();
                                let mut il = c.prepare(&format!(
                                    "PRAGMA index_list('{}')",
                                    name.replace("'", "''")
                                ))?;
                                for r in il.query_map([], |r| {
                                    Ok((
                                        r.get::<_, String>(1)?,
                                        r.get::<_, i64>(2)?,
                                        r.get::<_, String>(3)?,
                                    ))
                                })? {
                                    let (iname, unique, origin) = r?;
                                    let mut cols = Vec::new();
                                    let mut ix = c.prepare(&format!(
                                        "PRAGMA index_xinfo('{}')",
                                        iname.replace("'", "''")
                                    ))?;
                                    for q in ix.query_map([], |q| q.get::<_, Option<String>>(2))? {
                                        if let Some(v) = q? {
                                            cols.push(v);
                                        }
                                    }
                                    indexes.push(SchemaIndex {
                                        name: iname,
                                        unique: unique != 0,
                                        origin,
                                        columns: cols,
                                    });
                                }
                                let mut foreign_keys = Vec::new();
                                let mut fk = c.prepare(&format!(
                                    "PRAGMA foreign_key_list('{}')",
                                    name.replace("'", "''")
                                ))?;
                                for r in fk.query_map([], |r| {
                                    Ok(SchemaForeignKey {
                                        id: r.get(0)?,
                                        sequence: r.get(1)?,
                                        table: r.get(2)?,
                                        from: r.get(3)?,
                                        to: r.get(4)?,
                                        on_update: r.get(5)?,
                                        on_delete: r.get(6)?,
                                        match_clause: r.get(7)?,
                                    })
                                })? {
                                    foreign_keys.push(r?);
                                }
                                tables.push(SchemaTable {
                                    name,
                                    strict: strict != 0,
                                    without_rowid: wr != 0,
                                    columns,
                                    indexes,
                                    foreign_keys,
                                });
                            }
                            Ok(serde_json::to_value(SchemaInfo {
                                schema_version: version,
                                user_version,
                                identity,
                                objects,
                                tables,
                            })
                            .unwrap())
                        });
                    if managed {
                        match result {
                            Ok(value) => {
                                // Cleanup failure on the success path is just
                                // as uncertain as on the failure path (F-10):
                                // a failed finalization means the observation
                                // must not publish freshness and the handle
                                // is invalidated.
                                let injected = test_support::take_cleanup_fault(
                                    crate::operation::CleanupStage::ManagedSchemaCleanup,
                                )
                                .is_some();
                                let commit = if injected {
                                    Err(WorkerError::Sqlite(rusqlite::Error::InvalidQuery))
                                } else {
                                    crate::policy::trusted(|| {
                                        c.execute_batch("COMMIT").map_err(WorkerError::Sqlite)
                                    })
                                };
                                match commit {
                                    Ok(()) => Ok(value),
                                    Err(_) => {
                                        let _ = crate::policy::trusted(|| {
                                            c.execute_batch("ROLLBACK").map_err(WorkerError::Sqlite)
                                        });
                                        Err(WorkerError::Message(
                                            "managed schema cleanup failed; handle invalidated"
                                                .into(),
                                        ))
                                    }
                                }
                            }
                            Err(error) => {
                                let rollback = crate::policy::trusted(|| {
                                    if test_support::take_cleanup_fault(
                                        crate::operation::CleanupStage::ManagedSchemaCleanup,
                                    )
                                    .is_some()
                                    {
                                        return Err(WorkerError::Sqlite(
                                            rusqlite::Error::InvalidQuery,
                                        ));
                                    }
                                    c.execute_batch("ROLLBACK").map_err(WorkerError::Sqlite)
                                });
                                if rollback.is_err() || !c.is_autocommit() {
                                    return Err(WorkerError::Message(
                                        "managed schema cleanup failed; handle invalidated".into(),
                                    ));
                                }
                                Err(error)
                            }
                        }
                    } else {
                        result
                    }
                },
            )
            .await;
        let out = match out {
            Ok(out) => out,
            Err(e) => {
                let message = e.to_string();
                if message.contains("handle invalidated") {
                    // Managed cleanup could not establish a safe state:
                    // invalidate by removing the handle (async-safe, mirroring
                    // the query invalidation path).
                    let w = {
                        let mut s = self.inner.lock().await;
                        if let Some(identity_key) = s
                            .identities
                            .iter()
                            .find(|(_, v)| v.as_str() == id)
                            .map(|(k, _)| *k)
                        {
                            s.identities.remove(&identity_key);
                        }
                        s.gates.remove(id);
                        s.handles.remove(id).map(|(_, w)| w)
                    };
                    if let Some(w) = w {
                        let _ = w.shutdown().await;
                    }
                }
                return Err(classify_worker_error(e, tx_open));
            }
        };
        let schema: SchemaInfo = serde_json::from_value(out)
            .map_err(|e| CoreError::Worker(WorkerError::Message(e.to_string())))?;
        if serde_json::to_vec(&schema)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
            > self.config.schema_byte_limit
        {
            return Err(CoreError::SchemaTooLarge);
        }
        let mut s = self.inner.lock().await;
        if s.transaction_schema.contains_key(id) {
            // Active-transaction schema reads are coherent result snapshots,
            // not public committed observations. Keep the public handle state
            // unchanged and retain the result as a transaction-local overlay.
            if let Some(tx) = s.transaction_schema.get_mut(id) {
                tx.overlay = Some(schema.clone());
            }
        } else {
            let Some((h, _)) = s.handles.get_mut(id) else {
                return Err(CoreError::UnknownHandle);
            };
            h.schema_observed = true;
            h.schema_version = schema.schema_version;
            h.observation_generation = h.observation_generation.saturating_add(1);
            h.expired = false;
        }
        Ok(schema)
    }
}
