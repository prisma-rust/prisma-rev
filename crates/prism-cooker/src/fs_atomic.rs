//! Atomic file replacement for build artifacts.
//!
//! A plain [`std::fs::write`] truncates the destination first, so a crash in the
//! middle leaves a half-written file. These helpers stage the bytes next to the
//! destination and rename them into place, which is atomic on every platform the
//! cooker targets and keeps readers on the old file until the new one is whole.

use std::io;
use std::path::{Path, PathBuf};

/// Staging path next to `path`, so the final rename stays on one filesystem.
pub(crate) fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

fn create_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Write `bytes` to `path` through a same-directory staging file + rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_parent(path)?;
    let staging = staging_path(path);
    std::fs::write(&staging, bytes)?;
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}

/// Async variant of [`write_atomic`].
pub(crate) async fn write_atomic_async(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let staging = staging_path(path);
    tokio::fs::write(&staging, bytes).await?;
    if let Err(error) = tokio::fs::rename(&staging, path).await {
        let _ = tokio::fs::remove_file(&staging).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_the_target_and_leaves_no_staging_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/data.bin");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover staging files: {leftovers:?}"
        );
    }
}
