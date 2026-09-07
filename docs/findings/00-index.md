# Findings Corpus — sqlite-mcp adversarial review (debug facet, 2026-09-06)

Adversarial review of the completed initial implementation. **No implementation files were changed**; this directory is the handoff corpus for a separate planning/execution session. Baseline: `mise run ci` fully green immediately before review — every finding below therefore escaped the existing 46-test suite.

## Method

1. Four parallel adversarial review tracks (filesystem/lifecycle, SQL policy/state, concurrency/cancellation, protocol/config/envelopes) plus direct source inspection by the orchestrator.
2. Live reproduction attempts via throwaway probes (public core API crates under /tmp, production stdio binary probes, config/binary probes) — repository untouched.
3. One dedicated root-cause analysis subagent per finding (14 total), each returning Condition / Cause / Why-undetected / Correction / Prevention / Complexity / Residual risks. Reproduction status is stated per finding; non-reproductions are recorded honestly, not papered over.

## Index

| ID | Finding | Status / regression tests |
|---|---|---|
| [01](01-close-race-registry-unwrap.md) | close vs in-flight ops: post-await registry `unwrap()` panic; no close linearization | Resolved/closed — `close_waits_for_publication`, `close_racing_begin_refuses_active`, `close_reserves_identity_until_closed`, `close_does_not_block_other_handles` |
| [02](02-registry-lock-across-shutdown-await.md) | Registry mutex held across worker-shutdown awaits; shutdown serializes | Resolved/closed — `concurrent_shutdown_joins_all_workers`, `shutdown_report_scope_and_idempotence`, `close_failure_identity_reopen_matrix` |
| [03](03-shutdown-cancellation-and-exit-status.md) | Dead shutdown token (EOF waits out active query); failed session exits 0 | Resolved/closed — `shutdown_interrupts_progress_and_busy`, `stdio_eof_active_query_cleanup`, `stdio_sigint_cleanup`, `stdio_service_failure_nonzero`, `shutdown_cleanup_failure_reported`, `service_error_with_cleanup_failure_preserves_both`, `shutdown_report_joins_all_failures` |
| [04](04-config-validation-gaps.md) | 12 of 15 settings unvalidated; `usize→i32` wrap silently disables SQLite limits | Resolved/closed — `config_all_field_boundaries`, `config_policy_covers_serialized_fields`, `invalid_config_matrix_before_serving`, `engine_limit_readback_and_checked_conversion`, `configured_parameter_limit_single_source` |
| [05](05-create-database-fd-replacement-window.md) | create drops exclusive fd before init; no identity recheck | Resolved/closed — `create_replacement_checkpoint_matrix`, `create_descriptor_retained_through_close`, `create_identity_residual_contract` |
| [06](06-success-only-mutation-invalidation.md) | Failed attempted DDL leaves schema gate open (success-only flag drain) | Resolved/closed — `failed_ddl_invalidates_immediately`, `denied_stored_ddl_invalidates`, `queued_mutation_markers_isolated`, `dropped_caller_publishes_mutation_state`, `predispatch_rejection_keeps_observation` |
| [07](07-idle-expiry-accounting.md) | Commit succeeds past idle window; post-expiry commit says NO_TX_OPEN | Resolved/closed — `expired_direct_commit_never_persists`, `expired_followup_controls_exact_class`, `queued_activity_and_idle_boundary` |
| [08](08-cancelled-before-dispatch-executes.md) | Cancelled-while-queued requests still execute quick mutations | Resolved/closed — `queued_cancel_never_enters_closure`, `queued_deadline_preserves_transaction`, `interruption_precedence_matrix`, `expiry_rollback_failure_invalidates` |
| [09](09-result-cap-after-savepoint-release.md) | Byte-cap overflow detected after RELEASE → failed call's insert persists | Resolved/closed — `returning_byte_overflow_atomic`, `result_payload_exact_boundaries`, `result_encoding_boundary_matrix`, `returning_row_cap_still_drains`, `overflow_cleanup_failure_invalidates` |
| [10](10-schema-snapshot-tear.md) | get_schema reads without internal read transaction → torn observation | Resolved/closed — `schema_snapshot_external_ddl_interleaving`, `readonly_schema_snapshot_interleaving`, `schema_read_preserves_caller_transaction`, `schema_failure_cleanup_and_gate`, `schema_cleanup_failure_invalidates` |
| [11](11-argument-schema-laxity.md) | Unknown args ignored; `mode:"banana"` accepted and echoed | Resolved/closed — `closed_tool_schema_matrix`, `unknown_argument_dispatch_no_effects`, `invalid_mode_core_and_protocol`, `valid_mode_default_and_readonly` |
| [12](12-locked-class-misclassification.md) | SQLITE_LOCKED → INTERNAL envelope class (no Busy/Locked arms) | Resolved/closed — `core_error_envelope_variant_matrix` |
| [13](13-envelope-state-fidelity.md) | close-during-tx envelope: null state, false booleans, wrong next_moves | Resolved/closed — `close_active_envelope_exact`, `create_failure_recovery_matrix`, `structured_text_envelopes_equal` |
| [14](14-numeric-literal-preflight.md) | Digit-string heuristic: arbitrary rejection misreported as byte limit | Resolved/closed — `bare_digits_are_parser_errors`, `sql_utf8_byte_boundaries`, `numeric_select_values_execute` |

## Cross-cutting themes

- **Await-then-mutate state races** (01, 02): registry bookkeeping after `.await` without guards or scoped locks.
- **Control path excluded from invariants** (02, 07, 08): expiry, cancellation, and shutdown gates exist only on the `Run` path; `commit`/`rollback` bypass them.
- **Post-outcome validation/ordering** (06, 09): checks performed after the point where they can still restore atomic state.
- **Contract/implementation drift in envelopes and schemas** (11, 12, 13, 14): message-string classification, unconstrained schemas, outcome-blind guidance.
- **Config as unenforced policy** (04): validation stops at three fields; FFI casts silently disable engine limits.

## Handoff notes for the planning session

- Highest-value first fixes: 07, 08, 09 (data-truthfulness, live-proven), then 03, 04, 01.
- F-01's correction is **complex** (in-flight guard state model); F-02/F-14/F-12 are small; the rest are low-to-moderate.
- Several fixes interact: shutdown cancellation (03) needs one precedence rule with request cancellation (08); expiry gating (07) belongs in the same central gate as the cancelled/deadline dispatch check (08); worker shutdown completion truthfulness (02 adjacent note) should be planned with 03.
- Reproduction gaps that remain open (01, 05, 06, 10) need test-support barriers/events before they can be closed deterministically; the RCAs specify the hook designs.
