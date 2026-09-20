mod admission;
mod config;
mod core;
mod envelope;
mod handles;
mod merge;
mod operation;
mod paths;
mod policy;
mod schema;
mod sql;
pub mod test_support;
mod tools;
mod worker;
pub use config::{Config, ConfigError};
pub use core::{Core, CoreError, QueryResult};
pub use envelope::{Envelope, ErrorBody, ErrorInfo, HandleState};
pub use handles::Handle;
pub use merge::{
    ExtractionResult, FileByteCounts, Files, ImportResult, ImportValidation, MergeError,
};
pub use operation::{
    CleanupFailure, CleanupStage, EffectiveLimits, InterruptionCause, LimitInstallError,
    OperationKind, OperationOutcome, OperationRecord, PublishAction, ShutdownReport,
    ShutdownStatus, TransactionMode, WorkerJob, WorkerShutdownError,
};
pub use sql::Cell;
pub use tools::McpServer;
pub use worker::{CancelToken, RequestHandle};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_defaults_and_unknown_keys() {
        assert_eq!(Config::default().max_handles, 32);
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
    }
    #[test]
    fn typed_values_reject_nonfinite() {
        assert!(Cell::Real("NaN".into()).to_value().is_err());
    }
    #[test]
    fn authorizer_unknown_action_is_denied() {
        assert!(crate::policy::unknown_action_denied());
    }
    #[test]
    fn single_statement_parser_matrix() {
        assert!(crate::policy::validate_sql("SELECT ';';").is_ok());
        assert!(crate::policy::validate_sql("-- only comment").is_err());
        assert!(crate::policy::validate_sql("SELECT 1; SELECT 2").is_err());
        assert!(crate::policy::validate_sql("SELECT '").is_err());
    }
    #[test]
    fn envelope_serializes_version() {
        let e = Envelope {
            envelope_version: 1,
            handle_state: None,
            next_moves: vec!["open_database".into()],
            outcome: serde_json::json!({"result":true}),
        };
        assert_eq!(e.envelope_version, 1);
    }
}
