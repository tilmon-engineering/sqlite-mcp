# F-13 — Envelope state fidelity: close-during-transaction loses known handle state; `create_database` recovery hints are outcome-blind

**Severity:** Low
**Status:** RCA complete (Luna `rca-13-envelope-state`). Close-during-transaction envelope **live-captured verbatim** by the RCA. No implementation changes made.

## Condition

**Expected:** `handle_state` is null only when the handle is unresolved; for a known handle it carries the authoritative snapshot, error lifecycle booleans reflect reality, and `next_moves` are outcome-appropriate literal tool names (DESIGN.md:73).

**Observed (live envelope from production binary):** after begin, `close_database` returned:

```json
{"envelope_version":1,"error":{"class":"TX_ALREADY_OPEN","message":"transaction already open","transaction_continuable":false,"transaction_open":false},"handle_state":null,"next_moves":["open_database"]}
```

— while the preceding begin response showed the same handle with an active `deferred` transaction. The handler (`tools.rs:375-379`) passes `None` for every failure; neighboring handlers (`get_schema`, `begin_transaction`, `query`, `commit`, `rollback`) all look up and thread `list_handles().find(|h| h.id == a.handle)`. Additionally, `create_database` failures always advertise `next_moves: ["open_database"]` (`tools.rs:101-104`, shared `next()`) even for invalid paths, cancellation, or already-exists — guidance that does not follow from the outcome.

## Cause

The close handler discards the known registry snapshot; `next_moves` is computed from the tool name alone with no error/outcome input.

**Why tests missed it:** the envelope matrix commits before testing the no-transaction commit and closes only after a rollback; it asserts shape (valid tool names, result/error exclusivity), never semantic recovery correctness, non-null state for resolvable handles, or exact per-outcome moves.

## Proposed correction

- Close handler mirrors the handle-scoped pattern: look up the snapshot and pass it; for `TransactionOpen` report truthful `transaction_open: true` / `transaction_continuable: true` (worker-authoritative fields must still override the cached snapshot where present) and override `next_moves` to exactly `["commit", "rollback"]`.
- Make `create_database` guidance outcome-dependent: invalid path/policy or cancellation → no misleading `open_database` (usually `[]`); exclusive already-exists → `["open_database"]` only under a documented policy for that outcome; generic I/O → outcome-honest guidance. This may need a small create-specific recovery classifier since `CoreError::Path` lacks outcome detail.
- Upgrade `protocol_envelope_matrix.rs` from shape-checking to a contract matrix: resolvable handle ⇒ non-null snapshot; unknown ⇒ null; booleans agree with snapshot/authoritative fields; **exact** next moves per tool/error class — adding close-active, create invalid-path, create twice, and cancellation cases.

**Complexity:** low-to-moderate, localized. **Residual risks:** other close failures (expiry, invalidation) must select truthful moves rather than blindly implying commit/rollback; exact continuability semantics for the active-transaction case should be re-confirmed if lifecycle interpretations evolve; the create already-exists policy needs a decision.

## Closure

Resolved by authoritative handle snapshots, truthful lifecycle booleans, and outcome-specific recovery moves; `close_active_envelope_exact`, `create_failure_recovery_matrix`, and `structured_text_envelopes_equal` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-12-13-red.log` and `F-12-13-green.log`).
Residual: recovery guidance for less-common close failures and the already-exists policy remain bounded by the current contract.
