//! Git-independent SQLite merge snapshots and safe SQL-text import.
//!
//! This module deliberately does not use the SQLite command line client.  SQL
//! text is a protocol format: the importer treats it as untrusted input and
//! owns the destination transaction and output-file lifecycle.

use base64::Engine;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

pub const FORMAT_HEADER: &str = "-- sqlite-mcp merge-format: 1\n";
pub const MANIFEST_BYTE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Files {
    pub base_sql: Option<String>,
    pub ours_sql: Option<String>,
    pub theirs_sql: Option<String>,
    pub resolved_sql: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileByteCounts {
    pub base_sql: u64,
    pub ours_sql: u64,
    pub theirs_sql: u64,
    pub resolved_sql: u64,
    pub manifest: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarObservation {
    pub kind: String,
    pub path: String,
    pub present: bool,
    pub device_inode: Option<[u64; 2]>,
    pub size_bytes: Option<u64>,
    pub mtime_ns: Option<u128>,
    pub sha256_hex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceObservation {
    pub label: String,
    pub phase: String,
    pub main: SidecarObservation,
    pub rollback_journal: SidecarObservation,
    pub wal: SidecarObservation,
    pub shm: SidecarObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub workspace_path: String,
    pub files: Files,
    pub manifest_path: String,
    pub source_observations: Vec<SourceObservation>,
    pub file_byte_counts: FileByteCounts,
    pub format_version: u8,
    pub workspace_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportValidation {
    pub journal_mode: String,
    pub schema_summary: SchemaSummary,
    pub foreign_key_check: ForeignKeyCheck,
    pub integrity_check: IntegrityCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSummary {
    pub object_count: u64,
    pub table_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyCheck {
    pub ok: bool,
    pub violations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityCheck {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub output_path: String,
    pub output_state: String,
    pub statement_count: u64,
    pub byte_count: u64,
    pub validation: ImportValidation,
}

#[derive(Debug, Clone)]
pub struct MergeError {
    pub class: &'static str,
    pub message: String,
    pub details: Value,
}

impl MergeError {
    #[allow(clippy::too_many_arguments)]
    fn extraction(
        class: &'static str,
        message: impl Into<String>,
        workspace: Option<&Path>,
        files: Files,
        manifest: Option<&Path>,
        observations: Vec<SourceObservation>,
        counts: FileByteCounts,
        state: &str,
        cleanup_errors: Vec<String>,
        root_class: Option<&str>,
    ) -> Self {
        Self {
            class,
            message: message.into(),
            details: json!({
                "workspace_path": workspace.and_then(path_string),
                "files": files,
                "manifest_path": manifest.and_then(path_string),
                "source_observations": observations,
                "file_byte_counts": counts,
                "format_version": if state == "NotCreated" { Value::Null } else { Value::from(1) },
                "workspace_state": state,
                "cleanup_errors": cleanup_errors,
                "root_class": root_class,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn import(
        class: &'static str,
        message: impl Into<String>,
        output: Option<&Path>,
        state: &str,
        statements: u64,
        bytes: u64,
        validation: Option<ImportValidation>,
        cleanup_errors: Vec<String>,
        root_class: Option<&str>,
    ) -> Self {
        Self {
            class,
            message: message.into(),
            details: json!({
                "output_path": output.and_then(path_string),
                "output_state": state,
                "statement_count": statements,
                "byte_count": bytes,
                "validation": validation,
                "cleanup_errors": cleanup_errors,
                "root_class": root_class,
            }),
        }
    }
}

fn path_string(path: &Path) -> Option<String> {
    path.to_str().map(ToOwned::to_owned)
}

fn quoted_identifier(name: &str) -> Result<String, MergeError> {
    if name.as_bytes().contains(&0) {
        return Err(MergeError {
            class: "MERGE_UNREPRESENTABLE_CONTENT",
            message: format!("identifier contains NUL: {name:?}"),
            details: Value::Null,
        });
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256_file(path: &Path, cap: usize) -> Result<(u64, String), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_usize;
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n)
            .ok_or_else(|| "digest byte count overflow".to_owned())?;
        if count > cap {
            return Err(format!("source observation exceeds {cap} bytes"));
        }
        hasher.update(&buffer[..n]);
    }
    Ok((count as u64, format!("{:x}", hasher.finalize())))
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> Option<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    Some([metadata.dev(), metadata.ino()])
}
#[cfg(not(unix))]
fn identity(_metadata: &fs::Metadata) -> Option<[u64; 2]> {
    None
}

fn mtime_ns(metadata: &fs::Metadata) -> Option<u128> {
    metadata.modified().ok().and_then(|time| {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos())
    })
}

fn observe(path: &Path, kind: &str, cap: usize) -> Result<SidecarObservation, String> {
    let present = path.exists();
    let path_text = path_string(path).ok_or_else(|| "path is not valid UTF-8".to_owned())?;
    if !present {
        return Ok(SidecarObservation {
            kind: kind.to_owned(),
            path: path_text,
            present: false,
            device_inode: None,
            size_bytes: None,
            mtime_ns: None,
            sha256_hex: None,
        });
    }
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err(format!("{kind} sidecar is not a regular file"));
    }
    let (_, digest) = sha256_file(path, cap)?;
    Ok(SidecarObservation {
        kind: kind.to_owned(),
        path: path_text,
        present: true,
        device_inode: identity(&metadata),
        size_bytes: Some(metadata.len()),
        mtime_ns: mtime_ns(&metadata),
        sha256_hex: Some(digest),
    })
}

fn source_observation(
    label: &str,
    phase: &str,
    path: &Path,
    cap: usize,
) -> Result<SourceObservation, MergeError> {
    let main = observe(path, "main", cap).map_err(|message| MergeError {
        class: "MERGE_INPUT_INVALID",
        message,
        details: Value::Null,
    })?;
    let journal_path = PathBuf::from(format!("{}-journal", path.display()));
    let wal_path = PathBuf::from(format!("{}-wal", path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", path.display()));
    let rollback_journal =
        observe(&journal_path, "rollback_journal", cap).map_err(|message| MergeError {
            class: "MERGE_INPUT_INVALID",
            message,
            details: Value::Null,
        })?;
    let wal = observe(&wal_path, "wal", cap).map_err(|message| MergeError {
        class: "MERGE_INPUT_INVALID",
        message,
        details: Value::Null,
    })?;
    let shm = observe(&shm_path, "shm", cap).map_err(|message| MergeError {
        class: "MERGE_INPUT_INVALID",
        message,
        details: Value::Null,
    })?;
    if rollback_journal.present || wal.present || shm.present {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: format!("{label} has unsupported SQLite sidecar files"),
            details: json!({"label": label}),
        });
    }
    Ok(SourceObservation {
        label: label.to_owned(),
        phase: phase.to_owned(),
        main,
        rollback_journal,
        wal,
        shm,
    })
}

fn source_path(path: &str) -> Result<PathBuf, MergeError> {
    if path.as_bytes().contains(&0) || !Path::new(path).is_absolute() || path.starts_with("file:") {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "merge paths must be absolute literal filesystem paths".to_owned(),
            details: Value::Null,
        });
    }
    let canonical = fs::canonicalize(path).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    let metadata = fs::metadata(&canonical).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    if !metadata.is_file() {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "merge source is not a regular file".to_owned(),
            details: Value::Null,
        });
    }
    let mut header = [0_u8; 16];
    let mut file = File::open(&canonical).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    file.read_exact(&mut header).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    if &header != b"SQLite format 3\0" {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "source is not an initialized SQLite database".to_owned(),
            details: Value::Null,
        });
    }
    Ok(canonical)
}

fn make_workspace() -> Result<PathBuf, MergeError> {
    #[cfg(not(unix))]
    {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "merge workspaces require a verified Unix private-directory owner".to_owned(),
            details: Value::Null,
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let root = std::env::temp_dir();
        let root_text = root.to_str().ok_or_else(|| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "temporary directory path is not valid UTF-8".to_owned(),
            details: Value::Null,
        })?;
        if root_text.is_empty() {
            return Err(MergeError {
                class: "MERGE_INPUT_INVALID",
                message: "temporary directory path is empty".to_owned(),
                details: Value::Null,
            });
        }
        for _ in 0..16 {
            let candidate = root.join(format!("sqlite-mcp-merge-{}", Uuid::new_v4()));
            let mut builder = fs::DirBuilder::new();
            builder.recursive(false).mode(0o700);
            match builder.create(&candidate) {
                Ok(()) => {
                    let mode = fs::metadata(&candidate).map_err(|e| MergeError {
                        class: "MERGE_INPUT_INVALID",
                        message: e.to_string(),
                        details: Value::Null,
                    })?;
                    use std::os::unix::fs::PermissionsExt;
                    if mode.permissions().mode() & 0o777 != 0o700 {
                        let _ = fs::remove_dir(&candidate);
                        return Err(MergeError {
                            class: "MERGE_INPUT_INVALID",
                            message: "temporary workspace is not private".to_owned(),
                            details: Value::Null,
                        });
                    }
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(MergeError {
                        class: "MERGE_INPUT_INVALID",
                        message: error.to_string(),
                        details: Value::Null,
                    });
                }
            }
        }
        Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: "could not allocate a unique merge workspace".to_owned(),
            details: Value::Null,
        })
    }
}

fn write_bounded(path: &Path, bytes: &[u8], cap: usize) -> Result<u64, String> {
    if bytes.len() > cap {
        return Err(format!("artifact exceeds configured limit of {cap} bytes"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(bytes.len() as u64)
}

#[derive(Debug)]
struct ExclusiveOutputOwner {
    path: PathBuf,
    identity: Option<[u64; 2]>,
}

impl ExclusiveOutputOwner {
    fn capture(path: &Path, file: &File) -> Result<Self, String> {
        let descriptor = file.metadata().map_err(|e| e.to_string())?;
        let target = fs::metadata(path).map_err(|e| e.to_string())?;
        let descriptor_id = identity(&descriptor);
        let target_id = identity(&target);
        if descriptor_id != target_id {
            return Err("output identity changed during exclusive creation".to_owned());
        }
        Ok(Self {
            path: path.to_owned(),
            identity: descriptor_id,
        })
    }

    fn still_owned(&self) -> Result<bool, String> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        Ok(identity(&metadata) == self.identity)
    }

    fn remove_if_owned(self) -> Result<(), String> {
        if !self.still_owned()? {
            return Err("output identity changed or output disappeared".to_owned());
        }
        fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        if self.path.exists() {
            return Err("output remained after owned removal".to_owned());
        }
        Ok(())
    }
}

fn cleanup_workspace(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path).map_err(|e| e.to_string())?;
    if path.exists() {
        return Err("workspace remained after removal".to_owned());
    }
    Ok(())
}

fn schema_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn value_literal(value: ValueRef<'_>) -> Result<String, String> {
    match value {
        ValueRef::Null => Ok("NULL".to_owned()),
        ValueRef::Integer(v) => Ok(v.to_string()),
        ValueRef::Real(v) => {
            if !v.is_finite() {
                return Err("non-finite REAL is not representable".to_owned());
            }
            let mut text = v.to_string().replace("e+", "e");
            if v == 0.0 && v.is_sign_negative() {
                text = "-0.0".to_owned();
            } else if !text.contains('.') && !text.contains('e') && !text.contains('E') {
                text.push_str(".0");
            }
            Ok(text)
        }
        ValueRef::Text(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| "TEXT is not valid UTF-8".to_owned())?;
            if bytes.contains(&0) {
                return Err("TEXT contains NUL".to_owned());
            }
            Ok(format!("'{}'", text.replace('\'', "''")))
        }
        ValueRef::Blob(bytes) => Ok(format!(
            "X'{}'",
            bytes.iter().map(|b| format!("{b:02X}")).collect::<String>()
        )),
    }
}

fn typed_value(value: ValueRef<'_>) -> Result<Value, String> {
    Ok(match value {
        ValueRef::Null => json!({"kind":"null"}),
        ValueRef::Integer(v) => json!({"kind":"integer","value":v.to_string()}),
        ValueRef::Real(v) => json!({"kind":"real_bits","value":format!("{:016x}", v.to_bits())}),
        ValueRef::Text(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| "TEXT is not valid UTF-8".to_owned())?;
            if bytes.contains(&0) {
                return Err("TEXT contains NUL".to_owned());
            }
            json!({"kind":"text","value":b64(text.as_bytes())})
        }
        ValueRef::Blob(bytes) => json!({"kind":"blob","value":b64(bytes)}),
    })
}

fn class_rank(class: &str) -> u8 {
    match class {
        "table" => 0,
        "index" => 1,
        "view" => 2,
        "trigger" => 3,
        _ => 4,
    }
}

fn canonical_schema_rows(conn: &Connection) -> Result<Vec<(String, String, String)>, MergeError> {
    let mut stmt = conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
    ).map_err(|e| MergeError { class: "MERGE_INPUT_INVALID", message: e.to_string(), details: Value::Null })?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
    let mut result = Vec::new();
    for row in rows {
        let (kind, name, sql) = row.map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        if !sql.is_ascii()
            && sql.as_bytes().iter().any(|b| *b >= 0x80)
            && std::str::from_utf8(sql.as_bytes()).is_err()
        {
            return Err(MergeError {
                class: "MERGE_UNREPRESENTABLE_CONTENT",
                message: format!("schema SQL for {name:?} is not UTF-8"),
                details: Value::Null,
            });
        }
        let class = match kind.as_str() {
            "table" => "table",
            "index" => "index",
            "view" => "view",
            "trigger" => "trigger",
            _ => continue,
        };
        result.push((
            class.to_owned(),
            name,
            format!("{};", sql.trim_end_matches(';')),
        ));
    }
    result.sort_by(|a, b| {
        class_rank(&a.0)
            .cmp(&class_rank(&b.0))
            .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
            .then_with(|| a.2.as_bytes().cmp(b.2.as_bytes()))
    });
    Ok(result)
}

fn table_metadata(
    conn: &Connection,
    table: &str,
) -> Result<(bool, Vec<String>, bool, Vec<String>), MergeError> {
    let mut list = conn.prepare("PRAGMA table_list").map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    let mut found = None;
    let rows = list
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
    for row in rows {
        let (name, kind, wr) = row.map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        if name == table {
            found = Some((kind, wr != 0));
            break;
        }
    }
    let Some((kind, without_rowid)) = found else {
        return Err(MergeError {
            class: "MERGE_INPUT_INVALID",
            message: format!("table {table:?} is missing from table_list"),
            details: Value::Null,
        });
    };
    if kind != "table" {
        return Err(MergeError {
            class: "MERGE_UNREPRESENTABLE_CONTENT",
            message: format!("virtual table {table:?} is unsupported"),
            details: Value::Null,
        });
    }
    let pragma = format!(
        "PRAGMA table_xinfo({})",
        quoted_identifier(table).map_err(|e| MergeError {
            class: e.class,
            message: e.message,
            details: e.details
        })?
    );
    let mut columns_stmt = conn.prepare(&pragma).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    let columns = columns_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
    let mut names = Vec::new();
    let mut primary_key = Vec::new();
    let mut integer_primary_key = false;
    for col in columns {
        let (name, declared_type, pk, hidden) = col.map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        if hidden == 0 {
            names.push(name.clone());
        }
        if pk > 0 {
            primary_key.push((pk, name.clone()));
        }
        if pk == 1 && hidden == 0 && declared_type.trim().eq_ignore_ascii_case("INTEGER") {
            integer_primary_key = true;
        }
    }
    primary_key.sort_by_key(|(position, _)| *position);
    Ok((
        without_rowid,
        names,
        integer_primary_key,
        primary_key.into_iter().map(|(_, name)| name).collect(),
    ))
}

fn extract_one(
    conn: &Connection,
    label: &str,
    out: &mut String,
    statement_limit: usize,
) -> Result<u64, MergeError> {
    let schemas = canonical_schema_rows(conn)?;
    if schemas.is_empty() {
        out.push_str(FORMAT_HEADER);
        return Ok(0);
    }
    out.push_str(FORMAT_HEADER);
    out.push_str(&format!(
        "-- sqlite-mcp schema-baseline: v1 count={}\n",
        schemas.len()
    ));
    for (kind, name, sql) in &schemas {
        let bytes = sql.as_bytes();
        out.push_str(&format!(
            "-- sqlite-mcp schema-hash: v1 kind={kind} name_b64={} sql_sha256={} sql_bytes={}\n",
            b64(name.as_bytes()),
            schema_digest(bytes),
            b64(bytes)
        ));
    }
    out.push_str("-- sqlite-mcp schema-baseline-end: v1\n");
    let mut count = schemas.len() as u64;
    for (_kind, name, sql) in schemas.iter().filter(|(kind, _, _)| kind == "table") {
        out.push_str(sql);
        out.push('\n');
        let (without_rowid, columns, integer_primary_key, primary_key) =
            table_metadata(conn, name)?;
        let qname = quoted_identifier(name).map_err(|e| MergeError {
            class: e.class,
            message: e.message,
            details: e.details,
        })?;
        let selected_columns = if without_rowid || integer_primary_key {
            columns
                .iter()
                .map(|c| quoted_identifier(c).unwrap_or_else(|_| "\"\"".into()))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            std::iter::once("rowid".to_owned())
                .chain(
                    columns
                        .iter()
                        .map(|c| quoted_identifier(c).unwrap_or_else(|_| "\"\"".into())),
                )
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut query = format!("SELECT {selected_columns} FROM {qname}");
        if without_rowid {
            let order = primary_key
                .iter()
                .map(|c| quoted_identifier(c).unwrap_or_else(|_| "\"\"".into()))
                .collect::<Vec<_>>()
                .join(",");
            if order.is_empty() {
                return Err(MergeError {
                    class: "MERGE_UNREPRESENTABLE_CONTENT",
                    message: format!(
                        "WITHOUT ROWID table {name:?} has no deterministic primary key"
                    ),
                    details: Value::Null,
                });
            }
            query.push_str(&format!(" ORDER BY {order}"));
        } else {
            query.push_str(" ORDER BY rowid");
        }
        let mut rows = conn.prepare(&query).map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        let mut result = rows.query([]).map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        let mut ordinal = 0_u64;
        while let Some(row) = result.next().map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })? {
            if count >= statement_limit as u64 {
                return Err(MergeError {
                    class: "MERGE_LIMIT_EXCEEDED",
                    message: "merge statement limit exceeded".to_owned(),
                    details: Value::Null,
                });
            }
            let value_start = 0;
            let mut literals = Vec::new();
            let mut typed = Vec::new();
            for index in value_start..row.as_ref().column_count() {
                let value = row.get_ref(index).map_err(|e| MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: e.to_string(),
                    details: Value::Null,
                })?;
                literals.push(value_literal(value).map_err(|message| MergeError {
                    class: "MERGE_UNREPRESENTABLE_CONTENT",
                    message: format!("{label}.{name} row {ordinal}: {message}"),
                    details: Value::Null,
                })?);
                typed.push(typed_value(value).map_err(|message| MergeError {
                    class: "MERGE_UNREPRESENTABLE_CONTENT",
                    message: format!("{label}.{name} row {ordinal}: {message}"),
                    details: Value::Null,
                })?);
            }
            let annotation = json!({"values":typed});
            out.push_str(&format!(
                "-- sqlite-mcp typed-value: v1 table_b64={} row={} values_b64={}\n",
                b64(name.as_bytes()),
                ordinal,
                b64(serde_json::to_string(&annotation["values"])
                    .unwrap()
                    .as_bytes())
            ));
            let insert_columns = if without_rowid || integer_primary_key {
                columns
                    .iter()
                    .map(|c| quoted_identifier(c).unwrap())
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                std::iter::once("rowid".to_owned())
                    .chain(columns.iter().map(|c| quoted_identifier(c).unwrap()))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            out.push_str(&format!(
                "INSERT INTO {qname} ({insert_columns}) VALUES ({});\n",
                literals.join(",")
            ));
            count += 1;
            ordinal += 1;
        }
    }
    for (kind, _, sql) in schemas.iter().filter(|(kind, _, _)| kind != "table") {
        if count >= statement_limit as u64 {
            return Err(MergeError {
                class: "MERGE_LIMIT_EXCEEDED",
                message: "merge statement limit exceeded".to_owned(),
                details: Value::Null,
            });
        }
        let _ = kind;
        out.push_str(sql);
        out.push('\n');
        count += 1;
    }
    Ok(count)
}

fn manifest_json(
    sources: [&str; 3],
    observations: &[SourceObservation],
    files: &Files,
    counts: &FileByteCounts,
) -> Result<Vec<u8>, MergeError> {
    let value = json!({
        "format_version": 1,
        "labels": ["base", "ours", "theirs"],
        "canonical_source_paths": sources,
        "source_observations": observations,
        "files": files,
        "file_byte_counts": counts,
        "workspace_state": "Retained",
    });
    let mut bytes = serde_json::to_vec(&value).map_err(|e| MergeError {
        class: "MERGE_INPUT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    bytes.push(b'\n');
    if bytes.len() > MANIFEST_BYTE_LIMIT {
        return Err(MergeError {
            class: "MERGE_LIMIT_EXCEEDED",
            message: "manifest exceeds fixed limit".to_owned(),
            details: Value::Null,
        });
    }
    Ok(bytes)
}

pub fn extract(
    base_path: &str,
    ours_path: &str,
    theirs_path: &str,
    text_limit: usize,
    statement_limit: usize,
    observation_limit: usize,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<ExtractionResult, MergeError> {
    let cancelled = || cancel.as_ref().is_some_and(|token| token.is_cancelled());
    let inputs = match [
        source_path(base_path),
        source_path(ours_path),
        source_path(theirs_path),
    ] {
        [Ok(base), Ok(ours), Ok(theirs)] => [base, ours, theirs],
        results => {
            let error = results
                .into_iter()
                .find_map(Result::err)
                .unwrap_or(MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: "merge source validation failed".to_owned(),
                    details: Value::Null,
                });
            return Err(MergeError::extraction(
                error.class,
                error.message,
                None,
                Files {
                    base_sql: None,
                    ours_sql: None,
                    theirs_sql: None,
                    resolved_sql: None,
                },
                None,
                Vec::new(),
                FileByteCounts {
                    base_sql: 0,
                    ours_sql: 0,
                    theirs_sql: 0,
                    resolved_sql: 0,
                    manifest: 0,
                },
                "NotCreated",
                Vec::new(),
                None,
            ));
        }
    };
    let source_strings = match inputs
        .iter()
        .map(|path| path.to_str())
        .collect::<Option<Vec<_>>>()
    {
        Some(paths) => paths,
        None => {
            return Err(MergeError::extraction(
                "MERGE_INPUT_INVALID",
                "canonical source path is not valid UTF-8",
                None,
                Files {
                    base_sql: None,
                    ours_sql: None,
                    theirs_sql: None,
                    resolved_sql: None,
                },
                None,
                Vec::new(),
                FileByteCounts {
                    base_sql: 0,
                    ours_sql: 0,
                    theirs_sql: 0,
                    resolved_sql: 0,
                    manifest: 0,
                },
                "NotCreated",
                Vec::new(),
                None,
            ));
        }
    };
    let labels = ["base", "ours", "theirs"];
    let workspace = make_workspace()?;
    let mut observations = Vec::new();
    let mut files = Files {
        base_sql: None,
        ours_sql: None,
        theirs_sql: None,
        resolved_sql: None,
    };
    let mut counts = FileByteCounts {
        base_sql: 0,
        ours_sql: 0,
        theirs_sql: 0,
        resolved_sql: 0,
        manifest: 0,
    };
    let result = (|| {
        let mut sqls = Vec::new();
        for (index, (label, path)) in labels.iter().zip(inputs.iter()).enumerate() {
            if cancelled() {
                return Err(MergeError {
                    class: "MERGE_LIMIT_EXCEEDED",
                    message: "merge operation cancelled".to_owned(),
                    details: Value::Null,
                });
            }
            let before = source_observation(label, "before", path, observation_limit)?;
            if !before.main.present {
                return Err(MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: "source disappeared before open".to_owned(),
                    details: Value::Null,
                });
            }
            observations.push(before);
            let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: e.to_string(),
                    details: Value::Null,
                })?;
            connection
                .execute_batch("BEGIN DEFERRED")
                .map_err(|e| MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: e.to_string(),
                    details: Value::Null,
                })?;
            let mut text = String::new();
            extract_one(&connection, label, &mut text, statement_limit)?;
            connection
                .execute_batch("ROLLBACK")
                .map_err(|e| MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: e.to_string(),
                    details: Value::Null,
                })?;
            if text.len() > text_limit {
                return Err(MergeError {
                    class: "MERGE_LIMIT_EXCEEDED",
                    message: format!("{label}.sql exceeds configured text limit"),
                    details: Value::Null,
                });
            }
            sqls.push(text);
            let after = source_observation(label, "after", path, observation_limit)?;
            let before = observations.last().expect("before observation was pushed");
            if before.main != after.main
                || before.rollback_journal != after.rollback_journal
                || before.wal != after.wal
                || before.shm != after.shm
            {
                return Err(MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message: format!("source {label} changed during extraction"),
                    details: Value::Null,
                });
            }
            observations.push(after);
            let _ = index;
        }
        let names = ["base.sql", "ours.sql", "theirs.sql"];
        for ((label, text), name) in labels.iter().zip(sqls.iter()).zip(names) {
            if cancelled() {
                return Err(MergeError {
                    class: "MERGE_LIMIT_EXCEEDED",
                    message: "merge operation cancelled".to_owned(),
                    details: Value::Null,
                });
            }
            let path = workspace.join(name);
            let n = write_bounded(&path, text.as_bytes(), text_limit).map_err(|message| {
                MergeError {
                    class: "MERGE_INPUT_INVALID",
                    message,
                    details: Value::Null,
                }
            })?;
            match *label {
                "base" => {
                    files.base_sql = path_string(&path);
                    counts.base_sql = n;
                }
                "ours" => {
                    files.ours_sql = path_string(&path);
                    counts.ours_sql = n;
                }
                _ => {
                    files.theirs_sql = path_string(&path);
                    counts.theirs_sql = n;
                }
            }
        }
        let resolved = workspace.join("resolved.sql");
        let ours = fs::read(files.ours_sql.as_ref().unwrap()).map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        counts.resolved_sql =
            write_bounded(&resolved, &ours, text_limit).map_err(|message| MergeError {
                class: "MERGE_INPUT_INVALID",
                message,
                details: Value::Null,
            })?;
        files.resolved_sql = path_string(&resolved);
        let manifest_path = workspace.join("manifest.json");
        let mut manifest = Vec::new();
        for _ in 0..4 {
            manifest = manifest_json(
                [source_strings[0], source_strings[1], source_strings[2]],
                &observations,
                &files,
                &counts,
            )?;
            let next_count = manifest.len() as u64;
            if counts.manifest == next_count {
                break;
            }
            counts.manifest = next_count;
        }
        if counts.manifest != manifest.len() as u64 {
            return Err(MergeError {
                class: "MERGE_INPUT_INVALID",
                message: "manifest byte count did not stabilize".to_owned(),
                details: Value::Null,
            });
        }
        write_bounded(&manifest_path, &manifest, MANIFEST_BYTE_LIMIT).map_err(|message| {
            MergeError {
                class: "MERGE_INPUT_INVALID",
                message,
                details: Value::Null,
            }
        })?;
        Ok((manifest_path, counts.clone()))
    })();
    match result {
        Ok((manifest_path, counts)) => Ok(ExtractionResult {
            workspace_path: path_string(&workspace).unwrap(),
            files,
            manifest_path: path_string(&manifest_path).unwrap(),
            source_observations: observations,
            file_byte_counts: counts,
            format_version: 1,
            workspace_state: "Retained".to_owned(),
        }),
        Err(error) => {
            let cleanup = cleanup_workspace(&workspace);
            let state = if cleanup.is_ok() {
                "Removed"
            } else {
                "Uncertain"
            };
            let cleanup_errors = cleanup.err().into_iter().collect::<Vec<_>>();
            let details = MergeError::extraction(
                error.class,
                error.message,
                Some(&workspace),
                files,
                Some(&workspace.join("manifest.json")),
                observations,
                counts.clone(),
                state,
                cleanup_errors,
                None,
            );
            Err(details)
        }
    }
}

fn read_text(path: &str, limit: usize) -> Result<(PathBuf, String, u64), MergeError> {
    let source = source_path_text(path)?;
    let mut file = File::open(&source).map_err(|e| MergeError {
        class: "MERGE_FORMAT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > limit {
            return Err(MergeError {
                class: "MERGE_LIMIT_EXCEEDED",
                message: "SQL source exceeds configured text limit".to_owned(),
                details: Value::Null,
            });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.contains(&0) {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "SQL source contains NUL".to_owned(),
            details: Value::Null,
        });
    }
    let text = String::from_utf8(bytes.clone()).map_err(|_| MergeError {
        class: "MERGE_FORMAT_INVALID",
        message: "SQL source is not UTF-8".to_owned(),
        details: Value::Null,
    })?;
    Ok((source, text, bytes.len() as u64))
}

fn source_path_text(path: &str) -> Result<PathBuf, MergeError> {
    if path.as_bytes().contains(&0) || !Path::new(path).is_absolute() || path.starts_with("file:") {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "SQL path must be an absolute literal path".to_owned(),
            details: Value::Null,
        });
    }
    let canonical = fs::canonicalize(path).map_err(|e| MergeError {
        class: "MERGE_FORMAT_INVALID",
        message: e.to_string(),
        details: Value::Null,
    })?;
    if !canonical.is_file() {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "SQL path is not a regular file".to_owned(),
            details: Value::Null,
        });
    }
    Ok(canonical)
}

fn check_output_path(path: &str, input: &Path) -> Result<PathBuf, MergeError> {
    if path.as_bytes().contains(&0) || !Path::new(path).is_absolute() || path.starts_with("file:") {
        return Err(MergeError {
            class: "MERGE_OUTPUT_EXISTS",
            message: "output path must be an absolute literal path".to_owned(),
            details: Value::Null,
        });
    }
    let raw = PathBuf::from(path);
    let parent = raw.parent().ok_or_else(|| MergeError {
        class: "MERGE_OUTPUT_EXISTS",
        message: "output parent is missing".to_owned(),
        details: Value::Null,
    })?;
    if !parent.is_dir() {
        return Err(MergeError {
            class: "MERGE_OUTPUT_EXISTS",
            message: "output parent does not exist".to_owned(),
            details: Value::Null,
        });
    }
    let candidate = parent
        .canonicalize()
        .map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?
        .join(raw.file_name().ok_or_else(|| MergeError {
            class: "MERGE_OUTPUT_EXISTS",
            message: "output filename is missing".to_owned(),
            details: Value::Null,
        })?);
    if candidate == input {
        return Err(MergeError {
            class: "MERGE_OUTPUT_EXISTS",
            message: "SQL source and output alias the same file".to_owned(),
            details: Value::Null,
        });
    }
    if candidate.exists() {
        return Err(MergeError {
            class: "MERGE_OUTPUT_EXISTS",
            message: "output already exists".to_owned(),
            details: Value::Null,
        });
    }
    let file_name = candidate.file_name().ok_or_else(|| MergeError {
        class: "MERGE_OUTPUT_EXISTS",
        message: "output filename is missing".to_owned(),
        details: Value::Null,
    })?;
    let file_name = file_name.to_str().ok_or_else(|| MergeError {
        class: "MERGE_OUTPUT_EXISTS",
        message: "output filename is not valid UTF-8".to_owned(),
        details: Value::Null,
    })?;
    for suffix in ["-wal", "-shm", "-journal"] {
        if candidate
            .with_file_name(format!("{file_name}{suffix}"))
            .exists()
        {
            return Err(MergeError {
                class: "MERGE_OUTPUT_EXISTS",
                message: "output sidecar path is already reserved".to_owned(),
                details: Value::Null,
            });
        }
    }
    Ok(candidate)
}

fn validate_header(text: &str) -> Result<usize, MergeError> {
    if !text.starts_with(FORMAT_HEADER) || text.starts_with('\u{feff}') || text.contains("\r\n") {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "merge text must begin with the exact LF-terminated format header".to_owned(),
            details: Value::Null,
        });
    }
    Ok(FORMAT_HEADER.len())
}

fn parse_baseline(
    text: &str,
    mut offset: usize,
) -> Result<(BTreeMap<String, Vec<u8>>, usize), MergeError> {
    if text[offset..]
        .trim_start_matches([' ', '\n', '\t'])
        .starts_with("-- sqlite-mcp schema-baseline:")
    {
        let line_end = text[offset..].find('\n').ok_or_else(|| MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "unterminated schema baseline header".to_owned(),
            details: Value::Null,
        })? + offset;
        let line = &text[offset..line_end];
        let count_text = line
            .strip_prefix("-- sqlite-mcp schema-baseline: v1 count=")
            .ok_or_else(|| MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: "invalid schema baseline header".to_owned(),
                details: Value::Null,
            })?;
        let count: usize = count_text.parse().map_err(|_| MergeError {
            class: "MERGE_LIMIT_EXCEEDED",
            message: "invalid or excessive schema baseline count".to_owned(),
            details: Value::Null,
        })?;
        if count > 1_000_000 {
            return Err(MergeError {
                class: "MERGE_LIMIT_EXCEEDED",
                message: "schema baseline count exceeds limit".to_owned(),
                details: Value::Null,
            });
        }
        offset = line_end + 1;
        let mut map = BTreeMap::new();
        for _ in 0..count {
            let end = text[offset..].find('\n').ok_or_else(|| MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: "unterminated schema baseline record".to_owned(),
                details: Value::Null,
            })? + offset;
            let line = &text[offset..end];
            let payload = line
                .strip_prefix("-- sqlite-mcp schema-hash: v1 ")
                .ok_or_else(|| MergeError {
                    class: "MERGE_FORMAT_INVALID",
                    message: "invalid schema baseline record".to_owned(),
                    details: Value::Null,
                })?;
            let mut fields = BTreeMap::new();
            for item in payload.split(' ') {
                let Some((key, value)) = item.split_once('=') else {
                    return Err(MergeError {
                        class: "MERGE_FORMAT_INVALID",
                        message: "invalid schema baseline field".to_owned(),
                        details: Value::Null,
                    });
                };
                fields.insert(key, value);
            }
            let name = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(
                    fields
                        .get("name_b64")
                        .ok_or_else(|| MergeError {
                            class: "MERGE_FORMAT_INVALID",
                            message: "missing baseline name".to_owned(),
                            details: Value::Null,
                        })?
                        .as_bytes(),
                )
                .map_err(|_| MergeError {
                    class: "MERGE_FORMAT_INVALID",
                    message: "invalid baseline name".to_owned(),
                    details: Value::Null,
                })?;
            let name = String::from_utf8(name).map_err(|_| MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: "baseline name is not UTF-8".to_owned(),
                details: Value::Null,
            })?;
            let sql = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(
                    fields
                        .get("sql_bytes")
                        .ok_or_else(|| MergeError {
                            class: "MERGE_FORMAT_INVALID",
                            message: "missing baseline SQL".to_owned(),
                            details: Value::Null,
                        })?
                        .as_bytes(),
                )
                .map_err(|_| MergeError {
                    class: "MERGE_FORMAT_INVALID",
                    message: "invalid baseline SQL".to_owned(),
                    details: Value::Null,
                })?;
            let digest = fields.get("sql_sha256").ok_or_else(|| MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: "missing baseline digest".to_owned(),
                details: Value::Null,
            })?;
            if schema_digest(&sql) != *digest {
                return Err(MergeError {
                    class: "MERGE_FORMAT_INVALID",
                    message: "schema baseline digest mismatch".to_owned(),
                    details: Value::Null,
                });
            }
            map.insert(name, sql);
            offset = end + 1;
        }
        let end_marker = "-- sqlite-mcp schema-baseline-end: v1\n";
        if !text[offset..].starts_with(end_marker) {
            return Err(MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: "schema baseline is not closed".to_owned(),
                details: Value::Null,
            });
        }
        offset += end_marker.len();
        Ok((map, offset))
    } else {
        Ok((BTreeMap::new(), offset))
    }
}

fn split_statements(text: &str, offset: usize) -> Result<Vec<(usize, usize)>, MergeError> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut start = offset;
    let mut i = offset;
    let mut quote = None;
    let mut bracket_quote = false;
    let mut line_comment = false;
    let mut block_comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if line_comment {
            if c == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_comment {
            if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if bracket_quote {
            if c == b']' {
                if bytes.get(i + 1) == Some(&b']') {
                    i += 2;
                    continue;
                }
                bracket_quote = false;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                if bytes.get(i + 1) == Some(&q) {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            line_comment = true;
            i += 2;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            block_comment = true;
            i += 2;
            continue;
        }
        if c == b'[' {
            bracket_quote = true;
            i += 1;
            continue;
        }
        if matches!(c, b'\'' | b'"' | b'`') {
            quote = Some(c);
            i += 1;
            continue;
        }
        if c == b';' {
            let candidate = &text[start..=i];
            if candidate.trim().is_empty() {
                start = i + 1;
                i += 1;
                continue;
            }
            result.push((start, i + 1));
            start = i + 1;
        }
        i += 1;
    }
    if block_comment || bracket_quote || quote.is_some() {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "unterminated SQL comment or quoted literal".to_owned(),
            details: Value::Null,
        });
    }
    if !text[start..].trim().is_empty() {
        return Err(MergeError {
            class: "MERGE_FORMAT_INVALID",
            message: "executable SQL must be terminated by a semicolon".to_owned(),
            details: Value::Null,
        });
    }
    Ok(result)
}

fn statement_kind(sql: &str) -> &'static str {
    let mut trimmed = sql.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == ';');
    loop {
        if let Some(rest) = trimmed.strip_prefix("--") {
            trimmed = rest
                .find('\n')
                .map(|index| &rest[index + 1..])
                .unwrap_or("")
                .trim_start();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/*") {
            trimmed = rest
                .find("*/")
                .map(|index| &rest[index + 2..])
                .unwrap_or("")
                .trim_start();
            continue;
        }
        break;
    }
    let first = trimmed
        .split(|c: char| c.is_ascii_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "CREATE" => "create",
        "INSERT" => "insert",
        "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => "transaction",
        "PRAGMA" | "ATTACH" | "DETACH" | "VACUUM" => "policy",
        _ => "other",
    }
}

fn execute_one(conn: &Connection, sql: &str, statement_index: u64) -> Result<(), MergeError> {
    match statement_kind(sql) {
        "transaction" => {
            return Err(MergeError {
                class: "MERGE_POLICY_DENIED",
                message: format!(
                    "transaction control is not allowed in statement {statement_index}"
                ),
                details: Value::Null,
            });
        }
        "policy" => {
            return Err(MergeError {
                class: "MERGE_POLICY_DENIED",
                message: format!("statement {statement_index} is denied by merge policy"),
                details: Value::Null,
            });
        }
        "other" => {
            return Err(MergeError {
                class: "MERGE_FORMAT_INVALID",
                message: format!("statement {statement_index} is outside the merge grammar"),
                details: Value::Null,
            });
        }
        _ => {}
    }
    let mutation_signal = crate::policy::MutationSignal::new();
    crate::policy::check_stored_body(sql, &mutation_signal).map_err(|e| MergeError {
        class: "MERGE_POLICY_DENIED",
        message: format!("statement {statement_index}: {e}"),
        details: Value::Null,
    })?;
    crate::policy::prepare_exact(conn, sql).map_err(|e| MergeError {
        class: "MERGE_FORMAT_INVALID",
        message: format!("statement {statement_index}: {e}"),
        details: Value::Null,
    })?;
    let mut statement = conn.prepare(sql).map_err(|e| MergeError {
        class: "MERGE_POLICY_DENIED",
        message: format!("statement {statement_index}: {e}"),
        details: Value::Null,
    })?;
    let mut rows = statement.query([]).map_err(|e| MergeError {
        class: "MERGE_POLICY_DENIED",
        message: format!("statement {statement_index}: {e}"),
        details: Value::Null,
    })?;
    while rows
        .next()
        .map_err(|e| MergeError {
            class: "MERGE_POLICY_DENIED",
            message: format!("statement {statement_index}: {e}"),
            details: Value::Null,
        })?
        .is_some()
    {}
    Ok(())
}

pub fn import(
    sql_path: &str,
    output_path: &str,
    text_limit: usize,
    statement_limit: usize,
    image_limit: usize,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<ImportResult, MergeError> {
    let cancelled = || cancel.as_ref().is_some_and(|token| token.is_cancelled());
    let (source, text, byte_count) = match read_text(sql_path, text_limit) {
        Ok(value) => value,
        Err(error) => {
            return Err(MergeError::import(
                error.class,
                error.message,
                None,
                "NotCreated",
                0,
                0,
                None,
                Vec::new(),
                None,
            ));
        }
    };
    let output = match check_output_path(output_path, &source) {
        Ok(value) => value,
        Err(error) => {
            return Err(MergeError::import(
                error.class,
                error.message,
                Some(Path::new(output_path)),
                "NotCreated",
                0,
                byte_count,
                None,
                Vec::new(),
                None,
            ));
        }
    };
    let offset = match validate_header(&text) {
        Ok(value) => value,
        Err(error) => {
            return Err(MergeError::import(
                error.class,
                error.message,
                Some(&output),
                "NotCreated",
                0,
                byte_count,
                None,
                Vec::new(),
                None,
            ));
        }
    };
    let (baseline, body_offset) = match parse_baseline(&text, offset) {
        Ok(value) => value,
        Err(error) => {
            return Err(MergeError::import(
                error.class,
                error.message,
                Some(&output),
                "NotCreated",
                0,
                byte_count,
                None,
                Vec::new(),
                None,
            ));
        }
    };
    let statements = match split_statements(&text, body_offset) {
        Ok(value) => value,
        Err(error) => {
            return Err(MergeError::import(
                error.class,
                error.message,
                Some(&output),
                "NotCreated",
                0,
                byte_count,
                None,
                Vec::new(),
                None,
            ));
        }
    };
    if statements.len() > statement_limit {
        return Err(MergeError::import(
            "MERGE_LIMIT_EXCEEDED",
            "merge statement limit exceeded",
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        ));
    }
    if (!baseline.is_empty() && statements.is_empty())
        || (baseline.is_empty() && !statements.is_empty())
    {
        return Err(MergeError::import(
            "MERGE_FORMAT_INVALID",
            "nonempty merge text requires exactly one closed schema baseline",
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        ));
    }
    if cancelled() {
        return Err(MergeError::import(
            "MERGE_LIMIT_EXCEEDED",
            "merge operation cancelled",
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        ));
    }
    let conn = Connection::open_in_memory().map_err(|e| {
        MergeError::import(
            "MERGE_VALIDATION_FAILED",
            e.to_string(),
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        )
    })?;
    crate::policy::install_authorizer(&conn, false, crate::policy::MutationFlag::new()).map_err(
        |e| {
            MergeError::import(
                "MERGE_POLICY_DENIED",
                e.to_string(),
                Some(&output),
                "NotCreated",
                0,
                byte_count,
                None,
                Vec::new(),
                None,
            )
        },
    )?;
    crate::policy::trusted(|| conn.pragma_update(None, "foreign_keys", false)).map_err(|e| {
        MergeError::import(
            "MERGE_POLICY_DENIED",
            e.to_string(),
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        )
    })?;
    crate::policy::trusted(|| conn.execute_batch("BEGIN")).map_err(|e| {
        MergeError::import(
            "MERGE_VALIDATION_FAILED",
            e.to_string(),
            Some(&output),
            "NotCreated",
            0,
            byte_count,
            None,
            Vec::new(),
            None,
        )
    })?;
    let mut statement_count = 0_u64;
    let mut output_owner: Option<ExclusiveOutputOwner> = None;
    let replay = (|| {
        for (index, (start, end)) in statements.iter().enumerate() {
            if cancelled() {
                return Err(MergeError {
                    class: "MERGE_LIMIT_EXCEEDED",
                    message: "merge operation cancelled".to_owned(),
                    details: Value::Null,
                });
            }
            let sql = &text[*start..*end];
            if statement_kind(sql) == "create" {
                let trimmed = sql.trim();
                if let Some(name) = trimmed.split_whitespace().nth(2) {
                    let name = name.trim_matches(['"', '`', '[', ']']);
                    if let Some(expected) = baseline.get(name)
                        && expected.as_slice() != trimmed.as_bytes()
                    {
                        return Err(MergeError {
                            class: "MERGE_FORMAT_INVALID",
                            message: format!("schema statement {name:?} differs from its baseline"),
                            details: Value::Null,
                        });
                    }
                }
            }
            execute_one(&conn, sql, index as u64)?;
            statement_count += 1;
        }
        crate::policy::trusted(|| conn.execute_batch("COMMIT")).map_err(|e| MergeError {
            class: "MERGE_VALIDATION_FAILED",
            message: e.to_string(),
            details: Value::Null,
        })?;
        crate::policy::trusted(|| conn.pragma_update(None, "foreign_keys", true)).map_err(|e| {
            MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: e.to_string(),
                details: Value::Null,
            }
        })?;
        let violations = crate::policy::trusted(|| {
            conn.prepare("PRAGMA foreign_key_check")
                .and_then(|mut st| st.query_map([], |_| Ok(())).map(|rows| rows.count()))
        })
        .map_err(|e| MergeError {
            class: "MERGE_VALIDATION_FAILED",
            message: format!("foreign_key_check: {e}"),
            details: Value::Null,
        })?;
        if violations != 0 {
            return Err(MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: "foreign_key_check reported violations".to_owned(),
                details: Value::Null,
            });
        }
        let integrity: String = crate::policy::trusted(|| {
            conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
        })
        .map_err(|e| MergeError {
            class: "MERGE_VALIDATION_FAILED",
            message: format!("integrity_check: {e}"),
            details: Value::Null,
        })?;
        if integrity != "ok" {
            return Err(MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: integrity,
                details: Value::Null,
            });
        }
        let data = crate::policy::trusted(|| conn.serialize(rusqlite::MAIN_DB)).map_err(|e| {
            MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: format!("serialize: {e}"),
                details: Value::Null,
            }
        })?;
        if data.is_empty() || data.len() > image_limit {
            return Err(MergeError {
                class: "MERGE_LIMIT_EXCEEDED",
                message: "serialized SQLite image exceeds configured limit".to_owned(),
                details: Value::Null,
            });
        }
        let image = data.to_vec();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .map_err(|e| MergeError {
                class: if e.kind() == std::io::ErrorKind::AlreadyExists {
                    "MERGE_OUTPUT_EXISTS"
                } else {
                    "MERGE_INPUT_INVALID"
                },
                message: e.to_string(),
                details: Value::Null,
            })?;
        output_owner = Some(
            ExclusiveOutputOwner::capture(&output, &file).map_err(|message| MergeError {
                class: "MERGE_INPUT_INVALID",
                message,
                details: Value::Null,
            })?,
        );
        file.write_all(&image).map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        file.sync_all().map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        file.sync_all().map_err(|e| MergeError {
            class: "MERGE_INPUT_INVALID",
            message: e.to_string(),
            details: Value::Null,
        })?;
        drop(file);
        if output_owner
            .as_ref()
            .is_none_or(|owner| !owner.still_owned().unwrap_or(false))
        {
            return Err(MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: "output identity changed before validation".to_owned(),
                details: Value::Null,
            });
        }
        let verify = Connection::open_with_flags(&output, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: e.to_string(),
                details: Value::Null,
            })?;
        let mode: String = verify
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(|e| MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: e.to_string(),
                details: Value::Null,
            })?;
        let object_count: u64 = verify
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| MergeError {
                class: "MERGE_VALIDATION_FAILED",
                message: e.to_string(),
                details: Value::Null,
            })? as u64;
        let table_count: u64 = verify.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |row| row.get::<_, i64>(0)).map_err(|e| MergeError { class: "MERGE_VALIDATION_FAILED", message: e.to_string(), details: Value::Null })? as u64;
        Ok(ImportResult {
            output_path: path_string(&output).unwrap(),
            output_state: "Committed".to_owned(),
            statement_count,
            byte_count,
            validation: ImportValidation {
                journal_mode: mode,
                schema_summary: SchemaSummary {
                    object_count,
                    table_count,
                },
                foreign_key_check: ForeignKeyCheck {
                    ok: true,
                    violations: 0,
                },
                integrity_check: IntegrityCheck {
                    ok: true,
                    message: "ok".to_owned(),
                },
            },
        })
    })();
    match replay {
        Ok(result) => Ok(result),
        Err(error) => {
            let mut cleanup_errors = Vec::new();
            if let Some(owner) = output_owner.take()
                && let Err(cleanup) = owner.remove_if_owned()
            {
                cleanup_errors.push(cleanup);
            }
            if !cleanup_errors.is_empty() {
                return Err(MergeError::import(
                    "MERGE_CLEANUP_UNCERTAIN",
                    error.message,
                    Some(&output),
                    "Uncertain",
                    statement_count,
                    byte_count,
                    None,
                    cleanup_errors,
                    Some(error.class),
                ));
            }
            Err(MergeError::import(
                error.class,
                error.message,
                Some(&output),
                "Removed",
                statement_count,
                byte_count,
                None,
                Vec::new(),
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_canonical_and_lossless_for_supported_values() {
        assert_eq!(value_literal(ValueRef::Integer(4)), Ok("4".to_owned()));
        assert_eq!(value_literal(ValueRef::Real(-0.0)), Ok("-0.0".to_owned()));
        assert_eq!(
            value_literal(ValueRef::Text(b"a'b")),
            Ok("'a''b'".to_owned())
        );
        assert_eq!(
            value_literal(ValueRef::Blob(&[0, 255])),
            Ok("X'00FF'".to_owned())
        );
    }

    #[test]
    fn split_statements_honors_literals_comments_and_bracket_identifiers() {
        let text = "-- sqlite-mcp merge-format: 1\nCREATE TABLE [a;b](x TEXT); INSERT INTO [a;b] VALUES ('a;b');\n";
        let ranges = split_statements(text, FORMAT_HEADER.len()).unwrap();
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn output_owner_does_not_remove_replaced_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.sqlite");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let owner = ExclusiveOutputOwner::capture(&path, &file).unwrap();
        drop(file);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(owner.remove_if_owned().is_err());
        assert!(path.exists());
    }

    #[test]
    fn manifest_limit_is_fixed() {
        assert_eq!(MANIFEST_BYTE_LIMIT, 1024 * 1024);
    }
}
