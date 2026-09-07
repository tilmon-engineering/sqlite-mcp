use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_max_handles")]
    pub max_handles: usize,
    #[serde(default = "default_queue")]
    pub queue_capacity: usize,
    #[serde(default = "default_timeout")]
    pub query_timeout_ms: u64,
    #[serde(default = "default_idle")]
    pub writable_idle_seconds: u64,
    #[serde(default = "default_read_idle")]
    pub readonly_idle_seconds: u64,
    #[serde(default = "default_result_row_limit")]
    pub result_row_limit: usize,
    #[serde(default = "default_result_byte_limit")]
    pub result_byte_limit: usize,
    #[serde(default = "default_schema_byte_limit")]
    pub schema_byte_limit: usize,
    #[serde(default = "default_busy_wait")]
    pub busy_wait_ms: u64,
    #[serde(default = "default_sql_limit")]
    pub sql_byte_limit: usize,
    #[serde(default = "default_cell_limit")]
    pub cell_byte_limit: usize,
    #[serde(default = "default_columns")]
    pub column_limit: usize,
    #[serde(default = "default_parameters")]
    pub parameter_limit: usize,
    #[serde(default = "default_expression_depth")]
    pub expression_depth: usize,
    #[serde(default = "default_compound_terms")]
    pub compound_terms: usize,
}
fn default_max_handles() -> usize {
    32
}
fn default_queue() -> usize {
    16
}
fn default_timeout() -> u64 {
    30_000
}
fn default_idle() -> u64 {
    60
}
fn default_busy_wait() -> u64 {
    2_000
}
fn default_result_row_limit() -> usize {
    500
}
fn default_result_byte_limit() -> usize {
    1024 * 1024
}
fn default_schema_byte_limit() -> usize {
    2 * 1024 * 1024
}
fn default_sql_limit() -> usize {
    100 * 1024
}
fn default_cell_limit() -> usize {
    1024 * 1024
}
fn default_columns() -> usize {
    256
}
fn default_parameters() -> usize {
    1000
}
fn default_expression_depth() -> usize {
    100
}
fn default_compound_terms() -> usize {
    50
}
fn default_read_idle() -> u64 {
    600
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_handles: 32,
            queue_capacity: 16,
            query_timeout_ms: 30_000,
            writable_idle_seconds: 60,
            readonly_idle_seconds: 600,
            result_row_limit: default_result_row_limit(),
            result_byte_limit: default_result_byte_limit(),
            schema_byte_limit: default_schema_byte_limit(),
            busy_wait_ms: 2_000,
            sql_byte_limit: 100 * 1024,
            cell_byte_limit: 1024 * 1024,
            column_limit: 256,
            parameter_limit: 1000,
            expression_depth: 100,
            compound_terms: 50,
        }
    }
}
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}
impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        const POLICY: [(&str, u128, u128); 15] = [
            ("max_handles", 1, 1024),
            ("queue_capacity", 1, 4096),
            ("query_timeout_ms", 1, 300_000),
            ("writable_idle_seconds", 1, 86_400),
            ("readonly_idle_seconds", 1, 604_800),
            ("result_row_limit", 1, 100_000),
            ("result_byte_limit", 1, 67_108_864),
            ("schema_byte_limit", 1, 67_108_864),
            ("cell_byte_limit", 1, 67_108_864),
            ("busy_wait_ms", 1, 60_000),
            ("sql_byte_limit", 1, 1_048_576),
            ("column_limit", 1, 2048),
            ("parameter_limit", 1, 32_766),
            ("expression_depth", 1, 1000),
            ("compound_terms", 1, 500),
        ];
        let values = [
            ("max_handles", self.max_handles as u128),
            ("queue_capacity", self.queue_capacity as u128),
            ("query_timeout_ms", self.query_timeout_ms as u128),
            ("writable_idle_seconds", self.writable_idle_seconds as u128),
            ("readonly_idle_seconds", self.readonly_idle_seconds as u128),
            ("result_row_limit", self.result_row_limit as u128),
            ("result_byte_limit", self.result_byte_limit as u128),
            ("schema_byte_limit", self.schema_byte_limit as u128),
            ("cell_byte_limit", self.cell_byte_limit as u128),
            ("busy_wait_ms", self.busy_wait_ms as u128),
            ("sql_byte_limit", self.sql_byte_limit as u128),
            ("column_limit", self.column_limit as u128),
            ("parameter_limit", self.parameter_limit as u128),
            ("expression_depth", self.expression_depth as u128),
            ("compound_terms", self.compound_terms as u128),
        ];
        for ((policy_field, minimum, maximum), (value_field, value)) in POLICY.iter().zip(values) {
            debug_assert_eq!(*policy_field, value_field);
            if !(*minimum..=*maximum).contains(&value) {
                return Err(ConfigError::Invalid(format!(
                    "{value_field} must be {minimum}..={maximum} (got {value})"
                )));
            }
        }
        Ok(())
    }
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let c: Self = toml::from_str(&fs::read_to_string(path)?)?;
        c.validate()?;
        Ok(c)
    }
}
