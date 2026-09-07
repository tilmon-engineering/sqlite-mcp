# Contributor and agent guide

This repository is a Rust 2024 workspace for a local SQLite MCP server. Keep the server small, explicit, and reviewable: core owns policy/state/SQLite workers; the binary is a thin stdio adapter.

## Required checks

Use the configured mise toolchain and run from the repository root. Versions observed from the locked build are Rust `1.96.0 (ac68faa20)`, rmcp `1.7.0`, rmcp-macros `1.7.0`, rusqlite `0.40.1`, libsqlite3-sys `0.38.2`, and bundled SQLite `3.53.2` (`sqlite3.h` `SQLITE_VERSION`). rmcp-macros is pinned to 1.7.0 because rmcp 1.7.0's caret requirement otherwise resolves macros 1.8.0, which is incompatible:

```sh
mise install
mise run fmt      # cargo fmt --all -- --check
mise run lint     # cargo clippy --workspace --all-targets -- -D warnings
mise run test     # cargo test --workspace --locked
mise run build    # cargo build --workspace --locked
mise run ci       # all of the above
```

Do not report a check as passing unless it was actually run. At time of writing, `mise run ci` passes on this machine (format, Clippy `-D warnings`, 124 tests, and locked build); this is not a guarantee for other machines. Keep `Cargo.lock` current and use `--locked` for verification.

## Scope and architecture

- `crates/sqlite-mcp-core/` contains configuration, path validation, handle registry, worker lifecycle, SQL authorizer/statement boundary, schema snapshots, typed results, envelopes, and tool operations.
- `crates/sqlite-mcp/` contains the production stdio binary and CLI/config startup handling.
- One process may own multiple independent handles. Each handle owns one connection and worker; never add a global lock around long SQLite work.
- SQLite file locking, not an advisory lockfile, is the cross-process writer authority.
- Test support is Rust-only and must never become MCP/config surface area.

## Contract discipline

`DESIGN.md` is normative. Any change to tool names/arguments, path semantics, transaction transitions, SQL authorization, schema freshness, error classes, envelopes, caps, shutdown, or config fields must update DESIGN.md, README.md, and relevant executable workflow tests in the same change. Keep README examples synchronized with the documented workflow fixture and `config.example.toml` synchronized with the loader test.

The v1 tool set is exactly `create_database`, `open_database`, `list_handles`, `get_schema`, `begin_transaction`, `query`, `commit`, `rollback`, and `close_database`. Preserve required explicit `readonly`, absolute paths, mandatory schema observation before begin/query, explicit commit/rollback, positional typed parameters, one statement per query, and bounded outputs.

## Safety rules

Reject relative/URI/`~` paths, NULs, directories, special files, missing/empty/invalid databases, and duplicate device/inode identities (including symlinks/hardlinks). Creation must use exclusive filesystem creation and must not overwrite. Open must query the schema and confirm `db_readonly`; do not silently downgrade write access. Do not execute agent PRAGMAs or `pragma_*` table-valued functions, ATTACH/DETACH, transaction/savepoint control, extension loading, temporary/virtual-table creation, or filesystem-output operations. Unknown authorizer actions are denied; stored-body `CREATE VIEW`/`CREATE TRIGGER` containing `pragma_` is structurally rejected. Never use regex to split SQL statements or stop draining a DML `RETURNING` statement merely because output caps were reached.

Cancellation must be token-scoped and worker-owned; commit and cleanup rollback are awaited and are not falsely reported. Preserve truthful transaction state after BUSY, cancellation, interruption, or cleanup failure. stdout remains MCP protocol-only; logs go to stderr and must not include SQL or rows by default.

## Release workflow

Release work uses the shared workspace Cargo version for the app and core crate in lockstep, preserves inheritance, and keeps `Cargo.lock` synchronized. The release skill lives at `.polytoken/skills/release-sqlite-mcp/SKILL.md`; validate it locally with `polytoken validate skill .polytoken/skills/release-sqlite-mcp/SKILL.md`. The documented utility commands are `cargo run --locked -p xtask -- release-check TAG` and `cargo run --locked -p xtask -- release-notes TAG OUTPUT_PATH`, with `mise release-check`, `mise release-build`, and `mise workflow-check` wrappers. Use the two-commit history sequence, including a provisional changelog from observed history followed by a final notes-only commit, and identify the previous published ancestor (or repository root for the first release). Branch CI must pass before an immutable annotated `v{workspace version}` tag is created at the exact tested SHA. Never move a published tag; use the skill's draft recovery procedure.

## Change workflow

1. Read the relevant DESIGN.md section and existing tests before editing.
2. Make the smallest focused change; avoid unrelated formatting or dependency churn.
3. Add/adjust unconditional tests for lifecycle, races, caps, cancellation, and error state as applicable.
4. Run targeted tests, then `mise run ci` and inspect stdout/stderr behavior.
5. Review documentation and tool descriptions for exact executable contracts.
6. Report observed versions/results and residual platform limits; never claim unrun checks.

Do not add HTTP, database inventories, aliases, backups, import/export, deletion, arbitrary extensions, network-filesystem support, implicit permission prompts, or cross-repository dependencies without an approved scope change.
