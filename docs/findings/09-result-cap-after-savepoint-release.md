# F-09 — Result byte-limit enforced after savepoint release: a failed call's mutation persists

**Severity:** High
**Status:** RCA complete (Luna `rca-09-result-cap-effects`). **Live-reproduced** independently by the RCA and by the session probe. No implementation changes made.

## Condition

**Expected:** "A failed call rolls back its partial effects if outer transaction survives" (AC.6/DESIGN); the plan's stated outcome for result-cap overflow is class `RESULT_TOO_LARGE` with the savepoint restored.

**Observed (live, `result_byte_limit = 200`):** `INSERT INTO t VALUES (printf('%.900c','y')) RETURNING x` executed fully, the savepoint was RELEASEd, then `query_with_ct` measured the serialized result and returned `Err("result exceeds configured byte limit")`. Handle still showed the transaction open; `commit()` → **Ok**; external inspection: `count(*) = 1`, `length(x) = 900`. The **failed call's mutation persisted**. The envelope class is deterministically `INTERNAL` — there is no `RESULT_TOO_LARGE` variant in `CoreError`, and the message misses the tools.rs substring fallbacks.

## Cause

Ordering: the worker closure releases `SAVEPOINT agent_stmt` on successful execution (`core.rs:587-601`); only afterwards does the outer function measure `serde_json::to_vec(&QueryResult)` (`core.rs:662-672`) and fail. The error path at that point has no rollback capability. (The measurement also includes envelope counters, which is consistent with the general serialized-payload wording but should be reconciled with the documented columns+rows rule.)

**Why tests missed it:** result-cap tests use non-mutating SELECTs and row truncation; the savepoint-atomicity test uses `UPDATE OR FAIL`, which fails *inside* the closure where rollback-to-savepoint still runs. No test combines a byte-cap overflow with a mutating `RETURNING` and then checks persistence.

## Proposed correction

- Serialize/measure inside the worker closure, after fully draining rows and building `QueryResult`, but **before** `RELEASE agent_stmt`. On overflow, take the existing `ROLLBACK TO agent_stmt; RELEASE agent_stmt` path and return the overflow error — preserving the full-drain rule (caps limit materialization, never stop execution early).
- Add `CoreError::ResultTooLarge` mapped to class `RESULT_TOO_LARGE` rather than a fragile message match.
- Centralize one size helper so the measured object matches the documented definition.

**Prevention:** unconditional atomicity regression using a mutating `RETURNING` at serialized-size−1 / size / size+1 (shared helper mirroring production serialization): size−1 → `RESULT_TOO_LARGE`, statement rolled back, transaction open/continuable, post-commit persistence **absent**; size and size+1 → success and persistence. Protocol variant asserting `isError` + class.

**Complexity:** low-to-moderate, localized. **Residual risks:** overflow is detected after full materialization (peak memory bounded by row/cell limits, not the byte cap — this finding concerns atomicity, not memory); boundary fixtures are sensitive to `QueryResult` shape changes — the shared helper mitigates; reconcile counters-in-measurement vs the documented payload rule.

## Closure

Resolved by measuring serialized columns+rows before savepoint release, draining `RETURNING`, and rolling back overflow; all five named tests are GREEN in the integrated tree, including `overflow_cleanup_failure_invalidates` (the `SavepointRestore` cleanup fault is consumed at the rollback-to/release boundary and an injected failure invalidates the handle instead of returning a plain `RESULT_TOO_LARGE`).
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-09-red.log` and `F-09-green.log`).
Residual: overflow is detected after full materialization (peak memory is bounded by row/cell limits, not the byte cap — this finding concerns atomicity, not memory).
