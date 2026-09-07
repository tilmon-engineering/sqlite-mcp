# Changelog

All notable changes are documented here from observed repository history. Release sections use the shared Cargo workspace version and exact `v{version}` tag.

## [0.1.0] - 2026-09-07

> **Provisional:** these notes cover the observed history through the initial commit `2684862` only. After the implementation commit, inspect that observed history and then create a separate notes-only commit; do not claim the notes-only commit was inspected before it existed.

### Added

- Initial local SQLite MCP server over stdio, with explicit handle-scoped database and transaction operations.
- Bundled SQLite and bounded, policy-checked SQL execution with schema observation and typed results.
- Production `--help` and `--version` CLI commands using the compiled Cargo package version.
