use crate::schema::SchemaInfo;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Internal transaction-local schema bookkeeping. This is deliberately kept
/// outside the public `Handle` value so adding freshness machinery does not
/// change the exported Rust struct shape or serialized handle contract.
#[derive(Debug, Clone, Default)]
pub(crate) struct TransactionSchemaState {
    pub(crate) pre_observed: bool,
    pub(crate) pre_version: i64,
    pub(crate) pre_generation: u64,
    pub(crate) expected_version: i64,
    pub(crate) pending_schema_change: bool,
    pub(crate) overlay: Option<SchemaInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handle {
    pub id: String,
    pub path: String,
    pub readonly: bool,
    pub journal_mode: String,
    pub transaction_id: Option<String>,
    pub transaction_mode: Option<String>,
    pub schema_observed: bool,
    pub schema_version: i64,
    pub observation_generation: u64,
    pub expired: bool,
}

impl Handle {
    pub fn new(path: String, readonly: bool, journal_mode: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            path,
            readonly,
            journal_mode,
            transaction_id: None,
            transaction_mode: None,
            schema_observed: false,
            schema_version: -1,
            observation_generation: 0,
            expired: false,
        }
    }
}
