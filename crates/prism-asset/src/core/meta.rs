//! Asset sidecars (`.rmeta`): stable GUID + importer name (#466).
//!
//! Every source asset can have a `<path>.rmeta` next to it:
//!
//! ```json
//! { "guid": "0123456789abcdef0123456789abcdef", "importer": "TextureImporter" }
//! ```
//!
//! The editor creates these while scanning a project (see
//! `prism-editor-core::asset_meta`); this module is the shared, engine-side
//! reader used by the cooker, build pipeline and development scene loader.
//! Keeping the format in `prism-asset` lets the runtime resolve GUIDs without
//! depending on the editor data layer.
//!
//! The importer payload is intentionally untyped: it is a string today, but
//! future importers may write settings objects. Only `guid` is load-bearing.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sidecar suffix (appended to the full file name: `wood.png.rmeta`).
pub const META_SUFFIX: &str = ".rmeta";

static GUID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<path>.rmeta` relative to the same root as `path`.
pub fn meta_relative_path(path: &str) -> String {
    format!("{path}{META_SUFFIX}")
}

/// Contents of a `.rmeta` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetMeta {
    pub guid: String,
    #[serde(default)]
    pub importer: Value,
}

impl AssetMeta {
    /// Read a sidecar for `relative` (an asset path, not the sidecar path).
    /// Returns `None` when the file is missing, malformed, or the GUID is not
    /// 32 hexadecimal characters.
    pub fn read(root: &Path, relative: &str) -> Option<Self> {
        Self::read_sidecar(root, &meta_relative_path(relative))
    }

    /// Read a sidecar by its own relative path (`textures/wood.png.rmeta`).
    pub fn read_sidecar(root: &Path, sidecar_relative: &str) -> Option<Self> {
        let text = fs::read_to_string(root.join(sidecar_relative)).ok()?;
        let meta: Self = serde_json::from_str(&text).ok()?;
        valid_guid(&meta.guid).then_some(meta)
    }

    /// Write the sidecar for `relative`. The parent directory must exist.
    pub fn write(&self, root: &Path, relative: &str) -> io::Result<()> {
        let path = root.join(meta_relative_path(relative));
        let mut text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        text.push('\n');
        fs::write(path, text)
    }

    /// Read an existing sidecar, or generate and persist a new GUID.
    pub fn ensure(root: &Path, relative: &str, importer: &str) -> io::Result<Self> {
        if let Some(existing) = Self::read(root, relative) {
            return Ok(existing);
        }
        let meta = Self {
            guid: new_guid(relative),
            importer: Value::String(importer.to_string()),
        };
        meta.write(root, relative)?;
        Ok(meta)
    }
}

/// A valid GUID is exactly 32 ASCII hexadecimal characters.
pub fn valid_guid(guid: &str) -> bool {
    guid.len() == 32 && guid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Generate a stable 32-hex-digit GUID.
///
/// This mirrors the editor-side generator: timestamp + process id + atomic
/// counter + path through two FNV-1a passes. It is not cryptographic, but it
/// is unique enough for project-local asset identity.
pub fn new_guid(path: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = GUID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{nanos}:{}:{counter}:{path}", std::process::id());
    let high = fnv1a(seed.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let low = fnv1a(seed.as_bytes(), 0x8422_2325_cbf2_9ce4);
    format!("{high:016x}{low:016x}")
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// GUID → asset path and asset path → GUID, relative to an `assets/` root.
#[derive(Debug, Clone, Default)]
pub struct GuidIndex {
    by_guid: HashMap<String, String>,
    by_path: HashMap<String, String>,
}

impl GuidIndex {
    /// Scan `assets_root` recursively for `*.rmeta` files.
    ///
    /// Only sidecars whose asset file still exists are indexed, so a stale
    /// sidecar cannot make a deleted asset look resolvable. Malformed sidecars
    /// are ignored; a later editor scan can repair them.
    pub fn scan(assets_root: &Path) -> Self {
        let mut index = Self::default();
        let mut stack = vec![assets_root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(assets_root) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                let Some(asset_path) = relative.strip_suffix(META_SUFFIX) else {
                    continue;
                };
                if asset_path.is_empty() || !assets_root.join(asset_path).is_file() {
                    continue;
                }
                let Some(meta) = AssetMeta::read_sidecar(assets_root, &relative) else {
                    continue;
                };
                index
                    .by_guid
                    .insert(meta.guid.to_ascii_lowercase(), asset_path.to_string());
                index
                    .by_path
                    .insert(asset_path.to_string(), meta.guid.to_ascii_lowercase());
            }
        }
        index
    }

    /// Resolve a GUID to an asset path relative to the scanned `assets/` root.
    pub fn path_for_guid(&self, guid: &str) -> Option<&str> {
        self.by_guid
            .get(&guid.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Look up the GUID for an asset path relative to the scanned root.
    pub fn guid_for_path(&self, path: &str) -> Option<&str> {
        let path = path.replace('\\', "/");
        self.by_path.get(&path).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_guid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_guid.is_empty()
    }

    /// Every GUID in the index (used to protect a renamed directory's assets
    /// from path rewrites).
    pub fn guids(&self) -> impl Iterator<Item = &str> {
        self.by_guid.keys().map(String::as_str)
    }
}

#[cfg(test)]
#[path = "meta_tests.rs"]
mod tests;
