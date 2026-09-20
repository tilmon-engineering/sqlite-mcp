use std::fs;

fn contains_marker(text: &str, marker: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&marker.to_ascii_lowercase())
}

fn repository_file(path: &str) -> String {
    let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::read_to_string(repository_root.join(path))
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
}

#[test]
fn release_documentation_contract() {
    let readme = repository_file("README.md");
    let design = repository_file("DESIGN.md");
    let agents = repository_file("AGENTS.md");
    let changelog = repository_file("CHANGELOG.md");
    let skill = repository_file(".polytoken/skills/release-sqlite-mcp/SKILL.md");

    for (name, text, markers) in [
        (
            "README.md",
            readme.as_str(),
            &[
                "release",
                "Ubuntu 24.04",
                "GNU",
                "unsigned",
                "Gatekeeper",
                "SHA256SUMS",
            ][..],
        ),
        (
            "DESIGN.md",
            design.as_str(),
            &["Cargo", "tag", "version", "stdout protocol-only"],
        ),
        (
            "AGENTS.md",
            agents.as_str(),
            &["release-check", "release skill", "lockstep", "Cargo.lock"],
        ),
        (
            "CHANGELOG.md",
            changelog.as_str(),
            &["# Changelog", "## [0.1.0]", "2684862", "Provisional"],
        ),
        (
            ".polytoken/skills/release-sqlite-mcp/SKILL.md",
            skill.as_str(),
            &[
                "two-commit",
                "pre-1.0",
                "immutable",
                "annotated",
                "previous published release",
                "recovery",
                "HTTPS",
                "git push origin \"$TAG\"",
                "mise run ci",
                "version.workspace = true",
                "cargo pkgid",
                "cargo check --workspace",
            ],
        ),
    ] {
        for marker in markers {
            assert!(
                text.to_ascii_lowercase()
                    .contains(&marker.to_ascii_lowercase()),
                "{name} is missing required marker {marker:?}"
            );
        }
    }

    let targets = [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ];
    for target in targets {
        assert!(readme.contains(target), "README is missing target {target}");
        assert!(
            readme.contains(&format!(
                "https://github.com/tilmon-engineering/sqlite-mcp/releases/download/v0.2.0/sqlite-mcp-{target}.tar.gz"
            )),
            "README is missing immutable v0.2.0 download link for {target}"
        );
        assert!(design.contains(target), "DESIGN is missing target {target}");
        assert!(
            skill.contains(target),
            "release skill is missing target {target}"
        );
    }

    for marker in [
        "uname -s",
        "uname -m",
        "VERSION=",
        "TEMP_INSTALL=",
        "config.example.toml",
        "--config",
        "SHA256SUMS",
        "sha256sum --check",
        "shasum -a 256 --check",
        "CHECKSUM_COUNT=",
        "test \"$CHECKSUM_COUNT\" -eq 1",
        "CHECKSUM_ENTRY=",
        "tar -tzf",
        "install -m 0755",
        "--version",
        "--disable --proto '=https' --proto-redir '=https'",
        "macOS 15",
        "Ubuntu 24.04",
        "GNU/glibc",
        "unsigned",
        "unnotarized",
        "do not disable Gatekeeper globally",
        "On macOS, use the same `set -euo pipefail`",
    ] {
        assert!(
            contains_marker(&readme, marker),
            "README is missing installation/platform marker {marker:?}"
        );
    }
    assert!(
        readme.contains("```bash\nset -euo pipefail"),
        "README install snippet must enable Bash fail-closed mode"
    );
    assert!(
        !readme.contains("releases/latest"),
        "README download links must not use mutable latest URLs"
    );
    assert!(
        readme.contains("shasum -a 256 --check"),
        "README must document the macOS checksum command"
    );
    assert!(
        !readme.contains("pipe") || readme.contains("never pipe"),
        "README must not present download-to-shell installation"
    );
}

/// Every line that discusses matching a raw REST (`gh api`) release list must
/// reference `tag_name`; the camelCase `tagName` spelling is permitted only
/// on lines that also identify the separate `gh release ... --json` shape.
fn assert_rest_lines_use_tag_name(name: &str, text: &str) {
    for line in text.lines() {
        let mentions_rest_list = line.contains("gh api") && line.contains("releases")
            || line.contains("REST") && line.contains("release list");
        if !mentions_rest_list {
            continue;
        }
        assert!(
            line.contains("`tag_name`"),
            "{name} REST release-list line must match on `tag_name`: {line}"
        );
        if line.contains("`tagName`") {
            assert!(
                line.contains("--json"),
                "{name} REST-context camelCase `tagName` is rejected unless the line also \
                 identifies the `gh release --json` shape: {line}"
            );
        }
    }
}

#[test]
fn skill_documents_rest_tag_name() {
    let skill = repository_file(".polytoken/skills/release-sqlite-mcp/SKILL.md");
    assert_rest_lines_use_tag_name("SKILL.md", &skill);
    assert!(
        skill.contains("`tagName`") && skill.contains("--json"),
        "the separate `gh release --json` tagName shape must stay documented"
    );
}

#[test]
fn design_documents_rest_tag_name() {
    let design = repository_file("DESIGN.md");
    assert_rest_lines_use_tag_name("DESIGN.md", &design);
    assert!(
        design.contains("`tagName`") && design.contains("--json"),
        "DESIGN must retain the `gh release --json` tagName context"
    );
}

#[test]
fn workflow_documented_commands_contract() {
    let agents = repository_file("AGENTS.md");
    let skill = repository_file(".polytoken/skills/release-sqlite-mcp/SKILL.md");
    let mise = repository_file("mise.toml");

    let xtask_commands = [
        "release-check TAG",
        "release-notes TAG OUTPUT_PATH",
        "verify-public-release TAG EXPECTED_SHA EVIDENCE_PATH",
        "fetch-public-assets TAG ASSET_DIR",
        "verify-downloads TAG ASSET_DIR BINARY_PATH",
    ];
    for command in xtask_commands {
        assert!(
            agents.contains(command),
            "AGENTS is missing documented command {command:?}"
        );
        assert!(
            skill.contains(command),
            "release skill is missing documented command {command:?}"
        );
    }

    for task in ["release-check", "release-build", "workflow-check"] {
        assert!(
            mise.contains(&format!("[tasks.{task}]")),
            "mise is missing {task}"
        );
        assert!(agents.contains(task), "AGENTS is missing mise task {task}");
        assert!(
            skill.contains(task),
            "release skill is missing mise task {task}"
        );
    }

    for marker in [
        "git push origin HEAD:main",
        "git tag -a \"$TAG\"",
        "git push origin \"$TAG\"",
    ] {
        assert!(
            skill.contains(marker),
            "release skill is missing required release command {marker:?}"
        );
    }
    assert!(
        skill.contains(
            "Push `main` normally, then wait for successful branch CI at that exact final notes SHA"
        ) && skill.contains(
            "Only after that gate succeeds, create an annotated `v{version}` tag at the exact tested SHA"
        ) && skill.contains("push only that exact tag"),
        "release prose must order push-main, successful exact-SHA branch CI, tag creation, tag push"
    );
    assert!(
        skill.contains("publishedAt") && skill.contains("isDraft"),
        "release skill must use current publication metadata fields"
    );
}
