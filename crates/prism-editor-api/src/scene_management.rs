//! 场景管理门面：编辑器接口 +
//! 编辑器接口。
//!
//! 场景在 PrismaRev 里就是 `assets/scenes/*.scene.json` 作者文档，因此这里是
//! 「文件级打开 / 保存 / 枚举」+ 一个显式的运行时加载状态机。
//!
//! | | 这里 |
//! |---|---|
//! | `EditorSceneManager.NewScene(setup)` | [`new_scene`] |
//! | `EditorSceneManager.OpenScene(path)` | [`open_scene`] / [`open_scene_at_path`] |
//! | `EditorSceneManager.SaveScene(scene, path)` | [`save_scene`] / [`save_scene_as`] |
//! | `EditorSceneManager.SaveOpenScenes()` | [`save_open_scenes`] |
//! | `SceneManager.sceneCount` / `GetSceneAt` | [`scene_count`] / [`get_scene_at`] |
//! | `SceneManager.LoadScene(name, mode)` | [`SceneManagerState::load_scene`] |
//! | `SceneManager.SetActiveScene(scene)` | [`SceneManagerState::set_active_scene`] |
//!
//! 差异：没有引擎的多场景编辑（additive 打开多个 `.scene.json` 同时编辑）——
//! [`SceneManagerState`] 只记录「运行时加载了哪些场景」，不改变编辑器打开的文档。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::asset_database::asset_path_to_guid;
use crate::scene::{self, SceneEntry};

/// 场景描述。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Scene {
    /// 逻辑名（去掉后缀，如 `intro`）。
    pub name: String,
    /// 文件路径（枚举时是绝对路径，前端原样回传）。
    pub path: String,
    /// 路径派生的 GUID（见 [`crate::asset_database::asset_path_to_guid`]）。
    pub guid: String,
    /// 编辑器里是否有未保存修改 = `Scene.isDirty`。
    pub is_dirty: bool,
    /// 运行时是否已加载 = `Scene.isLoaded`。
    pub is_loaded: bool,
    /// 根实体数。
    pub root_count: usize,
    /// 实体总数。
    pub entity_count: usize,
    /// 构建顺序里的下标 = `Scene.buildIndex`（未构建时为 -1）。
    pub build_index: i32,
}

/// 加载模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LoadSceneMode {
    /// 替换当前加载的所有场景。
    Single,
    /// 追加到已加载场景之上。
    Additive,
}

/// 运行时加载状态 = `SceneManager` 的已加载场景集合 + 活动场景。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SceneManagerState {
    /// 已加载场景逻辑名（`Single` 会清空后只剩一个）。
    pub loaded: Vec<String>,
    /// 活动场景。
    pub active: Option<String>,
}

impl SceneManagerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// `SceneManager.LoadScene(name, mode)`。
    pub fn load_scene(&mut self, name: &str, mode: LoadSceneMode) {
        match mode {
            LoadSceneMode::Single => self.loaded = vec![name.to_string()],
            LoadSceneMode::Additive => {
                if !self.loaded.iter().any(|scene| scene == name) {
                    self.loaded.push(name.to_string());
                }
            }
        }
        self.active = Some(name.to_string());
    }

    /// 卸载场景（PrismaRev 没有引擎的 `UnloadSceneAsync`）。
    pub fn unload_scene(&mut self, name: &str) -> bool {
        let before = self.loaded.len();
        self.loaded.retain(|scene| scene != name);
        if self.active.as_deref() == Some(name) {
            self.active = self.loaded.last().cloned();
        }
        self.loaded.len() != before
    }

    /// `SceneManager.SetActiveScene(scene)`。
    pub fn set_active_scene(&mut self, name: &str) -> Result<(), String> {
        if !self.loaded.iter().any(|scene| scene == name) {
            return Err(format!("场景 {name} 未加载"));
        }
        self.active = Some(name.to_string());
        Ok(())
    }

    /// `SceneManager.sceneCount`。
    pub fn scene_count(&self) -> usize {
        self.loaded.len()
    }

    /// `SceneManager.GetSceneAt(index)`（返回逻辑名）。
    pub fn get_scene_at(&self, index: usize) -> Option<&str> {
        self.loaded.get(index).map(String::as_str)
    }

    /// 活动场景。
    pub fn active_scene(&self) -> Option<&str> {
        self.active.as_deref()
    }
}

// ---------------------------------------------------------------------------
// 编辑器场景管理（文件级）
// ---------------------------------------------------------------------------

/// 新建场景文档 = `EditorSceneManager.NewScene`。
///
/// `template` 为 `None` 时返回空场景（一个根实体），否则用 [`scene::template_scene_doc`]。
pub fn new_scene(template: Option<&str>) -> Value {
    match template {
        Some(id) => scene::template_scene_doc(id),
        None => scene::empty_scene_doc(),
    }
}

/// 打开项目里的场景（按逻辑名） = `EditorSceneManager.OpenScene`。
pub fn open_scene(project_root: &Path, name: &str) -> Result<Value, String> {
    let path = scene::scene_path(project_root, name)?;
    scene::read_doc_in_project(project_root, &path)
}

/// 按绝对路径打开场景。
pub fn open_scene_at_path(path: &Path) -> Result<Value, String> {
    match scene::project_root_for_doc(path) {
        Some(root) => scene::read_doc_in_project(&root, path),
        None => scene::read_doc(path),
    }
}

/// 保存场景（按逻辑名） = `EditorSceneManager.SaveScene`。返回写入的路径。
pub fn save_scene(project_root: &Path, name: &str, doc: &Value) -> Result<PathBuf, String> {
    let path = scene::scene_path(project_root, name)?;
    scene::write_doc_in_project(project_root, &path, doc)?;
    Ok(path)
}

/// 另存为（这里与 [`save_scene`] 等价：场景按逻辑名扁平存放）。
pub fn save_scene_as(project_root: &Path, name: &str, doc: &Value) -> Result<PathBuf, String> {
    save_scene(project_root, name, doc)
}

/// 保存一批已打开的场景 = `EditorSceneManager.SaveOpenScenes`。返回写入的路径。
pub fn save_open_scenes(
    project_root: &Path,
    docs: &[(String, Value)],
) -> Result<Vec<PathBuf>, String> {
    docs.iter()
        .map(|(name, doc)| save_scene(project_root, name, doc))
        .collect()
}

/// 场景数量 = `SceneManager.sceneCount`（这里是「项目里的场景文件数」）。
pub fn scene_count(project_root: &Path) -> Result<usize, String> {
    Ok(scene::list_scenes(project_root)?.len())
}

/// 第 `index` 个场景 = `SceneManager.GetSceneAt`。
pub fn get_scene_at(project_root: &Path, index: usize) -> Result<Scene, String> {
    let entries = scene::list_scenes(project_root)?;
    let entry = entries
        .get(index)
        .ok_or_else(|| format!("场景下标 {index} 越界（共 {}）", entries.len()))?;
    Ok(scene_from_entry(project_root, entry))
}

/// 列出全部场景描述（会读取每个文档以填充实体/根计数）。
pub fn get_loaded_scenes(project_root: &Path) -> Result<Vec<Scene>, String> {
    Ok(scene::list_scenes(project_root)?
        .iter()
        .map(|entry| scene_from_entry(project_root, entry))
        .collect())
}

/// 按逻辑名解析场景（不读文件内容） = `SceneManager.GetSceneByName`。
pub fn resolve_scene(project_root: &Path, name: &str) -> Result<Scene, String> {
    let entries = scene::list_scenes(project_root)?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| format!("找不到场景：{name}"))?;
    Ok(scene_from_entry(project_root, &entry))
}

/// 由文档构造场景描述（用于「已打开但未保存」的场景）。
pub fn scene_from_doc(project_root: &Path, name: &str, path: &str, doc: &Value) -> Scene {
    let mut scene = Scene {
        name: name.to_string(),
        path: path.to_string(),
        guid: guid_for(project_root, path),
        is_dirty: false,
        is_loaded: false,
        root_count: 0,
        entity_count: 0,
        build_index: -1,
    };
    fill_counts(&mut scene, doc);
    scene
}

fn scene_from_entry(project_root: &Path, entry: &SceneEntry) -> Scene {
    let mut scene = Scene {
        name: entry.name.clone(),
        path: entry.path.clone(),
        guid: guid_for(project_root, &entry.path),
        is_dirty: false,
        is_loaded: false,
        root_count: 0,
        entity_count: 0,
        build_index: -1,
    };
    if let Ok(doc) = scene::read_doc(Path::new(&entry.path)) {
        fill_counts(&mut scene, &doc);
    }
    scene
}

fn fill_counts(scene: &mut Scene, doc: &Value) {
    let Some(entities) = doc.get("entities").and_then(Value::as_array) else {
        return;
    };
    scene.entity_count = entities.len();
    scene.root_count = entities
        .iter()
        .filter(|entity| entity.get("parent").is_none_or(Value::is_null))
        .count();
}

fn guid_for(project_root: &Path, path: &str) -> String {
    let relative = Path::new(path)
        .strip_prefix(project_root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .ok()
        .unwrap_or_else(|| path.replace('\\', "/"));
    asset_path_to_guid(&relative).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "prism_editor_api_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(scene::scenes_dir(&dir)).unwrap();
        dir
    }

    #[test]
    fn new_scenes_validate() {
        let empty = new_scene(None);
        scene::validate_scene_doc(&empty).unwrap();
        assert_eq!(empty["entities"].as_array().unwrap().len(), 1);

        let templated = new_scene(Some("empty"));
        scene::validate_scene_doc(&templated).unwrap();
    }

    #[test]
    fn open_save_and_enumerate() {
        let dir = temp_dir("scenemgmt");
        let doc = scene::empty_scene_doc();
        let path = save_scene(&dir, "arena", &doc).unwrap();
        assert!(path.exists());

        let reopened = open_scene(&dir, "arena").unwrap();
        assert_eq!(reopened["entities"].as_array().unwrap().len(), 1);

        assert_eq!(scene_count(&dir).unwrap(), 1);
        let scene = get_scene_at(&dir, 0).unwrap();
        assert_eq!(scene.name, "arena");
        assert_eq!(scene.entity_count, 1);
        assert_eq!(scene.root_count, 1);
        assert!(scene.guid.len() == 32);
        assert!(get_scene_at(&dir, 9).is_err());

        let resolved = resolve_scene(&dir, "arena").unwrap();
        assert_eq!(resolved.name, "arena");
        assert!(resolve_scene(&dir, "missing").is_err());

        let listed = get_loaded_scenes(&dir).unwrap();
        assert_eq!(listed.len(), 1);

        let saved = save_open_scenes(&dir, &[("arena".into(), doc.clone())]).unwrap();
        assert_eq!(saved.len(), 1);
    }

    #[test]
    fn scene_manager_state_machine() {
        let mut state = SceneManagerState::new();
        assert_eq!(state.scene_count(), 0);

        state.load_scene("menu", LoadSceneMode::Single);
        state.load_scene("level1", LoadSceneMode::Additive);
        assert_eq!(state.scene_count(), 2);
        assert_eq!(state.get_scene_at(1), Some("level1"));
        assert_eq!(state.active_scene(), Some("level1"));

        // 重复 additive 不会重复加入。
        state.load_scene("level1", LoadSceneMode::Additive);
        assert_eq!(state.scene_count(), 2);

        state.set_active_scene("menu").unwrap();
        assert_eq!(state.active_scene(), Some("menu"));
        assert!(state.set_active_scene("nope").is_err());

        assert!(state.unload_scene("menu"));
        assert!(!state.unload_scene("menu"));
        assert_eq!(state.active_scene(), Some("level1"));

        state.load_scene("boss", LoadSceneMode::Single);
        assert_eq!(state.scene_count(), 1);
        assert_eq!(state.active_scene(), Some("boss"));
    }

    #[test]
    fn scene_from_doc_counts_roots() {
        let dir = Path::new("/tmp/proj");
        let doc = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "a", "parent": null, "transform": {}, "components": {} },
                { "name": "b", "parent": 0, "transform": {}, "components": {} },
                { "name": "c", "parent": null, "transform": {}, "components": {} }
            ]
        });
        let scene = scene_from_doc(dir, "t", "assets/scenes/t.scene.json", &doc);
        assert_eq!(scene.entity_count, 3);
        assert_eq!(scene.root_count, 2);
        assert_eq!(scene.guid.len(), 32);
    }
}
