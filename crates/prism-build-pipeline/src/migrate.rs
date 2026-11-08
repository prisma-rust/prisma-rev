//! 旧项目配置的一次性迁移：`settings.toml` → `settings.ini`、
//! `scenes.toml` → `scenes.json`。
//!
//! 只在**新文件不存在**时转换，因此重复执行是安全的（幂等），也不会覆盖
//! 用户已经改过的新格式文件。旧文件保留在磁盘上（引擎优先读新文件），
//! 迁移只负责让旧项目继续能跑；确认无碍后可以手工删除。
//!
//! `settings.toml` 的内容与 INI 方言完全重合（`[section]` + `key = value` +
//! 引号字符串 + 裸的布尔/数字），所以直接复用引擎的 INI 解析器重排一遍；
//! 场景清单的 `[[scenes]]` 数组表则复用
//! [`prism_scene::SceneManifest::parse`] 的旧格式分支。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prism_ini::Ini;
use prism_scene::SceneManifest;

/// 本次迁移写了哪些文件（`None` = 该文件不需要迁移）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationSummary {
    pub settings: Option<PathBuf>,
    pub scenes: Option<PathBuf>,
}

impl MigrationSummary {
    pub fn is_empty(&self) -> bool {
        self.settings.is_none() && self.scenes.is_none()
    }
}

/// 在 `assets/` 目录上执行迁移，返回实际写出的新文件。
///
/// `assets_dir` 是**包含** `settings.toml` / `scenes.toml` 的目录（通常是
/// `<project>/assets`）。
pub fn migrate_legacy_config(assets_dir: &Path) -> Result<MigrationSummary> {
    let mut summary = MigrationSummary::default();

    if let Some(path) = migrate_settings(assets_dir)? {
        log::info!(
            "migrated {} -> settings.ini",
            assets_dir.join("settings.toml").display()
        );
        summary.settings = Some(path);
    }
    if let Some(path) = migrate_scenes(assets_dir)? {
        log::info!(
            "migrated {} -> scenes.json",
            assets_dir.join("scenes.toml").display()
        );
        summary.scenes = Some(path);
    }

    Ok(summary)
}

fn migrate_settings(assets_dir: &Path) -> Result<Option<PathBuf>> {
    let legacy = assets_dir.join("settings.toml");
    let target = assets_dir.join("settings.ini");
    if target.is_file() || !legacy.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&legacy).with_context(|| format!("read {}", legacy.display()))?;
    let document = Ini::parse(&text);
    if document.sections().is_empty() {
        log::warn!("{} has no [section]; leaving it alone", legacy.display());
        return Ok(None);
    }
    std::fs::write(&target, document.render())
        .with_context(|| format!("write {}", target.display()))?;
    Ok(Some(target))
}

fn migrate_scenes(assets_dir: &Path) -> Result<Option<PathBuf>> {
    let legacy = assets_dir.join("scenes.toml");
    let target = assets_dir.join("scenes.json");
    if target.is_file() || !legacy.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&legacy).with_context(|| format!("read {}", legacy.display()))?;
    let manifest = SceneManifest::parse(&text)
        .map_err(|error| anyhow::anyhow!("parse {}: {error}", legacy.display()))?;
    let json =
        serde_json::to_string_pretty(&manifest).context("serialize the migrated scene manifest")?;
    std::fs::write(&target, format!("{json}\n"))
        .with_context(|| format!("write {}", target.display()))?;
    Ok(Some(target))
}

/// 解析 `build-scenes` 的清单路径：先迁移旧配置，再返回**实际要读**的文件。
///
/// 默认参数是 `assets/scenes.json`；旧项目只有 `scenes.toml` 时会先迁移成
/// 同目录的 `scenes.json`，因此这里只做「存在性 + 同目录回退」这一步。
pub fn resolve_scene_manifest(manifest: &Path) -> Result<PathBuf> {
    let assets_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    migrate_legacy_config(assets_dir)?;

    if manifest.is_file() {
        return Ok(manifest.to_path_buf());
    }
    let sibling = assets_dir.join("scenes.json");
    if sibling.is_file() {
        return Ok(sibling);
    }
    anyhow::bail!("scene manifest not found: {}", manifest.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_assets(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prism-migrate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrates_settings_keeping_every_section_and_key() {
        let assets = temp_assets("settings");
        std::fs::write(
            assets.join("settings.toml"),
            "[app]\ncompany = \"Acme\"\nname = \"Game\"\n\n[window]\nwidth = 1280\nvsync = false\n",
        )
        .unwrap();

        let summary = migrate_legacy_config(&assets).unwrap();
        assert!(summary.settings.is_some());
        assert!(summary.scenes.is_none());

        let text = std::fs::read_to_string(assets.join("settings.ini")).unwrap();
        let document = Ini::parse(&text);
        assert_eq!(document.get("app", "company"), Some("Acme"));
        assert_eq!(document.get_u32("window", "width"), Some(1280));
        assert_eq!(document.get_bool("window", "vsync"), Some(false));
        // 旧文件保留，用户确认后可自行删除。
        assert!(assets.join("settings.toml").is_file());
        std::fs::remove_dir_all(&assets).ok();
    }

    #[test]
    fn migrates_scenes_and_is_idempotent() {
        let assets = temp_assets("scenes");
        std::fs::write(
            assets.join("scenes.toml"),
            "# hand written\n[[scenes]]\nname = \"intro\"\npath = \"scenes/intro.rscn\"\n\
             [[scenes]]\nname = \"draft\"\npath = \"scenes/draft.rscn\"\nenabled = false\n",
        )
        .unwrap();

        let summary = migrate_legacy_config(&assets).unwrap();
        assert!(summary.scenes.is_some());
        let text = std::fs::read_to_string(assets.join("scenes.json")).unwrap();
        assert!(text.contains("\"intro\""), "{text}");
        assert!(text.contains("\"enabled\": false"), "{text}");

        // 第二次不会再写（新文件已存在）。
        let again = migrate_legacy_config(&assets).unwrap();
        assert!(again.is_empty(), "{again:?}");
        std::fs::remove_dir_all(&assets).ok();
    }

    #[test]
    fn resolve_manifest_prefers_the_new_file_and_migrates_first() {
        let assets = temp_assets("resolve");
        std::fs::write(
            assets.join("scenes.toml"),
            "[[scenes]]\nname = \"a\"\npath = \"scenes/a.rscn\"\n",
        )
        .unwrap();

        // 旧项目：只给了默认的新路径。
        let resolved = resolve_scene_manifest(&assets.join("scenes.json")).unwrap();
        assert_eq!(resolved, assets.join("scenes.json"));
        assert!(resolved.is_file());
        std::fs::remove_dir_all(&assets).ok();
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let assets = temp_assets("missing");
        let error = resolve_scene_manifest(&assets.join("scenes.json")).unwrap_err();
        assert!(error.to_string().contains("not found"));
        std::fs::remove_dir_all(&assets).ok();
    }
}
