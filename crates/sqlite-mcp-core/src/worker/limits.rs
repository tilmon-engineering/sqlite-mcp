use crate::{EffectiveLimits, LimitInstallError};
use rusqlite::{Connection, limits::Limit};

#[derive(Clone, Copy)]
pub struct WorkerLimits {
    pub cell_byte_limit: usize,
    pub sql_byte_limit: usize,
    pub column_limit: usize,
    pub expression_depth: usize,
    pub compound_terms: usize,
    pub parameter_limit: usize,
}

fn install_one(
    conn: &Connection,
    field: &'static str,
    limit: Limit,
    requested: usize,
) -> Result<i32, LimitInstallError> {
    let value =
        i32::try_from(requested).map_err(|_| LimitInstallError::Conversion { field, requested })?;
    conn.set_limit(limit, value)
        .map_err(|source| LimitInstallError::Sqlite { field, source })?;
    let effective = conn
        .limit(limit)
        .map_err(|source| LimitInstallError::Sqlite { field, source })?;
    if effective != value {
        return Err(LimitInstallError::Unsupported {
            field,
            requested,
            effective: Some(effective),
        });
    }
    Ok(effective)
}

/// Install the four worker engine limits and verify SQLite's effective values.
/// SQL and column caps remain Rust-side application policy and are not installed here.
pub fn install_limits(
    conn: &Connection,
    limits: &WorkerLimits,
) -> Result<EffectiveLimits, LimitInstallError> {
    Ok(EffectiveLimits {
        cell_byte_limit: install_one(
            conn,
            "cell_byte_limit",
            Limit::SQLITE_LIMIT_LENGTH,
            limits.cell_byte_limit,
        )?,
        expression_depth: install_one(
            conn,
            "expression_depth",
            Limit::SQLITE_LIMIT_EXPR_DEPTH,
            limits.expression_depth,
        )?,
        compound_terms: install_one(
            conn,
            "compound_terms",
            Limit::SQLITE_LIMIT_COMPOUND_SELECT,
            limits.compound_terms,
        )?,
        parameter_limit: install_one(
            conn,
            "parameter_limit",
            Limit::SQLITE_LIMIT_VARIABLE_NUMBER,
            limits.parameter_limit,
        )?,
    })
}
