# F-03 — Dead worker-shutdown token (shutdown cannot cancel an active query) + untruthful exit status after service failure

**Severity:** High
**Status:** RCA complete (Luna `rca-03-shutdown-token`). Token deadness verified from source; **exit-status defect live-reproduced**. No implementation changes made.

## Condition

**Expected:** EOF/Ctrl-C shutdown cancels active query work, awaits resolution, then rolls back and closes (DESIGN.md). A failed MCP session must not exit 0.

**Observed:**
1. **Dead token.** `Worker::start` creates `cancel: CancelToken` (`worker.rs:132-134`) but the worker thread never reads it. Active `Command::Run` installs only the per-request `ctx.token` into `busy_retry` (via `BUSY_CONTEXT`) and the progress handler (`worker.rs:203-216`). `Worker::shutdown` cancels `self.cancel` and enqueues FIFO `Command::Shutdown` (`:440-443`), which cannot dequeue until the active Run returns. Net: EOF during an active query waits up to the request deadline (default 30 s, longer for work not reaching a callback).
2. **Exit 0 on failure — REPRODUCED.** `serve` (`main.rs:47-50`) maps `running.waiting().await` to a message and discards it with `let _ =`, then returns `Ok(())` → `ExitCode::SUCCESS`. Live probe: initialize + initialized followed by malformed JSON → parse-error response delivered, process exited **0** with empty stderr.

## Cause

(1) Ownership/wiring: the worker-level token has no consumer; the busy context carries only the request token, and FIFO ordering cannot preempt an executing Run. (2) The service wait result is treated as observational rather than control flow.

**Why existing tests missed it:** shutdown tests close stdin only after all operations complete; subprocess tests rarely assert `ExitStatus`; cancellation tests exercise exactly the wired `ctx.token` path.

## Proposed correction

- Wire shutdown into both cancellation points: condition becomes `request_cancelled || shutdown_cancelled || deadline_elapsed` in **both** the progress handler and the busy handler (a lock-waiting query must also wake). Classify the interruption cause truthfully: shutdown-caused interruption is not `by_client: true`; define deterministic precedence between client-cancel and shutdown; deadline remains distinct. Cleanup rollback stays non-cancellable and awaited.
- Propagate the wait error: capture the result, always run `core.shutdown().await`, then return the original error → exit 1. Verify normal EOF still maps to success so ordinary shutdown is not converted into failure.

**Regression tests:** (a) core: active long query + `core.shutdown()` completes well under the request timeout, worker exits, transaction rolled back — include a lock-waiting variant so the busy path is covered; (b) subprocess: EOF mid-query exits within a bounded prompt interval with no uncommitted changes; (c) mid-session transport failure → nonzero exit + stderr, with a separate normal-EOF → 0 test.

**Complexity:** Medium, cross-crate. **Residual risks:** progress callbacks are cooperative (long native operations can still delay); shutdown/client-cancel precedence races need explicit rules; the exact error taxonomy of rmcp 1.7 `waiting()` should be confirmed against the pinned API; shutdown-ack semantics tie into F-02's adjacent finding.

## Closure

Resolved by wiring shutdown cancellation, awaited cleanup, SIGINT handling, and truthful service/cleanup failure propagation; `shutdown_interrupts_progress_and_busy`, `stdio_eof_active_query_cleanup`, `stdio_sigint_cleanup`, `stdio_service_failure_nonzero`, `shutdown_cleanup_failure_reported`, `service_error_with_cleanup_failure_preserves_both`, and `shutdown_report_joins_all_failures` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-03-red.log` and `F-03-green.log`).
Residual: rmcp 1.7 logs response-write failures without ending the session, so coverage uses initialize-failure plus injected `ConnectionRollback` fault; that fault is Rust-test-API only.
