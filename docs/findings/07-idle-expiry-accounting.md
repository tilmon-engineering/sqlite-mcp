# F-07 — Idle-expiry accounting defects: commit succeeds after the expiry window; post-expiry commit misreports class

**Severity:** High
**Status:** RCA complete (Luna `rca-07-expiry-accounting`). Sub-symptoms A1 and A2 **live-reconfirmed independently by the RCA** against the built binary, matching the session probe. A3 reframed honestly. No implementation changes made.

## Condition

**Expected:** the idle window rolls the transaction back; a subsequent query **or commit** reports `TX_EXPIRED` instead of silently continuing (DESIGN.md/plan).

**Observed:**
- **A1 (live, severe):** `writable_idle_seconds = 1`; writable transaction holds an INSERT; sleep 1.3 s (window expired); `commit()` directly → **Ok**; independent connection sees the row **persisted** (`count = 1`). The `Command::Control` path never checks the idle deadline or the expired flag, so expired work commits.
- **A2 (live, class lie):** same setup but a query runs first — the lazy `Run`-path expiry rolls back and reports `TX_EXPIRED`; the following `commit()` fails with **"no transaction open" → `NO_TX_OPEN`** instead of `TX_EXPIRED`. Registry state is also stale afterwards: it still reports transaction-open metadata while the worker has already rolled back.
- **A3 (reframed):** the original claim "a queued request crossing the deadline is rejected" is **not mechanically implied** — the worker resets `idle_deadline` after every completed command (`worker.rs:297-300`), so the plain long-Run-then-queued-Run sequence proceeds. Queued time still plays no role in activity accounting; treat as an underspecified design gap, not a demonstrated rejection.

## Cause

The expiry gate exists only in the `Command::Run` handler (`worker.rs:187-202`); `Command::Control` (`:302-311`) — used by `commit`/`rollback` via `run_control` — has none. After lazy rollback, the control callback sees autocommit and emits "no transaction open", which message-maps to `NO_TX_OPEN`; the expired reason is lost.

**Why tests missed it:** `idle_expiry_state.rs` drives expiry through commit requests and **tolerates** `TX_EXPIRED | NO_TX_OPEN` (never asserts the class, never checks persistence); concurrency tests call `expire_handle` explicitly, bypassing natural Control-path expiry.

## Proposed correction

- One centralized expiry gate used by **both** Run and Control at dequeue: already-expired → `TRANSACTION_EXPIRED` without invoking the callback; deadline elapsed with an open transaction → rollback, set expired, report `TX_EXPIRED`. Commit must never execute against an expired transaction.
- Core preserves the expired reason: registry handle marked expired, transaction id/mode cleared, subsequent controls report `TX_EXPIRED`; `NO_TX_OPEN` reserved for genuinely-never-open transactions.
- Decide and document queued-activity accounting (accepted-timestamp vs completion-time reset precedence) with a deterministic test.
- Regressions: (1) sleep past window → direct commit → `TX_EXPIRED` + external row count unchanged; (2) query-then-commit → both `TX_EXPIRED`; (3) deterministic queue-activity test using barriers/injected clock; (4) genuinely-idle no-transaction commit stays `NO_TX_OPEN`; remove the tolerant class assertion from `idle_expiry_state.rs`.

**Complexity:** A1/A2 low-to-moderate (localized worker helper + Core registry handling); full coherent correction including queued semantics moderate (Command payload/ordering changes). **Residual risks:** truthful state must survive rollback *failure*; distinguish Run-initiated expiry from explicit `Expire`; A3's exact contract needs a product decision before implementation.

## Closure

Resolved by applying expiry gates to Run and Control paths with typed `TX_EXPIRED` preservation; `expired_direct_commit_never_persists`, `expired_followup_controls_exact_class`, and `queued_activity_and_idle_boundary` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-07-08-red.log` and `F-07-08-green.log`).
Residual: queued-activity accounting remains bounded by the current architecture and does not establish a broader timing guarantee.
