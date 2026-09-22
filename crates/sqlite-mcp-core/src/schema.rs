use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaInfo {
    pub schema_version: i64,
    pub user_version: i64,
    pub identity: String,
    pub objects: Vec<SchemaObject>,
    pub tables: Vec<SchemaTable>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaTable {
    pub name: String,
    pub strict: bool,
    pub without_rowid: bool,
    pub columns: Vec<SchemaColumn>,
    pub indexes: Vec<SchemaIndex>,
    pub foreign_keys: Vec<SchemaForeignKey>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaColumn {
    pub name: String,
    pub declared_type: Option<String>,
    pub not_null: bool,
    pub default_value: Option<String>,
    pub primary_key_position: i64,
    pub hidden: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaIndex {
    pub name: String,
    pub unique: bool,
    pub origin: String,
    pub columns: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaForeignKey {
    pub id: i64,
    pub sequence: i64,
    pub table: String,
    pub from: String,
    pub to: String,
    pub on_update: String,
    pub on_delete: String,
    pub match_clause: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    pub object_type: String,
    pub name: String,
    pub table_name: Option<String>,
    pub sql: Option<String>,
}
