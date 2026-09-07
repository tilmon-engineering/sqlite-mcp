use std::fs;

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
                "ubuntu-24.04",
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
}
