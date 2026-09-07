# F-04 — Config validation covers 3 of 15 settings; unchecked values disable engine limits or create unbounded behavior

**Severity:** High
**Status:** RCA complete (Luna `rca-04-config-validation`; SQLite negative-limit rule cited from https://sqlite.org/draft/c3ref/limit.html). Zero-value acceptance live-confirmed by session probe. No implementation changes made.

## Condition

**Expected:** every resource setting is a validated positive override with a bounded sensible maximum, rejected before any protocol byte (DESIGN.md/README).

**Observed:** `Config::validate` (`config.rs:115-129`) checks only `max_handles`, `queue_capacity`, `query_timeout_ms`. The other **12** settings (RCA corrected the field count: `Config` has 15 fields) accept 0 and arbitrary magnitudes. Live: a config setting all twelve to 0 passed startup and completed MCP initialize (exit 0).

Representative degenerate behaviors (full per-field enumeration in the RCA):
- `sql_byte_limit = 0` → every non-empty SQL rejected before dispatch (server unusable but "valid").
- `result_row_limit = 0` → statements still execute/drain but materialize nothing.
- `busy_wait_ms = 0` → lock contention fails immediately; huge values stall workers.
- idle seconds huge (or the existing test fixture's `u64::MAX/1000`) → effectively never expire.
- **Engine-limit defeat:** `cell_byte_limit`, `expression_depth`, `compound_terms`, `parameter_limit` are cast `as i32` at `worker.rs:159-180` with no range check; values above `i32::MAX` wrap negative, and SQLite treats a negative `sqlite3_limit` newLimit as *unchanged* — the oversized configured cap **silently disables** the engine limit. `set_limit` return values are discarded, so failed installation is unobservable.
- `sql.rs:60` hardcodes `params.len() > 1000`, shadowing any configured `parameter_limit > 1000` (two sources of truth).

## Cause

Validation stops after three checks; no shared positive/bounded helper; unchecked numeric narrowing at the FFI boundary; duplicated parameter cap.

**Why tests missed it:** `ac_config.rs` exercises only the three implemented checks; the example config uses defaults; SQLite clamps positive limits to compile-time maxima, so worker startup succeeding never proves a limit was installed.

## Proposed correction

1. One validation helper rejecting 0 and values above explicit per-field maxima, error naming field and range, enforced before protocol serving (`Core::new` already re-validates).
2. Concrete bounds table for contract approval (recommendations, not current facts): `query_timeout_ms 1..=300_000`; `writable_idle_seconds 1..=86_400`; `readonly_idle_seconds 1..=604_800`; `result_row_limit 1..=100_000`; `result_byte_limit`/`schema_byte_limit`/`cell_byte_limit` 1..=64 MiB (cell additionally ≤ i32::MAX); `busy_wait_ms 1..=60_000`; `sql_byte_limit 1..=1 MiB`; `column_limit 1..=2048`; `parameter_limit 1..=32_766`; `expression_depth 1..=1000`; `compound_terms 1..=500` — verify the bundled SQLite build's hard maxima before promising values.
3. `i32::try_from` (never `as i32`) at FFI boundaries; optionally check `set_limit`'s returned prior value at worker init.
4. Single-source the parameter limit by threading the configured value into `sql::validate`.
5. Synchronize DESIGN.md/README/config.example.toml (repository contract requires it).

**Prevention:** table-driven `validate` test per field (0 / 1 / max−1 / max / max+1 / `usize::MAX`), per-field runtime boundary fixtures at limit−1/limit/limit+1, wrap-value rejection, and startup tests feeding all-zero and all-huge TOML asserting zero protocol bytes and nonzero exit. Treat config validation as a security/resource boundary, not deserialization hygiene.

**Complexity:** Medium, localized. **Residual risks:** maxima require product/security sign-off; SQLite compile-time hard maxima vary by build; JSON/base64 expansion can exceed raw cell limits (result-byte caps remain necessary); the existing `u64::MAX/1000` idle test fixture must be reconciled with any bound.

## Closure

Resolved by table-driven validation, checked engine-limit conversion/readback, and one configured parameter-limit source; `config_all_field_boundaries`, `config_policy_covers_serialized_fields`, `invalid_config_matrix_before_serving`, `engine_limit_readback_and_checked_conversion`, and `configured_parameter_limit_single_source` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-04-red-core.log`, `F-04-red-binary.log`; GREEN integrated evidence is recorded there).
Residual: SQLite compiled-in hard maxima may be lower than approved application ceilings.
Review addition: `Core::new` now runs `install_limits` on a disposable in-memory connection before serving, so unsupported engine ceilings are rejected before any protocol bytes (independent-review MEDIUM finding fixed).
