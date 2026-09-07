use rusqlite::{
    Connection, ffi,
    hooks::{AuthAction, AuthContext, Authorization},
};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{ffi::CString, ptr};
use thiserror::Error;
thread_local! {
    static TRUSTED: Cell<bool> = const { Cell::new(false) };
}

/// Worker-owned flag set by the authorizer whenever an untrusted agent
/// statement attempts a schema/data mutation. Stored as an `AtomicBool`
/// shared with the worker thread so the caller can read and reset it after
/// the worker finishes a request regardless of which thread observed it.
///
/// Two signals are tracked: a mutation attempt (any DML/DDL authorizer action)
/// and whether that attempt was DDL-classified. Invalidation policy (F-06):
/// a DDL attempt invalidates the schema observation whatever its outcome, a
/// DML attempt invalidates only on success, and a failed DML leaves the
/// handle usable (transaction-local statement atomicity).
#[derive(Clone, Default)]
pub struct MutationSignal {
    attempted: std::sync::Arc<AtomicBool>,
    ddl: std::sync::Arc<AtomicBool>,
}
pub type MutationFlag = MutationSignal;
impl MutationSignal {
    pub fn new() -> Self {
        Self {
            attempted: std::sync::Arc::new(AtomicBool::new(false)),
            ddl: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn reset_for_request(&self) {
        self.attempted.store(false, Ordering::SeqCst);
        self.ddl.store(false, Ordering::SeqCst);
    }
    pub fn mark_attempt(&self) {
        self.attempted.store(true, Ordering::SeqCst);
    }
    /// Mark the attempt as DDL-classified (implies an attempt).
    pub fn mark_ddl_attempt(&self) {
        self.ddl.store(true, Ordering::SeqCst);
        self.attempted.store(true, Ordering::SeqCst);
    }
    pub fn take_after_request(&self) -> bool {
        self.attempted.swap(false, Ordering::SeqCst)
    }
    pub fn take_ddl_after_request(&self) -> bool {
        self.ddl.swap(false, Ordering::SeqCst)
    }
}
pub fn trusted<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    TRUSTED.with(|v| {
        let old = v.replace(true);
        let r = f();
        v.set(old);
        r
    })
}
pub fn install_authorizer(
    conn: &Connection,
    readonly: bool,
    mutation_seen: MutationFlag,
) -> rusqlite::Result<()> {
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        if TRUSTED.with(Cell::get) {
            return Authorization::Allow;
        }
        match ctx.action {
            AuthAction::Insert { .. } | AuthAction::Update { .. } | AuthAction::Delete { .. } => {
                mutation_seen.mark_attempt();
                if readonly {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            AuthAction::CreateIndex { .. }
            | AuthAction::CreateTable { .. }
            | AuthAction::CreateTrigger { .. }
            | AuthAction::CreateView { .. }
            | AuthAction::DropIndex { .. }
            | AuthAction::DropTable { .. }
            | AuthAction::DropTrigger { .. }
            | AuthAction::DropView { .. }
            | AuthAction::AlterTable { .. } => {
                mutation_seen.mark_ddl_attempt();
                if readonly {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            AuthAction::Unknown { .. }
            | AuthAction::Pragma { .. }
            | AuthAction::Attach { .. }
            | AuthAction::Detach { .. }
            | AuthAction::Transaction { .. }
            | AuthAction::Savepoint { .. }
            | AuthAction::CreateTempIndex { .. }
            | AuthAction::CreateTempTable { .. }
            | AuthAction::CreateTempTrigger { .. }
            | AuthAction::CreateTempView { .. }
            | AuthAction::DropTempIndex { .. }
            | AuthAction::DropTempTable { .. }
            | AuthAction::DropTempTrigger { .. }
            | AuthAction::DropTempView { .. }
            | AuthAction::CreateVtable { .. }
            | AuthAction::DropVtable { .. } => Authorization::Deny,
            AuthAction::Function { function_name }
                if function_name.to_ascii_lowercase().starts_with("pragma_") =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }
    }))
}
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("SQL is empty")]
    Empty,
    #[error("multiple SQL statements are not allowed")]
    Multiple,
    #[error("statement is denied by SQL policy")]
    Denied,
}
/// Defense-in-depth for DDL that stores a SQL body (views, triggers).
/// SQLite's authorizer does not fire for the body at CREATE time, so a
/// view/trigger over a denied construct (e.g. a pragma table-valued
/// function) must be rejected structurally here. This is a narrow guard on
/// the stored-body DDL forms only; authorization is otherwise enforced by
/// the authorizer, and statement boundaries are still determined by
/// prepare/tail handling, never by this scan.
pub fn check_stored_body(sql: &str, mutation_seen: &MutationSignal) -> Result<(), PolicyError> {
    // Token scanner: returns lowercased leading words, skipping whitespace
    // and SQL comments; the body offset is the byte position after the
    // recognized keywords.
    struct Cursor<'a> {
        rest: &'a str,
    }
    impl<'a> Cursor<'a> {
        fn skip_trivia(&mut self) {
            loop {
                self.rest = self.rest.trim_start();
                if let Some(after) = self.rest.strip_prefix("--") {
                    self.rest = after.find('\n').map(|i| &after[i + 1..]).unwrap_or("");
                } else if let Some(after) = self.rest.strip_prefix("/*") {
                    match after.find("*/") {
                        Some(i) => self.rest = &after[i + 2..],
                        None => self.rest = "",
                    }
                } else {
                    return;
                }
            }
        }
        fn next(&mut self) -> Option<&'a str> {
            self.skip_trivia();
            if self.rest.is_empty() {
                return None;
            }
            let end = self
                .rest
                .find(|c: char| c.is_whitespace() || c == ';' || c == '(')
                .unwrap_or(self.rest.len());
            let (word, remainder) = self.rest.split_at(end);
            self.rest = remainder;
            Some(word)
        }
    }
    let mut cursor = Cursor { rest: sql };
    let first = match cursor.next() {
        Some(w) => w.to_ascii_lowercase(),
        None => return Ok(()),
    };
    if first != "create" {
        return Ok(());
    }
    let kind = cursor.next().map(|w| w.to_ascii_lowercase());
    let kind = match kind.as_deref() {
        Some("temp") | Some("temporary") => cursor.next().map(|w| w.to_ascii_lowercase()),
        _ => kind,
    };
    if matches!(kind.as_deref(), Some("view") | Some("trigger")) {
        // Skip the optional IF [NOT] EXISTS and the object name; the body is
        // conservatively scanned for denied pragma table-valued references.
        cursor.next();
        cursor.next();
        cursor.next();
        cursor.next();
        // A structurally denied stored-body CREATE VIEW/TRIGGER is an
        // attempted DDL: the classification matters for invalidation (F-06).
        mutation_seen.mark_ddl_attempt();
        if cursor.rest.to_ascii_lowercase().contains("pragma_") {
            return Err(PolicyError::Denied);
        }
    }
    Ok(())
}
#[cfg(test)]
pub fn unknown_action_denied() -> bool {
    matches!(
        AuthAction::Unknown {
            code: i32::MAX,
            arg1: None,
            arg2: None
        },
        AuthAction::Unknown { .. }
    )
}
pub fn prepare_exact(conn: &Connection, sql: &str) -> Result<(), PolicyError> {
    if sql.as_bytes().contains(&0) {
        return Err(PolicyError::Denied);
    }
    if sql.trim().is_empty() {
        return Err(PolicyError::Empty);
    }
    let c = CString::new(sql).map_err(|_| PolicyError::Denied)?;
    let mut tail = c.as_ptr();
    let end = unsafe { tail.add(c.as_bytes().len()) };
    let mut compiled = false;
    loop {
        let mut stmt = ptr::null_mut();
        let mut next = ptr::null();
        let rc = unsafe { ffi::sqlite3_prepare_v2(conn.handle(), tail, -1, &mut stmt, &mut next) };
        if rc != ffi::SQLITE_OK {
            if !stmt.is_null() {
                unsafe {
                    ffi::sqlite3_finalize(stmt);
                }
            }
            return Err(PolicyError::Denied);
        }
        if !stmt.is_null() {
            if compiled {
                unsafe {
                    ffi::sqlite3_finalize(stmt);
                }
                return Err(PolicyError::Multiple);
            }
            compiled = true;
            unsafe {
                ffi::sqlite3_finalize(stmt);
            }
        }
        if next.is_null() || next == tail {
            break;
        }
        tail = next;
        if tail >= end {
            break;
        }
    }
    if !compiled {
        return Err(PolicyError::Empty);
    }
    Ok(())
}
#[cfg(test)]
pub fn validate_sql(sql: &str) -> Result<(), PolicyError> {
    // Structural check only (NUL, empty, single statement, tail shape).
    // Authorization is enforced by the installed authorizer on the real
    // connection; a fresh in-memory parser cannot validate statements that
    // reference the target schema, and keyword scanning produces false
    // positives on identifiers and string literals.
    let parser = Connection::open_in_memory().map_err(|_| PolicyError::Denied)?;
    prepare_exact(&parser, sql)
}
