//! 场景清单（`assets/scenes.json`）的读写。
//!
//! 形状：`{"scenes": [{"name": "...", "path": "...", "enabled": true, "guid": "..."}]}`
//! （`guid` 可选，见 #466）——与引擎 `prism_engine::scene::SceneManifest` 的
//! serde 形状一致，顺序即构建 / 启动顺序。
//!
//! 旧项目的 `assets/scenes.toml`（`[[scenes]]` 数组表）仍可读，但**写端只产出
//! `scenes.json`**；一次性迁移由 `prism-build-pipeline build-scenes` 完成。

use std::path::{Path, PathBuf};

use prism_asset::GuidIndex;
use prism_scene::references::{path_to_asset_relative, SyncMode};
use serde::{Deserialize, Serialize};

/// 新格式清单路径（相对项目根）。
pub const MANIFEST_FILE: &str = "assets/scenes.json";
/// 旧格式清单路径（只读兼容）。
pub const LEGACY_MANIFEST_FILE: &str = "assets/scenes.toml";

fn default_enabled() -> bool {
    true
}

/// 清单里的一条可运行场景。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneListEntry {
    pub name: String,
    /// 作者路径（`.scene.json` / `.rscn` / 外部场景）。
    pub path: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 场景源文档的稳定 GUID（#466）；旧清单缺省时按路径工作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SceneManifestFile {
    #[serde(default)]
    scenes: Vec<SceneListEntry>,
}

/// 写回目标：`<root>/assets/scenes.json`。
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}

/// 旧格式路径：`<root>/assets/scenes.toml`。
pub fn legacy_manifest_path(root: &Path) -> PathBuf {
    root.join(LEGACY_MANIFEST_FILE)
}

/// 实际存在的清单文件：`scenes.json` 优先，回退旧 `scenes.toml`。
pub fn existing_manifest(root: &Path) -> Option<PathBuf> {
    [manifest_path(root), legacy_manifest_path(root)]
        .into_iter()
        .find(|path| path.is_file())
}

/// 解析清单文本：JSON 优先，失败时回退旧 TOML 格式。
pub fn parse(text: &str) -> Result<Vec<SceneListEntry>, String> {
    match serde_json::from_str::<SceneManifestFile>(text) {
        Ok(file) => Ok(file.scenes),
        Err(json_error) => parse_legacy_toml(text).map_err(|toml_error| {
            format!("不是 scenes.json（{json_error}），也不是旧 scenes.toml（{toml_error}）")
        }),
    }
}

/// 读取清单条目；两个文件都不存在时返回空列表。
pub fn read(root: &Path) -> Result<Vec<SceneListEntry>, String> {
    let Some(path) = existing_manifest(root) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    let mut scenes =
        parse(&text).map_err(|error| format!("解析 {} 失败: {error}", path.display()))?;
    sync_guids(root, &mut scenes, SyncMode::Load);
    Ok(scenes)
}

/// 序列化为 `scenes.json` 文本（带缩进，便于人工 review / diff）。
pub fn render(scenes: &[SceneListEntry]) -> Result<String, String> {
    let file = SceneManifestFile {
        scenes: scenes.to_vec(),
    };
    serde_json::to_string_pretty(&file).map_err(|error| format!("序列化场景列表失败: {error}"))
}

/// 校验并写回 `assets/scenes.json`（顺序与 enabled 直接决定 build-scenes 的行为）。
pub fn write(root: &Path, scenes: &[SceneListEntry]) -> Result<(), String> {
    let mut scenes = scenes.to_vec();
    sync_guids(root, &mut scenes, SyncMode::Save);
    validate(&scenes)?;
    let path = manifest_path(root);
    let text = render(&scenes)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|error| format!("无法写入 {}: {error}", path.display()))
}

/// Resolve / fill GUIDs for manifest entries.
///
/// `Load` resolves a known GUID to the current path and fills a missing GUID
/// from the source sidecar. `Save` also refreshes a stale GUID from the path.
fn sync_guids(root: &Path, scenes: &mut [SceneListEntry], mode: SyncMode) {
    let assets_root = root.join("assets");
    let index = GuidIndex::scan(&assets_root);
    for entry in scenes {
        if let Some(guid) = entry.guid.as_deref().filter(|guid| !guid.trim().is_empty()) {
            if let Some(resolved) = index.path_for_guid(guid) {
                entry.path = manifest_style_path(&entry.path, resolved);
            }
        }

        let has_guid = entry
            .guid
            .as_deref()
            .is_some_and(|guid| !guid.trim().is_empty());
        if mode == SyncMode::Load && has_guid {
            continue;
        }

        if let Some(asset) = sidecar_asset_path(&entry.path) {
            if let Some(found) = index.guid_for_path(&asset) {
                if entry.guid.as_deref() != Some(found) {
                    entry.guid = Some(found.to_string());
                }
            }
        }
    }
}

/// The scene manifest stores pak paths (`.rscn`); sidecars belong to the
/// authoring `.scene.json`, so map `.rscn` back to its source document.
fn sidecar_asset_path(path: &str) -> Option<String> {
    let relative = path_to_asset_relative("", path)?;
    if let Some(stem) = relative.strip_suffix(".rscn") {
        Some(format!("{stem}.scene.json"))
    } else {
        Some(relative)
    }
}

fn manifest_style_path(current: &str, resolved: &str) -> String {
    if current.ends_with(".rscn") && resolved.ends_with(".scene.json") {
        format!("{}.rscn", resolved.trim_end_matches(".scene.json"))
    } else {
        resolved.to_string()
    }
}

/// 场景名非空 / 路径非空 / 名字不重复。
pub fn validate(scenes: &[SceneListEntry]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for entry in scenes {
        if entry.name.trim().is_empty() {
            return Err("场景名不能为空".into());
        }
        if entry.path.trim().is_empty() {
            return Err(format!("场景 '{}' 的路径不能为空", entry.name));
        }
        if !seen.insert(entry.name.trim()) {
            return Err(format!("场景名重复：{}", entry.name));
        }
    }
    Ok(())
}

/// 旧格式 `scenes.toml` 的最小读取器（`[[scenes]]` + `name` / `path` / `enabled`）。
///
/// 与 `prism_engine::scene::SceneManifest::parse` 的回退分支保持同一份语义。
fn parse_legacy_toml(text: &str) -> Result<Vec<SceneListEntry>, String> {
    let mut scenes: Vec<SceneListEntry> = Vec::new();
    let mut current: Option<SceneListEntry> = None;
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
            current = Some(SceneListEntry {
                name: String::new(),
                path: String::new(),
                enabled: true,
                guid: None,
            });
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
    Ok(scenes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prism-editor-api-manifest-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        dir
    }

    #[test]
    fn missing_manifest_is_empty() {
        let root = temp_root("missing");
        assert!(read(&root).unwrap().is_empty());
        assert!(!manifest_path(&root).exists(), "读不创建文件");
    }

    #[test]
    fn writes_json_and_round_trips_order_and_enabled() {
        let root = temp_root("roundtrip");
        let entries = vec![
            SceneListEntry {
                name: "first".into(),
                path: "scenes/first.rscn".into(),
                enabled: true,
                guid: None,
            },
            SceneListEntry {
                name: "second".into(),
                path: "scenes/second.scene.json".into(),
                enabled: false,
                guid: None,
            },
        ];
        write(&root, &entries).unwrap();

        let text = std::fs::read_to_string(manifest_path(&root)).unwrap();
        assert!(text.contains("\"scenes\""), "{text}");
        assert!(text.contains("\"name\": \"first\""), "{text}");
        assert_eq!(read(&root).unwrap(), entries);
    }

    #[test]
    fn manifest_guid_resolves_and_is_filled_from_the_sidecar() {
        let root = temp_root("guid");
        std::fs::create_dir_all(root.join("assets/scenes")).unwrap();
        std::fs::write(root.join("assets/scenes/renamed.scene.json"), b"{}").unwrap();
        let meta = prism_asset::AssetMeta::ensure(
            &root,
            "assets/scenes/renamed.scene.json",
            "SceneImporter",
        )
        .unwrap();

        std::fs::write(
            manifest_path(&root),
            serde_json::json!({
                "scenes": [{
                    "name": "a",
                    "path": "scenes/old.rscn",
                    "guid": meta.guid
                }]
            })
            .to_string(),
        )
        .unwrap();

        let scenes = read(&root).unwrap();
        assert_eq!(scenes[0].path, "scenes/renamed.rscn");
        assert_eq!(scenes[0].guid.as_deref(), Some(meta.guid.as_str()));

        // Saving a path-only entry adds the sidecar GUID.
        write(
            &root,
            &[SceneListEntry {
                name: "a".into(),
                path: "scenes/renamed.rscn".into(),
                enabled: true,
                guid: None,
            }],
        )
        .unwrap();
        let text = std::fs::read_to_string(manifest_path(&root)).unwrap();
        assert!(text.contains(&meta.guid), "{text}");
    }

    #[test]
    fn legacy_toml_is_readable_and_json_wins() {
        let root = temp_root("legacy");
        std::fs::write(
            legacy_manifest_path(&root),
            "# old\n[[scenes]]\nname = \"intro\"\npath = \"scenes/intro.rscn\"\n\
             [[scenes]]\nname = \"off\"\npath = \"scenes/off.rscn\"\nenabled = false\n",
        )
        .unwrap();
        let scenes = read(&root).unwrap();
        assert_eq!(scenes.len(), 2);
        assert!(scenes[0].enabled);
        assert!(!scenes[1].enabled);

        // 新文件写好之后，旧文件不再参与解析。
        write(
            &root,
            &[SceneListEntry {
                name: "new".into(),
                path: "scenes/new.rscn".into(),
                enabled: true,
                guid: None,
            }],
        )
        .unwrap();
        let scenes = read(&root).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "new");
    }

    #[test]
    fn invalid_manifest_is_reported() {
        let root = temp_root("invalid");
        std::fs::write(manifest_path(&root), "scenes = 3\n").unwrap();
        assert!(read(&root).is_err());
    }

    #[test]
    fn validation_rejects_empty_and_duplicate_entries() {
        let root = temp_root("validate");
        assert!(write(
            &root,
            &[SceneListEntry {
                name: "".into(),
                path: "scenes/a.rscn".into(),
                enabled: true,
                guid: None,
            }]
        )
        .is_err());
        assert!(write(
            &root,
            &[SceneListEntry {
                name: "a".into(),
                path: " ".into(),
                enabled: true,
                guid: None,
            }]
        )
        .is_err());
        let duplicate = vec![
            SceneListEntry {
                name: "same".into(),
                path: "scenes/a.rscn".into(),
                enabled: true,
                guid: None,
            },
            SceneListEntry {
                name: "same".into(),
                path: "scenes/b.rscn".into(),
                enabled: true,
                guid: None,
            },
        ];
        assert!(write(&root, &duplicate).is_err());
        assert!(!manifest_path(&root).exists(), "校验失败不落盘");
    }
}
