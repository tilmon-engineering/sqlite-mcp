use sqlite_mcp_core::{Config, Core, CoreError};
use tempfile::tempdir;

#[tokio::test]
async fn absolute_path_contract() {
    let dir = tempdir().unwrap();
    let core = Core::new(Config::default()).unwrap();
    for p in ["relative.db", "~/tilde.db", "file:/tmp/uri.db", ""] {
        assert!(core.create_database(p).await.is_err(), "accepted {p:?}");
    }
    assert!(core.create_database("/tmp/bad\0name").await.is_err());
    let missing_parent = dir.path().join("missing/db.sqlite");
    assert!(
        core.create_database(missing_parent.to_str().unwrap())
            .await
            .is_err()
    );
    let absent = dir.path().join("absent.sqlite");
    assert!(matches!(
        core.open_database(absent.to_str().unwrap(), false).await,
        Err(CoreError::Path(_))
    ));
    assert!(!absent.exists());
    let empty = dir.path().join("empty.sqlite");
    std::fs::File::create(&empty).unwrap();
    assert!(
        core.open_database(empty.to_str().unwrap(), false)
            .await
            .is_err()
    );
    let text = dir.path().join("text.sqlite");
    std::fs::write(&text, b"not sqlite").unwrap();
    assert!(
        core.open_database(text.to_str().unwrap(), false)
            .await
            .is_err()
    );
    assert!(
        core.open_database(dir.path().to_str().unwrap(), false)
            .await
            .is_err()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn exclusive_create_race() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("race.sqlite");
    let core = Core::new(Config::default()).unwrap();
    let (a, b) = tokio::join!(
        core.create_database(path.to_str().unwrap()),
        core.create_database(path.to_str().unwrap())
    );
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    let (resolved, mode) = a.or(b).unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    let h = core.open_database(&resolved, false).await.unwrap();
    let schema = core.get_schema(&h.id).await.unwrap();
    assert!(schema.objects.is_empty());
    assert_eq!(schema.schema_version, 0);
    core.shutdown().await;
}

#[tokio::test]
async fn journal_mode_existing_is_preserved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("existing.sqlite");
    let core = Core::new(Config::default()).unwrap();
    let (path, _) = core.create_database(path.to_str().unwrap()).await.unwrap();
    let h = core.open_database(&path, false).await.unwrap();
    assert_eq!(h.journal_mode.to_ascii_lowercase(), "wal");
    core.close_database(&h.id).await.unwrap();
    core.shutdown().await;
}
