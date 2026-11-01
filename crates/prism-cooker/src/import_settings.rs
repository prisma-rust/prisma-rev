//! 每资产导入设置（#514）：`<asset>.import.json` 旁挂文件。
//!
//! 格式很小，只覆盖 cooker 真正会消费的字段：
//!
//! ```json
//! { "maxSize": 1024, "generateMips": false }
//! ```
//!
//! - 字段缺失 = 跟随 profile（不是「关掉」）；写 `null` 与缺失等价；
//! - 文件名是 `assets/sprites/boat.png.import.json`，与 Unity 的 `.meta` 同样的
//!   「身份跟着源文件走」思路，但不引入 GUID；
//! - 编辑器（`prism-editor-egui`）用同一份 JSON 键读写，所以这里也是格式的单一事实源
//!   （编辑器工作区不链接 cooker，只能按文档同步这两个键）。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cooker::profile::CookSettings;

/// 纹理导入设置覆盖（缺省 = 跟随 profile）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportSettings {
    /// 最长边上限（像素）；`None` / `0` = 不限。
    pub max_size: Option<u32>,
    /// 是否生成 mip 链；`None` = 跟随 profile。
    pub generate_mips: Option<bool>,
}

impl ImportSettings {
    /// 源文件旁边的旁挂路径：`boat.png` → `boat.png.import.json`。
    pub fn sidecar_path(source: &Path) -> PathBuf {
        let mut path = source.as_os_str().to_owned();
        path.push(".import.json");
        PathBuf::from(path)
    }

    /// 读旁挂文件；不存在 / 解析失败时返回默认值并 `warn!`（坏文件不能让构建失败）。
    pub fn load(source: &Path) -> Self {
        let path = Self::sidecar_path(source);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!("import settings {} are invalid: {error}", path.display());
                Self::default()
            }
        }
    }

    /// 写旁挂文件（编辑器用）。父目录必须已存在。
    pub fn save(&self, source: &Path) -> io::Result<()> {
        let path = Self::sidecar_path(source);
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(path, text)
    }

    /// 覆盖到 cooker 设置上；`None` 的字段保持 profile 值。
    pub fn apply(&self, settings: &mut CookSettings) {
        if let Some(max_size) = self.max_size {
            settings.texture.max_size = max_size;
        }
        if let Some(generate_mips) = self.generate_mips {
            settings.texture.generate_mips = generate_mips;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_appends_to_the_extension() {
        assert_eq!(
            ImportSettings::sidecar_path(Path::new("assets/sprites/boat.png")),
            PathBuf::from("assets/sprites/boat.png.import.json")
        );
    }

    #[test]
    fn missing_or_broken_sidecars_fall_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("prism-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("tex.png");
        std::fs::write(&source, b"not an image").unwrap();

        assert_eq!(ImportSettings::load(&source), ImportSettings::default());

        std::fs::write(ImportSettings::sidecar_path(&source), b"{ nope").unwrap();
        assert_eq!(ImportSettings::load(&source), ImportSettings::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_and_loading_round_trips_and_applies_only_present_fields() {
        let dir = std::env::temp_dir().join(format!("prism-import-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("tex.png");
        let settings = ImportSettings {
            max_size: Some(512),
            generate_mips: Some(false),
        };
        settings.save(&source).unwrap();
        assert_eq!(ImportSettings::load(&source), settings);

        let mut profile = CookSettings::default();
        settings.apply(&mut profile);
        assert_eq!(profile.texture.max_size, 512);
        assert!(!profile.texture.generate_mips);

        // 缺省字段不覆盖 profile。
        ImportSettings::default().apply(&mut profile);
        assert_eq!(profile.texture.max_size, 512);

        std::fs::remove_dir_all(&dir).ok();
    }
}
