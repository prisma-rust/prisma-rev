//! 编辑器门面。
//!
//! 直接架在 [`crate::scene`] 的预制体原语之上；所有「预制体」都是
//! `assets/**/*.prefab.json` 作者文档，反向链接记在实例根实体的
//! [`crate::scene::PREFAB_LINK_COMPONENT`] 组件里。
//!
//! | | 这里 |
//! |---|---|
//! | `SaveAsPrefabAsset(instanceRoot, path)` | [`save_as_prefab_asset`] |
//! | `SaveAsPrefabAssetAndConnect(instanceRoot, path, mode)` | [`save_as_prefab_asset_and_connect`] |
//! | `InstantiatePrefab(asset)` | [`instantiate_prefab`] |
//! | `ApplyPrefabInstance(instanceRoot)` | [`apply_prefab_instance`] |
//! | `RevertPrefabInstance(instanceRoot)` | [`revert_prefab_instance`] |
//! | `UnpackPrefabInstance(root, mode)` | [`unpack_prefab_instance`] |
//! | `GetPrefabAssetType(asset)` | [`get_prefab_asset_type`] |
//! | `GetPrefabInstanceStatus(root)` | [`get_prefab_instance_status`] |
//! | `GetCorrespondingObjectFromSource(obj)` | [`get_corresponding_object_from_source`] |
//! | `GetNearestPrefabInstanceRoot(obj)` | [`get_nearest_prefab_instance_root`] |
//! | `IsPartOfPrefabInstance(obj)` | [`is_part_of_prefab_instance`] |
//!
//! 差异：没有 nested prefab / override / variant 覆盖系统——实例与非实例只有
//! 「有没有反向链接」两种状态；`PrefabUnpackMode` 的两个取值都等价于解除链接。

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::asset_ops::normalize_asset_path;
use crate::scene::{self, InstantiateResult, MoveResult, PrefabLink, PREFAB_LINK_COMPONENT};

/// 新建预制体的默认目录（引擎的 `Prefabs` 约定）。
pub const DEFAULT_PREFAB_FOLDER: &str = "assets/prefabs";

/// 资产是不是预制体 = `PrefabAssetType`。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PrefabAssetType {
    /// 普通预制体。
    Regular,
    /// 预制体变体（根实体本身又是别的预制体的实例）。
    Variant,
    /// 不是预制体（文档为空 / 结构不合法）。
    NotAPrefab,
}

/// 实例的链接状态 = `PrefabInstanceStatus`。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PrefabInstanceStatus {
    /// 该实体不在任何预制体实例里。
    NotAPrefab,
    /// 该实体属于某个实例（本身是根或其后代）。
    Connected,
}

/// 解包模式 = `PrefabUnpackMode`。两者在本实现里等价（见模块头差异说明）。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PrefabUnpackMode {
    OutermostRoot,
    Completely,
}

/// [`save_as_prefab_asset_and_connect`] 的结果：预制体资产 + 替换成实例后的场景。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrefabConnect {
    /// 写出的预制体文档（根上没有反向链接）。
    pub asset: Value,
    /// 场景文档：原子树已被替换成该预制体的实例，并记录了反向链接。
    pub scene: Value,
    /// 预制体路径（相对项目根）。
    pub path: String,
}

/// 读取预制体文件。
pub fn read_prefab(project_root: &Path, prefab_path: &str) -> Result<Value, String> {
    let rel = normalize_asset_path(prefab_path)?;
    scene::read_doc_in_project(project_root, &project_root.join(rel))
}

/// 从场景里提取子树作为预制体资产（根的反向链接会被剥掉）= `SaveAsPrefabAsset`。
pub fn save_as_prefab_asset(scene_doc: &Value, root: usize) -> Result<Value, String> {
    scene::extract_subtree(scene_doc, root)
}

/// 提取子树 + 就地替换成实例并记录链接 = `SaveAsPrefabAssetAndConnect`。
pub fn save_as_prefab_asset_and_connect(
    scene_doc: &Value,
    root: usize,
    prefab_path: &str,
) -> Result<PrefabConnect, String> {
    let rel = normalize_asset_path(prefab_path)?;
    let asset = scene::extract_subtree(scene_doc, root)?;
    let replaced = scene::revert_prefab_instance(scene_doc, root, &asset, Some(&rel))?;
    Ok(PrefabConnect {
        asset,
        scene: replaced.doc,
        path: rel,
    })
}

/// 把预制体实例化进场景（读取预制体文件 + 记录链接）= `InstantiatePrefab`。
pub fn instantiate_prefab(
    project_root: &Path,
    scene_doc: &Value,
    prefab_path: &str,
    parent: Option<u32>,
) -> Result<InstantiateResult, String> {
    let rel = normalize_asset_path(prefab_path)?;
    let prefab = scene::read_doc_in_project(project_root, &project_root.join(&rel))?;
    scene::instantiate_prefab(scene_doc, &prefab, parent, Some(&rel))
}

/// 把实例的当前内容写回预制体文件 = `ApplyPrefabInstance`。返回写入的预制体文档。
pub fn apply_prefab_instance(
    project_root: &Path,
    scene_doc: &Value,
    root: usize,
) -> Result<Value, String> {
    let link = link_at(scene_doc, root)?;
    let asset = scene::extract_subtree(scene_doc, root)?;
    let rel = normalize_asset_path(&link.prefab)?;
    let path = project_root.join(rel);
    let asset = scene::sync_doc_for_save(project_root, &path, &asset)?;
    scene::write_doc(&path, &asset)?;
    Ok(asset)
}

/// 把实例还原成预制体的当前内容 = `RevertPrefabInstance`。
pub fn revert_prefab_instance(
    project_root: &Path,
    scene_doc: &Value,
    root: usize,
) -> Result<MoveResult, String> {
    let link = link_at(scene_doc, root)?;
    let prefab = read_prefab(project_root, &link.prefab)?;
    scene::revert_prefab_instance(scene_doc, root, &prefab, Some(&link.prefab))
}

/// 同步本文档里所有同源实例 = 批量 `RevertPrefabInstance`。
pub fn sync_prefab_instances(
    project_root: &Path,
    scene_doc: &Value,
    prefab_path: &str,
) -> Result<Value, String> {
    let rel = normalize_asset_path(prefab_path)?;
    let prefab = scene::read_doc_in_project(project_root, &project_root.join(&rel))?;
    scene::sync_prefab_instances(scene_doc, &rel, &prefab)
}

/// 解除实例链接（只去记录，不动实体内容）= `UnpackPrefabInstance`。
pub fn unpack_prefab_instance(
    scene_doc: &Value,
    root: usize,
    _mode: PrefabUnpackMode,
) -> Result<Value, String> {
    scene::unlink_prefab_instance(scene_doc, root)
}

/// 文档里所有实例根 = 反向链接列表。
pub fn get_prefab_instance_roots(scene_doc: &Value) -> Vec<PrefabLink> {
    scene::prefab_instances(scene_doc)
}

/// 预制体资产的类型（普通 / 变体 / 非预制体）= `GetPrefabAssetType`。
pub fn get_prefab_asset_type(prefab: &Value) -> PrefabAssetType {
    let Ok(entities) = entities(prefab) else {
        return PrefabAssetType::NotAPrefab;
    };
    if entities.is_empty() {
        return PrefabAssetType::NotAPrefab;
    }
    if scene::prefab_instances(prefab)
        .iter()
        .any(|link| link.index == 0)
    {
        PrefabAssetType::Variant
    } else {
        PrefabAssetType::Regular
    }
}

/// 实体的实例状态 = `GetPrefabInstanceStatus`。
pub fn get_prefab_instance_status(scene_doc: &Value, root: usize) -> PrefabInstanceStatus {
    if is_part_of_prefab_instance(scene_doc, root) {
        PrefabInstanceStatus::Connected
    } else {
        PrefabInstanceStatus::NotAPrefab
    }
}

/// 实体（或其祖先）是否属于某个预制体实例 = `IsPartOfPrefabInstance`。
pub fn is_part_of_prefab_instance(scene_doc: &Value, root: usize) -> bool {
    get_nearest_prefab_instance_root(scene_doc, root).is_some()
}

/// 最近的实例根（含实体自身）= `GetNearestPrefabInstanceRoot`。
pub fn get_nearest_prefab_instance_root(scene_doc: &Value, root: usize) -> Option<usize> {
    // 先规范化：v3 起 `parent` 引用实体 id，缺 id 的老文档需要先补齐。
    let doc = &prism_scene::normalize_scene_doc(scene_doc).unwrap_or_else(|_| scene_doc.clone());
    let entities = entities(doc).ok()?;
    if root >= entities.len() {
        return None;
    }
    let mut current = Some(root);
    let mut guard = 0;
    while let Some(index) = current {
        if entity_link(&entities[index]).is_some() {
            return Some(index);
        }
        guard += 1;
        if guard > entities.len() {
            return None; // 数据成环，保守放弃
        }
        // `parent` 是 id → 解析成下标再上溯。
        let parent_id = entities[index].get("parent").and_then(Value::as_u64);
        current = parent_id.and_then(|parent_id| {
            entities
                .iter()
                .position(|entity| entity.get("id").and_then(Value::as_u64) == Some(parent_id))
        });
    }
    None
}

/// 实例根对应的来源 = `GetCorrespondingObjectFromSource`。
pub fn get_corresponding_object_from_source(scene_doc: &Value, root: usize) -> Option<PrefabLink> {
    let nearest = get_nearest_prefab_instance_root(scene_doc, root)?;
    scene::prefab_instances(scene_doc)
        .into_iter()
        .find(|link| link.index == nearest)
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

fn entities(doc: &Value) -> Result<&Vec<Value>, String> {
    doc.get("entities")
        .and_then(Value::as_array)
        .ok_or_else(|| "文档缺少 entities 数组".to_string())
}

fn entity_link(entity: &Value) -> Option<String> {
    entity
        .get("components")?
        .get(PREFAB_LINK_COMPONENT)?
        .get("prefab")?
        .as_str()
        .map(str::to_string)
}

fn link_at(scene_doc: &Value, root: usize) -> Result<PrefabLink, String> {
    scene::prefab_instances(scene_doc)
        .into_iter()
        .find(|link| link.index == root)
        .ok_or_else(|| format!("实体下标 {root} 不是预制体实例根"))
}

/// 便于前端消费的反向链接 DTO（与引擎的 `PrefabInstance` 概念对应）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrefabInstanceInfo {
    pub index: usize,
    pub prefab: String,
    pub status: PrefabInstanceStatus,
}

/// 把所有实例根整理成 DTO 列表。
pub fn prefab_instance_infos(scene_doc: &Value) -> Vec<PrefabInstanceInfo> {
    get_prefab_instance_roots(scene_doc)
        .into_iter()
        .map(|link| PrefabInstanceInfo {
            index: link.index,
            prefab: link.prefab,
            status: PrefabInstanceStatus::Connected,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 三个实体：`Root`(0) + `Child`(1) 是要提取的子树，`Keep`(2) 留在场景里。
    fn scene_doc() -> Value {
        json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "Root", "parent": null, "transform": {}, "components": {} },
                { "name": "Child", "parent": 0, "transform": {}, "components": {} },
                { "name": "Keep", "parent": null, "transform": {}, "components": {} }
            ]
        })
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "prism_editor_api_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("assets/prefabs")).unwrap();
        dir
    }

    #[test]
    fn extract_and_classify() {
        let doc = scene_doc();
        let asset = save_as_prefab_asset(&doc, 0).unwrap();
        assert_eq!(asset["entities"].as_array().unwrap().len(), 2);
        assert_eq!(get_prefab_asset_type(&asset), PrefabAssetType::Regular);
        assert_eq!(
            get_prefab_asset_type(&json!({ "entities": [] })),
            PrefabAssetType::NotAPrefab
        );

        // 变体：根本身又是实例。
        let mut variant = asset.clone();
        variant["entities"][0]["components"][PREFAB_LINK_COMPONENT] =
            json!({ "prefab": "assets/prefabs/base.prefab.json" });
        assert_eq!(get_prefab_asset_type(&variant), PrefabAssetType::Variant);
    }

    #[test]
    fn connect_instantiate_and_revert() {
        let dir = temp_dir("prefab_rt");
        let doc = scene_doc();

        let connect =
            save_as_prefab_asset_and_connect(&doc, 0, "assets/prefabs/hero.prefab.json").unwrap();
        scene::write_doc(&dir.join(&connect.path), &connect.asset).unwrap();
        // 实例（Root + Child）就地替换掉原子树，「Keep」留在原位之后。
        assert_eq!(connect.scene["entities"].as_array().unwrap().len(), 3);
        assert_eq!(connect.scene["entities"][0]["name"], json!("Root"));
        assert_eq!(connect.scene["entities"][2]["name"], json!("Keep"));

        let link = get_corresponding_object_from_source(&connect.scene, 1).unwrap();
        assert_eq!(link.prefab, "assets/prefabs/hero.prefab.json");
        assert_eq!(link.index, 0);
        assert!(is_part_of_prefab_instance(&connect.scene, 1));
        assert!(!is_part_of_prefab_instance(&connect.scene, 2));
        assert_eq!(get_nearest_prefab_instance_root(&connect.scene, 1), Some(0));
        assert_eq!(
            get_prefab_instance_status(&connect.scene, 1),
            PrefabInstanceStatus::Connected
        );

        // 再实例化一份。
        let result = instantiate_prefab(&dir, &connect.scene, &connect.path, None).unwrap();
        assert_eq!(scene::prefab_instances(&result.doc).len(), 2);

        // 还原：内容回来、名字保留、下标不变（返回新的实例根下标）。
        let reverted = revert_prefab_instance(&dir, &result.doc, 0).unwrap();
        assert_eq!(reverted.index, 0);
        assert_eq!(
            reverted.doc["entities"][reverted.index]["name"],
            json!("Root")
        );

        // 解包：只解除该实例根的链接。
        let unpacked =
            unpack_prefab_instance(&result.doc, 0, PrefabUnpackMode::Completely).unwrap();
        assert_eq!(scene::prefab_instances(&unpacked).len(), 1);
        assert!(!is_part_of_prefab_instance(&unpacked, 0));

        assert_eq!(prefab_instance_infos(&result.doc).len(), 2);
    }

    #[test]
    fn apply_writes_back_to_asset() {
        let dir = temp_dir("prefab_apply");
        let doc = scene_doc();
        let connect =
            save_as_prefab_asset_and_connect(&doc, 0, "assets/prefabs/hero.prefab.json").unwrap();
        scene::write_doc(&dir.join(&connect.path), &connect.asset).unwrap();

        // 改个名字后 Apply，应该写回预制体（实例根就地替换在 0）。
        let mut edited = connect.scene.clone();
        edited["entities"][0]["name"] = json!("Renamed");
        let asset = apply_prefab_instance(&dir, &edited, 0).unwrap();
        assert_eq!(asset["entities"][0]["name"], json!("Renamed"));

        let on_disk = read_prefab(&dir, "assets/prefabs/hero.prefab.json").unwrap();
        assert_eq!(on_disk["entities"][0]["name"], json!("Renamed"));

        // 非实例根 Apply 应该报错（下标 2 是「Keep」）。
        assert!(apply_prefab_instance(&dir, &edited, 2).is_err());
    }
}
