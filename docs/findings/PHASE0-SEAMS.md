# Phase 0 frozen seams and dispatch contract

Authoritative supplement to the approved plan for all five finding owners.
Baseline: `mise run ci` green (26 test-result lines, no failures). No Git
repository exists, so evidence fingerprints use SHA-256 file hashes.

## Shared declarations (already in the tree, orchestrator-owned)

`crates/sqlite-mcp-core/src/operation.rs` (re-exported at crate root):
`OperationKind{Query,Schema,Begin(TransactionMode),Commit,Rollback,Expire,Close}`,
`InterruptionCause{Client,Deadline,Shutdown}`, `TransactionMode` with exact
lowercase `parse`/`render` (no uppercase aliases), `OperationOutcome<T>` with
`result/transaction_open/transaction_continuable/expired/invalidated/
mutation_attempted/cleanup_error` (autocommit forces lifecycle false;
invalidated implies non-continuable), `CleanupFailure{stage,
known_transaction_open,known_invalidated,detail}` + `CleanupStage`,
`PublishAction`, `OperationRecord`, `WorkerJob = worker::Job` (now `pub`),
`ShutdownStatus{connection_closed,thread_joined}` with `is_success()`,
`WorkerShutdownError{RollbackUncertain,CloseUncertain,JoinUncertain}`,
`ShutdownReport` with sorted `entries` + `registry_cleanup_errors` and derived
`is_success()`, `EffectiveLimits`, `LimitInstallError`. Pin test:
`operation::shared_seams_compile::shared_seams_compile` (green).

`crates/sqlite-mcp/src/lib.rs`: `pub async fn serve_with_transport<T, E, A>(
config: Config, transport: T) -> Result<(), ServeFailure>` with
`T: rmcp::transport::IntoTransport<RoleServer, E, A>`,
`E: Error + Send + Sync + 'static`; `ServeFailure{primary, cleanup_errors}`
and `serve_stdio(config)` wrapper. `main.rs` CLI/config parsing unchanged.

## Reviewer decisions incorporated (binding)

1. **Shutdown API is async.** `Worker::shutdown(&self) -> impl Future<Output =
   Result<ShutdownStatus, WorkerShutdownError>>`, `Core::shutdown(&self)` async
   returning `ShutdownReport`, `McpServer::shutdown` async forwarding. Blocking
   SQLite thread joins must happen off Tokio worker threads (`spawn_blocking`
   or dedicated-thread join). L implements; P updates the tools.rs forwarder to
   the frozen async signature.
2. **Feature-explicit core regression commands.** Hook-dependent core tests
   run as `mise exec -- cargo test --locked -p sqlite-mcp-core --features
   test-support --test <binary>`. Pure policy/config tests may omit the
   feature. Binary-crate tests already get the feature via its dev-dependency.
3. **Production serving seam** is `serve_with_transport` (above). The
   instrumented child (`crates/sqlite-mcp/tests/harness.rs`, entry
   `harness_child`) passes a test-owned TCP transport; production `main` uses
   `stdio()`. Do not duplicate or copy serving orchestration.

## Deterministic infrastructure (already in the tree, orchestrator-owned)

`test_support` now provides keyed lossless events with generations
(`EventKey::operation(id).with_generation(g)`, `arm_keyed`, `wait_keyed`,
`emit_keyed`, `arm_matches_pending`, `release_arm`, `gate_released`, `count`,
`ALL_EVENTS`), bounded waits everywhere, bounded worker-side barrier release
(10s), `reset_registry()` (child entry), one-shot cleanup-fault injection
(`inject_cleanup_fault`/`take_cleanup_fault` keyed by `CleanupStage`), and a
clock gated on feature+marker (`now_ms` ignores injected values without the
marker; production artifacts are unaffected). New events available:
`AdmissionEnqueue/Dequeue`, `ClosureEntry`, `PostWorkerPrePublication`,
`ControlEntry/Return`, `ShutdownRequested/Cleanup/Closed`,
`SchemaVersionRead`, `CreationDescriptorCaptured`,
`CreationPreOpenCheckpoint`, `CreationPostOpenCheckpoint`,
`CreationPostInitCheckpoint`. Existing events/`emit` unchanged.

Child control protocol (see `tests/harness.rs`): `arm/emit/wait/release/
clock/now/count/serve/exit` commands, one reply per command (`started` for a
gate-pausing emit on a background thread). Parent helper:
`support::harness::ChildHarness` (nonce handshake, bounded waits, kill/reap on
failure). Production artifact for inert-proof:
`mise exec -- cargo build --locked -p sqlite-mcp --target-dir
target/production-verification` (verified: core features `["default"]`, bin
`[]`) — already built and `production_test_hooks_inert` is green.

## TDD and evidence protocol (mandatory)

1. Compute `sha256sum` of every production file you will change BEFORE
   editing; include in your RED report (pre-fix fingerprint).
2. Author your named regression tests in your exclusive regression files.
   Add required hook call sites if a test cannot fail-for-the-right-reason
   without them, but no behavioral correction.
3. Run your focused tests (feature-explicit command when hooks are involved),
   retain output:
   `mise exec -- cargo test --locked -p <crate> [--features test-support]
   --test <binary> 2>&1 | tee
   /home/edwards/.local/share/polytoken/sessions/0adz7t-tidal/evidence/<F-id>-red.log`
   Each named test must FAIL with an assertion attributable to the original
   defect (not compile error, missing hook, or broken-harness timeout).
4. **STOP and report.** Send: per-finding test name, command, exit status,
   failing assertion excerpt, log path, pre-fix hashes. Do NOT implement any
   fix. The orchestrator records RED in
   `docs/findings/closure-evidence.json` and authorizes GREEN in a follow-up
   message. No finding may move to GREEN without recorded RED.
5. Owners run focused tests only; never a full workspace CI run. No dependency
   changes, no cross-owner production edits, no edits to shared files
   (operation.rs, test_support.rs, lib.rs, manifests, docs). API-change needs
   go through the orchestrator (old/new signature, dependents, impact).

## Ownership (exclusive files)

- **L** F-01/02/03/07/08: `core.rs`, `handles.rs`, `worker.rs` (not
  worker/limits.rs), binary `main.rs`+`lib.rs`; regressions: core
  `regression_lifecycle.rs`, binary `regression_shutdown.rs`,
  `regression_expiry.rs`. L also owns begin-mode wiring via
  `TransactionMode::parse` before admission.
- **Q** F-06/09/10/14: `core/query.rs`, `core/schema_ops.rs`, `policy.rs`,
  `schema.rs`; regressions: core `regression_query_schema.rs`,
  `regression_result_atomicity.rs`. Q implements `MutationSignal`
  (`reset_for_request`/`mark_attempt`/`take_after_request`) in policy.rs and
  consumes `sql::validate(sql, params, parameter_limit)` (C's seam).
- **C** F-04: `config.rs`, `sql.rs` parameter validation, `worker/limits.rs`,
  `config.example.toml`; regressions: core `regression_config_limits.rs`,
  binary `regression_config_startup.rs`. C supplies `install_limits(conn,
  &WorkerLimits) -> Result<EffectiveLimits, LimitInstallError>` in
  worker/limits.rs (L calls it) and threads `parameter_limit` through
  `sql::validate`.
- **F** F-05: `core/create.rs`, `paths.rs`; regression: core
  `regression_create_identity.rs`. F adds checkpoint hook call sites emitting
  `Creation*Checkpoint` events (needed for deterministic RED) without changing
  descriptor lifecycle yet.
- **P** F-11/12/13: `crates/sqlite-mcp-core/src/tools.rs`, `envelope.rs`,
  `core/error.rs`, new transaction-mode module; regressions: core
  `regression_protocol_contract.rs`, `regression_envelope_contract.rs`. P
  consumes typed outcomes; never parses messages to infer lifecycle.

Frozen admission/shutdown/close semantics, per-finding implementation
requirements, and the approved config ceilings are in the plan
(`/home/edwards/.local/share/polytoken/sessions/0adz7t-tidal/plan-002.md`).
