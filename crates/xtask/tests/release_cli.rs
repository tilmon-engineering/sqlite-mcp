use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("xtask-{prefix}-{stamp}-{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn fixture(prefix: &str, version: &str, changelog: &str) -> PathBuf {
    let root = temp_dir(prefix);
    write(
        root.join("Cargo.toml"),
        &format!(
            "[workspace]\nmembers = [\"crates/sqlite-mcp\", \"crates/sqlite-mcp-core\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \"{version}\"\nedition = \"2024\"\nrust-version = \"1.96\"\n"
        ),
    );
    for package in ["sqlite-mcp", "sqlite-mcp-core"] {
        write(
            root.join(format!("crates/{package}/Cargo.toml")),
            "[package]\nname = \"PACKAGE\"\nversion.workspace = true\nedition.workspace = true\n",
        );
        let manifest = fs::read_to_string(root.join(format!("crates/{package}/Cargo.toml")))
            .unwrap()
            .replace("PACKAGE", package);
        write(root.join(format!("crates/{package}/Cargo.toml")), &manifest);
        write(
            root.join(format!("crates/{package}/src/lib.rs")),
            "pub fn fixture() {}\n",
        );
    }
    write(root.join("CHANGELOG.md"), changelog);
    let status = Command::new("cargo")
        .args(["generate-lockfile"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(status.success(), "cargo generate-lockfile failed");
    root
}

fn xtask(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn valid_changelog(version: &str) -> String {
    format!(
        "# Changelog\n\n## [{version}] - 2026-01-02\n### Added\n- fixture change\n\n## [Unreleased]\n- next\n"
    )
}

fn assert_failed_with(root: &Path, args: &[&str], needle: &str) {
    let out = xtask(root, args);
    assert!(!out.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(needle),
        "stderr {stderr:?} did not contain {needle:?}"
    );
}

#[test]
fn release_versions_match() {
    let root = fixture("versions", "0.7.3", &valid_changelog("0.7.3"));
    let out = xtask(&root, &["release-check", "v0.7.3"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "release metadata valid: v0.7.3\n"
    );
}

#[test]
fn release_rejects_manifest_lock_and_tag_mismatch() {
    let root = fixture("tag", "0.7.3", &valid_changelog("0.7.3"));
    assert_failed_with(&root, &["release-check", "0.7.4"], "invalid release tag");

    let root = fixture("lock", "0.7.3", &valid_changelog("0.7.3"));
    let lock = root.join("Cargo.lock");
    let text = fs::read_to_string(&lock)
        .unwrap()
        .replace("version = \"0.7.3\"", "version = \"0.7.2\"");
    write(&lock, &text);
    assert_failed_with(&root, &["release-check", "v0.7.3"], "Cargo.lock entry");

    for package in ["sqlite-mcp", "sqlite-mcp-core"] {
        let root = fixture("literal", "0.7.3", &valid_changelog("0.7.3"));
        let manifest = root.join(format!("crates/{package}/Cargo.toml"));
        write(
            &manifest,
            &format!(
                "[package]\nname = \"{package}\"\nversion = \"0.7.3\"\nedition.workspace = true\n"
            ),
        );
        assert_failed_with(
            &root,
            &["release-check", "v0.7.3"],
            "exactly version.workspace = true",
        );
    }

    let root = fixture("member", "0.7.3", &valid_changelog("0.7.3"));
    let manifest = root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace(", \"crates/sqlite-mcp-core\"", "");
    write(&manifest, &text);
    assert_failed_with(
        &root,
        &["release-check", "v0.7.3"],
        "workspace missing crates/sqlite-mcp-core",
    );
}

#[test]
fn repository_release_check() {
    let root = fixture("repository", "0.7.3", &valid_changelog("0.7.3"));
    let out = xtask(&root, &["release-check", "v0.7.3"]);
    assert!(out.status.success());
    let notes = root.join("notes.md");
    let out = xtask(&root, &["release-notes", "v0.7.3", notes.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(notes).unwrap(),
        "### Added\n- fixture change\n"
    );
}

#[test]
fn notes_exact_section() {
    let root = fixture("notes-exact", "0.7.3", &valid_changelog("0.7.3"));
    let output = root.join("out.md");
    let out = xtask(
        &root,
        &["release-notes", "v0.7.3", output.to_str().unwrap()],
    );
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        "### Added\n- fixture change\n"
    );
}

#[test]
fn notes_invalid_fenced_and_no_output() {
    let cases = [
        ("invalid", "not a changelog\n", "changelog must begin"),
        (
            "fenced",
            "# Changelog\n\n```\n## [0.7.3] - 2026-01-02\n- fake\n```\n",
            "expected exactly one",
        ),
        (
            "empty",
            "# Changelog\n\n## [0.7.3] - 2026-01-02\n\n## [Unreleased]\n- next\n",
            "section is empty",
        ),
    ];
    for (name, changelog, error) in cases {
        let root = fixture(name, "0.7.3", changelog);
        let output = root.join("must-not-exist.md");
        assert_failed_with(
            &root,
            &["release-notes", "v0.7.3", output.to_str().unwrap()],
            error,
        );
        assert!(
            !output.exists(),
            "failed notes command created output for {name}"
        );
    }
}
