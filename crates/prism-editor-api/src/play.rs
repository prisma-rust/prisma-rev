//! 场景清单（`assets/scenes.json`）：枚举可运行场景 + 运行时包状态。
//!
//! 编辑器「运行」需要知道引擎真正认识的场景名（manifest 的
//! `scenes[].name`），而不是 `assets/scenes/*.scene.json` 的文件名——
//! 两者可以不同（例如外部 glTF 场景）。本模块只读取清单，不 cook。
//!
//! 读写实现（含旧 `scenes.toml` 回退）在 [`crate::scene_manifest`]；这里提供
//! 编辑器侧的类型与「清单条目 → 作者文档」的映射。

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use crate::scene_manifest::SceneListEntry;

/// `assets/scenes.json` 中的一个可运行场景。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayableScene {
    /// 引擎 `LaunchConfig.scene` 使用的名字。
    pub name: String,
    /// 清单里的资源路径（`.rscn` 或外部场景文件）。
    pub path: String,
    /// 对应的作者文档（`assets/scenes/<name>.scene.json`，存在时）。
    pub doc_path: Option<String>,
}

/// 运行时场景包状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenePackageStatus {
    /// `assets/scenes.pak` 是否存在。
    pub exists: bool,
    /// 包大小（字节，包不存在时为 0）。
    pub size_bytes: u64,
}

/// `assets/scenes.json` 的路径（写端目标）。
pub fn manifest_path(root: &Path) -> PathBuf {
    crate::scene_manifest::manifest_path(root)
}

/// 读取可编辑的场景列表（保留顺序；`scenes.json` 缺失时回退旧 `scenes.toml`）。
pub fn read_scene_manifest(root: &Path) -> Result<Vec<SceneListEntry>, String> {
    crate::scene_manifest::read(root)
}

/// 写回场景列表（顺序与 enabled 直接决定 build-scenes 的行为）。
pub fn write_scene_manifest(root: &Path, scenes: &[SceneListEntry]) -> Result<(), String> {
    crate::scene_manifest::write(root, scenes)
}

/// 由清单条目路径推出作者文档（相对 `assets/`）。
///
/// 与 `prism-build-pipeline` 的 `build-scenes` 使用同一约定：`.scene.json` 是作者文档
/// 本身，`.rscn` 是它的 cook 产物，两者都映射回 `<stem>.scene.json`，并保留子目录。
/// 其他格式（外部 glTF 等）没有对应的作者文档。
fn doc_relative_path(entry_path: &str) -> Option<String> {
    let stem = entry_path
        .strip_suffix(".scene.json")
        .or_else(|| entry_path.strip_suffix(".rscn"))?;
    Some(format!("{stem}.scene.json"))
}

/// 枚举清单中所有启用的场景，保持清单顺序（= 构建 / 启动顺序）。
pub fn list_playable_scenes(root: &Path) -> Result<Vec<PlayableScene>, String> {
    let scenes = read_scene_manifest(root)?;

    let assets = root.join("assets");
    Ok(scenes
        .into_iter()
        .filter(|entry| entry.enabled)
        .map(|entry| {
            let doc = doc_relative_path(&entry.path)
                .map(|rel| assets.join(rel))
                .filter(|doc| doc.is_file());
            PlayableScene {
                name: entry.name,
                path: entry.path,
                doc_path: doc.map(|doc| doc.to_string_lossy().into_owned()),
            }
        })
        .collect())
}

/// 运行时包状态（`assets/scenes.pak`）。
pub fn scene_package_status(root: &Path) -> ScenePackageStatus {
    let path = root.join("assets").join("scenes.pak");
    match std::fs::metadata(&path) {
        Ok(meta) => ScenePackageStatus {
            exists: true,
            size_bytes: meta.len(),
        },
        Err(_) => ScenePackageStatus {
            exists: false,
            size_bytes: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个只含空 `assets/` 的项目目录；同名目录先清空，避免测试互相污染。
    fn temp_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prism-editor-api-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        dir
    }

    fn entry(name: &str, path: &str) -> SceneListEntry {
        SceneListEntry {
            name: name.into(),
            path: path.into(),
            enabled: true,
            guid: None,
        }
    }

    #[test]
    fn missing_manifest_is_empty() {
        let dir = std::env::temp_dir().join("prism-editor-api-play-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(list_playable_scenes(&dir).unwrap().is_empty());
    }

    #[test]
    fn manifest_entries_map_to_docs() {
        let dir = std::env::temp_dir().join("prism-editor-api-play-map");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets/scenes")).unwrap();
        write_scene_manifest(&dir, &[entry("intro", "scenes/intro.rscn")]).unwrap();
        std::fs::write(dir.join("assets/scenes/intro.scene.json"), "{}").unwrap();

        let scenes = list_playable_scenes(&dir).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "intro");
        assert!(scenes[0].doc_path.is_some());
    }

    #[test]
    fn disabled_scene_is_not_playable() {
        let dir = std::env::temp_dir().join("prism-editor-api-play-disabled");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        write_scene_manifest(
            &dir,
            &[
                SceneListEntry {
                    enabled: false,
                    ..entry("a", "scenes/a.rscn")
                },
                entry("b", "scenes/b.rscn"),
            ],
        )
        .unwrap();
        let scenes = list_playable_scenes(&dir).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "b");
    }

    #[test]
    fn scene_json_entry_maps_to_the_same_document() {
        let dir = temp_project("play-scene-json-doc");
        write_scene_manifest(&dir, &[entry("intro", "scenes/intro.scene.json")]).unwrap();
        std::fs::create_dir_all(dir.join("assets/scenes")).unwrap();
        std::fs::write(dir.join("assets/scenes/intro.scene.json"), "{}").unwrap();

        let scenes = list_playable_scenes(&dir).unwrap();
        assert_eq!(scenes.len(), 1);
        let doc = scenes[0].doc_path.as_deref().expect("doc path");
        assert_eq!(
            Path::new(doc),
            dir.join("assets").join("scenes").join("intro.scene.json")
        );
    }

    #[test]
    fn nested_scene_paths_keep_their_directory() {
        let dir = temp_project("play-nested-doc");
        write_scene_manifest(&dir, &[entry("chapter", "scenes/chapter/intro.rscn")]).unwrap();
        std::fs::create_dir_all(dir.join("assets/scenes/chapter")).unwrap();
        std::fs::write(dir.join("assets/scenes/chapter/intro.scene.json"), "{}").unwrap();

        let scenes = list_playable_scenes(&dir).unwrap();
        let doc = scenes[0].doc_path.as_deref().expect("doc path");
        assert_eq!(
            Path::new(doc),
            dir.join("assets")
                .join("scenes")
                .join("chapter")
                .join("intro.scene.json")
        );
    }

    #[test]
    fn non_scene_formats_have_no_doc_path() {
        let dir = temp_project("play-no-doc");
        write_scene_manifest(&dir, &[entry("sponza", "models/sponza.gltf")]).unwrap();

        let scenes = list_playable_scenes(&dir).unwrap();
        assert_eq!(scenes[0].doc_path, None);
    }

    #[test]
    fn playable_scenes_keep_manifest_order() {
        let dir = temp_project("play-order");
        write_scene_manifest(
            &dir,
            &[
                entry("zulu", "scenes/zulu.rscn"),
                entry("alpha", "scenes/alpha.rscn"),
                entry("mike", "scenes/mike.rscn"),
            ],
        )
        .unwrap();

        let names: Vec<String> = list_playable_scenes(&dir)
            .unwrap()
            .into_iter()
            .map(|scene| scene.name)
            .collect();
        assert_eq!(names, ["zulu", "alpha", "mike"]);
    }

    #[test]
    fn legacy_toml_manifest_still_lists_scenes() {
        let dir = temp_project("play-legacy");
        std::fs::write(
            dir.join("assets/scenes.toml"),
            "[[scenes]]\nname = \"old\"\npath = \"scenes/old.rscn\"\n",
        )
        .unwrap();
        let scenes = list_playable_scenes(&dir).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "old");
    }
}
