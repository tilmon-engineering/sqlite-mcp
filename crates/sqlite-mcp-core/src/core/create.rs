use super::{Core, CoreError};
use crate::paths;
use rusqlite::{Connection, OpenFlags};
use std::os::unix::fs::MetadataExt;

fn creation_identity_error(
    target: &std::path::Path,
    retained: (u64, u64),
    target_identity: Result<(u64, u64), std::io::Error>,
) -> CoreError {
    let target_report = match target_identity {
        Ok(identity) => format!("target identity is dev={}, ino={}", identity.0, identity.1),
        Err(error) => format!("target identity unavailable ({error})"),
    };
    CoreError::Io(std::io::Error::other(format!(
        "created database identity changed at {}: retained original dev={}, ino={}; {target_report}",
        target.display(),
        retained.0,
        retained.1
    )))
}

fn check_creation_identity(
    target: &std::path::Path,
    descriptor: &std::fs::File,
    retained: (u64, u64),
) -> Result<(), CoreError> {
    let descriptor_identity = descriptor
        .metadata()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    let target_identity =
        std::fs::metadata(target).map(|metadata| (metadata.dev(), metadata.ino()));
    if !matches!(descriptor_identity, Ok(identity) if identity == retained)
        || !matches!(target_identity, Ok(identity) if identity == retained)
    {
        return Err(creation_identity_error(target, retained, target_identity));
    }
    Ok(())
}

impl Core {
    pub async fn create_database(&self, path: &str) -> Result<(String, String), CoreError> {
        self.create_database_with_ct(path, None).await
    }
    pub async fn create_database_with_ct(
        &self,
        path: &str,
        ct: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(String, String), CoreError> {
        if ct
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(CoreError::Cancelled {
                transaction_open: false,
                transaction_continuable: false,
            });
        }
        let (target, _) = paths::create_target(path)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&target)?;
        let retained = {
            let metadata = f.metadata()?;
            (metadata.dev(), metadata.ino())
        };
        crate::test_support::emit(crate::test_support::Event::CreationPreOpenCheckpoint);
        check_creation_identity(&target, &f, retained)?;
        let c = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        crate::test_support::emit(crate::test_support::Event::CreationPostOpenCheckpoint);
        check_creation_identity(&target, &f, retained)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=0;")?;
        let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        drop(c);
        crate::test_support::emit(crate::test_support::Event::CreationPostInitCheckpoint);
        check_creation_identity(&target, &f, retained)?;
        Ok((target.to_string_lossy().into_owned(), mode))
    }
}
