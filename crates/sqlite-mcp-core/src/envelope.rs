use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub envelope_version: u8,
    pub handle_state: Option<HandleState>,
    pub next_moves: Vec<String>,
    #[serde(flatten)]
    pub outcome: T,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandleState {
    pub handle: String,
    pub path: String,
    pub readonly: bool,
    pub journal_mode: String,
    pub transaction_id: Option<String>,
    pub transaction_mode: Option<String>,
    pub schema_observed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorInfo,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub class: String,
    pub message: String,
    pub transaction_open: bool,
    pub transaction_continuable: bool,
}
