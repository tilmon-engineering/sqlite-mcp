# F-02 — Registry mutex held across worker-shutdown awaits (cross-handle blocking; shutdown serialization)

**Severity:** Medium
**Status:** RCA complete (Luna `rca-02-registry-lock-await`). Source-level lock lifetime verified; no runtime latency probe run (stated honestly). No implementation changes made.

## Condition

**Expected:** metadata enumeration and unrelated-handle operations must not wait on one handle's cleanup; unrelated workers must not be serialized during shutdown (DESIGN.md lifecycle section).

**Observed:**
- `close_database` (`core.rs:239-263`) binds `let mut s = self.inner.lock().await`, validates/removes the handle, then executes `w.shutdown().await` while the named guard is still alive — Rust NLL does not release a named `MutexGuard` early; its drop scope is the enclosing function. No `drop(s)` or inner scope exists at this site.
- `Core::shutdown` (`core.rs:258-263`) drains all handles and awaits each worker **sequentially under the same guard**.
- The await is real: `Worker::shutdown` awaits a bounded mpsc send (`worker.rs:440-444`), which can suspend on queue backpressure. Every registry consumer (`list_handles`, `open_database`, `worker()`, `transaction_open()`, begin/query prechecks and post-await updates) stalls for that interval; shutdown additionally serializes up to 1024 workers.

**Calibration (adjacent issue verified by RCA):** `Worker::shutdown` drops the oneshot receiver after sending `Command::Shutdown` — it does **not** await rollback/connection-close completion, despite DESIGN.md's ordered-shutdown wording. That is a separate lifecycle truthfulness concern to track alongside this fix.

## Cause

One `tokio::sync::Mutex` (`Core.inner`) holds all registry state, and both cleanup paths hold its guard across worker I/O awaits. Tests never overlap close/shutdown with a registry consumer: the independence test (`ac_concurrency.rs:65-105`) completes all work before `core.shutdown()`; the stdio shutdown test inspects state only after process exit.

## Proposed correction

- `close_database`: perform validation/removal in an inner block, return the owned `Worker`, await shutdown **after** the guard drops (the block-scoped pattern already exists in the invalidation cleanup at `core.rs:630-644` — use it as the template).
- `Core::shutdown`: collect workers under the lock, release it, then await — concurrently (`join_all` or bounded join) so unrelated workers close in parallel.
- Separate follow-up: make `Worker::shutdown` retain/await its completion receiver so cleanup truthfulness matches the design text; do not let the lock fix silently weaken cleanup ordering.

**Prevention:** review rule — never hold `Core.inner` across any `.await`; structural helper that returns owned workers (makes release unavoidable); deterministic regression opening two idle handles, blocking A's shutdown via a test-support barrier, and asserting `list_handles()`/`get_schema(B)` complete under a short timeout; overlap close/shutdown with `list_handles` specifically (not just long SQL).

**Complexity:** low-to-moderate, localized. **Residual risks:** after the fix, observers can see a removed handle while its worker is still finishing (intended close contract, but reopen/SQLite-lock interplay should be tested); concurrent shutdown must stay bounded and truthful on cleanup failure; shutdown-completion semantics are the adjacent open item.

## Closure

Resolved by releasing registry state before close awaits, concurrent idempotent shutdown reporting (`ShutdownReport`/`ShutdownStatus` with verified rollback, off-runtime thread join), and — after the independent lifecycle review — wiring the worker shutdown token into busy retries and progress callbacks so shutdown actually interrupts active queries/busy waits (`SERVER_SHUTDOWN` reported truthfully, never as a fake cancel/deadline). `concurrent_shutdown_joins_all_workers`, `shutdown_report_scope_and_idempotence`, and `close_failure_identity_reopen_matrix` are GREEN.
RED and GREEN evidence are recorded in `closure-evidence.json` (`F-01-02-red.log` and `F-01-02-green.log`).
Residual: the capacity-bounded FIFO admission queue is not implemented; close ordering, async `ShutdownReport`, shutdown cancellation of active work, and caller-independent publication (per-operation coordinator; publication strictly precedes the next gate-ordered operation) are landed and named tests pass.
