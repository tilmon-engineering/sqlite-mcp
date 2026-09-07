# F-14 — Undocumented content-based preflight: pure-numeric SQL longer than 8 characters rejected with a false "byte limit" error

**Severity:** Low
**Status:** RCA complete (Luna `rca-14-digit-heuristic`, including a direct bundled-SQLite parser probe). Live-confirmed by session probe. **RCA corrected the original premise** — details below. No implementation changes made.

## Condition

**Expected:** the only documented content restriction on `sql` is the positive `sql_byte_limit` byte cap (DESIGN.md/README); size failures are reported as size failures.

**Observed:** `query_with_ct` (`core.rs:497-503`) rejects when `sql.len() > sql_byte_limit` **or** when the non-empty SQL is entirely ASCII digits and longer than 8 bytes — independent of any configured limit — returning `"SQL exceeds configured bytes limit"` semantics ("SQL exceeds configured byte limit"). Live: `"123456789"` → that error; control `"SELECT 123456789"` → ok. The threshold is exactly 9 bytes; any non-digit byte defeats the predicate.

**Premise correction (RCA, bundled SQLite 3.53.2 probe):** bare digit text is **not a valid SQLite statement at all** — `prepare("123456789")` (and 8-digit, signed, decimal, hex, semicolon variants) fails with a syntax error at offset 0. So the heuristic suppresses no executable functionality; the genuine defects are (a) an **undocumented content-based rejection** diverging from the contract, and (b) a **misattributed error** — a parser-level input error reported as a configured-size failure, which misleads clients into retrying with "shorter SQL".

## Cause

A leftover over-broad guard (provenance unverifiable — the directory has no git history); it shares the error string with the legitimate length check.

**Why tests missed it:** coverage uses a prefixed literal (`SELECT 123456789`), never digit-only input; no test asserts acceptance depends solely on byte length or asserts cap boundaries.

## Proposed correction

Delete the digit clause; retain only `sql.len() > self.config.sql_byte_limit`. Do **not** implement bare-numeric-statement execution — SQLite's parser rejects it; that would be a separate product decision.

**Prevention:** regression tests: (1) high cap + 9-digit-only input → normal SQLite syntax/policy error, *not* the byte-limit message (fails against current code); (2) 8-digit boundary documented; (3) valid statements at cap−1/cap/cap+1 with the exact byte-limit message at cap+1, including multibyte UTF-8 to pin byte-vs-char semantics; (4) a content-equivalence assertion (equivalent-size statements treated alike absent an explicit documented rule). Review guidance: any intentional SQL-content restriction must be a named, documented helper — not an anonymous clause in a compound boolean.

**Complexity:** low — one-line deletion plus focused tests. **Residual risks:** the post-fix server error for bare digits is inferred from the identical prepare path (verified directly against the bundled library), not yet exercised through `Core::query`; behavior claims hold for the pinned rusqlite 0.40.1 / SQLite 3.53.2 only.

## Closure

Resolved by removing the digit-content heuristic while retaining byte-length preflight; `bare_digits_are_parser_errors`, `sql_utf8_byte_boundaries`, and `numeric_select_values_execute` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-14-red.log` and `F-14-green.log`).
Residual: bare-digit behavior is pinned to the bundled rusqlite/SQLite versions and parser path.
