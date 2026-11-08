//! Filesystem helpers shared by the cooker CLI, the asset database and the packer.

use std::io;
use std::path::{Path, PathBuf};

/// Sibling path used as the staging file for [`write_atomic`].
///
/// Keeping the staging file in the same directory matters: a rename across volumes
/// is a copy, which is neither atomic nor safe for readers that still hold the old file.
pub(crate) fn sibling_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "staging".to_string());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

/// Write `bytes` to `path` through a sibling temp file and a rename.
///
/// Truncating the destination in place leaves a half-written file behind when the
/// process dies mid-write (Ctrl-C, OOM, CI timeout), and a half-written `.pak` is read
/// by offset from a running editor or game. A same-directory rename is atomic, and
/// Windows replaces an existing file while readers keep their old handle.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let staging = sibling_temp_path(path);
    if let Err(error) = std::fs::write(&staging, bytes) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}
