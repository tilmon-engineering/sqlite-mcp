# F-06 — Schema-observation invalidation is success-only: failed attempted DDL leaves the gate open

**Severity:** Medium
**Status:** RCA complete (Luna `rca-06-mutation-drain`). Causal sequence source-verified line-by-line; the RCA's own probe build failed on stale rustc metadata (E0514), so runtime reproduction rests on the reviewer trace — stated honestly. No implementation changes made.

## Condition

This finding is historical. The original contract conservatively invalidated on any attempted DDL and drained mutation markers only on successful outcomes. The active contract now distinguishes authorizer classification from schema freshness: an actual cookie-changing local schema mutation remains usable for subsequent statements in the same transaction, and only an actual schema change committed requires one `get_schema` after commit.

**Historical observed behavior:** `query_with_ct` cleared `w.mutation_seen` before dispatch; the authorizer set the flag for untrusted mutations at prepare time, including preflight `prepare_exact`; and the flag was consumed only after a successful outcome, allowing marker leakage across failed requests.

Reproducer (trace-verified): after `CREATE TABLE t(x)` + re-observation, query `CREATE TABLE t(x)` (duplicate) → error returned, handle still `schema_observed=true`; a following `SELECT 1` is **accepted**, and only that later success finally drains the stale flag and invalidates. Variants with the same skip: `CREATE TABLE t(x); SELECT 1` (preflight sets flag, returns Multiple) and — worse — `CREATE VIEW v AS SELECT * FROM pragma_table_info('t')`, denied structurally by `check_stored_body` (`policy.rs:162-171`) **before any authorizer callback**, so no flag is set at all.

## Cause

Success-only flag drain; the structural stored-body denial has no mutation-attempt marker.

**Why tests missed it:** schema/atomicity tests assert invalidation only after *successful* DDL; no test asserts gate state immediately after a failed attempt.

## Proposed correction

- Restructure the outcome handling: capture the worker result, drain the flag **once unconditionally**, and when set, invalidate only if the handle still exists (`if let Some` — no-op on removed/invalidated handles), then classify/return the original outcome. Calls rejected before dispatch (parameter/SQL-size, unknown handle, expired) legitimately set no flag; the SCHEMA_STALE precheck path must not invent one.
- Treat structural `CREATE VIEW`/`CREATE TRIGGER` stored-body denial as an attempted DDL: have `check_stored_body`'s rejection path set the mutation-attempt marker (or return a policy outcome carrying it). Exactly one generation increment per dispatched query even though both prepare phases can fire the authorizer (the flag is an atomic bool — drain once).

**Prevention:** core regressions asserting state **immediately after the error**: duplicate-DDL → `schema_observed=false`, generation +1, next `SELECT 1` rejected by the stale gate; two-statement preflight variant; structural-denial variant; successful-DDL control asserting exactly one increment; cancellation/invalidation variants draining the flag without touching removed handles.

**Complexity:** low-to-moderate, localized (one outcome-restructure in `core.rs`, one policy marker, focused tests). **Residual risks:** runtime reproduction still outstanding; the "attempted DDL" boundary for pre-dispatch rejections is a policy interpretation that should be written down either way.

## Closure

Resolved by unconditional mutation-signal draining and attempted-DDL invalidation; `failed_ddl_invalidates_immediately`, `denied_stored_ddl_invalidates`, `queued_mutation_markers_isolated`, `dropped_caller_publishes_mutation_state`, and `predispatch_rejection_keeps_observation` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-06-red.log` and `F-06-green.log`).
Residual: pre-dispatch rejection boundaries remain intentionally conservative policy semantics.
Residual (independent review): RESOLVED by the caller-independent publication coordinator — query execution and the post-await drain/invalidation run in a core-owned coordinator task, so a dropped caller can no longer skip the marker drain. The real-drop regression `dropped_caller_publishes_mutation_state` (worker frozen at `BeginCompletion`, caller aborted mid-flight, then released) records the deterministic RED (log sha256 7faa3d865cc24c14d4cdec701cb195d03bc3b106b65ace486ffb1e0a1d482403: "dropped caller skipped attempted-DDL invalidation") and GREEN (log sha256 17a5916d1561026f4dd59b340441933cddc4574653b85a447a95ff6bacf51559) evidence. Pre-dispatch rejections still mark nothing, and same-process serialization (HOOK_LOCK/FAULT_LOCK) prevents cross-test marker theft.
