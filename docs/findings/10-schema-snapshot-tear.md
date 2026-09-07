# F-10 — `get_schema` outside a transaction has no internal read transaction (torn snapshot can become the authoritative observation)

**Severity:** Medium
**Status:** RCA complete (Luna `rca-10-schema-tear`). Source-level absence of any snapshot boundary verified conclusively; the RCA's interleaving probe did not run (environment issues) — no empirical mismatch counts, stated honestly. No implementation changes made.

## Condition

**Expected:** outside an explicit transaction, `get_schema` uses an internal short managed read transaction and captures identity + schema_version **in the same snapshot** as the schema reads (DESIGN.md §7).

**Observed:** the worker closure (`core.rs:683-795`) runs, with no `BEGIN`/`SAVEPOINT` around it: `PRAGMA schema_version`, `SELECT … FROM sqlite_schema`, `PRAGMA table_list`, then per-table `table_xinfo` / `index_list` / `index_xinfo` / `foreign_key_list` — dozens of independent autocommit reads. The `trusted` wrapper toggles only the authorizer bypass (`policy.rs:31-40`); it is not a transaction boundary. An external writer committing DDL between statements yields a mixed `SchemaInfo` (e.g., pre-DDL version paired with post-DDL objects), which is then **published** (`core.rs:797-812`) as the authoritative observation gating `begin_transaction`'s version comparison (`core.rs:422-428`). Secondary: `identity` is `c.path()` (the registered path), not fresh file identity — a weaker reading of the identity+version pairing.

## Cause

Autocommit gives each metadata statement its own read snapshot; nothing wraps the sequence. The contract sentence exists in DESIGN but was never implemented.

**Why tests missed it:** `schema_snapshot_freshness_and_gate` performs DDL **between whole `get_schema` calls**, never inside one; no concurrent-writer fixture exists.

## Proposed correction

When `tx_open` is false: enter a server-owned trusted `BEGIN DEFERRED` before the first read, run the entire existing sequence, trusted `COMMIT` on success; on any error, trusted `ROLLBACK` and **do not publish the observation**. Explicit ownership so it never nests inside a caller transaction (which already provides the snapshot). Works on readonly connections (`BEGIN DEFERRED` needs no write permission). Await cleanup and inspect `is_autocommit` per worker conventions. Decide the identity question explicitly: either document path-identity semantics or implement a defined identity+version pairing protocol.

**Prevention:** test-support barrier after the version read (or between phases): pause the worker, commit known DDL externally on a WAL connection, resume, and assert the returned snapshot is wholly pre- or wholly post-DDL — never mixed version/object markers; readonly-handle variant; cleanup-failure variant asserting no observation published; retain the existing inter-call freshness test.

**Complexity:** low-to-moderate implementation; moderate test complexity (deterministic interleaving hook). Documentation sync required. **Residual risks:** the read transaction runs longer than one statement (writer contention/WAL retention, bounded by the query timeout); consistency is per-connection; changes between snapshot commit and a later `begin_transaction` remain covered by the existing version gate; external file replacement stays out of the supported model.

## Closure

Resolved: the observation now runs as a managed read transaction on idle handles (worker-inside `is_autocommit` check; `BEGIN DEFERRED` … verified `COMMIT`, non-cancellable verified rollback on failure), so the version read and schema enumeration see one coherent WAL snapshot. Deterministic interleaving coverage (`SchemaVersionRead` barrier + external writer) passes on writable and readonly handles; `ManagedSchemaCleanup` fault injection on either cleanup path invalidates and removes the handle; caller transactions are preserved un-nested and un-committed.
All five named tests are GREEN in the integrated tree. Evidence in `closure-evidence.json` (`F-10-red.log`, `F-10-green.log`).
Residual: the interleaving tests were un-ignored only after the managed-read fix landed, so their deterministic RED exists only against the pre-fix source (recorded honestly here); an invalidation attempt with `futures::executor::block_on` from the docs/Q finalize pass was rejected and rewritten async-safe.
