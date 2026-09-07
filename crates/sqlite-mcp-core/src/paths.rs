use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;
#[derive(Debug, Error)]
pub enum PathError {
    #[error("path must be absolute")]
    Relative,
    #[error("path contains NUL")]
    Nul,
    #[error("URI paths are not supported")]
    Uri,
    #[error("path is not a regular file")]
    NotFile,
    #[error("path does not exist")]
    Missing,
    #[error("path error: {0}")]
    Io(#[from] std::io::Error),
}
pub fn validate_literal(path: &str) -> Result<(), PathError> {
    if path.as_bytes().contains(&0) {
        return Err(PathError::Nul);
    }
    if path.starts_with("file:") {
        return Err(PathError::Uri);
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(PathError::Relative);
    }
    Ok(())
}
pub fn existing(path: &str) -> Result<PathBuf, PathError> {
    validate_literal(path)?;
    let p = fs::canonicalize(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PathError::Missing
        } else {
            PathError::Io(e)
        }
    })?;
    if !p.is_file() {
        return Err(PathError::NotFile);
    }
    Ok(p)
}
pub fn create_target(path: &str) -> Result<(PathBuf, PathBuf), PathError> {
    validate_literal(path)?;
    let p = PathBuf::from(path);
    let parent = p.parent().ok_or(PathError::Missing)?.canonicalize()?;
    let name = p.file_name().ok_or(PathError::Missing)?;
    Ok((parent.join(name), parent))
}
