# Changelog

All notable changes are documented here from observed repository history. Release sections use the shared Cargo workspace version and exact `v{version}` tag.

Provisional first-release notes below reflect inspected history from root commit `2684862` through `d0e010d`. The completed four-platform release implementation will be included in a separate final notes-only commit after its implementation commit is inspected.

## [0.1.0] - 2026-09-07

### Added

- Initial local SQLite MCP server over stdio, with explicit handle-scoped database and transaction operations.
- Bundled SQLite and bounded, policy-checked SQL execution with schema observation and typed results.
- Production `--help`, `--version`, and `-V` CLI commands, with the version compiled from the shared Cargo workspace version.
- Tag-triggered native binary release workflow for Linux amd64 (`x86_64-unknown-linux-gnu`) and macOS arm64 (`aarch64-apple-darwin`), with executable archives and SHA-256 checksums.
- Developer-only release validation and changelog tooling, native binary and archive checks, and a history-backed release runbook.

### Platform notes

- The Linux GNU binary uses the Ubuntu 24.04 build environment's glibc baseline; it is not a musl/static universal Linux binary.
- The macOS arm64 binary is unsigned and unnotarized; Gatekeeper restrictions may apply.
