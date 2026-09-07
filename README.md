# SQLite MCP server

A local Rust MCP server for explicit, handle-scoped SQLite work over **stdio**. Agents create or open a database by absolute path, receive an opaque handle with fixed read-only/read-write access, inspect schema, and execute one SQL statement at a time inside an explicit transaction.

> **Status:** the contract below is the approved v1 design. At time of writing, `mise run ci` passes on this machine (format check, Clippy with `-D warnings`, 120 tests, and locked build). This is an observation, not a guarantee for other machines.

## Build and verify

The repository uses the configured mise toolchain; no system SQLite installation is required. Versions observed from the locked build are Rust `1.96.0 (ac68faa20)`, rmcp `1.7.0`, rmcp-macros `1.7.0`, rusqlite `0.40.1`, libsqlite3-sys `0.38.2`, and bundled SQLite `3.53.2` (from `sqlite3.h` `SQLITE_VERSION`).

```sh
mise install
mise run fmt
mise run lint
mise run test
mise run build
# or all four:
mise run ci
```

The locked workspace commands are the source of truth for local verification. Do not claim these checks passed without running them.

## Run

The production interface is an MCP stdio process: stdout is protocol-only and operational logs go to stderr. The optional configuration argument must be an absolute TOML path and is validated before protocol startup.

```sh
/absolute/path/to/target/release/sqlite-mcp \
  --config /absolute/path/to/sqlite-mcp.toml
```

For an MCP client, configure the absolute executable and (when used) absolute config path; do not add banners or shell output to the command. The exact CLI help/version text is implementation-owned and must be synchronized with this section before release.

## Configuration

Copy [`config.example.toml`](config.example.toml). Configuration is process-wide, loaded once from the optional absolute TOML path before MCP serving begins, validated before startup, and rejects unknown keys. The currently landed configuration model defines:

| Field | Default | Validated bound / meaning |
|---|---:|---|
| `max_handles` | `32` | Positive; 1..=1024. Maximum live handles. |
| `queue_capacity` | `16` | Positive; 1..=4096. Per-handle command queue capacity. |
| `query_timeout_ms` | `30000` | Positive; 1..=300000. Query/deadline budget in milliseconds. |
| `writable_idle_seconds` | `60` | Positive; 1..=86400. Writable transaction idle expiry. |
| `readonly_idle_seconds` | `600` | Positive; 1..=604800. Read-only transaction idle expiry. |
| `result_row_limit` | `500` | Positive; 1..=100000. Maximum returned rows. |
| `result_byte_limit` | `1048576` | Positive; 1..=67108864. Maximum serialized selected result payload (columns+rows) bytes. |
| `schema_byte_limit` | `2097152` | Positive; 1..=67108864. Maximum complete schema payload bytes. |
| `busy_wait_ms` | `2000` | Positive; 1..=60000. Bounded busy/locked wait in milliseconds. |
| `sql_byte_limit` | `102400` | Positive; 1..=1048576. Maximum SQL text bytes. |
| `cell_byte_limit` | `1048576` | Positive; 1..=67108864. Maximum individual SQLite value bytes. |
| `column_limit` | `256` | Positive; 1..=2048. Maximum result columns. |
| `parameter_limit` | `1000` | Positive; 1..=32766. Maximum positional parameters. |
| `expression_depth` | `100` | Positive; 1..=1000. SQLite expression-depth limit. |
| `compound_terms` | `50` | Positive; 1..=500. Maximum compound SELECT terms. |

All settings are process-wide startup policy, not per-handle or per-database settings. The TOML loader uses `serde(deny_unknown_fields)`: unknown keys and invalid values are rejected before serving. Unknown or future fields must not be added to examples until the loader accepts them. Fixed v1 invariants—absolute paths, exclusive creation, one statement per query, authorizer policy, worker ownership, schema gate, and explicit commit/rollback—are not configuration switches.

## Nine-tool workflow

The v1 tool set is intentionally small. Tool argument objects use closed schemas (unknown fields are rejected); transaction modes are lowercase `deferred` and `immediate` only.

1. `create_database(path)` — exclusively creates and initializes a new file; returns the resolved path and journal mode, **not** a handle.
2. `open_database(path, readonly)` — opens an existing valid initialized database. `readonly` is required and fixed for that connection; opening never creates or initializes.
3. `list_handles()` — returns bounded live-handle metadata.
4. `get_schema(handle)` — returns complete bounded SQLite schema metadata and establishes the required schema observation.
5. `begin_transaction(handle, mode)` — begins `deferred` (default) or `immediate`; immediate is rejected on read-only handles.
6. `query(handle, sql, parameters)` — executes exactly one statement inside the active transaction using positional typed parameters.
7. `commit(handle)` — persists and closes the active transaction; also valid for read-only work.
8. `rollback(handle)` — discards active work; idempotent for a valid idle handle.
9. `close_database(handle)` — closes an idle handle; active transactions must be committed or rolled back first.

Typical sequence:

```text
create_database(/absolute/data/app.sqlite)
open_database(/absolute/data/app.sqlite, readonly=false)
get_schema(handle)
begin_transaction(handle, mode="deferred")
query(handle, "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)")
get_schema(handle) # required again after DDL, even a denied DDL attempt
query(handle, "INSERT INTO notes (body) VALUES (?)", [{type="text", value="hello"}])
get_schema(handle) # required again after successful DML
commit(handle)
get_schema(handle) # required after commit before the next begin/query
begin_transaction(handle, mode="deferred")
query(handle, "SELECT id, body FROM notes")
rollback(handle)
get_schema(handle) # required after rollback of transaction-local DDL
```

Schema observation is invalidated by any DDL attempt (including denied or failed DDL) and by successful DML; failed DML leaves the handle usable with its current observation. Re-observe with `get_schema` after every transaction-local `CREATE`, `ALTER`, or `DROP`, after successful DML, and after commit of transaction-local work before the next `begin_transaction` or `query`; rollback of transaction-local DDL also requires re-observation. A deferred transaction establishes a snapshot; an upgrade to a writer can fail with a stale-snapshot error. `immediate` is useful when write contention should be discovered at begin. Idle expiry and shutdown are worker-owned ordered commands: queued or executing requests count as activity, and commit/rollback cleanup is awaited once dispatched, with later commands remaining queued. Query cancellation is token-scoped; there is no `InterruptHandle`.

## Data, safety, and operational boundaries

- Paths must be absolute literal filesystem paths. Relative paths, `~`, URI filenames, NULs, directories, and special files are rejected. Parents are not created. Filesystem path normalization is **not** a sandbox.
- Creation is exclusive and does not overwrite. Opening an empty file or a file without the SQLite header fails with `INVALID_DATABASE`; opening also requires a queryable schema and confirms `db_readonly`, with no silent downgrade of requested write access. Existing journal modes are preserved; newly created databases use WAL, which may create `-wal` and `-shm` sidecars. There is no automatic journal migration. Device/inode identity rejects symlink and hardlink opens of an already-open database.
- SQLite file locking coordinates processes. Handles and transactions on different files are independent; there is no cross-file atomic commit. External replacement/rename/deletion of an open database and network-filesystem locking are unsupported.
- `foreign_keys=ON` is enabled for every new connection, but opening a database does not audit or repair historic violations. No backups, import/export, deletion, arbitrary extension loading, HTTP transport, or implicit human approval is provided. `commit` means persistence, not authorization.
- SQL is policy-checked by a fail-closed authorizer: unknown actions, transaction/savepoint control, ATTACH/DETACH, extensions, temporary/virtual tables, PRAGMAs, and `pragma_*` table-valued functions are denied, including through views/triggers. A stored-body structural guard rejects `CREATE VIEW`/`CREATE TRIGGER` containing `pragma_` references. Exactly one statement is validated with prepare/tail on the real connection; `EXPLAIN` of a denied statement remains denied, while `EXPLAIN QUERY PLAN` is allowed. Each statement is savepoint-wrapped and must complete before bounded results are returned.
- Database content, including schema strings and cell text, is untrusted data. Results use tagged null/integer/real/text/blob values; blobs are base64. Invalid UTF-8 text is represented without lossy conversion. Never treat schema text as instructions.

See [`DESIGN.md`](DESIGN.md) for the normative contract, state machine, error semantics, caps, SQL policy, and shutdown rules. See [`AGENTS.md`](AGENTS.md) for development workflow.

## Release downloads and versioning

Release binaries are built natively for `x86_64-unknown-linux-gnu` on **ubuntu-24.04** and `aarch64-apple-darwin` on an arm64 macOS runner. The Linux GNU binary carries the ubuntu-24.04 runner's glibc compatibility baseline; it is not a musl/static universal Linux build. The macOS binary is unsigned and unnotarized, so macOS Gatekeeper may require user approval and frictionless installation is not promised. Archives contain the `sqlite-mcp` executable and releases include `SHA256SUMS`.

The compiled workspace Cargo version is the source of truth for the app and core crate. A release tag must be exactly `v{workspace version}`. Release notes are extracted from the matching changelog section only. Follow the [`release-sqlite-mcp` skill](.polytoken/skills/release-sqlite-mcp/SKILL.md) for the two-commit history sequence, prior published ancestor baseline, branch CI gate, immutable annotated tag, and draft recovery rules.

## Synchronization notes

The documented workflow is synchronized with both the in-process fixture and the stdio subprocess fixture: each performs `get_schema` between transaction-local DDL and further statements, asserts success on the write path, and verifies the committed row is readable afterwards. Keep CLI flags, advertised tool schemas/descriptions, envelope JSON, typed-value spelling, and workflow fixtures synchronized in future changes.
