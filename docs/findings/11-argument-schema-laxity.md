# F-11 — Tool argument schemas are laxer than the contract: unknown keys silently ignored; `begin_transaction.mode` accepts any string

**Severity:** Medium
**Status:** RCA complete (Luna `rca-11-arg-schema-laxity`, including rmcp 1.7.0 / rmcp-macros 1.7.0 / schemars 1.2.2 source tracing). Both symptoms **live-confirmed** against the production binary (session probe and RCA probe). No implementation changes made.

## Condition

**Expected:** `mode` is the enum `deferred|immediate` (DESIGN.md:21, README); argument objects reject unknown members (typed public API; typos like `"readnly"` must not be silently coerced).

**Observed (live):**
- `open_database` with extra key `totally_unknown_key: true` dispatched normally and returned a handle. The five argument structs (`tools.rs:55-83`) lack `#[serde(deny_unknown_fields)]`, and the advertised `tools/list` schemas carry no `additionalProperties`.
- `begin_transaction` with `mode: "banana"` **succeeded**, executed `BEGIN DEFERRED`, and echoed `"banana"` into both the result and `handle_state.transaction_mode`. `BeginArgs.mode` is an unconstrained `String`; `core.rs:396` treats everything except (case-insensitive) `"immediate"` as deferred, and `core.rs:441-444` stores the arbitrary lowercased input.
- Mechanism (RCA, registry-source verified): rmcp-macros generates the schema via `schema_for_type::<Parameters<T>>()` delegating to `T::json_schema`; dispatch deserializes with serde, which ignores unknown fields by default. Schemars 1.2.2 propagates `deny_unknown_fields` to `additionalProperties: false` (covered by its integration tests). Required/type mismatches already fail with `-32602` — the only invalid-argument coverage that exists.

## Cause

Lenient serde struct defaults (open objects) plus a free-form `String` for a two-valued field, with no core-side whitelist either (`ReadonlyImmediate` is the only mode check).

**Why tests missed it:** protocol tests cover required/type failures and the default mode only; the test comment overstates "SDK-level schema validation" — unknown keys and unconstrained strings are valid under the generated schema.

## Proposed correction

1. `#[serde(deny_unknown_fields)]` on all five argument structs (expected to propagate to `additionalProperties: false` via schemars; verify the emitted schema, with an explicit `input_schema` fallback if not).
2. Replace `mode: String` with a lowercase enum `{deferred, immediate}` (serde default `deferred`) → invalid strings fail at dispatch with `-32602`.
3. Keep a core-side whitelist for the public `Core::begin_transaction(&str, …)` API: new `CoreError::InvalidTransactionMode` → stable class `INVALID_TRANSACTION_MODE`, rejected before any worker command. **Unknown mode must error, not silently default** — the mode changes lock-acquisition and contention behavior, so coercing a typo violates caller intent. Decide case policy (canonical lowercase recommended; document any aliases).
4. Contract tightening is intentional; note it in DESIGN.md/README per repository discipline.

**Prevention:** protocol test asserting `additionalProperties == false` for every argument-bearing tool schema and enum fields match documented values; hostile dispatch probes (unknown key → `-32602`, no handle created; `banana` → `-32602`); core-level invalid-mode → `InvalidTransactionMode` with no transaction opened; valid modes unchanged.

**Complexity:** low-to-medium, localized (`tools.rs` + one core error + tests). **Residual risks:** post-attribute schema output not yet compile-verified (conclusion based on schemars' tests); rejecting previously-ignored extras and uppercase modes is a deliberate compatibility change; only representative structs were live-probed.

## Closure

Resolved by closed argument schemas, lowercase mode parsing, and core admission validation; `closed_tool_schema_matrix`, `unknown_argument_dispatch_no_effects`, `invalid_mode_core_and_protocol`, and `valid_mode_default_and_readonly` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-11-red.log` and integrated GREEN evidence in the manifest).
Residual: rmcp 1.7/schemars default-key behavior remains a documented compatibility limitation; functional default behavior is covered.
