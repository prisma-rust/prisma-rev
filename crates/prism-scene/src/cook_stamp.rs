//! 场景包的构建戳（#976）。
//!
//! `assets/scenes.pak` 里除了场景和贴图，还会写一条 [`COOK_STAMP_NAME`] 记录
//! 打包时的组件 schema 指纹与工程文档的最新修改时间。运行时挂包时拿它和当前
//! 引擎 / 磁盘上的文档对一下，就能在「包过期」时给出明确提示，而不是安静地
//! 画出一张错的画面。
//!
//! 为什么需要它：`.pak` 的头部只有 `version = 1`，两个都用 v1 写出来的包
//! （一个用旧管线、一个用新管线）在运行时分不出来；旧包里的
//! `scenes/lake.rscn` 照样能被解出来，只是内容早就对不上作者文档了。
//!
//! 旧包没有这条记录，解析会返回 [`StampCheck::Missing`] —— 仍然能跑，
//! 但会建议重新打包。

use serde::{Deserialize, Serialize};

/// 戳在包内的路径（相对包根）。
pub const COOK_STAMP_NAME: &str = "__cook_stamp.json";

/// 当前构建戳格式版本。
pub const COOK_STAMP_VERSION: u32 = 1;

/// 打包时写进 `.pak` 的一条元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookStamp {
    /// 格式版本（本文件字段变化时 +1）。
    pub version: u32,
    /// [`crate::registry::ComponentRegistry::schema_fingerprint`]。
    pub schema: u64,
    /// 参与 cook 的工程文档（`scenes.json` / `settings.ini` / `*.scene.json` /
    /// `*.prefab.json`）里最新的 mtime，Unix 秒。
    pub sources_mtime: u64,
}

impl Default for CookStamp {
    fn default() -> Self {
        Self {
            version: COOK_STAMP_VERSION,
            schema: 0,
            sources_mtime: 0,
        }
    }
}

impl CookStamp {
    pub fn new(schema: u64, sources_mtime: u64) -> Self {
        Self {
            version: COOK_STAMP_VERSION,
            schema,
            sources_mtime,
        }
    }

    /// 序列化成写进包里的文本（带换行，方便 diff）。
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        text
    }

    /// 解析包里的戳；字段缺失或版本不认识时返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let version = value.get("version")?.as_u64()? as u32;
        if version != COOK_STAMP_VERSION {
            return None;
        }
        Some(Self {
            version,
            schema: value.get("schema")?.as_u64()?,
            sources_mtime: value.get("sources_mtime")?.as_u64()?,
        })
    }
}

/// 一次校验的结论；调用方（引擎）负责把它翻译成日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampCheck {
    /// 包是当前引擎 + 当前文档打出来的。
    Fresh,
    /// 包内没有戳：由更旧的管线构建，无法判断新鲜度。
    Missing,
    /// 组件 schema 变了（引擎升级 / 组件字段变动），需要重新打包。
    SchemaChanged { cooked: u64, current: u64 },
    /// 场景文档比包新（改了场景但忘了重新打包）。
    SourcesNewer { cooked: u64, newest: u64 },
}

impl StampCheck {
    /// 是否应该让用户重新打包。
    pub fn needs_rebuild(&self) -> bool {
        !matches!(self, StampCheck::Fresh)
    }
}

/// 比较戳与当前状态。
///
/// `current_schema` 传当前引擎的 schema 指纹；`newest_source` 传磁盘上工程
/// 文档的最新 mtime（发布版没有文档，传 0 = 跳过新鲜度检查）。
///
/// mtime 允许 2 秒误差：Windows 的 FAT/网络盘的秒级粒度足以让「刚打包完」
/// 被误判成过期。
pub fn check(stamp: Option<CookStamp>, current_schema: u64, newest_source: u64) -> StampCheck {
    let Some(stamp) = stamp else {
        return StampCheck::Missing;
    };
    if stamp.schema != current_schema {
        return StampCheck::SchemaChanged {
            cooked: stamp.schema,
            current: current_schema,
        };
    }
    if newest_source > stamp.sources_mtime.saturating_add(2) {
        return StampCheck::SourcesNewer {
            cooked: stamp.sources_mtime,
            newest: newest_source,
        };
    }
    StampCheck::Fresh
}

/// 扫出「会被 cook 进场景包的作者文档」的最新 mtime（Unix 秒）。
///
/// 只认 `*.scene.json` / `*.prefab.json` / `scenes.json` / `settings.ini`：
/// 散装贴图由 `.pak` 自己的清单兜底，把它们算进来会让每次改图都提示过期。
/// 发布版没有这些文件，返回 0（= 跳过新鲜度检查）。
pub fn newest_source_mtime(asset_root: &std::path::Path) -> u64 {
    fn visit(dir: &std::path::Path, newest: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, newest);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let tracked = name.ends_with(".scene.json")
                || name.ends_with(".prefab.json")
                || name == "scenes.json"
                || name == "settings.ini";
            if !tracked {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            let secs = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            *newest = (*newest).max(secs);
        }
    }

    let mut newest = 0u64;
    visit(asset_root, &mut newest);
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_round_trips_through_json() {
        let stamp = CookStamp::new(0xdead_beef, 1_700_000_000);
        let parsed = CookStamp::parse(&stamp.to_json()).expect("parse");
        assert_eq!(parsed, stamp);
    }

    #[test]
    fn unknown_or_broken_stamps_parse_to_none() {
        assert!(CookStamp::parse("").is_none());
        assert!(CookStamp::parse("{").is_none());
        // 未来版本的戳：不猜它的语义，交给「没有戳」那条分支。
        assert!(CookStamp::parse(r#"{"version": 99, "schema": 1, "sources_mtime": 2}"#).is_none());
        assert!(CookStamp::parse(r#"{"version": 1}"#).is_none());
    }

    #[test]
    fn matching_stamp_is_fresh() {
        let stamp = CookStamp::new(7, 100);
        assert_eq!(check(Some(stamp), 7, 100), StampCheck::Fresh);
        assert!(!StampCheck::Fresh.needs_rebuild());
    }

    #[test]
    fn a_changed_schema_needs_a_rebuild() {
        let stamp = CookStamp::new(7, 100);
        assert_eq!(
            check(Some(stamp), 8, 100),
            StampCheck::SchemaChanged {
                cooked: 7,
                current: 8
            }
        );
    }

    #[test]
    fn a_newer_scene_document_needs_a_rebuild() {
        let stamp = CookStamp::new(7, 100);
        assert_eq!(
            check(Some(stamp), 7, 200),
            StampCheck::SourcesNewer {
                cooked: 100,
                newest: 200
            }
        );
        // mtime 的秒级粒度 / 网络盘回拨：2 秒以内不算过期。
        assert_eq!(check(Some(stamp), 7, 102), StampCheck::Fresh);
    }

    #[test]
    fn a_package_without_a_stamp_is_flagged() {
        assert_eq!(check(None, 7, 0), StampCheck::Missing);
        assert!(StampCheck::Missing.needs_rebuild());
    }

    #[test]
    fn shipped_builds_skip_the_source_check() {
        // 发布包里没有 `.scene.json`，扫出来是 0，只比 schema。
        let stamp = CookStamp::new(7, 1_700_000_000);
        assert_eq!(check(Some(stamp), 7, 0), StampCheck::Fresh);
    }

    #[test]
    fn newest_source_mtime_only_looks_at_author_documents() {
        let dir = std::env::temp_dir().join(format!(
            "prism-cook-stamp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::write(dir.join("scenes/a.scene.json"), b"{}").unwrap();
        std::fs::write(dir.join("settings.ini"), b"[app]\n").unwrap();
        std::fs::write(dir.join("scenes.pak"), b"pak").unwrap();

        let newest = newest_source_mtime(&dir);
        assert!(newest > 0, "文档应被扫到");

        // 删掉文档后只剩 pak，返回 0（发布版形态）。
        std::fs::remove_dir_all(dir.join("scenes")).unwrap();
        std::fs::remove_file(dir.join("settings.ini")).unwrap();
        assert_eq!(newest_source_mtime(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
