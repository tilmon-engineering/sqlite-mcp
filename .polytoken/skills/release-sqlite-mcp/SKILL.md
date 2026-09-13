---
name: release-sqlite-mcp
description: Release sqlite-mcp binaries from a history-backed, immutable tag with shared Cargo version discipline.
---

# Release sqlite-mcp

Use this runbook for an authorized release. It covers the two-commit history sequence, shared version discipline, changelog extraction, branch CI, and immutable tag recovery. Do not publish, push, or move a tag without explicit authorization.

## Preconditions and version discipline

1. Start from a clean tree and verify the GitHub host, repository, account, permissions, and HTTPS remote access. Inspect the configured remote, then use an explicit HTTPS URL for release operations; do not expose credentials in URLs:

   ```sh
   git remote -v
   git remote set-url origin https://github.com/tilmon-engineering/sqlite-mcp.git
   git fetch --tags origin
   git status --short
   ```

   If the repository or account differs from the authorized target, stop. Fetching is safe; do not fetch or push arbitrary remotes.
2. Identify the previous published release tag that is an ancestor of the current branch. Inspect candidates with `git tag --list 'v*' --sort=-version:refname` and verify each candidate with `git merge-base --is-ancestor TAG HEAD` plus `gh release view TAG --json tagName,targetCommitish,isDraft,publishedAt`. Treat `publishedAt` being non-null and `isDraft == false` as the publication check; do not use the obsolete publication-status field. For the first release there is no prior published ancestor: inspect history from the repository root and use observed commits only; do not infer a release from a chronological tag that is not an ancestor.
3. The workspace Cargo version is the source of truth. In the root `Cargo.toml`, set the shared `[workspace.package]` version; preserve `version.workspace = true` in both `crates/sqlite-mcp/Cargo.toml` and `crates/sqlite-mcp-core/Cargo.toml` (do not replace inheritance with literals). Regenerate the lockfile with `cargo check --workspace --locked` only after the intended lockfile change is available, or, when the version change requires lockfile rewriting, run `cargo check --workspace` and then verify with `cargo check --workspace --locked`. Review the resulting `Cargo.lock` entries for both product packages. Do not add a tag-injected override.
4. Choose SemVer deliberately for pre-1.0 releases: use `0.MINOR.PATCH`; increment PATCH for compatible fixes, documentation, and internal changes; increment MINOR for additive user-visible features or compatibility-affecting changes that remain intentionally pre-1.0; reserve `1.0.0` for an explicitly approved stability commitment. Any intentional breaking API/CLI/core contract change below 1.0 requires an explicit version decision and release review rather than an automatic patch bump.
5. The release tag is exactly `v{workspace version}`. Validate the tag, manifests, metadata, and lockfile before producing assets; never accept a non-`v` spelling or a tag whose version differs from Cargo.

## Two-commit history sequence

1. Make and review the implementation/version/lockfile commit with provisional changelog notes based only on already-observed history.
2. Inspect history including that implementation commit. For a subsequent release, review `git log PREVIOUS_TAG..HEAD` and relevant diffs; for the first release, review from the root. Summarize actual user-visible app/core changes, not future promises or test-count claims.
3. Make a separate final release-notes commit. Do not claim that the notes-only commit was part of the history inspected to write its own notes.
4. Run checks against the final notes commit and record both SHAs. Push `main` normally, then wait for successful branch CI at that exact final notes SHA across all four native runner/target rows. Only after that gate succeeds, create an annotated `v{version}` tag at the exact tested SHA and push only that exact tag. Never tag or push before the branch gate.

## Verification and commands

Run the full repository gate before tagging. The required CI command is `mise run ci`; do not substitute an unrun claim for its output. Also validate the local skill and release-specific checks with:

```sh
mise run ci
polytoken validate skill .polytoken/skills/release-sqlite-mcp/SKILL.md
cargo run --locked -p xtask -- release-check TAG
cargo run --locked -p xtask -- release-notes TAG OUTPUT_PATH
mise release-check TAG
mise release-build
mise workflow-check
```

Use the exact command names and pass tag/path values as arguments, not shell source. Before committing, record the implementation SHA and then the final notes-only SHA:

```sh
git rev-parse HEAD
git log --oneline PREVIOUS_TAG..HEAD   # omit the range for the first release
# commit implementation/version/lockfile changes, then inspect that commit
git rev-parse HEAD
# commit final changelog notes separately
git rev-parse HEAD
```

After the final notes commit passes checks, derive and verify the exact tag and tested SHA using quoted shell variables:

```sh
PACKAGE_ID="$(cargo pkgid --locked -p sqlite-mcp)"
VERSION="${PACKAGE_ID##*@}"
TAG="v${VERSION}"
TESTED_SHA="$(git rev-parse HEAD)"
test "$(git rev-parse HEAD)" = "$TESTED_SHA"
test "$TAG" = "v${VERSION}"
test -z "$(git tag --list "$TAG")"  # stop if the immutable tag already exists
git tag -a "$TAG" "$TESTED_SHA" -m "Release $TAG"
test "$(git rev-list -n 1 "$TAG")" = "$TESTED_SHA"
git show --no-patch --format='%H %D' "$TAG"
```

`cargo pkgid` supplies the validated workspace package version; `release-check` remains authoritative for manifest, lockfile, changelog, and tag policy. Push the branch over HTTPS first, without force, and push only the exact tag (never all tags):

```sh
git push origin HEAD:main
git push origin "$TAG"
```

Use `gh` to verify the branch run before tagging and the tag run/release after tagging. These commands inspect rather than mutate releases:

```sh
gh run list --repo tilmon-engineering/sqlite-mcp --branch main --limit 10
gh run watch RUN_ID --repo tilmon-engineering/sqlite-mcp --exit-status
gh run list --repo tilmon-engineering/sqlite-mcp --workflow release.yml --limit 10
gh run watch TAG_RUN_ID --repo tilmon-engineering/sqlite-mcp --exit-status
gh release view "$TAG" --repo tilmon-engineering/sqlite-mcp --json tagName,targetCommitish,isDraft,publishedAt,body,assets
```

Confirm the watched branch run is successful before tag creation, the tag run references `TESTED_SHA`, the release is non-draft only when publication is authorized, and assets/body/checksums match the exact notes. Validate the actual production binary's compiled version and ensure stdout remains protocol-only except for explicit CLI help/version output.

## Native assets, downloads, and platform limits

Build exactly four native rows: `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` on Ubuntu 24.04 GNU/glibc runners, plus `aarch64-apple-darwin` on macOS 15 arm64 and `x86_64-apple-darwin` on macOS 15 Intel. Select downloads using both `uname -s` and `uname -m`. Publish exactly the target-named archives and `SHA256SUMS`; each archive must contain only the executable member `sqlite-mcp`, with checksums verified before extraction or execution.

The GNU Linux binaries have the Ubuntu 24.04 runner’s glibc baseline; do not describe them as musl/static or universally portable. macOS binaries are unsigned and unnotarized; Gatekeeper may require explicit user approval. Do not promise frictionless installation, older-macOS compatibility, or recommend disabling Gatekeeper globally.

Anonymous manual downloads must use the versioned GitHub release URL and curl flags `--disable --proto '=https' --proto-redir '=https' --fail --location --silent --show-error`, with no credentials or shell piping. Verify the selected checksum line and exact archive member before installing with `install -m 0755` into a configurable temporary/user-owned `TEMP_INSTALL` prefix. On macOS use `shasum -a 256 --check` instead of `sha256sum --check`. The developer-only commands `verify-public-release TAG EXPECTED_SHA EVIDENCE_PATH`, `fetch-public-assets TAG ASSET_DIR`, and `verify-downloads TAG ASSET_DIR BINARY_PATH` provide read-only metadata, asset, and local executable checks.

Branch CI must pass before creating the tag. Push `main`, wait for successful CI at the exact final notes SHA across all four native rows, then create the immutable annotated tag and push only that exact tag. Tag CI must validate the exact tagged SHA before publication. Never move or overwrite a published tag or release.

## Recovery

If a tag or release already exists, stop and inspect its target and assets. Never delete or move it automatically. A matching existing draft may be resumed only after verifying its tag target, exact SHA, and expected assets; use `publishedAt` (non-null) together with `isDraft == false` to identify a published release. A published release or immutable tag requires a new commit and new version. Retry transient failures at the same SHA, preserve credentials, and report authentication or recovery blockers without printing secrets.
