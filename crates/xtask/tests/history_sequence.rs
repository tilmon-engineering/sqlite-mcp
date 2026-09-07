use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("xtask-history-{stamp}-{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[test]
fn history_sequence_scenario() {
    let root = temp_dir();
    write(
        &root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/sqlite-mcp\", \"crates/sqlite-mcp-core\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \"0.7.3\"\nedition = \"2024\"\nrust-version = \"1.96\"\n",
    );
    for package in ["sqlite-mcp", "sqlite-mcp-core"] {
        write(
            &root,
            &format!("crates/{package}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{package}\"\nversion.workspace = true\nedition.workspace = true\n"
            ),
        );
        write(
            &root,
            &format!("crates/{package}/src/lib.rs"),
            "pub fn fixture() {}\n",
        );
    }
    write(
        &root,
        "CHANGELOG.md",
        "# Changelog\n\n## [0.7.3] - 2026-01-02\n### Added\n- provisional implementation notes\n\n## [Unreleased]\n- next\n",
    );
    let cargo = Command::new("cargo")
        .args(["generate-lockfile"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(cargo.success());

    run(&root, &["init", "-q"]);
    run(&root, &["config", "user.email", "test@example.invalid"]);
    run(&root, &["config", "user.name", "Release Test"]);
    run(&root, &["add", "."]);
    run(
        &root,
        &["commit", "-qm", "implementation and provisional notes"],
    );
    let implementation = String::from_utf8(run(&root, &["rev-parse", "HEAD"]).stdout).unwrap();
    let implementation = implementation.trim().to_owned();
    let observed = String::from_utf8(run(&root, &["log", "--oneline", "--root"]).stdout).unwrap();
    assert!(observed.contains("implementation and provisional notes"));
    assert!(!observed.contains("final release notes"));

    write(
        &root,
        "CHANGELOG.md",
        "# Changelog\n\n## [0.7.3] - 2026-01-02\n### Added\n- final user-visible release notes\n\n## [Unreleased]\n- next\n",
    );
    run(&root, &["add", "CHANGELOG.md"]);
    run(&root, &["commit", "-qm", "final release notes"]);
    let final_sha = String::from_utf8(run(&root, &["rev-parse", "HEAD"]).stdout).unwrap();
    let final_sha = final_sha.trim().to_owned();
    assert_ne!(implementation, final_sha);
    let parent = String::from_utf8(run(&root, &["rev-parse", "HEAD^"]).stdout).unwrap();
    assert_eq!(parent.trim(), implementation);

    run(&root, &["tag", "-a", "v0.7.3", "-m", "Release v0.7.3"]);
    let tag_type = String::from_utf8(run(&root, &["cat-file", "-t", "v0.7.3"]).stdout).unwrap();
    assert_eq!(tag_type.trim(), "tag");
    let tagged_sha =
        String::from_utf8(run(&root, &["rev-list", "-n", "1", "v0.7.3"]).stdout).unwrap();
    assert_eq!(tagged_sha.trim(), final_sha);

    let notes = root.join("release-notes.md");
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release-notes", "v0.7.3", notes.to_str().unwrap()])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(notes).unwrap(),
        "### Added\n- final user-visible release notes\n"
    );
}
