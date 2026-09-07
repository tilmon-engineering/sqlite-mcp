# F-12 — `SQLITE_LOCKED` is misclassified as `INTERNAL` in the MCP envelope

**Severity:** Low (implementation fix trivial; verification is the hard part)
**Status:** RCA complete (Luna `rca-12-locked-class`). Display strings and mapping mechanically verified; live `SQLITE_LOCKED` reproduced at the SQLite level (shared-cache fixture); end-to-end public-path capture not obtained (server rejects URIs) — stated honestly. No implementation changes made.

## Condition

**Expected:** DESIGN.md:75 maps BUSY/LOCKED variants to the stable envelope class `BUSY` with bounded-wait info and recovery semantics.

**Observed:** `classify_worker_error` (`core.rs:309-367`) correctly builds `CoreError::Locked { primary_code, extended_code, … }` for primary `SQLITE_LOCKED`, whose Display is `database locked (primary=6, extended=262)`. But `tools.rs::error` (`:125-143`) has **no `CoreError::Busy` or `CoreError::Locked` variant arms** — `Busy` reaches class `BUSY` only because its message happens to contain "busy"; `Locked` contains neither `busy` nor the other fallbacks (`no transaction`, `transaction required`) and falls through to **`INTERNAL`**. Lifecycle booleans are unaffected (the separate `explicit_lifecycle` match includes `Locked`). RCA live fixture: two shared-cache connections on one file, one holding a write transaction, the other's same-table write → `SQLITE_LOCKED_SHAREDCACHE` (262) — a real, distinct, recoverable condition clients would see as INTERNAL.

## Cause

The envelope classifier mixes variant matching with message-substring heuristics; a dedicated variant whose wording differs from the heuristic slips through.

**Why tests missed it:** `ac_concurrency.rs:109-147` asserts the **core** error variant only and never dispatches through the envelope; no Locked protocol fixture exists.

## Proposed correction

- Add explicit arms — minimum: `CoreError::Busy { .. } | CoreError::Locked { .. } => "BUSY"` — preferably via a centralized, table-testable `classify_core_error(&CoreError) -> &'static str`; keep message fallbacks only as a narrowly documented compatibility path for generic wrapper variants.
- Regressions: unit test feeding `CoreError::Locked { primary 6, extended 262, open: true, continuable: true }` through the classifier asserting class `BUSY` plus booleans; a table-driven test covering **every** `CoreError` variant against the DESIGN class table (representative values for wrapper variants), so a future enum addition fails visibly; a protocol-level contention class assertion where a fixture can produce Locked without weakening the absolute-path policy.

**Complexity:** low (one/two-line arm + helper + table test). **Residual risks:** substring heuristics remain fragile for generic `Worker`/`Sqlite` wrappers (a typed error-class design would eliminate the class of bug); end-to-end public-envelope Locked capture still outstanding; verified against the pinned rusqlite/SQLite versions only.

## Closure

Resolved by typed BUSY/LOCKED classification with preserved busy metadata; `core_error_envelope_variant_matrix` is GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-12-red.log` and `F-12-green.log`).
Residual: typed classification is covered at unit level in `core/error.rs` tests; an end-to-end public-envelope LOCKED fixture remains unavailable.
