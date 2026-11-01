use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// 用于 AssetId::generate() 的全局原子计数器
// ---------------------------------------------------------------------------

/// [`AssetId::generate`] 使用的进程全局序列计数器。
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// AssetId – 全局唯一的基于 u64 的标识符
// ---------------------------------------------------------------------------

/// 一个全局唯一的 64 位资源标识符。
///
/// The high 32 bits 编码 a **generation** (monotonically increasing 纪元
/// the low 32 bits 编码 a **serial** within that generation.
///
/// `tombstone` values use generation `u32::MAX` — they 比较 higher than
/// any live ID and can be used as deletion sentinels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct AssetId(pub u64);

impl AssetId {
    /// Serial 遮罩 the low 32 bits.
    const SERIAL_MASK: u64 = 0x0000_0000_FFFF_FFFF;
    /// Shift for the generation field.
    const GENERATION_SHIFT: u64 = 32;

    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// 创建 a new `AssetId` from its raw u64 representation.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Extract the raw u64. Useful for 存储 / checksums.
    pub const fn into_raw(self) -> u64 {
        self.0
    }

    /// Generate a fresh process-local ID using an atomic counter.
    ///
    /// Runtime-generated IDs use **generation 0**, which is reserved away from
    /// the persisted cooker/database namespace (generation >= 1) so that
    /// `AssetId::generate()` can never collide with `AssetIdGenerator::new(1, ..)`.
    ///
    /// This is **not** persisted across restarts. 编辑器 代码 should
    /// 调用 [`AssetIdGenerator::next`] if it has 访问 to the database
    /// sequence.
    pub fn generate() -> Self {
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        assert!(
            serial <= Self::SERIAL_MASK,
            "runtime AssetId serial space exhausted"
        );
        Self(serial)
    }

    /// 构建 a tombstone sentinel for the given serial.
    /// Tombstones are IDs with generation = `u32::MAX` — they 排序 after
    /// every live ID and can never collide with a future `generate()` 调用
    pub fn tombstone(serial: u64) -> Self {
        let serial = serial & Self::SERIAL_MASK;
        Self((u64::from(u32::MAX) << Self::GENERATION_SHIFT) | serial)
    }

    /// Returns `true` when this ID is a tombstone (deleted marker).
    pub fn is_tombstone(self) -> bool {
        self.generation() == u32::MAX
    }

    /// 由**资产相对路径**派生一个稳定的 ID（构建期打包用，#530）。
    ///
    /// `pack_directory` 用它给包内每个文件分配 ID，材质/预制体这类在 cook 时
    /// 就要把依赖 ID 烘进数据块的类型（`RMAT` 里存的就是 `AssetId`）因此能在
    /// 打包前算出与包内完全一致的 ID —— 顺序分配做不到这一点：往目录里多加
    /// 一个文件就会把后面所有 ID 平移，烘进材质里的贴图 ID 全部指错。
    ///
    /// 路径先按 `/` 归一化（`\\` → `/`），因此 Windows 与 POSIX 构建产出相同
    /// ID；generation 固定为 1（cooker/数据库命名空间），serial 取 FNV-1a 的
    /// 低 32 位并避开 0。32 位空间下不同路径仍有极小概率碰撞，打包器会检测
    /// 并报错而不是静默覆盖。
    pub fn from_path(path: &str) -> Self {
        Self::from_raw((1u64 << Self::GENERATION_SHIFT) | u64::from(path_serial(path).max(1)))
    }

    /// Extract the generation 分量
    pub fn generation(self) -> u32 {
        (self.0 >> Self::GENERATION_SHIFT) as u32
    }

    /// Extract the serial 分量
    pub fn serial(self) -> u32 {
        (self.0 & Self::SERIAL_MASK) as u32
    }
}

// ---------------------------------------------------------------------------
// Display / 调试
// ---------------------------------------------------------------------------

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({:016x})", self.0)
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AssetId({:016x} gen={} serial={})",
            self.0,
            self.generation(),
            self.serial()
        )
    }
}

// ---------------------------------------------------------------------------
// Serde
// ---------------------------------------------------------------------------

impl serde::Serialize for AssetId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for AssetId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(Self)
    }
}

// ---------------------------------------------------------------------------
// Persistent ID 生成器 编辑器
// ---------------------------------------------------------------------------

/// A persisted asset-ID 生成器 that tracks the 下一个 serial in a file.
pub struct AssetIdGenerator {
    next_serial: u64,
    generation: u32,
}

impl AssetIdGenerator {
    /// 创建 a fresh 生成器 The initial serial is loaded from
    /// `current_max` (0 = start from serial 1).
    pub fn new(generation: u32, current_max: u64) -> Self {
        Self {
            next_serial: current_max + 1,
            generation,
        }
    }

    /// Allocate the 下一个 资源 ID (monotonically increasing).
    ///
    /// Panics after 2^32 allocations instead of silently truncating the serial
    /// and reusing IDs.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> AssetId {
        let serial = self.next_serial;
        assert!(
            serial <= AssetId::SERIAL_MASK,
            "AssetIdGenerator serial space exhausted for generation {}",
            self.generation
        );
        self.next_serial += 1;
        AssetId((u64::from(self.generation) << AssetId::GENERATION_SHIFT) | serial)
    }

    /// 当前 serial value 下一个 to be assigned).
    pub fn current_serial(&self) -> u64 {
        self.next_serial
    }

    /// Generation 纪元
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

/// `path` 的 FNV-1a 32 位哈希（路径先 `/` 归一化）。
///
/// 选 FNV-1a 是因为它足够短、无依赖、跨平台一致；这里的用途只是「同一路径
/// 任何时候得到同一个 ID」，不需要密码学强度。
fn path_serial(path: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in path.replace('\\', "/").as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
#[path = "id_tests.rs"]
mod tests;
