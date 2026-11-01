//! # prism-asset::runtime
//!
//! 轻量级运行时资源加载器，仅依赖 `prism-asset` 的 `core` 与 `package` 模块。
//!
//! 此模块是游戏代码唯一面向消费者的资产读取 API。它提供 [`ResourceManager`]，
//! 用于加载 `.pak` 包并通过 [`AssetId`] 查询解析 [`Handle<T>`] 引用。
//!
//! ## 设计约束
//!
//! - 运行时无源文件路径——所有访问均通过 [`AssetId`]。
//! - 不依赖构建工具（`prism-cooker` 的 importer/cooker/db 等）。
//! - 支持同步 + 异步加载。
//! - 通过 LRU 淘汰控制内存预算（阶段 3）。
//! - 通过文件监视器支持热重载（[`HotReloadWatcher`]，运行时总是可用）。
//! - 大资源流式读取（阶段 3，特性 `streaming`）。

use crate::core::{AnyHandle, AssetId, AssetType, Handle};
#[cfg(feature = "streaming")]
use crate::package::PackageError;
use crate::package::PackageReader;
#[cfg(feature = "streaming")]
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Hot-reload support
// ---------------------------------------------------------------------------

// 轮询器只依赖 `std`，运行时和构建期都能用；以前它藏在 `hot-reload` feature 后面，
// 而打开该 feature 的只有构建期的 `prism-cooker`，于是运行期根本拿不到 watcher（#495）。
mod hot_reload;

pub use hot_reload::{HotReloadError, HotReloadEvent, HotReloadWatcher};

// ---------------------------------------------------------------------------
// Typed 资源 wrappers (RTEX/RMES/RMAT/SPIR-V/RSCN decoders)
// ---------------------------------------------------------------------------

pub mod typed;

pub use typed::{
    AnimClipAsset, MaterialAsset, MeshAsset, SceneAsset, ShaderAsset, SkeletonAsset,
    SkinnedMeshAsset, TextureAsset,
};

pub use crate::formats::{RmatInfo, RmesInfo};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("Asset not loaded: {0}")]
    NotLoaded(AssetId),

    #[error("Asset not found in any package: {0}")]
    NotFound(AssetId),

    #[error("Package error: {0}")]
    Package(#[from] crate::package::PackageError),

    #[error("Handle generation mismatch for slot {index}: expected {expected}, got {got}")]
    GenerationMismatch { index: u32, expected: u32, got: u32 },

    #[error("Slot {index} is empty")]
    SlotEmpty { index: u32 },

    #[error("Asset type mismatch: expected {expected:?}, got {got:?}")]
    TypeMismatch {
        expected: &'static str,
        got: AssetType,
    },

    #[error("Failed to deserialize {asset_type:?}: {reason}")]
    DeserializeFailed {
        asset_type: AssetType,
        reason: String,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Memory budget would be exceeded ({current} + {needed} > {max})")]
    OutOfMemory { current: u64, needed: u64, max: u64 },

    #[error("Cannot unpin asset {0}: pin count is already zero")]
    PinUnderflow(AssetId),
}

// ---------------------------------------------------------------------------
// 槽 内部 存储
// ---------------------------------------------------------------------------

/// One 槽 in the 运行时 槽 数组
#[derive(Clone)]
struct Slot {
    generation: u32,
    asset_id: AssetId,
    asset_type: AssetType,
    /// The raw cooked data, as stored in the .pak (uncompressed).
    data: Option<Vec<u8>>,
    /// Handle of the 槽 for type-safe 访问
    #[allow(dead_code)]
    handle: AnyHandle,
    /// 最后一个 访问 时间戳 (for LRU eviction). `Cell` lets read-only
    /// accessors (`get_raw`, `read_stream`) refresh recency without `&mut`.
    last_access: Cell<Instant>,
    /// 大小 of the 资源 data in 字节 (for 内存 tracking)
    size_bytes: u64,
    /// Load sequence stamp (for FIFO eviction), assigned when data is cached.
    load_epoch: u64,
    /// Number of active pins protecting this slot from automatic eviction.
    pin_count: u32,
}

impl Slot {
    fn new(index: u32) -> Self {
        Self {
            generation: 0,
            asset_id: AssetId::from_raw(0),
            asset_type: AssetType::Unknown,
            data: None,
            handle: AnyHandle::from_raw(index as u64),
            last_access: Cell::new(Instant::now()),
            size_bytes: 0,
            load_epoch: 0,
            pin_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Eviction 策略
// ---------------------------------------------------------------------------

/// The eviction 策略 when 内存 budget is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// No automatic eviction 调用者 must manage manually).
    None,
    /// Evict least-recently-accessed assets 第一个
    #[default]
    Lru,
    /// Evict oldest-loaded assets 第一个
    Fifo,
}

// ---------------------------------------------------------------------------
// 内存 Tracker
// ---------------------------------------------------------------------------

/// Tracks 内存 用法 and enforces budget via eviction.
#[derive(Debug, Clone)]
struct MemoryTracker {
    /// 最大 内存 budget in 字节 (0 = unlimited).
    budget: u64,
    /// 当前 内存 用法 in 字节
    current: u64,
    /// Eviction 策略
    policy: EvictionPolicy,
}

impl MemoryTracker {
    fn new() -> Self {
        Self {
            budget: 0,
            current: 0,
            policy: EvictionPolicy::default(),
        }
    }

    fn set_budget(&mut self, bytes: u64) {
        self.budget = bytes;
    }

    fn set_policy(&mut self, policy: EvictionPolicy) {
        self.policy = policy;
    }

    fn can_fit(&self, bytes: u64) -> bool {
        self.budget == 0 || self.current + bytes <= self.budget
    }

    fn account_add(&mut self, bytes: u64) {
        self.current += bytes;
    }

    fn account_remove(&mut self, bytes: u64) {
        self.current = self.current.saturating_sub(bytes);
    }

    fn usage_ratio(&self) -> f32 {
        if self.budget == 0 {
            0.0
        } else {
            self.current as f32 / self.budget as f32
        }
    }
}

// ---------------------------------------------------------------------------
// 资源 管理器
// ---------------------------------------------------------------------------

/// 已注册的 `.pak`：连来源路径一起保存。
///
/// `on_pak_changed` 只有知道每个 reader 来自哪个文件，才能把**陈旧 reader** 换掉；
/// 否则热重载之后新 `load` 仍会从旧字节里读。
struct LoadedPackage {
    /// `None` = 从内存字节注册（[`ResourceManager::load_package_bytes`]），磁盘上没有可替换的文件。
    ///
    /// 只有 `on_pak_changed` 读它（热重载要靠它找回被替换的那个 reader）。
    path: Option<PathBuf>,
    reader: PackageReader,
}

/// The central 运行时 资源 管理器
///
/// Manages a 槽 数组 of loaded assets, indexed by [`Handle<T>`].
/// Packages are loaded and their assets registered. The 管理器 owns the
/// loaded data and provides generation-counted handles for safe 访问
///
/// ## Phase 3 features
///
/// - **Memory budget**: 调用 [`set_memory_budget()`](ResourceManager::set_memory_budget)
///   to cap 总计 loaded 字节 LRU or FIFO eviction.
/// - **Hot reload**: use [`HotReloadWatcher`] to watch `.pak` files, then hand each
///   event to [`ResourceManager::on_pak_changed`].
/// - **Streaming**: 调用 [`read_stream()`](ResourceManager::read_stream) to
///   iterate over an asset's data in fixed-size chunks (zero-copy within a
///   loaded 包).
///
pub struct ResourceManager {
    /// 槽 数组 indexed by handle 索引
    slots: Vec<Slot>,
    /// 映射表 from AssetId -> 槽 索引
    id_map: HashMap<AssetId, u32>,
    /// 映射表 from source-relative path -> AssetId, populated by
    /// [`Self::load_path_manifest`]. Lets the engine 解析 scene
    /// `mesh_path`/`material_path` strings to 运行时 `AssetId`s. 空 when
    /// no manifest has been loaded (path-based lookup is unavailable).
    path_map: HashMap<String, AssetId>,
    /// Loaded 包 readers.
    packages: Vec<LoadedPackage>,
    /// 下一个 free 槽 索引
    next_slot: u32,
    /// 内存 tracking.
    memory: MemoryTracker,
    /// Monotonic 加载 计数器 (for FIFO eviction).
    load_epoch: u64,
}

impl ResourceManager {
    /// 创建 a new 空 资源 管理器
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            id_map: HashMap::new(),
            path_map: HashMap::new(),
            packages: Vec::new(),
            next_slot: 1, // 0 reserved for null handle
            memory: MemoryTracker::new(),
            load_epoch: 0,
        }
    }

    // ------------------------------------------------------------------
    // 内存 budget 控制
    // ------------------------------------------------------------------

    /// 集合 the 内存 budget in 字节 (0 = unlimited).
    ///
    /// When the budget would be exceeded during 加载 assets will be
    /// evicted according to the eviction 策略
    pub fn set_memory_budget(&mut self, bytes: u64) {
        self.memory.set_budget(bytes);
    }

    /// 当前 内存 budget (0 = unlimited).
    pub fn memory_budget(&self) -> u64 {
        self.memory.budget
    }

    /// 当前 内存 用法 (sum of loaded 资源 data sizes).
    pub fn memory_usage(&self) -> u64 {
        self.memory.current
    }

    /// 比率 of used to budgeted 内存 (0.0 – 1.0). Returns 0.0 if
    /// the budget is unlimited.
    pub fn memory_usage_ratio(&self) -> f32 {
        self.memory.usage_ratio()
    }

    /// 集合 the eviction 策略
    pub fn set_eviction_policy(&mut self, policy: EvictionPolicy) {
        self.memory.set_policy(policy);
    }

    /// 当前 eviction 策略
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.memory.policy
    }

    /// Try to free at least `target_bytes` by evicting assets.
    /// Returns 字节 actually freed.
    pub fn evict(&mut self, target_bytes: u64) -> u64 {
        if target_bytes == 0 || self.memory.policy == EvictionPolicy::None {
            return 0;
        }

        let mut freed: u64 = 0;

        // 构建 eviction candidate 列表: (slot, last_access, size, load_epoch)
        let mut candidates: Vec<(u32, Instant, u64, u64)> = self
            .id_map
            .values()
            .filter_map(|&idx| {
                let slot = &self.slots[idx as usize];
                if slot.data.is_some() && slot.pin_count == 0 {
                    Some((
                        idx,
                        slot.last_access.get(),
                        slot.size_bytes,
                        slot.load_epoch,
                    ))
                } else {
                    None
                }
            })
            .collect();

        match self.memory.policy {
            EvictionPolicy::Lru => {
                // 排序 by 访问 时间 (oldest 第一个
                candidates.sort_by_key(|&(_, time, _, _)| time);
            }
            EvictionPolicy::Fifo => {
                // FIFO orders by when the data was loaded, not last access.
                candidates.sort_by_key(|&(_, _, _, epoch)| epoch);
            }
            EvictionPolicy::None => return 0,
        }

        for (idx, _time, size, _epoch) in &candidates {
            if freed >= target_bytes {
                break;
            }
            let slot = &mut self.slots[*idx as usize];
            if slot.pin_count > 0 {
                continue;
            }
            if slot.data.take().is_some() {
                self.memory.account_remove(*size);
                freed += *size;
                tracing::debug!("Evicted slot {} (freed {} bytes)", idx, size);
            }
        }

        freed
    }

    // ------------------------------------------------------------------
    // 包 management
    // ------------------------------------------------------------------

    /// 加载 and register all assets from a `.pak` file.
    pub fn load_package(&mut self, path: impl AsRef<Path>) -> Result<(), RuntimeError> {
        let path = path.as_ref().to_path_buf();
        let reader = PackageReader::open(&path)?;
        self.register_package(LoadedPackage {
            path: Some(path),
            reader,
        });
        Ok(())
    }

    /// 从内存中的 `.pak` 字节加载资源，供 Android asset/嵌入式宿主使用。
    pub fn load_package_bytes(&mut self, bytes: &[u8]) -> Result<(), RuntimeError> {
        let reader = PackageReader::from_bytes(bytes)?;
        self.register_package(LoadedPackage { path: None, reader });
        Ok(())
    }

    /// 加载 and register all assets from a `.pak` file 异步
    #[cfg(feature = "async-io")]
    pub async fn load_package_async(
        &mut self,
        path: impl AsRef<Path> + Send,
    ) -> Result<(), RuntimeError> {
        let path = path.as_ref().to_path_buf();
        let reader = PackageReader::open_async(&path).await?;
        self.register_package(LoadedPackage {
            path: Some(path),
            reader,
        });
        Ok(())
    }

    /// 加载 a path manifest (`.pak.meta.json` written by `prism-cook build`
    /// so the engine can 解析 source-relative 资源 paths to 运行时
    /// `AssetId`s.
    ///
    /// The manifest is the **only** 运行时 源 of path->id 映射 the
    /// `.pak` 二进制 itself stores only `AssetId`s (paths are an editor-side
    /// concept). Without this 调用 [`Self::id_by_path`] always returns
    /// `None`.
    ///
    /// The manifest 格式 (produced by `cmd_build`) is a JSON 对象 with an
    /// `assets` 数组 each entry having `id` (hex 字符串 and `path` 字符串
    pub fn load_path_manifest(&mut self, path: impl AsRef<Path>) -> Result<(), RuntimeError> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            RuntimeError::DeserializeFailed {
                asset_type: AssetType::Binary,
                reason: format!("read path manifest: {e}"),
            }
        })?;
        self.load_path_manifest_from_str(&text)
    }

    /// Same as [`Self::load_path_manifest`] but parses an in-memory JSON 字符串
    pub fn load_path_manifest_from_str(&mut self, text: &str) -> Result<(), RuntimeError> {
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|e| RuntimeError::DeserializeFailed {
                asset_type: AssetType::Binary,
                reason: format!("parse path manifest JSON: {e}"),
            })?;

        let assets = json
            .get("assets")
            .and_then(|a| a.as_array())
            .ok_or_else(|| RuntimeError::DeserializeFailed {
                asset_type: AssetType::Binary,
                reason: "manifest missing 'assets' array".into(),
            })?;

        let mut added = 0usize;
        for entry in assets {
            let id_str = entry.get("id").and_then(|v| v.as_str());
            let path = entry.get("path").and_then(|v| v.as_str());
            let (Some(id_str), Some(path)) = (id_str, path) else {
                continue;
            };
            // `id` is a hex 字符串 like "0x0000000100000001".
            let raw = if let Some(stripped) = id_str.strip_prefix("0x") {
                u64::from_str_radix(stripped, 16)
            } else {
                u64::from_str_radix(id_str, 16)
            }
            .map_err(|e| RuntimeError::DeserializeFailed {
                asset_type: AssetType::Binary,
                reason: format!("manifest asset id '{id_str}' is not hex: {e}"),
            })?;
            let id = AssetId::from_raw(raw);
            self.path_map.insert(path.to_owned(), id);
            added += 1;
        }

        tracing::info!("Loaded path manifest: {} entries", added);
        Ok(())
    }

    /// 解析 a source-relative 资源 path to its 运行时 `AssetId`.
    ///
    /// Returns `None` when no manifest has been loaded or the path isn't
    /// registered. The lookup is case-sensitive and expects forward-slash
    /// separators (matching the manifest written by `prism-cook build`
    pub fn id_by_path(&self, path: &str) -> Option<AssetId> {
        self.path_map.get(path).copied()
    }

    /// Register an already-open 包 reader.
    fn register_package(&mut self, package: LoadedPackage) {
        let asset_count = package.reader.asset_count();
        for record in package.reader.records() {
            let asset_id = AssetId::from_raw(record.id);
            if self.id_map.contains_key(&asset_id) {
                tracing::warn!("Duplicate asset ID in package: {asset_id}");
                continue;
            }

            let index = self.next_slot;
            self.next_slot += 1;

            if index as usize >= self.slots.len() {
                // Extend the vec to include this 索引
                self.slots.resize(index as usize + 1, Slot::new(0));
            }
            self.slots[index as usize] = Slot {
                generation: 0,
                asset_id,
                asset_type: AssetType::from_u32(record.type_id),
                data: None,
                handle: AnyHandle::from_raw(index as u64),
                last_access: Cell::new(Instant::now()),
                size_bytes: record.size, // track uncompressed size
                load_epoch: 0,
                pin_count: 0,
            };
            self.id_map.insert(asset_id, index);
        }

        self.packages.push(package);
        tracing::info!(
            "Registered {} assets from package (total slots: {})",
            asset_count,
            self.id_map.len()
        );
    }

    // ------------------------------------------------------------------
    // 资源 lookup
    // ------------------------------------------------------------------

    /// Check if an 资源 is registered (without loading its data).
    pub fn is_registered(&self, id: AssetId) -> bool {
        self.id_map.contains_key(&id)
    }

    /// Number of registered assets.
    pub fn asset_count(&self) -> usize {
        self.id_map.len()
    }

    /// Number of loaded packages.
    pub fn package_count(&self) -> usize {
        self.packages.len()
    }

    /// Get the [`AssetType`] of a registered 资源
    pub fn asset_type(&self, id: AssetId) -> Option<AssetType> {
        self.id_map
            .get(&id)
            .map(|&idx| self.slots[idx as usize].asset_type)
    }

    /// Check whether an asset is marked for streaming in any loaded package.
    pub fn is_streamed(&self, id: AssetId) -> bool {
        self.packages
            .iter()
            .any(|package| package.reader.is_asset_streamed(id))
    }

    // ------------------------------------------------------------------
    // Dependency 分辨率 (topological)
    // ------------------------------------------------------------------

    /// 加载 an 资源 and all its dependencies recursively.
    ///
    /// Assets are loaded in topological order (dependencies 第一个
    /// Returns the handle to the requested root 资源
    pub fn load_with_deps<T: Asset + 'static>(
        &mut self,
        id: AssetId,
    ) -> Result<Handle<T>, RuntimeError> {
        self.load_deps_recursive(id)?;

        // Now 加载 the requested 资源 itself.
        self.load(id)
    }

    /// 加载 an 资源 and all its dependencies recursively, then
    /// return raw 字节 Useful for assets that aren't `Asset`-typed.
    pub fn load_with_deps_raw(&mut self, id: AssetId) -> Result<Vec<u8>, RuntimeError> {
        self.load_deps_recursive(id)?;
        self.load_raw_bytes(id)
    }

    /// Recursively 解析 and 加载 all dependencies of an 资源
    ///
    /// Uses a DFS that tracks the 当前 path for cycle detection.
    /// Returns 成功 when all deps are loaded.
    fn load_deps_recursive(&mut self, id: AssetId) -> Result<(), RuntimeError> {
        let mut visited: HashMap<AssetId, bool> = HashMap::new(); // false = temp (in-progress)
        let mut load_order: Vec<AssetId> = Vec::new();

        self.dfs_deps(id, &mut visited, &mut load_order)?;

        // 加载 in order (dependencies 第一个
        for &dep_id in &load_order {
            // Only 加载 if not already loaded.
            if self.get_raw_bytes(dep_id).is_err() {
                self.load_raw_bytes(dep_id)?;
            }
        }
        Ok(())
    }

    fn dfs_deps(
        &self,
        id: AssetId,
        visited: &mut HashMap<AssetId, bool>,
        load_order: &mut Vec<AssetId>,
    ) -> Result<(), RuntimeError> {
        match visited.get(&id) {
            Some(&true) => return Ok(()), // already processed
            Some(&false) => {
                // Cycle detected — just warn and skip this 分支
                tracing::warn!("Dependency cycle detected involving {id}");
                return Ok(());
            }
            None => {}
        }

        visited.insert(id, false); // mark as in-progress

        // 查找 the 包 that 包含 this 资源 and get its deps.
        for package in &self.packages {
            let reader = &package.reader;
            if let Some(record) = reader.find_record(id) {
                let deps_raw = reader.dependencies(record);
                for &dep_raw in deps_raw {
                    let dep_id = AssetId::from_raw(dep_raw);
                    self.dfs_deps(dep_id, visited, load_order)?;
                }
                break;
            }
        }

        visited.insert(id, true); // mark as done
        load_order.push(id);
        Ok(())
    }

    /// 查找 a 槽 索引 for an 资源 ID.
    fn slot_index(&self, id: AssetId) -> Option<u32> {
        self.id_map.get(&id).copied()
    }

    // ------------------------------------------------------------------
    // 同步 loading
    // ------------------------------------------------------------------

    /// 加载 raw 字节 for an 资源 without going through 资源 trait
    fn load_raw_bytes(&mut self, id: AssetId) -> Result<Vec<u8>, RuntimeError> {
        let slot_index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;

        // If already loaded, return a 复制
        {
            let slot = &self.slots[slot_index as usize];
            if let Some(ref data) = slot.data {
                slot.last_access.set(Instant::now());
                return Ok(data.clone());
            }
        }

        // 查找 and 读取 from a 包
        let data = self.read_from_packages(id)?;

        // Check 内存 budget before storing.
        let size = data.len() as u64;
        if !self.memory.can_fit(size) {
            if self.memory.policy != EvictionPolicy::None {
                // Try to evict.
                let freed = self.evict(size + 1024 * 1024); // free +1MB margin
                if !self.memory.can_fit(size) {
                    return Err(RuntimeError::OutOfMemory {
                        current: self.memory.current,
                        needed: size,
                        max: self.memory.budget,
                    });
                }
                tracing::debug!("Evicted {freed} bytes to make room for {size}");
            } else {
                return Err(RuntimeError::OutOfMemory {
                    current: self.memory.current,
                    needed: size,
                    max: self.memory.budget,
                });
            }
        }

        // 存储
        let epoch = self.load_epoch;
        {
            let slot = &mut self.slots[slot_index as usize];
            slot.data = Some(data.clone());
            slot.last_access.set(Instant::now());
            slot.size_bytes = size;
            slot.generation += 1;
            slot.load_epoch = epoch;
        }
        self.memory.account_add(size);
        self.load_epoch += 1;

        Ok(data)
    }

    /// 加载 an 资源 by ID and return raw 字节
    fn get_raw_bytes(&self, id: AssetId) -> Result<Vec<u8>, RuntimeError> {
        let slot_index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;
        let slot = &self.slots[slot_index as usize];
        let data = slot.data.as_ref().ok_or(RuntimeError::NotLoaded(id))?;
        slot.last_access.set(Instant::now());
        Ok(data.clone())
    }

    /// 加载 an 资源 by ID and return a typed handle.
    ///
    /// 第一个 访问 reads the data from the .pak and caches it. Subsequent
    /// calls return immediately.
    pub fn load<T: Asset + 'static>(&mut self, id: AssetId) -> Result<Handle<T>, RuntimeError> {
        let slot_index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;

        // If data is already loaded, 更新 访问 时间 and return handle.
        // Validate `T` even on the cached path: callers such as
        // `load_with_deps` may have populated the slot as raw bytes first.
        {
            let slot = &mut self.slots[slot_index as usize];
            if let Some(data) = slot.data.as_ref() {
                T::from_bytes(data).map_err(|_| RuntimeError::TypeMismatch {
                    expected: std::any::type_name::<T>(),
                    got: slot.asset_type,
                })?;
                slot.last_access.set(Instant::now());
                return Ok(Handle::new(slot_index, slot.generation));
            }
        }

        // 读取 raw data.
        let data = self.read_from_packages(id)?;

        // 反序列化 through the 资源 trait
        let _asset = T::from_bytes(&data).map_err(|_e| RuntimeError::TypeMismatch {
            expected: std::any::type_name::<T>(),
            got: self.slots[slot_index as usize].asset_type,
        })?;

        // Check 内存 budget.
        let size = data.len() as u64;
        if !self.memory.can_fit(size) {
            if self.memory.policy != EvictionPolicy::None {
                let _freed = self.evict(size + 1024 * 1024);
                if !self.memory.can_fit(size) {
                    return Err(RuntimeError::OutOfMemory {
                        current: self.memory.current,
                        needed: size,
                        max: self.memory.budget,
                    });
                }
            } else {
                return Err(RuntimeError::OutOfMemory {
                    current: self.memory.current,
                    needed: size,
                    max: self.memory.budget,
                });
            }
        }

        // 存储 data and 更新 metadata.
        let epoch = self.load_epoch;
        {
            let slot = &mut self.slots[slot_index as usize];
            slot.data = Some(data);
            slot.last_access.set(Instant::now());
            slot.size_bytes = size;
            slot.generation += 1;
            slot.load_epoch = epoch;
        }
        self.memory.account_add(size);
        self.load_epoch += 1;

        let gen = self.slots[slot_index as usize].generation;
        Ok(Handle::new(slot_index, gen))
    }

    /// 读取 raw 字节 from loaded packages for a given 资源 ID.
    fn read_from_packages(&self, id: AssetId) -> Result<Vec<u8>, RuntimeError> {
        for package in &self.packages {
            if let Some(data) = package.reader.read_asset_data(id)? {
                return Ok(data);
            }
        }
        Err(RuntimeError::NotFound(id))
    }

    // ------------------------------------------------------------------
    // Streaming reads
    // ------------------------------------------------------------------

    /// 读取 an asset's data in chunks (streaming).
    ///
    /// Returns an 迭代器 yielding `Vec<u8>` chunks of at most
    /// `chunk_size` 字节 This avoids loading the entire 资源 into
    /// 内存 at once. The 资源 is not cached in the 槽 — it is
    /// streamed directly from the 包
    ///
    /// Requires 特性 `streaming` 启用 by 默认
    ///
    /// Returns `None` if the 资源 is not registered or its data cannot
    /// be 读取
    #[cfg(feature = "streaming")]
    pub fn read_stream(
        &self,
        id: AssetId,
        chunk_size: usize,
    ) -> Option<impl Iterator<Item = Vec<u8>> + '_> {
        // A zero chunk size would make `StreamIter` yield empty chunks forever
        // and make `size_hint` divide by zero; clamp it to a single byte.
        let chunk_size = chunk_size.max(1);

        // 1. If already cached in slot, stream directly from the cached slice without copying.
        if let Some(slot_index) = self.slot_index(id) {
            let slot = &self.slots[slot_index as usize];
            if let Some(data) = slot.data.as_ref() {
                slot.last_access.set(Instant::now());
                return Some(StreamIter::Slice(SliceStreamIter {
                    data: Cow::Borrowed(data.as_slice()),
                    pos: 0,
                    chunk_size,
                }));
            }
        }

        // 2. Otherwise stream from the package. A file-backed reader (#509) has no
        //    slice to borrow, so the stored block is read into memory once and the
        //    iterator owns it — still bounded by one asset, not by the whole `.pak`.
        for package in &self.packages {
            if let Some(record) = package.reader.find_record(id) {
                let raw = match package.reader.asset_record_raw_slice(record) {
                    Ok(slice) => Cow::Borrowed(slice),
                    Err(PackageError::NotInMemory) => {
                        match package.reader.asset_record_raw_bytes(record) {
                            Ok(bytes) => Cow::Owned(bytes),
                            Err(_) => continue,
                        }
                    }
                    Err(_) => continue,
                };
                if let Some(slot_index) = self.slot_index(id) {
                    self.slots[slot_index as usize]
                        .last_access
                        .set(Instant::now());
                }
                if record.is_compressed() {
                    let source: Box<dyn std::io::Read + '_> = Box::new(std::io::Cursor::new(raw));
                    if let Ok(decoder) = zstd::Decoder::new(source) {
                        return Some(StreamIter::Compressed(CompressedStreamIter {
                            decoder,
                            chunk_size,
                            total_size: record.size as usize,
                            read_so_far: 0,
                        }));
                    }
                } else {
                    return Some(StreamIter::Slice(SliceStreamIter {
                        data: raw,
                        pos: 0,
                        chunk_size,
                    }));
                }
            }
        }

        None
    }

    // ------------------------------------------------------------------
    // Get typed references
    // ------------------------------------------------------------------

    /// Get a typed 引用 to already-loaded 资源 data.
    pub fn get<T: Asset + 'static>(&mut self, handle: Handle<T>) -> Result<T, RuntimeError> {
        let index = handle.index() as usize;
        if index >= self.slots.len() {
            return Err(RuntimeError::SlotEmpty {
                index: handle.index(),
            });
        }
        let slot = &mut self.slots[index];
        if slot.generation != handle.generation() {
            return Err(RuntimeError::GenerationMismatch {
                index: handle.index(),
                expected: handle.generation(),
                got: slot.generation,
            });
        }
        slot.last_access.set(Instant::now());
        let data = slot
            .data
            .as_ref()
            .ok_or(RuntimeError::NotLoaded(slot.asset_id))?;
        T::from_bytes(data).map_err(|_| RuntimeError::TypeMismatch {
            expected: std::any::type_name::<T>(),
            got: slot.asset_type,
        })
    }

    /// Get raw 字节 for an already-loaded 资源
    pub fn get_raw(&self, id: AssetId) -> Result<Vec<u8>, RuntimeError> {
        self.get_raw_bytes(id)
    }

    // ------------------------------------------------------------------
    // Unloading
    // ------------------------------------------------------------------

    /// Unload a specific 资源 by handle, freeing its data.
    pub fn unload<T: ?Sized>(&mut self, handle: Handle<T>) {
        let index = handle.index() as usize;
        if index < self.slots.len() {
            let slot = &mut self.slots[index];
            if slot.generation == handle.generation() {
                let size = slot.size_bytes;
                if slot.data.take().is_some() {
                    self.memory.account_remove(size);
                    slot.pin_count = 0;
                    tracing::debug!("Unloaded slot {index}");
                }
            }
        }
    }

    /// Unload all assets.
    pub fn unload_all(&mut self) {
        for slot in &mut self.slots {
            if slot.data.take().is_some() {
                self.memory.account_remove(slot.size_bytes);
                slot.pin_count = 0;
            }
        }
        self.memory.current = 0;
        tracing::info!(
            "Unloaded all assets (memory freed: {})",
            self.memory.current
        );
    }

    /// Unload an asset by ID (convenience).
    pub fn unload_id(&mut self, id: AssetId) -> Result<(), RuntimeError> {
        let index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;
        let slot = &mut self.slots[index as usize];
        let size = slot.size_bytes;
        if slot.data.take().is_some() {
            self.memory.account_remove(size);
            slot.pin_count = 0;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Pinning / lease management
    // ------------------------------------------------------------------

    /// Pin an asset to prevent it from being evicted from memory.
    ///
    /// Increments the slot's pin count. As long as `pin_count > 0`,
    /// automatic eviction (LRU / FIFO) will never discard this asset's data.
    ///
    /// Returns the updated pin count on success.
    pub fn pin<T: ?Sized>(&mut self, handle: Handle<T>) -> Result<u32, RuntimeError> {
        let index = handle.index() as usize;
        if index >= self.slots.len() {
            return Err(RuntimeError::SlotEmpty {
                index: handle.index(),
            });
        }
        let slot = &mut self.slots[index];
        if slot.generation != handle.generation() {
            return Err(RuntimeError::GenerationMismatch {
                index: handle.index(),
                expected: handle.generation(),
                got: slot.generation,
            });
        }
        if slot.data.is_none() {
            return Err(RuntimeError::NotLoaded(slot.asset_id));
        }
        slot.pin_count = slot.pin_count.saturating_add(1);
        Ok(slot.pin_count)
    }

    /// Unpin an asset, allowing it to be evicted when memory is needed.
    ///
    /// Decrements the slot's pin count. If the pin count reaches 0, the
    /// asset becomes eligible for eviction.
    ///
    /// Returns the updated pin count on success, or `Err(RuntimeError::PinUnderflow)`
    /// if the asset was not pinned.
    pub fn unpin<T: ?Sized>(&mut self, handle: Handle<T>) -> Result<u32, RuntimeError> {
        let index = handle.index() as usize;
        if index >= self.slots.len() {
            return Err(RuntimeError::SlotEmpty {
                index: handle.index(),
            });
        }
        let slot = &mut self.slots[index];
        if slot.generation != handle.generation() {
            return Err(RuntimeError::GenerationMismatch {
                index: handle.index(),
                expected: handle.generation(),
                got: slot.generation,
            });
        }
        if slot.pin_count == 0 {
            return Err(RuntimeError::PinUnderflow(slot.asset_id));
        }
        slot.pin_count -= 1;
        Ok(slot.pin_count)
    }

    /// Check whether an asset handle is currently pinned.
    pub fn is_pinned<T: ?Sized>(&self, handle: Handle<T>) -> bool {
        self.pin_count(handle) > 0
    }

    /// Get the current pin count for an asset handle.
    pub fn pin_count<T: ?Sized>(&self, handle: Handle<T>) -> u32 {
        let index = handle.index() as usize;
        if index < self.slots.len() {
            let slot = &self.slots[index];
            if slot.generation == handle.generation() {
                return slot.pin_count;
            }
        }
        0
    }

    /// Pin an asset using a type-erased [`AnyHandle`].
    pub fn pin_any(&mut self, handle: AnyHandle) -> Result<u32, RuntimeError> {
        self.pin(Handle::<()>::from(handle))
    }

    /// Unpin an asset using a type-erased [`AnyHandle`].
    pub fn unpin_any(&mut self, handle: AnyHandle) -> Result<u32, RuntimeError> {
        self.unpin(Handle::<()>::from(handle))
    }

    /// Check whether a type-erased [`AnyHandle`] is pinned.
    pub fn is_any_pinned(&self, handle: AnyHandle) -> bool {
        self.is_pinned(Handle::<()>::from(handle))
    }

    /// Get the pin count for a type-erased [`AnyHandle`].
    pub fn any_pin_count(&self, handle: AnyHandle) -> u32 {
        self.pin_count(Handle::<()>::from(handle))
    }

    /// Pin an asset by `AssetId`.
    pub fn pin_id(&mut self, id: AssetId) -> Result<u32, RuntimeError> {
        let index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;
        let slot = &mut self.slots[index as usize];
        if slot.data.is_none() {
            return Err(RuntimeError::NotLoaded(id));
        }
        slot.pin_count = slot.pin_count.saturating_add(1);
        Ok(slot.pin_count)
    }

    /// Unpin an asset by `AssetId`.
    pub fn unpin_id(&mut self, id: AssetId) -> Result<u32, RuntimeError> {
        let index = self.slot_index(id).ok_or(RuntimeError::NotFound(id))?;
        let slot = &mut self.slots[index as usize];
        if slot.pin_count == 0 {
            return Err(RuntimeError::PinUnderflow(id));
        }
        slot.pin_count -= 1;
        Ok(slot.pin_count)
    }

    /// Check whether an asset ID is currently pinned.
    pub fn is_id_pinned(&self, id: AssetId) -> bool {
        self.id_pin_count(id) > 0
    }

    /// Get the current pin count for an asset ID.
    pub fn id_pin_count(&self, id: AssetId) -> u32 {
        if let Some(index) = self.slot_index(id) {
            if let Some(slot) = self.slots.get(index as usize) {
                return slot.pin_count;
            }
        }
        0
    }

    // ------------------------------------------------------------------
    // Hot-reload
    // ------------------------------------------------------------------

    /// Called by the [`HotReloadWatcher`] when a `.pak` file changes.
    ///
    /// 两件事必须同时做到，否则热重载是假的：
    /// 1. **换掉 reader** —— 管理器里留的是打开时的文件字节（`PackageReader` 持有
    ///    `Arc<Vec<u8>>`），不替换的话之后新 `load` 仍从旧字节读；
    /// 2. **只刷新仍在内存里的资源** —— 已卸载（被淘汰）的槽不该因为热重载被急切加载回来，
    ///    它们下次 `load` 时自然会从新 reader 读到新数据。
    ///
    /// 刷新是**原地换内容**：`generation` 不变，所以热重载前发出的 [`Handle`] 仍然有效
    /// （只用于「槽被回收给别的资源」时才递增）。
    ///
    /// 未知路径视为新出现的 `.pak`，按普通注册处理。
    pub fn on_pak_changed(&mut self, path: &Path) -> Result<(), RuntimeError> {
        let reader = PackageReader::open(path)?;

        let Some(index) = self
            .packages
            .iter()
            .position(|package| package.path.as_deref() == Some(path))
        else {
            tracing::info!("Hot-reload registered new package {}", path.display());
            self.register_package(LoadedPackage {
                path: Some(path.to_path_buf()),
                reader,
            });
            return Ok(());
        };

        self.packages[index].reader = reader;

        let ids: Vec<AssetId> = self.packages[index]
            .reader
            .records()
            .iter()
            .map(|record| AssetId::from_raw(record.id))
            .collect();

        let mut refreshed = 0usize;
        for asset_id in ids {
            let Some(&slot_index) = self.id_map.get(&asset_id) else {
                continue;
            };
            let slot_index = slot_index as usize;
            if self.slots[slot_index].data.is_none() {
                // 没加载过 / 已被淘汰：不为它做急切加载。
                continue;
            }
            let Ok(Some(data)) = self.packages[index].reader.read_asset_data(asset_id) else {
                continue;
            };

            let new_size = data.len() as u64;
            let epoch = self.load_epoch;
            {
                let slot = &mut self.slots[slot_index];
                let old_size = slot.size_bytes;
                if old_size != new_size {
                    self.memory.account_remove(old_size);
                    self.memory.account_add(new_size);
                }
                slot.data = Some(data);
                slot.size_bytes = new_size;
                // Deliberately **no** `generation` bump: the slot still holds
                // the same asset, just newer bytes. Bumping it here would make
                // every `Handle` handed out before the reload fail with
                // `GenerationMismatch` — a hot reload must refresh content
                // without invalidating live handles. Generation only moves
                // when a slot is recycled for a different asset.
                slot.last_access.set(Instant::now());
                slot.load_epoch = epoch;
            }
            self.load_epoch += 1;
            refreshed += 1;
            tracing::info!("Hot-reloaded asset {asset_id}");
        }

        tracing::info!(
            "Replaced package {} ({} loaded asset(s) refreshed)",
            path.display(),
            refreshed
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // 迭代
    // ------------------------------------------------------------------

    /// Iterate all registered 资源 IDs and their types.
    pub fn assets(&self) -> impl Iterator<Item = (AssetId, AssetType)> + '_ {
        self.id_map.iter().map(|(id, &idx)| {
            let slot = &self.slots[idx as usize];
            (*id, slot.asset_type)
        })
    }

    /// Iterate all currently loaded (in-memory) 资源 IDs.
    pub fn loaded_assets(&self) -> impl Iterator<Item = AssetId> + '_ {
        self.slots.iter().filter_map(|slot| {
            if slot.data.is_some() && slot.asset_id != AssetId::from_raw(0) {
                Some(slot.asset_id)
            } else {
                None
            }
        })
    }
}

impl Default for ResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ResourceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceManager")
            .field("slots", &self.slots.len())
            .field("registered", &self.id_map.len())
            .field("packages", &self.packages.len())
            .field("memory_usage", &self.memory.current)
            .field("memory_budget", &self.memory.budget)
            .field("eviction_policy", &self.memory.policy)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Streaming 迭代器
// ---------------------------------------------------------------------------

#[cfg(feature = "streaming")]
enum StreamIter<'a> {
    Slice(SliceStreamIter<'a>),
    Compressed(CompressedStreamIter<'a>),
}

#[cfg(feature = "streaming")]
struct SliceStreamIter<'a> {
    /// Borrowed for in-memory packages, owned when the bytes came from disk.
    data: Cow<'a, [u8]>,
    pos: usize,
    chunk_size: usize,
}

#[cfg(feature = "streaming")]
impl<'a> Iterator for SliceStreamIter<'a> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let end = (self.pos + self.chunk_size).min(self.data.len());
        let chunk = self.data[self.pos..end].to_vec();
        self.pos = end;
        Some(chunk)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.data.len() - self.pos;
        let chunks = remaining.div_ceil(self.chunk_size.max(1));
        (chunks, Some(chunks))
    }
}

#[cfg(feature = "streaming")]
struct CompressedStreamIter<'a> {
    /// Boxed so the decoder can read either a borrowed slice or an owned
    /// buffer read from disk, without making the struct self-referential.
    decoder: zstd::Decoder<'static, std::io::BufReader<Box<dyn std::io::Read + 'a>>>,
    chunk_size: usize,
    total_size: usize,
    read_so_far: usize,
}

#[cfg(feature = "streaming")]
impl<'a> Iterator for CompressedStreamIter<'a> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.read_so_far >= self.total_size {
            return None;
        }
        let mut buf = vec![0u8; self.chunk_size];
        let mut total_read = 0;
        while total_read < self.chunk_size {
            match std::io::Read::read(&mut self.decoder, &mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(_) => return None,
            }
        }
        if total_read == 0 {
            return None;
        }
        buf.truncate(total_read);
        self.read_so_far += total_read;
        Some(buf)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.total_size.saturating_sub(self.read_so_far);
        let chunks = remaining.div_ceil(self.chunk_size.max(1));
        (chunks, Some(chunks))
    }
}

#[cfg(feature = "streaming")]
impl<'a> Iterator for StreamIter<'a> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Slice(it) => it.next(),
            Self::Compressed(it) => it.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Slice(it) => it.size_hint(),
            Self::Compressed(it) => it.size_hint(),
        }
    }
}

// ---------------------------------------------------------------------------
// 资源 trait (for deserialization)
// ---------------------------------------------------------------------------

/// trait that 资源 types must implement to be loadable through the
/// [`ResourceManager`].
pub trait Asset: Sized + Send + 'static {
    /// The expected [`AssetType`] for this 类型
    fn asset_type() -> AssetType;

    /// 反序列化 from raw 字节
    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError>;

    /// 序列化 后 to 字节
    fn into_bytes(self) -> Vec<u8>;
}

/// Simple blob 资源 — wraps raw 字节
impl Asset for Vec<u8> {
    fn asset_type() -> AssetType {
        AssetType::Binary
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        Ok(data.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self
    }
}

#[cfg(test)]
mod tests;
