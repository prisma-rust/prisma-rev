//! 资产 sidecar（`.rmeta`）：稳定 GUID + 导入器名（#466）。
//!
//! 每个源资产旁边可以有 `<path>.rmeta`（例如 `assets/textures/wood.png.rmeta`）：
//!
//! ```json
//! { "guid": "0123456789abcdef0123456789abcdef", "importer": "TextureImporter" }
//! ```
//!
//! GUID 在**首次扫描时生成一次**，之后文件重命名 / 移动时 sidecar 跟着走，所以
//! 引用方将来可以按 GUID 找到它，而不必在文件一动时改写所有文档。当前版本只把
//! 身份落到磁盘并让编辑器资产详情显示它（`.scene.json` 里的 `*_guid` 字段是下一步）。
//!
//! 这里刻意不引入 `uuid` 依赖：编辑器数据层只依赖 serde / serde_json / log。
//! 生成器用时间戳 + 进程 id + 原子计数 + 路径做两轮 FNV-1a，输出 32 位十六进制，
//! 与 `assets::asset_guid` 的旧形状一致；它不是密码学随机数，但足够作为项目内身份。

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// sidecar 后缀（追加在完整文件名后面：`wood.png.rmeta`）。
pub const META_SUFFIX: &str = ".rmeta";

static GUID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<path>.rmeta` 的规范路径（相对项目根，与资产路径同一形状）。
pub fn meta_relative_path(path: &str) -> String {
    format!("{path}{META_SUFFIX}")
}

/// `.rmeta` 文件内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetMeta {
    pub guid: String,
    #[serde(default)]
    pub importer: String,
}

impl AssetMeta {
    /// 读 sidecar；文件不存在、JSON 坏、GUID 形状不对都返回 `None`。
    pub fn read(root: &Path, relative: &str) -> Option<Self> {
        let text = fs::read_to_string(root.join(meta_relative_path(relative))).ok()?;
        let meta: AssetMeta = serde_json::from_str(&text).ok()?;
        valid_guid(&meta.guid).then_some(meta)
    }

    /// 写 sidecar（父目录必须已存在；写入失败原样返回错误）。
    pub fn write(&self, root: &Path, relative: &str) -> io::Result<()> {
        let path = root.join(meta_relative_path(relative));
        let mut text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        text.push('\n');
        fs::write(path, text)
    }

    /// 读已有的 sidecar；缺失 / 损坏时生成新 GUID 并写回。
    pub fn ensure(root: &Path, relative: &str, importer: &str) -> io::Result<Self> {
        if let Some(existing) = Self::read(root, relative) {
            return Ok(existing);
        }
        let meta = AssetMeta {
            guid: new_guid(relative),
            importer: importer.to_string(),
        };
        meta.write(root, relative)?;
        Ok(meta)
    }
}

/// 一个合法 GUID：32 位 ASCII 十六进制（大小写都接受，写回时保持原样）。
pub fn valid_guid(guid: &str) -> bool {
    guid.len() == 32 && guid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 生成一个稳定的 32 位十六进制 GUID（见模块文档）。
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

#[cfg(test)]
#[path = "asset_meta_tests.rs"]
mod tests;
