use serde::{Deserialize, Serialize};
use uuid::Uuid;
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
