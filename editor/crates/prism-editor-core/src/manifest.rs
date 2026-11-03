//! `assets/scenes.json` 场景清单（与 Web 编辑器的 Build Settings 同语义）。
//!
//! 顺序 = 构建顺序 = 启动顺序；第一个 `enabled = true` 的条目是启动场景。
//! `path` 是 `.pak` 里的逻辑路径（`scenes/xxx.rscn`），作者文档用
//! [`doc_relative_path`] 映射回 `assets/scenes/xxx.scene.json`。
//!
//! 形状：`{"scenes": [{"name": "...", "path": "...", "enabled": true, "guid": "..."}]}`。
//! 旧项目的 `assets/scenes.toml`（`[[scenes]]` 数组表）仍可读，写端只产出
//! `scenes.json`；一次性迁移由 `prism-build-pipeline build-scenes` 完成。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// 新格式清单路径（相对项目根）。
pub const MANIFEST_FILE: &str = "assets/scenes.json";
/// 旧格式清单路径（只读兼容）。
pub const LEGACY_MANIFEST_FILE: &str = "assets/scenes.toml";

fn default_enabled() -> bool {
    true
}

/// 清单里的一条场景。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneEntry {
    pub name: String,
    /// 逻辑路径（相对 `assets/`），例如 `scenes/default.rscn`。
    pub path: String,
    /// 是否参与构建 / 启动；缺省视为 true（兼容旧清单）。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 场景源文档的稳定 GUID（#466）。旧清单没有该键时按路径工作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
}

impl SceneEntry {
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            enabled: true,
            guid: None,
        }
    }
}

/// `assets/scenes.json` 的完整内容。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneManifest {
    #[serde(default)]
    pub scenes: Vec<SceneEntry>,
}

impl SceneManifest {
    pub fn read(root: &Path) -> io::Result<Self> {
        let Some(path) = Self::existing_path(root) else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(&path)?;
        let mut manifest = Self::parse(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("解析 {} 失败: {error}", path.display()),
            )
        })?;
        manifest.sync_guids(root);
        Ok(manifest)
    }

    /// 实际存在的清单文件：`scenes.json` 优先，回退旧 `scenes.toml`。
    pub fn existing_path(root: &Path) -> Option<PathBuf> {
        [MANIFEST_FILE, LEGACY_MANIFEST_FILE]
            .into_iter()
            .map(|rel| root.join(rel))
            .find(|path| path.is_file())
    }

    /// 解析：JSON 优先，失败时回退旧 TOML 格式。
    pub fn parse(text: &str) -> Result<Self, String> {
        match serde_json::from_str::<Self>(text) {
            Ok(manifest) => Ok(manifest),
            Err(json_error) => parse_legacy_toml(text).map_err(|toml_error| {
                format!("不是 scenes.json（{json_error}），也不是旧 scenes.toml（{toml_error}）")
            }),
        }
    }

    /// 校验后写回（新格式）。
    pub fn write(&self, root: &Path) -> io::Result<()> {
        let mut manifest = self.clone();
        manifest.sync_guids(root);
        manifest.validate()?;
        let json = serde_json::to_string_pretty(&manifest).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("序列化场景清单失败: {error}"),
            )
        })?;
        let path = root.join(MANIFEST_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{json}\n"))
    }

    /// Fill missing GUIDs from the scene source's `.rmeta` sidecar.
    ///
    /// `path` is a logical `.rscn` / `.scene.json` path; the sidecar lives next
    /// to the authoring `.scene.json`. Existing GUIDs are never overwritten.
    fn sync_guids(&mut self, root: &Path) {
        for entry in &mut self.scenes {
            if entry
                .guid
                .as_deref()
                .is_some_and(|guid| !guid.trim().is_empty())
            {
                continue;
            }
            let Some(doc_rel) = doc_relative_path(&entry.path) else {
                continue;
            };
            if let Some(meta) =
                crate::asset_meta::AssetMeta::read(root, &format!("assets/{doc_rel}"))
            {
                entry.guid = Some(meta.guid);
            }
        }
    }

    fn validate(&self) -> io::Result<()> {
        let mut names = HashSet::new();
        for entry in &self.scenes {
            if entry.name.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "场景名不能为空",
                ));
            }
            if entry.path.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("场景 '{}' 的路径不能为空", entry.name),
                ));
            }
            if !names.insert(entry.name.trim().to_string()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("场景名重复：{}", entry.name),
                ));
            }
        }
        Ok(())
    }

    /// 启动场景 = 第一个启用项。
    pub fn active(&self) -> Option<&SceneEntry> {
        self.scenes.iter().find(|entry| entry.enabled)
    }

    /// 构建顺序里的场景（只含启用项）。
    pub fn enabled(&self) -> impl Iterator<Item = &SceneEntry> {
        self.scenes.iter().filter(|entry| entry.enabled)
    }

    pub fn set_enabled(&mut self, index: usize, enabled: bool) -> bool {
        let Some(entry) = self.scenes.get_mut(index) else {
            return false;
        };
        if entry.enabled == enabled {
            return false;
        }
        entry.enabled = enabled;
        true
    }

    /// 上移 / 下移一条，返回新的下标。
    pub fn move_entry(&mut self, index: usize, delta: i32) -> Option<usize> {
        if index >= self.scenes.len() {
            return None;
        }
        let target = index as i32 + delta;
        if target < 0 || target as usize >= self.scenes.len() {
            return None;
        }
        let target = target as usize;
        self.scenes.swap(index, target);
        Some(target)
    }

    pub fn remove(&mut self, index: usize) -> Option<SceneEntry> {
        if index >= self.scenes.len() {
            return None;
        }
        Some(self.scenes.remove(index))
    }

    /// 某作者文档（相对 `assets/`）是否已经在清单里。
    pub fn contains_document(&self, doc_relative: &str) -> bool {
        let doc_relative = doc_relative.trim_start_matches("assets/");
        self.scenes
            .iter()
            .filter_map(|entry| doc_relative_path(&entry.path))
            .any(|path| path == doc_relative)
    }

    /// 把一个作者文档（相对 `assets/`，如 `scenes/foo.scene.json`）追加到清单末尾。
    ///
    /// 名字取文件名主干并自动去重（`foo-2`）；已存在同文档时返回 `None`。
    pub fn add_document(&mut self, doc_relative: &str) -> Option<usize> {
        let doc_relative = doc_relative.trim_start_matches("assets/");
        if doc_relative.is_empty() || self.contains_document(doc_relative) {
            return None;
        }
        let stem = doc_relative
            .strip_suffix(".scene.json")
            .unwrap_or(doc_relative);
        let base = stem.rsplit('/').next().unwrap_or(stem);
        let name = self.unique_name(base);
        let path = format!("{stem}.rscn");
        self.scenes.push(SceneEntry::new(name, path));
        Some(self.scenes.len() - 1)
    }

    fn unique_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() {
            "scene"
        } else {
            base
        };
        if !self.scenes.iter().any(|entry| entry.name == base) {
            return base.to_string();
        }
        for suffix in 2..10_000 {
            let candidate = format!("{base}-{suffix}");
            if !self.scenes.iter().any(|entry| entry.name == candidate) {
                return candidate;
            }
        }
        format!("{base}-{}", self.scenes.len() + 1)
    }
}

/// 旧格式 `scenes.toml` 的最小读取器（`[[scenes]]` + `name` / `path` / `enabled`）。
fn parse_legacy_toml(text: &str) -> Result<SceneManifest, String> {
    let mut scenes: Vec<SceneEntry> = Vec::new();
    let mut current: Option<SceneEntry> = None;
    let mut saw_table = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[scenes]]" {
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            current = Some(SceneEntry::new("", ""));
            saw_table = true;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = crate::ini::parse_value(value);
        match key.trim() {
            "name" => entry.name = value,
            "path" => entry.path = value,
            "enabled" => {
                entry.enabled = !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off"
                )
            }
            _ => {}
        }
    }
    if let Some(entry) = current.take() {
        scenes.push(entry);
    }

    if !saw_table {
        return Err("没有 [[scenes]] 表".into());
    }
    Ok(SceneManifest { scenes })
}

/// 清单路径（`scenes/x.rscn` / `scenes/x.scene.json`）→ 作者文档路径。
///
/// 其他格式（外部 glTF 等）没有对应的作者文档，返回 `None`。
pub fn doc_relative_path(entry_path: &str) -> Option<String> {
    let path = entry_path.trim().trim_start_matches("assets/");
    let stem = path
        .strip_suffix(".scene.json")
        .or_else(|| path.strip_suffix(".rscn"))?;
    Some(format!("{stem}.scene.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prism-editor-manifest-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_manifest_is_empty() {
        let dir = temp_dir("missing");
        let manifest = SceneManifest::read(&dir).unwrap();
        assert!(manifest.scenes.is_empty());
        assert!(manifest.active().is_none());
    }

    #[test]
    fn round_trip_keeps_order_and_enabled() {
        let dir = temp_dir("roundtrip");
        let manifest = SceneManifest {
            scenes: vec![
                SceneEntry::new("intro", "scenes/intro.rscn"),
                SceneEntry {
                    name: "default".into(),
                    path: "scenes/default.rscn".into(),
                    enabled: false,
                    guid: None,
                },
            ],
        };
        manifest.write(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).unwrap();
        assert!(text.contains("\"scenes\""), "{text}");
        let read = SceneManifest::read(&dir).unwrap();
        assert_eq!(read, manifest);
        assert_eq!(read.active().unwrap().name, "intro");
    }

    #[test]
    fn write_fills_and_preserves_scene_guid() {
        let dir = temp_dir("guid");
        std::fs::create_dir_all(dir.join("assets/scenes")).unwrap();
        let meta = crate::asset_meta::AssetMeta::ensure(
            &dir,
            "assets/scenes/intro.scene.json",
            "SceneImporter",
        )
        .unwrap();
        let manifest = SceneManifest {
            scenes: vec![SceneEntry::new("intro", "scenes/intro.rscn")],
        };
        manifest.write(&dir).unwrap();

        let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).unwrap();
        assert!(text.contains(&meta.guid), "{text}");
        let read = SceneManifest::read(&dir).unwrap();
        assert_eq!(read.scenes[0].guid.as_deref(), Some(meta.guid.as_str()));
    }

    #[test]
    fn legacy_toml_manifest_is_readable() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join(LEGACY_MANIFEST_FILE),
            "[[scenes]]\nname = \"old\"\npath = \"scenes/old.rscn\"\n",
        )
        .unwrap();
        let manifest = SceneManifest::read(&dir).unwrap();
        assert!(manifest.scenes[0].enabled);
        assert_eq!(manifest.scenes[0].name, "old");
    }

    #[test]
    fn invalid_manifest_is_reported() {
        let dir = temp_dir("invalid");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), "scenes = 3\n").unwrap();
        assert!(SceneManifest::read(&dir).is_err());
    }

    #[test]
    fn validation_rejects_empty_and_duplicate_names() {
        let dir = temp_dir("validate");
        let empty = SceneManifest {
            scenes: vec![SceneEntry::new("", "scenes/a.rscn")],
        };
        assert_eq!(
            empty.write(&dir).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let duplicate = SceneManifest {
            scenes: vec![
                SceneEntry::new("same", "scenes/a.rscn"),
                SceneEntry::new("same", "scenes/b.rscn"),
            ],
        };
        assert_eq!(
            duplicate.write(&dir).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!dir.join(MANIFEST_FILE).exists(), "校验失败不落盘");
    }

    #[test]
    fn document_paths_map_both_formats() {
        assert_eq!(
            doc_relative_path("scenes/default.rscn").as_deref(),
            Some("scenes/default.scene.json")
        );
        assert_eq!(
            doc_relative_path("assets/scenes/sub/a.scene.json").as_deref(),
            Some("scenes/sub/a.scene.json")
        );
        assert_eq!(doc_relative_path("models/city.gltf"), None);
    }

    #[test]
    fn add_document_dedupes_names_and_paths() {
        let mut manifest = SceneManifest::default();
        manifest
            .scenes
            .push(SceneEntry::new("level", "scenes/level.rscn"));
        let index = manifest
            .add_document("assets/scenes/level2.scene.json")
            .unwrap();
        assert_eq!(manifest.scenes[index].name, "level2");
        assert_eq!(manifest.scenes[index].path, "scenes/level2.rscn");
        assert!(manifest.add_document("scenes/level2.scene.json").is_none());

        // 同名（不同目录）自动加后缀。
        let other = manifest
            .add_document("scenes/sub/level2.scene.json")
            .unwrap();
        assert_eq!(manifest.scenes[other].name, "level2-2");
    }

    #[test]
    fn enable_move_remove_and_active() {
        let mut manifest = SceneManifest {
            scenes: vec![
                SceneEntry::new("a", "scenes/a.rscn"),
                SceneEntry::new("b", "scenes/b.rscn"),
                SceneEntry::new("c", "scenes/c.rscn"),
            ],
        };
        assert!(manifest.set_enabled(0, false));
        assert!(!manifest.set_enabled(0, false), "重复写入返回 false");
        assert_eq!(manifest.active().unwrap().name, "b");
        assert_eq!(manifest.enabled().count(), 2);

        let index = manifest.move_entry(2, -1).unwrap();
        assert_eq!(index, 1);
        assert_eq!(
            manifest
                .scenes
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert!(manifest.move_entry(0, -1).is_none());
        assert!(manifest.move_entry(2, 1).is_none());

        assert_eq!(manifest.remove(1).unwrap().name, "c");
        assert_eq!(manifest.scenes.len(), 2);
        assert!(manifest.remove(9).is_none());
    }
}
