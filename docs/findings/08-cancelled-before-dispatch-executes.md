# F-08 — Requests cancelled before worker dispatch still execute and can commit quick mutations

**Severity:** High
**Status:** RCA complete (Luna `rca-08-cancelled-dispatch`). **Live-reproduced** by the RCA via public API. No implementation changes made.

## Condition

**Expected:** a request whose token was cancelled (or whose deadline elapsed) before the worker dispatches it must not execute at all; the honest outcome is a `CANCELLED` (respectively deadline) error with the transaction untouched and continuable.

**Observed (live):** with a long recursive CTE occupying the worker, a queued `INSERT INTO t VALUES (1)` whose token was cancelled **before dequeue** executed anyway: `Ok(QueryResult { changes: 1, execution_complete: true, .. })`. In that run the post-execution cancellation cleanup happened to roll back the outer transaction (a later commit reported "no transaction open"), so durable persistence was not directly demonstrated — but the forbidden execution (`changes: 1`) is conclusive, and a quick mutation not reaching the progress handler can leave its effect committable.

Mechanism: `Command::Run` handling (`worker.rs:187-216`) checks only the worker `expired` flag and idle deadline, then installs callbacks and unconditionally invokes `f(conn, &ctx)` at line 216 — no `ctx.token.is_cancelled()` or deadline check immediately before invocation. The progress handler fires per 1000 VDBE ops and the busy handler only during lock waits; a one-row INSERT can complete without either observing cancellation.

**Why tests missed it:** cancellation coverage cancels an **executing expensive** query (progress handler fires). No test queues a request, cancels it while queued, and asserts the closure never ran; outcome-only assertions are masked when cleanup later rolls back.

## Proposed correction

Pre-execution gate at dequeue, immediately before callback setup/`f()`: token cancelled → `WorkerError::Interrupted { by_client: true, transaction_open: !conn.is_autocommit(), transaction_continuable: !conn.is_autocommit() }` **without invoking `f`**; deadline elapsed → the `by_client: false` equivalent. No statement ran, so the outer transaction remains open/continuable — do **not** route this through the post-call rollback path. Share one classification helper with the post-call logic.

**Prevention:** unconditional queued-cancel regression: begin tx; long query holds worker (test-support handover signal, not sleeps); queue a mutation with a fresh token; cancel it pre-dispatch; assert `CoreError::Cancelled`, **no side effect** (commit afterward, verify row absent), transaction still continuable. Deadline-while-queued variant asserting `DeadlineExceeded` with the same no-execution assertions. Keep existing executing-query cancellation tests.

**Complexity:** low implementation, medium verification. **Residual risks:** the nanosecond window between check and closure entry is inherent scheduling semantics; classification must not roll back an untouched outer transaction; interaction with F-03's shutdown cancellation needs one precedence rule covering all three causes.

## Closure

Resolved by the worker pre-dispatch cancellation/deadline gate and precedence matrix; `queued_cancel_never_enters_closure`, `queued_deadline_preserves_transaction`, `interruption_precedence_matrix`, and `expiry_rollback_failure_invalidates` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-07-08-red.log` and `F-07-08-green.log`).
Residual: the inherent check-to-closure-entry scheduling window remains.
