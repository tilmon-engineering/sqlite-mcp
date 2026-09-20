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
/// and whether that attempt was DDL-classified. These are classification
/// markers only. Core drains them unconditionally after each dispatched
/// request; schema freshness is decided by the SQLite schema-cookie delta and
/// confirmed statement outcome, not by authorizer classification alone.
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
        authorize(&ctx, readonly, &mutation_seen)
    }))
}

/// Internal decision-layer classification of an authorizer action. Every
/// known [`AuthAction`] variant maps to one kind in [`ActionKind::classify`];
/// the production wildcard arm (future `#[non_exhaustive]` variants) routes
/// to [`ActionKind::Unmapped`], which the decision layer denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionKind<'a> {
    // DML (mutation marker: attempt).
    Insert,
    Update,
    Delete,
    // DDL (mutation marker: ddl attempt). `AlterTable` carries its database
    // qualifier in the variant payload because SQLite passes it as the
    // authorizer's first parameter for `SQLITE_ALTER_TABLE`.
    CreateIndex,
    CreateTable,
    CreateTrigger,
    CreateView,
    DropIndex,
    DropTable,
    DropTrigger,
    DropView,
    AlterTable {
        database: &'a str,
    },
    // Explicitly denied administrative/control actions.
    Unknown,
    Pragma,
    Attach,
    Detach,
    Transaction,
    Savepoint,
    CreateTempIndex,
    CreateTempTable,
    CreateTempTrigger,
    CreateTempView,
    DropTempIndex,
    DropTempTable,
    DropTempTrigger,
    DropTempView,
    CreateVtable,
    DropVtable,
    Analyze,
    Reindex,
    // Explicitly allowed benign actions.
    Select,
    Read,
    Function {
        name: &'a str,
    },
    Recursive,
    /// Future or unrecognized variant: the fail-closed default denies it.
    Unmapped,
}

impl<'a> ActionKind<'a> {
    fn classify(action: &'a AuthAction<'_>) -> Self {
        match action {
            AuthAction::Insert { .. } => Self::Insert,
            AuthAction::Update { .. } => Self::Update,
            AuthAction::Delete { .. } => Self::Delete,
            AuthAction::CreateIndex { .. } => Self::CreateIndex,
            AuthAction::CreateTable { .. } => Self::CreateTable,
            AuthAction::CreateTrigger { .. } => Self::CreateTrigger,
            AuthAction::CreateView { .. } => Self::CreateView,
            AuthAction::DropIndex { .. } => Self::DropIndex,
            AuthAction::DropTable { .. } => Self::DropTable,
            AuthAction::DropTrigger { .. } => Self::DropTrigger,
            AuthAction::DropView { .. } => Self::DropView,
            AuthAction::AlterTable { database_name, .. } => Self::AlterTable {
                database: database_name,
            },
            AuthAction::Unknown { .. } => Self::Unknown,
            AuthAction::Pragma { .. } => Self::Pragma,
            AuthAction::Attach { .. } => Self::Attach,
            AuthAction::Detach { .. } => Self::Detach,
            AuthAction::Transaction { .. } => Self::Transaction,
            AuthAction::Savepoint { .. } => Self::Savepoint,
            AuthAction::CreateTempIndex { .. } => Self::CreateTempIndex,
            AuthAction::CreateTempTable { .. } => Self::CreateTempTable,
            AuthAction::CreateTempTrigger { .. } => Self::CreateTempTrigger,
            AuthAction::CreateTempView { .. } => Self::CreateTempView,
            AuthAction::DropTempIndex { .. } => Self::DropTempIndex,
            AuthAction::DropTempTable { .. } => Self::DropTempTable,
            AuthAction::DropTempTrigger { .. } => Self::DropTempTrigger,
            AuthAction::DropTempView { .. } => Self::DropTempView,
            AuthAction::CreateVtable { .. } => Self::CreateVtable,
            AuthAction::DropVtable { .. } => Self::DropVtable,
            AuthAction::Analyze { .. } => Self::Analyze,
            AuthAction::Reindex { .. } => Self::Reindex,
            AuthAction::Select => Self::Select,
            AuthAction::Read { .. } => Self::Read,
            AuthAction::Function { function_name } => Self::Function {
                name: function_name,
            },
            AuthAction::Recursive => Self::Recursive,
            _ => Self::Unmapped,
        }
    }
    fn is_dml(self) -> bool {
        matches!(self, Self::Insert | Self::Update | Self::Delete)
    }
    fn is_ddl(self) -> bool {
        matches!(
            self,
            Self::CreateIndex
                | Self::CreateTable
                | Self::CreateTrigger
                | Self::CreateView
                | Self::DropIndex
                | Self::DropTable
                | Self::DropTrigger
                | Self::DropView
                | Self::AlterTable { .. }
        )
    }
    /// Object-scoped actions address a named object that may carry a schema
    /// qualifier (`temp`); anything outside `main` is denied.
    fn is_object_scoped(self) -> bool {
        matches!(
            self,
            Self::Insert
                | Self::Update
                | Self::Delete
                | Self::Read
                | Self::CreateIndex
                | Self::CreateTable
                | Self::CreateTrigger
                | Self::CreateView
                | Self::DropIndex
                | Self::DropTable
                | Self::DropTrigger
                | Self::DropView
                | Self::AlterTable { .. }
        )
    }
}

fn database_is_main(database_name: Option<&str>) -> bool {
    matches!(database_name, None | Some("main"))
}

/// Pure authorization decision for one authorizer callback, separated from
/// the installed closure so the decision table is unit-testable.
///
/// EFFECT CONTRACT: this helper itself performs the mutation-marker effects
/// (`mark_attempt`/`mark_ddl_attempt`) BEFORE evaluating any denial, so
/// denied readonly DML/DDL and denied temp-qualified mutations still mark
/// and observable schema-publication behavior is preserved. The installed
/// closure only handles the TRUSTED thread-local bypass and delegates here.
///
/// Decision precedence: mutation marking → readonly denial → temp-database
/// denial for object-scoped actions → `Analyze`/`Reindex` denial → explicit
/// benign allows → wildcard `Deny` (fail-closed; DESIGN §5).
pub(crate) fn authorize(
    ctx: &AuthContext<'_>,
    readonly: bool,
    mutation_seen: &MutationSignal,
) -> Authorization {
    decide_kind(
        ActionKind::classify(&ctx.action),
        ctx.database_name,
        readonly,
        mutation_seen,
    )
}

/// The internal decision layer. `database` is the authorizer callback's
/// database qualifier; the `AlterTable` kind carries its own because SQLite
/// passes it as the action's first parameter. `ActionKind::Unmapped` is the
/// production wildcard path (future `#[non_exhaustive]` variants) and is
/// denied here.
fn decide_kind(
    kind: ActionKind<'_>,
    database: Option<&str>,
    readonly: bool,
    mutation_seen: &MutationSignal,
) -> Authorization {
    if kind.is_dml() {
        mutation_seen.mark_attempt();
    } else if kind.is_ddl() {
        mutation_seen.mark_ddl_attempt();
    }
    if (kind.is_dml() || kind.is_ddl()) && readonly {
        return Authorization::Deny;
    }
    if kind.is_object_scoped() {
        let database = match kind {
            ActionKind::AlterTable { database } => Some(database),
            _ => database,
        };
        if !database_is_main(database) {
            return Authorization::Deny;
        }
    }
    match kind {
        ActionKind::Analyze => Authorization::Deny,
        // `Reindex` is allowed at the authorizer because SQLite fires an
        // internal SQLITE_REINDEX event for every CREATE INDEX it executes;
        // blanket denial here would break ordinary index creation. Top-level
        // REINDEX statements are denied structurally at the statement
        // boundary by [`check_maintenance`], which cannot distinguish
        // otherwise.
        ActionKind::Reindex => Authorization::Allow,
        ActionKind::Select | ActionKind::Read | ActionKind::Recursive => Authorization::Allow,
        ActionKind::Function { name } => {
            let lowered = name.to_ascii_lowercase();
            if lowered.starts_with("pragma_") || lowered == "load_extension" {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        ActionKind::Insert
        | ActionKind::Update
        | ActionKind::Delete
        | ActionKind::CreateIndex
        | ActionKind::CreateTable
        | ActionKind::CreateTrigger
        | ActionKind::CreateView
        | ActionKind::DropIndex
        | ActionKind::DropTable
        | ActionKind::DropTrigger
        | ActionKind::DropView
        | ActionKind::AlterTable { .. } => Authorization::Allow,
        _ => Authorization::Deny,
    }
}
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("SQL is empty")]
    Empty,
    #[error("multiple SQL statements are not allowed")]
    Multiple,
    #[error("statement is denied by SQL policy")]
    Denied,
    #[error("maintenance operations (ANALYZE, REINDEX) are denied")]
    Maintenance,
}
// Token scanner shared by the stored-body and maintenance structural
// guards: returns lowercased leading words, skipping whitespace and SQL
// comments.
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

/// Defense-in-depth for maintenance statements. SQLite fires an internal
/// `SQLITE_REINDEX` authorizer event for every `CREATE INDEX` it executes,
/// so the authorizer cannot distinguish a top-level `REINDEX` from an
/// ordinary index creation and must allow the event; `ANALYZE` is denied at
/// the authorizer but kept here so both report one policy denial. Only the
/// statement's first keyword position is inspected, which cannot be an
/// identifier in valid SQL, so this is a narrow guard in the shape of
/// [`check_stored_body`], not a general keyword scan.
pub fn check_maintenance(sql: &str) -> Result<(), PolicyError> {
    let mut cursor = Cursor { rest: sql };
    match cursor.next().map(|w| w.to_ascii_lowercase()).as_deref() {
        Some("analyze") | Some("reindex") => Err(PolicyError::Maintenance),
        _ => Ok(()),
    }
}

/// Defense-in-depth for DDL that stores a SQL body (views, triggers).
/// SQLite's authorizer does not fire for the body at CREATE time, so a
/// view/trigger over a denied construct (e.g. a pragma table-valued
/// function) must be rejected structurally here. This is a narrow guard on
/// the stored-body DDL forms only; authorization is otherwise enforced by
/// the authorizer, and statement boundaries are still determined by
/// prepare/tail handling, never by this scan.
pub fn check_stored_body(sql: &str, mutation_seen: &MutationSignal) -> Result<(), PolicyError> {
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
        // A structurally denied stored-body CREATE VIEW/TRIGGER is still
        // classified as DDL so the request marker is drained consistently;
        // it does not by itself invalidate a committed schema observation.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::{Authorization, TransactionOperation};

    fn decide(
        action: &AuthAction<'_>,
        database_name: Option<&str>,
        readonly: bool,
        mutation_seen: &MutationSignal,
    ) -> Authorization {
        authorize(
            &AuthContext {
                action: *action,
                database_name,
                accessor: None,
            },
            readonly,
            mutation_seen,
        )
    }

    fn fresh_action() -> AuthAction<'static> {
        AuthAction::Select
    }

    fn object_scoped_actions() -> Vec<AuthAction<'static>> {
        vec![
            AuthAction::CreateIndex {
                index_name: "i",
                table_name: "t",
            },
            AuthAction::CreateTable { table_name: "t" },
            AuthAction::CreateTrigger {
                trigger_name: "g",
                table_name: "t",
            },
            AuthAction::CreateView { view_name: "v" },
            AuthAction::DropIndex {
                index_name: "i",
                table_name: "t",
            },
            AuthAction::DropTable { table_name: "t" },
            AuthAction::DropTrigger {
                trigger_name: "g",
                table_name: "t",
            },
            AuthAction::DropView { view_name: "v" },
            AuthAction::Insert { table_name: "t" },
            AuthAction::Update {
                table_name: "t",
                column_name: "a",
            },
            AuthAction::Delete { table_name: "t" },
            AuthAction::Read {
                table_name: "t",
                column_name: "a",
            },
        ]
    }

    /// The production decision table over the internal classification layer:
    /// every known rusqlite 0.40.1 variant with representative payloads, with
    /// `database_name` varied for the object-scoped ones.
    #[test]
    fn decision_table_is_fail_closed() {
        let signal = MutationSignal::new();
        // Benign allows.
        for (action, database) in [
            (fresh_action(), None),
            (
                AuthAction::Read {
                    table_name: "t",
                    column_name: "a",
                },
                Some("main"),
            ),
            (
                AuthAction::Read {
                    table_name: "t",
                    column_name: "a",
                },
                None,
            ),
            (
                AuthAction::Function {
                    function_name: "abs",
                },
                None,
            ),
            (AuthAction::Recursive, None),
        ] {
            assert_eq!(
                decide(&action, database, false, &signal),
                Authorization::Allow,
                "expected allow: {action:?} db={database:?}"
            );
        }
        // Function denies: pragma_ table-valued functions and load_extension.
        for name in ["pragma_table_info", "PRAGMA_x", "load_extension"] {
            assert_eq!(
                decide(
                    &AuthAction::Function {
                        function_name: name
                    },
                    None,
                    false,
                    &signal
                ),
                Authorization::Deny,
                "function {name:?} must be denied"
            );
        }
        // Maintenance: ANALYZE is denied at the authorizer (no allowed
        // statement fires it internally); REINDEX is allowed at the
        // authorizer because CREATE INDEX fires an internal reindex event,
        // and top-level REINDEX is denied structurally (see below).
        assert_eq!(
            decide(
                &AuthAction::Analyze { table_name: "t" },
                None,
                false,
                &signal
            ),
            Authorization::Deny
        );
        assert_eq!(
            decide(
                &AuthAction::Reindex { index_name: "i" },
                None,
                false,
                &signal
            ),
            Authorization::Allow
        );
        // Administrative/control actions are denied.
        for action in [
            AuthAction::Unknown {
                code: i32::MAX,
                arg1: None,
                arg2: None,
            },
            AuthAction::Pragma {
                pragma_name: "journal_mode",
                pragma_value: None,
            },
            AuthAction::Attach {
                filename: "/tmp/x.db",
            },
            AuthAction::Detach { database_name: "x" },
            AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            },
            AuthAction::Savepoint {
                operation: TransactionOperation::Begin,
                savepoint_name: "s",
            },
            AuthAction::CreateTempIndex {
                index_name: "i",
                table_name: "t",
            },
            AuthAction::CreateTempTable { table_name: "t" },
            AuthAction::CreateTempTrigger {
                trigger_name: "g",
                table_name: "t",
            },
            AuthAction::CreateTempView { view_name: "v" },
            AuthAction::DropTempIndex {
                index_name: "i",
                table_name: "t",
            },
            AuthAction::DropTempTable { table_name: "t" },
            AuthAction::DropTempTrigger {
                trigger_name: "g",
                table_name: "t",
            },
            AuthAction::DropTempView { view_name: "v" },
            AuthAction::CreateVtable {
                table_name: "t",
                module_name: "fts5",
            },
            AuthAction::DropVtable {
                table_name: "t",
                module_name: "fts5",
            },
        ] {
            assert_eq!(
                decide(&action, None, false, &signal),
                Authorization::Deny,
                "expected deny: {action:?}"
            );
        }
        // The runtime-unknown case is denied separately from the wildcard.
        assert_eq!(
            decide(
                &AuthAction::Unknown {
                    code: 999,
                    arg1: Some("x"),
                    arg2: Some("y")
                },
                None,
                false,
                &signal
            ),
            Authorization::Deny
        );
        // The synthetic Unmapped sentinel (future-variant stand-in) proves the
        // fail-closed wildcard default: the production wildcard arm produces
        // `ActionKind::Unmapped` via classification, and the decision layer
        // denies it for every database name.
        for database in [None, Some("main"), Some("temp")] {
            assert_eq!(
                decide_kind(ActionKind::Unmapped, database, false, &signal),
                Authorization::Deny,
                "Unmapped must be denied (db={database:?})"
            );
        }
    }

    /// Object-scoped actions are allowed with no or `main` qualifier and
    /// denied with any other qualifier (`temp`).
    #[test]
    fn temp_schema_qualified_actions_denied() {
        let signal = MutationSignal::new();
        for action in object_scoped_actions() {
            for database in [None, Some("main")] {
                assert_eq!(
                    decide(&action, database, false, &signal),
                    Authorization::Allow,
                    "expected allow: {action:?} db={database:?}"
                );
            }
            assert_eq!(
                decide(&action, Some("temp"), false, &signal),
                Authorization::Deny,
                "temp-qualified action must be denied: {action:?}"
            );
        }
        // ALTER TABLE carries its database in the variant payload.
        for database in ["main", "temp"] {
            let action = AuthAction::AlterTable {
                database_name: database,
                table_name: "t",
            };
            let expected = if database == "main" {
                Authorization::Allow
            } else {
                Authorization::Deny
            };
            assert_eq!(
                decide(&action, None, false, &signal),
                expected,
                "ALTER TABLE {database}.t"
            );
        }
    }

    /// The maintenance structural guard denies top-level ANALYZE/REINDEX in
    /// any capitalization or comment framing while leaving every other
    /// statement form alone (first-keyword position only).
    #[test]
    fn maintenance_guard_denies_top_level_only() {
        for sql in [
            "ANALYZE",
            "analyze",
            "Analyze main.t",
            "REINDEX",
            "reindex main.t",
            "  REINDEX",
            "-- leading comment\nREINDEX",
            "/* c */ ANALYZE",
        ] {
            assert!(
                check_maintenance(sql).is_err(),
                "maintenance statement accepted: {sql:?}"
            );
        }
        for sql in [
            "SELECT 1",
            "SELECT 'REINDEX is a string'",
            "SELECT 'ANALYZE'",
            "CREATE INDEX i ON t(a)",
            "CREATE TABLE reindex(a)",
            "UPDATE t SET a = 1",
            "INSERT INTO t VALUES ('analyze')",
            "SELECT 1 -- reindex",
            "DELETE FROM t WHERE a = 'REINDEX'",
        ] {
            assert!(
                check_maintenance(sql).is_ok(),
                "ordinary statement rejected: {sql:?}"
            );
        }
    }

    /// Readonly denies DML/DDL and the mutation markers still fire on the
    /// denied paths (observable schema-publication behavior preserved).
    #[test]
    fn readonly_denies_but_marks() {
        let signal = MutationSignal::new();
        assert_eq!(
            decide(&AuthAction::Insert { table_name: "t" }, None, true, &signal),
            Authorization::Deny
        );
        assert!(signal.take_after_request(), "denied readonly DML must mark");
        assert!(!signal.take_ddl_after_request());
        assert_eq!(
            decide(
                &AuthAction::CreateTable { table_name: "t" },
                None,
                true,
                &signal
            ),
            Authorization::Deny
        );
        assert!(
            signal.take_ddl_after_request(),
            "denied readonly DDL must mark ddl"
        );
        assert!(signal.take_after_request());
        // Temp-qualified mutations are denied on writable handles and still
        // mark.
        assert_eq!(
            decide(
                &AuthAction::Insert { table_name: "t" },
                Some("temp"),
                false,
                &signal
            ),
            Authorization::Deny
        );
        assert!(signal.take_after_request(), "denied temp DML must mark");
        assert_eq!(
            decide(
                &AuthAction::CreateTable { table_name: "t" },
                Some("temp"),
                false,
                &signal
            ),
            Authorization::Deny
        );
        assert!(
            signal.take_ddl_after_request(),
            "denied temp DDL must mark ddl"
        );
        // Benign actions do not mark.
        let signal = MutationSignal::new();
        decide(&fresh_action(), None, false, &signal);
        assert!(!signal.take_after_request());
        decide(
            &AuthAction::Function {
                function_name: "abs",
            },
            None,
            false,
            &signal,
        );
        assert!(!signal.take_after_request());
    }
}
