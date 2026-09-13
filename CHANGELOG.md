# Changelog

All notable changes are documented here from observed repository history. Release sections use the shared Cargo workspace version and exact `v{version}` tag.

First-release notes were finalized from inspected history spanning root commit `2684862` through implementation commit `5d35c82`; the subsequent notes-only commit is not part of that inspected history.

## [0.1.0] - 2026-09-13

### Added

- Initial local SQLite MCP server over stdio, with explicit handle-scoped database and transaction operations.
- Bundled SQLite and bounded, policy-checked SQL execution with schema observation and typed results.
- Production `--help`, `--version`, and `-V` CLI commands, with the version compiled from the shared Cargo workspace version.
- Native binary release workflow for Linux x86_64 and Arm64 (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) and macOS Intel and Apple Silicon (`x86_64-apple-darwin`, `aarch64-apple-darwin`), with executable archives and SHA-256 checksums.
- Draft-first publication with exact asset and changelog checks, immutable annotated tags, and native packaged-binary smoke tests.
- Anonymous public release metadata and downloaded-binary verification, bounded HTTPS-only downloads, and retained CI metadata evidence.
- Developer-only release validation and changelog tooling, checksum-verified installation instructions, and a history-backed release runbook.

### Platform notes

- Linux GNU builds use Ubuntu 24.04 and its glibc environment; they are not musl/static universal Linux binaries and do not establish older-distribution compatibility.
- macOS builds target native macOS 15 runners. Binaries are unsigned and unnotarized; Gatekeeper restrictions may apply. Older macOS compatibility and interactive Gatekeeper installation are not established by CI.
