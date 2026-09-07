use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use rusqlite::Connection;
use sqlite_mcp_core::{Config, Core};
use tempfile::{TempDir, tempdir};

static TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[derive(Clone, Copy, Debug)]
enum Checkpoint {
    PreOpen,
    PostOpen,
    PostInit,
}

impl Checkpoint {
    fn event(self) -> sqlite_mcp_core::test_support::Event {
        use sqlite_mcp_core::test_support::Event;
        match self {
            Self::PreOpen => Event::CreationPreOpenCheckpoint,
            Self::PostOpen => Event::CreationPostOpenCheckpoint,
            Self::PostInit => Event::CreationPostInitCheckpoint,
        }
    }
}

fn inode(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).expect("database metadata");
    (metadata.dev(), metadata.ino())
}

fn valid_sentinel(path: &Path) -> Vec<u8> {
    let connection = Connection::open(path).expect("create valid sentinel");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sentinel(value TEXT);")
        .expect("initialize valid sentinel");
    drop(connection);
    fs::read(path).expect("read sentinel")
}

async fn replacement_at(checkpoint: Checkpoint) -> Vec<String> {
    let _guard = test_lock().await;
    // The focused RED command enables this marker; setting it here also makes
    // the test self-describing when run directly with cargo test.
    // SAFETY: this focused integration target serializes all hook-using tests.
    unsafe { std::env::set_var("SQLITE_MCP_TEST_SUPPORT", "1") };
    sqlite_mcp_core::test_support::reset_registry();

    let dir = tempdir().expect("temporary directory");
    let target = dir.path().join("created.sqlite");
    let original = dir.path().join("original.sqlite");
    let sentinel = dir.path().join("sentinel.sqlite");
    let sentinel_bytes = valid_sentinel(&sentinel);
    let barrier = sqlite_mcp_core::test_support::arm(checkpoint.event());
    let core = Core::new(Config::default()).expect("core");
    let task_core = core.clone();
    let target_string = target.to_str().expect("target path").to_owned();
    let task = tokio::spawn(async move { task_core.create_database(&target_string).await });

    // The armed checkpoint emit parks the runtime thread inside the create
    // task, so the observe→swap→release sequence must run on a blocking
    // thread; otherwise the single-threaded test runtime is starved until the
    // bounded emit timeout fires and the swap happens too late.
    let checkpoint_event = checkpoint.event();
    let swap_target = target.clone();
    let swap_original = original.clone();
    let swap_sentinel = sentinel.clone();
    let (original_identity, sentinel_identity) = tokio::task::spawn_blocking(move || {
        sqlite_mcp_core::test_support::wait_for(checkpoint_event, Duration::from_secs(2))
            .expect("creation checkpoint emitted");
        let original_identity = inode(&swap_target);
        fs::rename(&swap_target, &swap_original).expect("retain originally-created inode");
        fs::rename(&swap_sentinel, &swap_target).expect("install prepared sentinel");
        let sentinel_identity = inode(&swap_target);
        barrier.release();
        (original_identity, sentinel_identity)
    })
    .await
    .expect("checkpoint swapper");

    let result = task.await.expect("creation task");
    let mut failures = Vec::new();
    match result {
        Ok(_) => failures.push(format!(
            "checkpoint {checkpoint:?} silently accepted replacement"
        )),
        Err(error) => {
            let report = error.to_string();
            if !report.contains(&format!(
                "dev={}, ino={}",
                original_identity.0, original_identity.1
            )) {
                failures.push(format!(
                    "checkpoint {checkpoint:?} error omitted retained original inode"
                ));
            }
        }
    }
    if inode(&original) != original_identity {
        failures.push("retained original inode changed".into());
    }
    if inode(&target) != sentinel_identity {
        failures.push("replacement inode was unlinked or changed".into());
    }
    if fs::read(&target).expect("replacement bytes") != sentinel_bytes {
        failures.push("replacement bytes/schema changed".into());
    }
    if !target.exists() {
        failures.push("replacement pathname was removed".into());
    }
    if matches!(checkpoint, Checkpoint::PostInit) && target.with_extension("sqlite-wal").exists() {
        failures.push("post-init replacement acquired a sidecar after SQLite close".into());
    }
    core.shutdown().await;
    failures
}

#[tokio::test]
async fn create_replacement_checkpoint_matrix() {
    let mut failures = Vec::new();
    for checkpoint in [
        Checkpoint::PreOpen,
        Checkpoint::PostOpen,
        Checkpoint::PostInit,
    ] {
        failures.extend(replacement_at(checkpoint).await);
    }
    assert!(failures.is_empty(), "checkpoint results: {failures:?}");
}

#[tokio::test]
async fn create_descriptor_retained_through_close() {
    replacement_at(Checkpoint::PostInit).await;
}

#[test]
fn create_identity_residual_contract() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace parent")
        .parent()
        .expect("repository root");
    let design = fs::read_to_string(root.join("DESIGN.md")).expect("DESIGN.md");
    let readme = fs::read_to_string(root.join("README.md")).expect("README.md");
    assert!(design.contains("not hostile-filesystem-race proof"));
    assert!(design.contains("replacement/rename/deletion of an open database is unsupported"));
    // Residual limitation is normative in DESIGN.md; README points readers to
    // that contract rather than restating the detailed checkpoint boundary.
    assert!(readme.contains("See [`DESIGN.md`](DESIGN.md)"));
}

#[tokio::test]
async fn exclusive_create_race_strengthened_contract() {
    let _guard = test_lock().await;
    let dir: TempDir = tempdir().expect("temporary directory");
    let path: PathBuf = dir.path().join("race.sqlite");
    let core = Core::new(Config::default()).expect("core");
    let (a, b) = tokio::join!(
        core.create_database(path.to_str().expect("path")),
        core.create_database(path.to_str().expect("path")),
    );
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    let (resolved, mode) = a.or(b).expect("one exclusive creator succeeds");
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    assert_eq!(inode(Path::new(&resolved)), inode(&path));
    let header = fs::read(&path).expect("created database bytes");
    assert!(header.starts_with(b"SQLite format 3\0"));
    assert!(
        path.exists(),
        "successful exclusive creation remains present"
    );
    core.shutdown().await;
}
