//! Crash-safe file replacement shared by the database and the package writer.

use std::path::{Path, PathBuf};

/// Write `bytes` next to `path` and rename the file into place.
///
/// `std::fs::write` truncates the destination first, so a crash in the middle
/// of a save leaves truncated JSON or a half-written `.pak`. Renaming a sibling
/// file is atomic, and the old handle keeps pointing at the old file on
/// Windows too.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staging = staging_path(path);
    std::fs::write(&staging, bytes)?;
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}

/// Sibling temp name with the process id, so two writers cannot share it.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "package".to_string());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}
