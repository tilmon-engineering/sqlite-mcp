use crate::{paths, worker::WorkerError};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    Path(#[from] paths::PathError),
    #[error("{0}")]
    Worker(#[from] WorkerError),
    #[error("handle not found")]
    UnknownHandle,
    #[error("transaction already open")]
    TransactionOpen,
    #[error("schema must be observed before begin")]
    SchemaRequired,
    #[error("invalid transaction mode; expected deferred or immediate")]
    InvalidTransactionMode,
    #[error("readonly handles only permit deferred transactions")]
    ReadonlyImmediate,
    #[error("database already open")]
    AlreadyOpen,
    #[error("database file is not a valid initialized SQLite database")]
    InvalidDatabase,
    #[error("handle limit reached")]
    HandleLimitReached,
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("schema exceeds configured byte limit")]
    SchemaTooLarge,
    #[error("schema observation is stale")]
    SchemaStale,
    #[error("transaction expired")]
    TransactionExpired,
    #[error(
        "database busy (primary={primary_code}, extended={extended_code}, transaction_open={transaction_open}, continuable={transaction_continuable})"
    )]
    Busy {
        primary_code: i32,
        extended_code: i32,
        transaction_open: bool,
        transaction_continuable: bool,
    },
    #[error(
        "database busy due to stale snapshot (primary={primary_code}, extended={extended_code})"
    )]
    BusySnapshot {
        primary_code: i32,
        extended_code: i32,
        transaction_open: bool,
        transaction_continuable: bool,
    },
    #[error("database locked (primary={primary_code}, extended={extended_code})")]
    Locked {
        primary_code: i32,
        extended_code: i32,
        transaction_open: bool,
        transaction_continuable: bool,
    },
    #[error("request cancelled")]
    Cancelled {
        transaction_open: bool,
        transaction_continuable: bool,
    },
    #[error("query deadline exceeded")]
    DeadlineExceeded {
        transaction_open: bool,
        transaction_continuable: bool,
    },
}

#[allow(dead_code)]
pub(crate) fn classify_worker_error(error: WorkerError, transaction_open: bool) -> CoreError {
    if error.to_string().contains("TRANSACTION_EXPIRED") {
        return CoreError::TransactionExpired;
    }
    if let WorkerError::Sqlite(ref e) = error {
        let extended = e.sqlite_extended_error_code().unwrap_or(-1);
        let primary = if extended >= 0 { extended & 0xff } else { -1 };
        if extended == rusqlite::ffi::SQLITE_BUSY_SNAPSHOT {
            return CoreError::BusySnapshot {
                primary_code: primary,
                extended_code: extended,
                transaction_open,
                transaction_continuable: transaction_open,
            };
        }
        if primary == rusqlite::ffi::SQLITE_BUSY || primary == rusqlite::ffi::SQLITE_LOCKED {
            return CoreError::Busy {
                primary_code: primary,
                extended_code: extended,
                transaction_open,
                transaction_continuable: transaction_open,
            };
        }
        if primary == rusqlite::ffi::SQLITE_LOCKED {
            return CoreError::Locked {
                primary_code: primary,
                extended_code: extended,
                transaction_open,
                transaction_continuable: transaction_open,
            };
        }
    }
    if let WorkerError::Interrupted {
        by_client,
        transaction_open: open,
        transaction_continuable: continuable,
    } = error
    {
        return if by_client {
            CoreError::Cancelled {
                transaction_open: open,
                transaction_continuable: continuable,
            }
        } else {
            CoreError::DeadlineExceeded {
                transaction_open: open,
                transaction_continuable: continuable,
            }
        };
    }
    if error.to_string().contains("deadline") || error.to_string().contains("interrupted") {
        CoreError::Cancelled {
            transaction_open,
            transaction_continuable: transaction_open,
        }
    } else {
        CoreError::Worker(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(code: i32) -> WorkerError {
        WorkerError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some(format!("sqlite code {code}")),
        ))
    }

    #[test]
    fn typed_busy_and_locked_codes_classify_as_busy() {
        for code in [6, 262] {
            let classified = classify_worker_error(sqlite(code), true);
            assert!(
                matches!(
                    classified,
                    CoreError::Busy {
                        primary_code: 6,
                        ..
                    }
                ),
                "SQLite code {code} classified as {classified:?}"
            );
        }
    }

    #[test]
    fn typed_busy_snapshot_remains_distinct() {
        let classified = classify_worker_error(sqlite(rusqlite::ffi::SQLITE_BUSY_SNAPSHOT), true);
        assert!(matches!(classified, CoreError::BusySnapshot { .. }));
    }
}
