use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn temp_dir(prefix: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("xtask-native-{prefix}-{stamp}-{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn xtask(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .current_dir(root())
        .output()
        .unwrap()
}
static RELEASE_BINARY: OnceLock<PathBuf> = OnceLock::new();
fn tag() -> String {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let version = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|line| line.strip_suffix('"'))
        .unwrap();
    format!("v{version}")
}
fn artifact() -> PathBuf {
    RELEASE_BINARY
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("SQLITE_MCP_RELEASE_BINARY") {
                return PathBuf::from(path);
            }
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--locked",
                    "--release",
                    "-p",
                    "sqlite-mcp",
                    "--bin",
                    "sqlite-mcp",
                ])
                .current_dir(root())
                .status()
                .expect("build release binary");
            assert!(status.success(), "release binary build failed");
            root().join("target/release/sqlite-mcp")
        })
        .clone()
}

#[test]
fn native_release_version_smoke() {
    let binary = artifact();
    assert!(
        binary.is_file(),
        "build release artifact first: {}",
        binary.display()
    );
    let current_tag = tag();
    let out = xtask(&["native-smoke", &current_tag, binary.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "native smoke failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn native_archive_smoke() {
    let binary = artifact();
    assert!(binary.is_file());
    let dir = temp_dir("archive");
    let target = String::from_utf8(Command::new("rustc").args(["-vV"]).output().unwrap().stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap()
        .to_owned();
    let current_tag = tag();
    let out = xtask(&[
        "package",
        &current_tag,
        &target,
        binary.to_str().unwrap(),
        dir.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "package failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let archive = dir.join(format!("sqlite-mcp-{target}.tar.gz"));
    assert!(archive.is_file());
    let listing = Command::new("tar")
        .args(["-tzf", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&listing.stdout), "sqlite-mcp\n");
}

#[test]
fn native_archive_rejects_nonexecutable() {
    let dir = temp_dir("nonexec");
    let fake = dir.join("fake");
    fs::write(&fake, b"not executable").unwrap();
    let outdir = dir.join("out");
    let target = String::from_utf8(Command::new("rustc").args(["-vV"]).output().unwrap().stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap()
        .to_owned();
    let out = xtask(&[
        "package",
        "v0.1.0",
        &target,
        fake.to_str().unwrap(),
        outdir.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not executable"));
}
