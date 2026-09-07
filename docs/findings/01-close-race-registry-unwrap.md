# F-01 — `close_database` can panic in-flight operations (registry `unwrap()` after await); close has no linearization point

**Severity:** High
**Status:** RCA complete (Luna `rca-01-close-race`). Confidence: causal mechanism established by control-flow analysis (two independent adversarial traces plus RCA line-level verification). **Not reproduced live** — two probe attempts failed for honest reasons (scheduling; environment rustc metadata mismatch). No implementation changes made.

## Condition

**Expected:** `close_database` serializes with in-flight operations or fails with a deterministic error; the public API never panics.

**Observed:** Three core operations unconditionally dereference the registry entry *after* awaiting the worker:

- `get_schema_with_ct` — `s.handles.get_mut(id).unwrap()` at `crates/sqlite-mcp-core/src/core.rs:806-812`
- `begin_transaction_with_ct` — same pattern at `core.rs:440-442`
- `rollback` — same pattern at `core.rs:457-459`

`close_database` (`core.rs:239-257`) removes the handle and identity entries under the lock, then awaits `w.shutdown()`. Because every operation first *clones* the `Worker` handle (`core.rs:369-377`), an operation suspended awaiting its worker response can complete after the registry entry is gone; when it resumes and reacquires the lock, `unwrap()` panics the Tokio task instead of returning a `CoreError`.

Correction to the original attack claim: `query_with_ct`'s post-await mutation bookkeeping is already guarded with `if let` (`core.rs:652-657`) and does not panic in this revision.

**Probe evidence:** a 40-iteration spawned-task race (get_schema vs close, `yield_now` interleaving) produced 0 panics — scheduling let the operation finish first. The RCA's /tmp probe did not execute (dependency metadata mismatch). A deterministic reproduction requires a worker-handover barrier (test-support), not a long query inside a transaction: `close_database` refuses handles with `transaction_id.is_some()`, so the long-query repro sketch is contract-incompatible.

## Cause

1. Operation passes the registry lookup and clones the `Worker`.
2. Its command is queued/executing; the async caller suspends awaiting the oneshot response.
3. `close_database` acquires the lock, removes the entries, and only then awaits shutdown. There is no in-flight refcount, closing state, or linearization protocol tying the clone to registry lifetime.
4. The worker finishes the queued command and sends its response although the registry entry is gone; FIFO `Shutdown` does not restore membership.
5. The caller resumes, reacquires the lock, and unwraps a missing key → panic/`JoinError` in the Tokio task. Close visibility relative to queued work is timing-dependent (no stable linearization point).

**Why existing tests missed it:** handle-serialization tests (`ac_concurrency.rs:64-106`) never remove a handle while an operation is suspended; lifecycle tests close only idle, fully-quiesced handles. Every test awaits each operation before the next lifecycle action, so the post-await relookup always succeeds.

## Proposed correction

- Introduce explicit close linearization in `Core` state: operations acquire a per-handle in-flight guard/refcount **before** cloning the `Worker` and release it after all post-await bookkeeping; `close_database` marks the handle closing (rejecting new guards), waits for the count to reach zero, then removes the entry and shuts the worker down. A per-handle async operation gate with exclusive close ownership is the alternative shape.
- Defensive secondary measure (necessary, not sufficient): replace the unconditional `unwrap()`s with an explicit `UnknownHandle` error. Alone this removes the panic but leaves nondeterministic close semantics and can report success for work that raced with close.

**Prevention control:** a barrier-based regression test using the existing test-support worker-handover event (not sleeps/yields) racing close vs `get_schema` on an idle handle, asserting no `JoinError` and a documented close outcome; an invariant review that every registry access after an await is guard-protected; protocol-level race coverage; documented close linearization semantics.

**Complexity:** Complex — requires a state-model change (guard lifetime across every `Worker`-cloning path, including `expire_handle`, `commit`, `rollback`, and global `shutdown`), wakeup/ordering decisions vs idle expiry, and new test infrastructure. Localized to core/worker lifecycle but cross-cutting in correctness.

**Residual risks:** live reproduction still outstanding; an incomplete guard rollout leaves sibling post-await races; close may block up to the query timeout under the wait policy (cancellation/shutdown interactions must stay truthful); `Worker::shutdown`'s fire-and-forget ack (`worker.rs:440-444`) interacts with removal ordering and needs explicit treatment.

## References

`crates/sqlite-mcp-core/src/core.rs:239-257, 369-377, 440-445, 457-462, 652-657, 683-812`; `crates/sqlite-mcp-core/src/worker.rs:440-444`; `crates/sqlite-mcp-core/tests/ac_concurrency.rs`, `ac_paths.rs`, `engine_lifecycle.rs`.

## Closure

Resolved by close publication/shutdown ordering, registry-safe post-await handling, and — after the independent lifecycle review — per-handle operation gates that serialize begin/commit/rollback/close/query/observe so a publishing begin can no longer race close (the registry-unwrap panic and false-success scenarios are structurally prevented). `close_waits_for_publication`, `close_racing_begin_refuses_active`, `close_reserves_identity_until_closed`, and `close_does_not_block_other_handles` are GREEN (regression_lifecycle 17/17).
RED evidence is recorded in `closure-evidence.json` (`F-01-02-red.log`); GREEN evidence is `F-01-02-green.log`.
Residual: the capacity-bounded FIFO admission queue (including close-pending quarantine) remains unimplemented — it bounds admission, which was never a proven live symptom. The publication half of the coordinator contract is implemented: each admitted operation runs in a core-owned coordinator that publishes exactly once even when the caller future is dropped (`drop_after_admission_publishes_once`, `dropped_commit_publishes_idle_state`, `dropped_query_expiry_publishes_tombstone` freeze the worker mid-operation, abort the caller, and assert authoritative state). Registry-entry removal racing a publication now reports `HANDLE_UNKNOWN` instead of panicking on `get_mut().unwrap()`.
