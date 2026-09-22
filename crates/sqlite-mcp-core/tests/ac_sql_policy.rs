use sqlite_mcp_core::{Cell, Config, Core};
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, Core, String, String) {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("db.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    core.get_schema(&h.id).await.unwrap();
    core.begin_transaction(&h.id, "deferred").await.unwrap();
    core.query(&h.id, "CREATE TABLE t(a)", &[]).await.unwrap();
    (dir, core, path, h.id)
}

#[tokio::test]
async fn sql_policy_escape_matrix() {
    let (_dir, core, path, id) = setup().await;
    for sql in [
        "ATTACH DATABASE '/tmp/x' AS x",
        "DETACH x",
        "PRAGMA journal_mode",
        "PRAGMA writable_schema=1",
        "SELECT * FROM pragma_table_info('t')",
        "VACUUM",
        "VACUUM INTO '/tmp/v.db'",
        "COMMIT",
        "BEGIN",
        "ROLLBACK",
        "SAVEPOINT foo",
        "RELEASE foo",
        "CREATE VIRTUAL TABLE vt USING fts5(a)",
        "CREATE TEMP TABLE tmp(a)",
        "SELECT load_extension('x')",
        "EXPLAIN PRAGMA journal_mode",
        "CREATE VIEW pv AS SELECT * FROM pragma_table_info('t')",
        "CREATE TRIGGER pt AFTER INSERT ON t BEGIN PRAGMA user_version; END",
    ] {
        assert!(
            core.query(&id, sql, &[]).await.is_err(),
            "policy accepted {sql}"
        );
    }
    // Denied DDL does not change the schema cookie or the committed
    // observation, so the transaction remains usable.
    let ok = core.query(&id, "EXPLAIN QUERY PLAN SELECT 1", &[]).await;
    assert!(ok.is_ok());
    assert!(
        core.query(&id, "SELECT 1; DROP TABLE t", &[])
            .await
            .is_err()
    );
    assert!(core.query(&id, "", &[]).await.is_err());
    assert!(core.query(&id, "SELECT\0 1", &[]).await.is_err());
    core.rollback(&id).await.unwrap();
    assert!(!std::path::Path::new("/tmp/v.db").exists());
    assert!(path.ends_with("db.sqlite"));
    core.shutdown().await;
}

fn assert_integer_one(result: &sqlite_mcp_core::QueryResult) {
    assert_eq!(result.rows.len(), 1, "expected one row: {result:?}");
    assert_eq!(result.rows[0].len(), 1, "expected one cell: {result:?}");
    assert!(
        matches!(&result.rows[0][0], Cell::Integer(value) if value == "1"),
        "expected integer 1: {result:?}"
    );
}

#[tokio::test]
async fn approved_connection_pragma_getters_return_one() {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core
        .create_database(dir.path().join("pragma.sqlite").to_str().unwrap())
        .await
        .unwrap();
    for readonly in [false, true] {
        let handle = core.open_database(&path, readonly).await.unwrap();
        core.get_schema(&handle.id).await.unwrap();
        core.begin_transaction(&handle.id, "deferred")
            .await
            .unwrap();
        for sql in [
            "PRAGMA foreign_keys",
            "pragma recursive_triggers;",
            " /* leading */ PRAGMA\nforeign_keys /* trailing */ ; -- done\n",
        ] {
            assert_integer_one(&core.query(&handle.id, sql, &[]).await.unwrap());
        }
        core.rollback(&handle.id).await.unwrap();
        core.close_database(&handle.id).await.unwrap();
    }
    core.shutdown().await;
}

#[tokio::test]
async fn approved_pragma_setters_and_noncanonical_sources_remain_denied() {
    let (_dir, core, _path, id) = setup().await;
    for sql in [
        "PRAGMA foreign_keys=ON",
        "PRAGMA recursive_triggers(1)",
        "PRAGMA main.foreign_keys",
        "PRAGMA \"foreign_keys\"",
        "PRAGMA /* split */ foreign_keys",
        "EXPLAIN PRAGMA foreign_keys",
        "PRAGMA user_version",
        "PRAGMA journal_mode",
        "SELECT * FROM pragma_foreign_keys",
    ] {
        let error = core.query(&id, sql, &[]).await.expect_err(sql);
        assert!(
            matches!(error, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
            "expected typed policy denial for {sql}, got {error:?}"
        );
        assert_integer_one(&core.query(&id, "PRAGMA foreign_keys", &[]).await.unwrap());
    }
    for sql in [
        "PRAGMA foreign_keys()",
        "PRAG/**/MA foreign_keys",
        "PRAGMA foreign_keys garbage",
        "PRAGMA journal_mode garbage",
        "PRAGMA writable_schema garbage",
        "CREATE VIEW malformed AS SELECT * FROM pragma_table_info('t') WHERE (",
        "CREATE TRIGGER malformed AFTER INSERT ON t BEGIN SELECT * FROM pragma_table_info('t'); END garbage",
        "ANALYZE garbage extra",
        "REINDEX garbage extra",
    ] {
        let malformed = core
            .query(&id, sql, &[])
            .await
            .expect_err("malformed pragma must fail");
        assert!(
            !matches!(malformed, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
            "malformed SQL must retain its SQLite class for {sql}: {malformed:?}"
        );
        core.query(&id, "SELECT 1", &[])
            .await
            .expect("transaction must recover after malformed SQL");
    }
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn valid_compound_trigger_and_second_statement_boundary_are_preserved() {
    let (_dir, core, _path, id) = setup().await;
    core.query(
        &id,
        "CREATE TRIGGER compound AFTER INSERT ON t BEGIN UPDATE t SET a = a; SELECT 1; END;",
        &[],
    )
    .await
    .unwrap();
    for sql in [
        "SELECT 1; PRAGMA foreign_keys",
        "PRAGMA foreign_keys; SELECT 1",
        "PRAGMA recursive_triggers; SELECT 1",
    ] {
        let error = core
            .query(&id, sql, &[])
            .await
            .expect_err("second top-level statement must be rejected");
        assert!(
            !matches!(error, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
            "multiple statements must retain the boundary class: {error:?}"
        );
        core.query(&id, "SELECT 1", &[])
            .await
            .expect("transaction must recover after multiple statements");
    }
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn schema_qualified_temp_objects_denied() {
    let (_dir, core, _path, id) = setup().await;
    for sql in ["CREATE TABLE temp.t2(a)", "CREATE VIEW temp.v AS SELECT 1"] {
        let err = core
            .query(&id, sql, &[])
            .await
            .err()
            .unwrap_or_else(|| panic!("temp-schema create accepted: {sql}"));
        assert!(
            matches!(err, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
            "expected typed policy denial for {sql}, got: {err:?}"
        );
    }
    // `CREATE INDEX temp.i` never reaches the authorizer: SQLite
    // structurally rejects a TEMP index on a non-TEMP table. The
    // authorizer-level temp denial for CreateIndex is pinned by the
    // policy unit decision table, while this parser rejection must not be
    // relabeled as a policy denial.
    let temp_index = core
        .query(&id, "CREATE INDEX temp.i ON t(a)", &[])
        .await
        .expect_err("TEMP index on a non-TEMP table must be rejected");
    assert!(
        !matches!(temp_index, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
        "SQLite parser rejection must retain its non-policy class: {temp_index:?}"
    );
    // Controls: unqualified and `main.`-qualified creates succeed on the
    // writable handle.
    core.query(&id, "CREATE TABLE plain_t2(a)", &[])
        .await
        .expect("unqualified create");
    core.query(&id, "CREATE TABLE main.t2(a)", &[])
        .await
        .expect("main-qualified create");
    core.query(&id, "CREATE INDEX main.i ON t(a)", &[])
        .await
        .expect("main-qualified index");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn analyze_and_reindex_denied() {
    let (_dir, core, _path, id) = setup().await;
    // REINDEX only fires the authorizer when an index exists to reindex.
    core.query(&id, "CREATE INDEX i ON t(a)", &[])
        .await
        .expect("setup index");
    for sql in [
        "ANALYZE",
        "REINDEX main.t",
        "REINDEX",
        // Adjacent-comment spellings are valid SQLite and must be denied
        // before execution (the authorizer cannot catch REINDEX).
        "REINDEX/**/i",
        "REINDEX/*c*/i",
        "REINDEX/**/",
        "ANALYZE/**/",
        "ANALYZE/*c*/main.t",
        // EXPLAIN is a diagnostic prefix: the underlying maintenance
        // statement is still denied (R2-2).
        "EXPLAIN REINDEX",
        "EXPLAIN/**/REINDEX",
        "EXPLAIN QUERY PLAN REINDEX",
        "EXPLAIN QUERY PLAN ANALYZE",
    ] {
        let err = core
            .query(&id, sql, &[])
            .await
            .err()
            .unwrap_or_else(|| panic!("maintenance operation accepted: {sql}"));
        assert!(
            matches!(err, sqlite_mcp_core::CoreError::PolicyDenied { .. }),
            "expected maintenance policy denial for {sql}, got: {err}"
        );
    }
    // No effect: the ordinary read path still works and the transaction
    // remains usable.
    core.query(&id, "SELECT count(*) FROM t", &[])
        .await
        .expect("read after denied maintenance");
    core.query(&id, "EXPLAIN QUERY PLAN SELECT 1", &[])
        .await
        .expect("EXPLAIN QUERY PLAN of an allowed statement stays allowed");
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn stored_body_guard_accepts_pragma_named_identifiers() {
    // Quoted identifiers (and string literals) containing `pragma_` cannot
    // trigger the stored-body structural denial (R2-1).
    let (_dir, core, _path, id) = setup().await;
    core.query(&id, "CREATE VIEW \"a-b-pragma_x\" AS SELECT 1", &[])
        .await
        .expect("quoted view name containing pragma_ must be allowed");
    core.query(&id, "CREATE VIEW \"weird-name\" AS SELECT 1", &[])
        .await
        .expect("quoted view name with punctuation must be allowed");
    core.query(
        &id,
        "CREATE TRIGGER \"t-pragma_x\" AFTER INSERT ON t BEGIN SELECT 1; END",
        &[],
    )
    .await
    .expect("quoted trigger name containing pragma_ must be allowed");
    // Schema-qualified unquoted names containing pragma_ are object names,
    // not body references (R3-3).
    core.query(&id, "CREATE VIEW main.pragma_view AS SELECT 1", &[])
        .await
        .expect("schema-qualified view name containing pragma_ must be allowed");
    core.query(
        &id,
        "CREATE TRIGGER main.pragma_trigger AFTER INSERT ON t BEGIN SELECT 1; END",
        &[],
    )
    .await
    .expect("schema-qualified trigger name containing pragma_ must be allowed");
    // Comment text cannot execute a pragma reference (R3-2).
    core.query(
        &id,
        "CREATE VIEW v_comment AS SELECT 1 /* pragma_table_info */",
        &[],
    )
    .await
    .expect("block comment containing pragma_ must be allowed");
    core.query(
        &id,
        "CREATE VIEW v_line AS SELECT 1 -- pragma_table_info\n",
        &[],
    )
    .await
    .expect("line comment containing pragma_ must be allowed");
    // A bracket-quoted column list with parentheses is one component (R3-1).
    core.query(&id, "CREATE VIEW v([x(]) AS SELECT 1", &[])
        .await
        .expect("bracket-quoted column list must be allowed");
    // Bare pragma_-containing aliases and column names are legitimate (R4-1).
    core.query(&id, "CREATE VIEW alias_v AS SELECT 1 AS pragma_alias", &[])
        .await
        .expect("pragma_-containing column alias must be allowed");
    core.query(&id, "SELECT * FROM \"a-b-pragma_x\"", &[])
        .await
        .expect("the created view is readable");
    // A pragma table-valued function CALL in a stored body is rejected —
    // including quoted call spellings, which SQLite accepts (R4-2), and
    // behind a bracket-quoted column list (R3-1 bypass).
    for sql in [
        "CREATE VIEW bad AS SELECT * FROM pragma_table_info('t')",
        "CREATE VIEW bad_q AS SELECT * FROM \"pragma_table_info\"('t')",
        "CREATE VIEW bad_b AS SELECT * FROM `pragma_table_info`('t')",
        "CREATE VIEW bad_k AS SELECT * FROM [pragma_table_info]('t')",
        "CREATE VIEW v([x(]) AS SELECT * FROM pragma_table_info('t')",
    ] {
        let result = core.query(&id, sql, &[]).await;
        let err = result.expect_err("pragma TVF call in stored body must be denied");
        assert!(
            err.to_string().contains("denied by SQL policy"),
            "unexpected denial shape for {sql}: {err}"
        );
    }
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn parameter_validation_and_schema_recheck() {
    let (_dir, core, _path, id) = setup().await;
    assert!(
        core.query(&id, "INSERT INTO t VALUES (?)", &[])
            .await
            .is_err()
    );
    assert!(
        core.query(
            &id,
            "INSERT INTO t VALUES (?)",
            &[Cell::Integer("1".into()), Cell::Integer("2".into())]
        )
        .await
        .is_err()
    );
    assert!(
        core.query(&id, "INSERT INTO t VALUES (?)", &[Cell::Real("NaN".into())])
            .await
            .is_err()
    );
    core.query(&id, "CREATE VIEW ok AS SELECT 1", &[])
        .await
        .unwrap();
    assert!(core.query(&id, "SELECT * FROM ok", &[]).await.is_ok());
    core.rollback(&id).await.unwrap();
    core.shutdown().await;
}
