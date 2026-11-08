//! # prism-cooker::db
//!
//! 编辑器端资源数据库，追踪项目中所有导入的资源。
//!
//! 数据库位于 `Project/Library/AssetDatabase.json`，将 `Assets/` 下的每个文件
//! 映射到其稳定的 [`AssetId`]、[`AssetType`]、导入器配置和依赖图。
//!
//! 数据库位于 `Project/Library/AssetDatabase.json`，将 `Assets/` 下的每个文件
//! 映射到导入的 AssetId 和元数据。

#![allow(clippy::pedantic, clippy::nursery, clippy::cargo)]

use prism_asset::core::{AssetId, AssetType};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Asset not found: {0}")]
    AssetNotFound(AssetId),

    #[error("Asset not found by path: {0}")]
    AssetNotFoundByPath(PathBuf),

    #[error("Duplicate path: {0}")]
    DuplicatePath(PathBuf),
}

// ---------------------------------------------------------------------------
// Asset State
// ---------------------------------------------------------------------------

/// Lifecycle state of an asset in the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetState {
    /// Asset is present and usable.
    Normal,
    /// Source file exists but the asset has missing dependencies.
    Missing,
    /// Asset was deleted (tombstone).
    Deleted,
}

// ---------------------------------------------------------------------------
// Asset Record
// ---------------------------------------------------------------------------

/// A single entry in the asset database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRecord {
    /// Globally unique ID.
    pub id: AssetId,
    /// Path relative to the `Assets/` directory, using `/` separators.
    pub path: String,
    /// The high-level asset type.
    pub asset_type: AssetType,
    /// Name of the importer that created this record.
    pub importer_name: String,
    /// xxh3 hash of the source file contents.
    pub source_hash: u64,
    /// xxh3 hash of the import settings JSON.
    pub import_settings_hash: u64,
    /// IDs of assets this one depends on.
    pub dependencies: Vec<AssetId>,
    /// Current state.
    pub state: AssetState,
    /// Monotonically increasing version counter.
    pub version: u32,
}

impl AssetRecord {
    /// Create a new record.
    pub fn new(id: AssetId, path: String, asset_type: AssetType, importer_name: &str) -> Self {
        Self {
            id,
            path,
            asset_type,
            importer_name: importer_name.to_string(),
            source_hash: 0,
            import_settings_hash: 0,
            dependencies: Vec::new(),
            state: AssetState::Normal,
            version: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Import Cache Entry
// ---------------------------------------------------------------------------

/// One entry in the import cache, keyed by source file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportCacheEntry {
    /// xxh3 hash of the source file.
    pub source_hash: u64,
    /// xxh3 hash of the import settings.
    pub settings_hash: u64,
    /// Asset ID that was produced.
    pub asset_id: AssetId,
    /// Importer version that produced this entry.
    pub importer_version: u32,
}

// ---------------------------------------------------------------------------
// Asset Database
// ---------------------------------------------------------------------------

/// The editor-side asset database.
///
/// This is the authoritative source of truth for "what assets exist" in the
/// editor. The runtime never touches it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetDatabase {
    /// All asset records.
    records: Vec<AssetRecord>,
    /// Index: relative path → AssetId.
    #[serde(skip)]
    path_index: HashMap<String, AssetId>,
    /// Current maximum serial value (for ID generation).
    next_serial: u64,
    /// Generation epoch.
    generation: u32,
}

impl AssetDatabase {
    /// Create an empty database.
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            path_index: HashMap::new(),
            next_serial: 1,
            generation: 1,
        }
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    /// Number of live records (excluding tombstones).
    pub fn len(&self) -> usize {
        self.records
            .iter()
            .filter(|r| r.state != AssetState::Deleted)
            .count()
    }

    /// Returns `true` if the database has no live records.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate live records (tombstones are persisted but not yielded).
    pub fn records(&self) -> impl Iterator<Item = &AssetRecord> {
        self.records
            .iter()
            .filter(|r| r.state != AssetState::Deleted)
    }

    /// Iterate mutable live records.
    pub fn records_mut(&mut self) -> impl Iterator<Item = &mut AssetRecord> {
        self.records
            .iter_mut()
            .filter(|r| r.state != AssetState::Deleted)
    }

    /// Get a live record by ID 线性 scan — databases are small in the 编辑器
    pub fn get(&self, id: AssetId) -> Option<&AssetRecord> {
        self.records
            .iter()
            .find(|r| r.id == id && r.state != AssetState::Deleted)
    }

    /// Get a mutable live record by ID.
    pub fn get_mut(&mut self, id: AssetId) -> Option<&mut AssetRecord> {
        self.records
            .iter_mut()
            .find(|r| r.id == id && r.state != AssetState::Deleted)
    }

    /// Find an asset by its relative path.
    pub fn get_by_path(&self, path: &str) -> Option<&AssetRecord> {
        let normalized = normalize_path(path);
        self.path_index
            .get(&normalized)
            .and_then(|id| self.get(*id))
    }

    /// Find an asset ID by relative path.
    pub fn id_by_path(&self, path: &str) -> Option<AssetId> {
        let normalized = normalize_path(path);
        self.path_index.get(&normalized).copied()
    }

    // ------------------------------------------------------------------
    // Mutators
    // ------------------------------------------------------------------

    /// Insert or update an asset record. Returns the assigned ID.
    ///
    /// If a record with the same path already exists, its `id` is reused.
    pub fn insert(&mut self, record: AssetRecord) -> Result<AssetId, DatabaseError> {
        let normalized = normalize_path(&record.path);

        // Check for duplicate path.
        if let Some(existing_id) = self.path_index.get(&normalized) {
            if *existing_id != record.id {
                return Err(DatabaseError::DuplicatePath(PathBuf::from(&record.path)));
            }
        }

        let id = record.id;
        self.path_index.insert(normalized, id);

        // Replace if exists, else push.
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == id) {
            *existing = record;
        } else {
            self.records.push(record);
        }

        Ok(id)
    }

    /// 移除 a record: keep it in the database as a persisted tombstone.
    ///
    /// The record stays serialized with [`AssetState::Deleted`] so a later
    /// `load` can distinguish "deleted" from "never imported"; accessors such
    /// as [`Self::get`] and [`Self::records`] skip tombstones.
    pub fn remove(&mut self, id: AssetId) -> Option<AssetRecord> {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.id == id && r.state != AssetState::Deleted)?;
        let normalized = normalize_path(&record.path);
        record.state = AssetState::Deleted;
        self.path_index.remove(&normalized);
        Some(record.clone())
    }

    /// Generate a fresh asset ID.
    pub fn generate_id(&mut self) -> AssetId {
        let serial = self.next_serial;
        self.next_serial += 1;
        AssetId::from_raw((u64::from(self.generation) << 32) | (serial & 0x0000_0000_FFFF_FFFF))
    }

    /// Current serial.
    pub fn current_serial(&self) -> u64 {
        self.next_serial
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    /// Load the database from a JSON file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let mut db: Self = serde_json::from_str(&content)?;
        db.rebuild_index();
        Ok(db)
    }

    /// Async load via tokio.
    pub async fn load_async(path: impl AsRef<Path> + Send) -> Result<Self, DatabaseError> {
        let content = tokio::fs::read_to_string(path.as_ref()).await?;
        let mut db: Self = serde_json::from_str(&content)?;
        db.rebuild_index();
        Ok(db)
    }

    /// Save the database to a JSON file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), DatabaseError> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path.as_ref(), content)?;
        Ok(())
    }

    /// Async save via tokio.
    pub async fn save_async(&self, path: impl AsRef<Path> + Send) -> Result<(), DatabaseError> {
        let content = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path.as_ref(), content).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn rebuild_index(&mut self) {
        self.path_index.clear();
        for r in &self.records {
            if r.state != AssetState::Deleted {
                let norm = normalize_path(&r.path);
                self.path_index.insert(norm, r.id);
            }
        }
    }
}

impl Default for AssetDatabase {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Import Cache
// ---------------------------------------------------------------------------

/// Incremental import cache.
///
/// Maps source file paths (relative to `Assets/`) to their last-known hash
/// and the importer version that processed them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportCache {
    entries: HashMap<String, ImportCacheEntry>,
}

impl ImportCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check if a file needs re-importing.
    ///
    /// Returns `true` if the file is unchanged (same hash + same settings hash
    /// + same importer version).
    pub fn is_up_to_date(
        &self,
        path: &str,
        source_hash: u64,
        settings_hash: u64,
        importer_version: u32,
    ) -> bool {
        let normalized = normalize_path(path);
        self.entries.get(&normalized).is_some_and(|e| {
            e.source_hash == source_hash
                && e.settings_hash == settings_hash
                && e.importer_version == importer_version
        })
    }

    /// Record a successful import.
    pub fn record(
        &mut self,
        path: &str,
        source_hash: u64,
        settings_hash: u64,
        asset_id: AssetId,
        importer_version: u32,
    ) {
        let normalized = normalize_path(path);
        self.entries.insert(
            normalized,
            ImportCacheEntry {
                source_hash,
                settings_hash,
                asset_id,
                importer_version,
            },
        );
    }

    /// Get the cache entry for a path, if any.
    pub fn get(&self, path: &str) -> Option<&ImportCacheEntry> {
        let normalized = normalize_path(path);
        self.entries.get(&normalized)
    }

    /// Remove a cache entry.
    pub fn remove(&mut self, path: &str) {
        let normalized = normalize_path(path);
        self.entries.remove(&normalized);
    }

    /// Load from JSON file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let content = std::fs::read_to_string(path.as_ref())?;
        Ok(serde_json::from_str(&content)?)
    }

    /// Save to JSON file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), DatabaseError> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path.as_ref(), content)?;
        Ok(())
    }

    /// Async save.
    pub async fn save_async(&self, path: impl AsRef<Path> + Send) -> Result<(), DatabaseError> {
        let content = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path.as_ref(), content).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize a path: `/` separators, no trailing slash, no `./` prefix.
pub fn normalize_path(path: &str) -> String {
    let mut result: String = path.replace('\\', "/").trim().to_string();
    // Strip leading `./`
    while result.starts_with("./") {
        result = result[2..].to_string();
    }
    // Strip leading `/`
    while result.starts_with('/') {
        result = result[1..].to_string();
    }
    // Strip trailing `/`
    while result.ends_with('/') {
        result = result[..result.len() - 1].to_string();
    }
    if result.is_empty() {
        result = ".".to_string();
    }
    result
}

#[cfg(test)]
mod tests;
