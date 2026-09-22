# Authorized system access

## Workstation sqlite-mcp deployment

**Recorded:** 2026-09-22

The operator preauthorizes agents working in this repository to deploy a reviewed and verified `sqlite-mcp` build from this repository to this workstation and to update this repository's project-local Polytoken configuration so the `sqlite-mcp` MCP server executes that deployed version.

### Authorized scope

- Install the verified binary beneath the active operator user's `${HOME}/.local/share/sqlite-mcp/<version>/sqlite-mcp` directory.
- Update only the `mcp_servers.sqlite-mcp.command` value in this repository's `.polytoken/config.yaml` to the exact absolute path of that versioned binary.
- Disable and re-enable this repository's `sqlite-mcp` MCP child when needed to make the configured version active.
- Create and later remove temporary install files and uniquely named rollback backups required for an atomic, verified deployment.
- Run bounded version, hash, process-attribution, and disposable-database MCP smoke checks against the deployed child.

This preauthorization applies to future deployments from this repository as well as the currently approved `0.2.0` deployment, provided the candidate has passed this repository's required checks and any task-specific review gates.

### Required safety gates

- Resolve the active daemon's runtime `HOME` and effective project config; do not guess paths.
- Before changing an executable that may be running, disable the exact configured MCP child and verify zero matching attributed children.
- Accept only user-owned regular non-symlink candidate, destination, temporary, and backup files. Use atomic finalization and verify candidate and installed hashes.
- For replacement, retain a verified backup until restart, smoke testing, and required tracker work succeed. For first install, use no-clobber finalization and remove only an exactly identified failed candidate during rollback.
- After enabling, require exactly one child attributed to the active daemon whose command and opened executable match the configured versioned path and reviewed hash.
- If config, process, destination, candidate, or rollback identity is ambiguous, stop without overwriting or deleting unknown state.

### Boundaries

- Do not modify other MCP server entries, global Polytoken configuration, credentials, permissions, or unrelated binaries.
- Do not touch a separately owned or separately configured sqlite-mcp process, including the existing `0.1.0` installation, unless separately authorized.
- This does not authorize publishing a release, changing tags, weakening repository checks, installing an unreviewed build, or broadening sqlite-mcp's product capabilities.
- Report the configured path, installed version and hash, process verification, smoke-test result, retained backup state, and any rollback or unresolved risk.
