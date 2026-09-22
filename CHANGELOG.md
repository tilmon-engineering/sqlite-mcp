# Changelog

All notable changes are documented here from observed repository history. Release sections use the shared Cargo workspace version and exact `v{version}` tag.

Provisional first-release notes were finalized from inspected history spanning root commit `2684862` through implementation commit `e9544c5`; the subsequent notes-only commits are not part of that inspected history.

## [0.3.0] - 2026-09-22

Provisional notes summarize the reviewed working-tree implementation relative to published `v0.2.0`; they will be finalized from the implementation commit before tagging.

### Added

- Direct, read-only query support for exactly `PRAGMA foreign_keys` and `PRAGMA recursive_triggers`, with both connection invariants established and read back before writable, reopened, or read-only workers are published.
- Trusted `get_schema.user_version` metadata alongside the distinct schema-cookie `schema_version`, including richer deterministic column, index, and foreign-key metadata for compatibility inspection.

### Fixed

- Intentional SQL-policy rejections now report the typed `POLICY_DENIED` class with truthful worker-observed transaction state across authorizer, stored-body, maintenance, and pre-execution guards, while malformed SQL and unrelated SQLite failures retain their native classes.
- The worker now installs one bounded busy handler, synchronously reports startup readiness, and fails without publishing a handle when connection initialization cannot establish the required invariants.
- PRAGMA source classification, statement-boundary handling, stored-body protection, and merge's independent blanket PRAGMA denial are covered by expanded regression tests and synchronized documentation.

### Platform notes

- Platform support and artifact properties are unchanged from 0.2.0: Linux GNU builds use Ubuntu 24.04 and its glibc environment (not musl/static); macOS binaries are unsigned and unnotarized and target macOS 15 runners.

## [0.2.0] - 2026-09-20

Provisional notes were finalized from inspected history spanning `v0.1.0` through implementation commit `dadf95f` (15 commits); the subsequent notes-only commit is not part of that inspected history.

### Added

- Git-independent SQLite merge tools: `extract_sqlite_merge` produces deterministic logical SQL schema snapshots plus sidecar observations for three-way comparison, and `import_sqlite_text` replays a validated SQL image into a new database. Both are explicit-path, bounded, never invoke the SQLite CLI, and never overwrite existing inputs or outputs.
- Server-owned admission control for long-running work: merge operations are admitted with bounded concurrency, participate in ordered shutdown, and honor per-request cancellation without leaving workspace side effects.
- Fail-closed SQL authorizer hardening as a documented guarantee: unmapped authorizer actions are denied, schema-qualified `temp.` object access is denied like TEMP objects, `ANALYZE` is denied, top-level `REINDEX` (including `EXPLAIN`-prefixed spellings) is denied, and stored view/trigger bodies containing `pragma_*` table-valued function calls in any identifier quoting are rejected before creation.
- Per-request enforcement of the configured `busy_wait_ms` bound on every worker command path, with lock exhaustion reported as the retryable `BUSY` error class (including blocked commits, which preserve the open transaction) instead of waiting the full query deadline.
- Ordered SIGINT shutdown: Ctrl-C now triggers ordered worker shutdown with rollback of open transactions and exits zero, including when cancellation arrives during initialization.

### Fixed

- `begin_transaction` on a handle with an open transaction now reports `TX_ALREADY_OPEN` with truthful, unchanged transaction state instead of an internal error with a raw SQLite message.
- Result payloads exceeding the configured byte cap now report `RESULT_TOO_LARGE` (previously `INTERNAL`), with the transaction preserved and usable.
- Affected-row counts (`changes`) are now per-statement: only DML statements report counts; SELECT, DDL, and zero-row DML report zero instead of inheriting the previous statement's count.
- The documented error-class registry now matches the emitted set exactly, including all eight `MERGE_*` classes, `SERVER_SHUTDOWN`, `INVALID_TRANSACTION_MODE`, and `RESULT_TOO_LARGE`.
- The release publisher's raw GitHub REST release lookups now decode the snake_case REST fields and paginate the release list correctly, so draft discovery and duplicate-draft recovery work as documented.

### Platform notes

- Platform support and artifact properties are unchanged from 0.1.0: Linux GNU builds use Ubuntu 24.04 and its glibc environment (not musl/static); macOS binaries are unsigned and unnotarized and target macOS 15 runners.

## [0.1.0] - 2026-09-13

### Added

- Initial local SQLite MCP server over stdio, with explicit handle-scoped database and transaction operations.
- Bundled SQLite and bounded, policy-checked SQL execution with schema observation and typed results.
- Production `--help`, `--version`, and `-V` CLI commands, with the version compiled from the shared Cargo workspace version.
- Native binary release workflow for Linux x86_64 and Arm64 (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) and macOS Intel and Apple Silicon (`x86_64-apple-darwin`, `aarch64-apple-darwin`), with executable archives and SHA-256 checksums.
- Draft-first publication with exact asset and changelog checks, immutable annotated tags, and native packaged-binary smoke tests.
- Anonymous public release metadata and downloaded-binary verification, bounded HTTPS-only downloads, and retained CI metadata evidence.
- Developer-only release validation and changelog tooling, checksum-verified installation instructions, and a history-backed release runbook.

### Platform notes

- Linux GNU builds use Ubuntu 24.04 and its glibc environment; they are not musl/static universal Linux binaries and do not establish older-distribution compatibility.
- macOS builds target native macOS 15 runners. Binaries are unsigned and unnotarized; Gatekeeper restrictions may apply. Older macOS compatibility and interactive Gatekeeper installation are not established by CI.
