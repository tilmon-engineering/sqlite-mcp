use base64::Engine;
use rusqlite::types::{Value, ValueRef};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Cell {
    Null,
    Integer(String),
    Real(String),
    Text(String),
    Blob(String),
    TextBytes(String),
}
impl Cell {
    pub fn to_value(&self) -> Result<Value, String> {
        match self {
            Self::Null => Ok(Value::Null),
            Self::Integer(v) => v
                .parse::<i64>()
                .map(Value::Integer)
                .map_err(|_| "invalid integer".into()),
            Self::Real(v) => {
                let x = v.parse::<f64>().map_err(|_| "invalid real".to_string())?;
                if !x.is_finite() {
                    return Err("real must be finite".into());
                }
                Ok(Value::Real(x))
            }
            Self::Text(v) => Ok(Value::Text(v.clone())),
            Self::Blob(v) => base64::engine::general_purpose::STANDARD
                .decode(v)
                .map(Value::Blob)
                .map_err(|_| "invalid base64".into()),
            Self::TextBytes(_) => Err("text_bytes parameters are unsupported".into()),
        }
    }
}
pub fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(x) => Cell::Integer(x.to_string()),
        ValueRef::Real(x) => Cell::Real(if x.is_finite() {
            x.to_string()
        } else if x.is_sign_negative() {
            "-Infinity".into()
        } else {
            "Infinity".into()
        }),
        ValueRef::Text(x) => match std::str::from_utf8(x) {
            Ok(s) => Cell::Text(s.into()),
            Err(_) => Cell::TextBytes(base64::engine::general_purpose::STANDARD.encode(x)),
        },
        ValueRef::Blob(x) => Cell::Blob(base64::engine::general_purpose::STANDARD.encode(x)),
    }
}
pub fn validate_with_limit(
    sql: &str,
    params: &[Cell],
    parameter_limit: usize,
) -> Result<Vec<Value>, String> {
    if sql.as_bytes().contains(&0) {
        return Err("SQL contains NUL byte".into());
    }
    if params.len() > parameter_limit {
        return Err("too many parameters".into());
    }
    params.iter().map(Cell::to_value).collect()
}
