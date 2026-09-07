# F-05 — `create_database` drops the exclusive descriptor before initialization; no identity recheck (replacement window)

**Severity:** Medium
**Status:** RCA complete (Luna `rca-05-create-fd-window`). Source-level TOCTOU verified; **not reproduced live** (micro-window; honestly reported — a rename-loop probe proves nothing on a miss, and no interposer/hook exists). No implementation changes made.

## Condition

**Expected:** creation retains the exclusive fd through initialization and compares device/inode before and after SQLite opens/initializes, rejecting detected replacement without touching the replacement and never unlinking a path that may now belong to someone else (DESIGN.md:37-41).

**Observed:** `create_database_with_ct` (`core.rs:125-135`) runs `create_new` → **`drop(f)`** (line 130) → `Connection::open(&target)` by pathname (131) → WAL/`user_version` initialization (132-133). `paths::create_target` canonicalizes only the parent. Between descriptor close and the pathname reopen (and again across SQLite's internal opens), a directory writer can unlink/rename/symlink-swap the pathname; SQLite then initializes whichever inode the pathname now denotes, and the tool returns success describing a database other than the exclusively created one. The design's "not hostile-filesystem-proof" qualification does not excuse omitting the specifically required creation-time check.

## Cause

Descriptor lifetime ends before the pathname reopen; no fstat/device-inode comparison exists at any point (against the created object or the opened connection); initialization SQL runs before any identity validation.

**Why tests missed it:** `exclusive_create_race` (`ac_paths.rs:46-63`) races only two same-process creators; there is no replacement actor, no identity assertion, and test-support has no creation-phase barrier to pause at the vulnerable boundary.

## Proposed correction

- Retain the `File`; capture `(dev, ino)` immediately after `create_new`.
- Obtain the identity of the database file SQLite actually opened at **fd level** (a pathname `stat` after open can itself race), and compare before executing any initialization SQL; on mismatch, fail closed, initialize nothing, never unlink.
- Recheck identity after initialization per DESIGN.md.
- Implementation caveats (from RCA): `/proc/self/fd/N` reopen breaks SQLite `-wal`/`-shm` sidecar naming — do not adopt without validating sidecar placement; strict "never touch the replacement" semantics across SQLite's internal open sequence may require VFS-level binding of the opened main file, with the residual window explicitly documented if only a bounded check sequence is feasible.

**Prevention:** test-support events `CreateAfterExclusiveOpen`, `CreateAfterSqliteOpenBeforeInit`, `CreateAfterInitBeforeFinalIdentityCheck` (Rust-only, marker-gated); sentinel-swap test asserting a replacement/identity error, untouched sentinel bytes, only the original inode initialized, and the replaced pathname not removed — asserting inode identity, not path existence.

**Complexity:** root-cause confirmation low; identity capture medium; strict no-touch guarantee medium-high (VFS/platform work); deterministic test instrumentation medium. Documentation sync required.

**Residual risks:** empirical reproduction rate unknown; retaining the fd prevents inode-reuse effects but not directory-entry replacement; replacement during SQLite's internal opens is observable only post-hoc without VFS binding; Linux-specific paths must fail closed elsewhere.

## Closure

Resolved by retaining identity through initialization and checkpoint detection; `create_replacement_checkpoint_matrix`, `create_descriptor_retained_through_close`, and `create_identity_residual_contract` are GREEN.
RED/GREEN evidence is recorded in `closure-evidence.json` (`F-05-red.log` and `F-05-green.log`).
Residual: this is checkpoint detection only, not proof against a hostile replacement race.
