//! 作者文档（`.scene.json` / `.prefab.json`）的定位、枚举、读写与校验。
//!
//! 场景与预制体**共用同一种文档形状**（[`prism_scene::SceneJson`]：`{version,
//! entities}`），因此这里用 [`DocKind`] 区分目录与后缀，其余逻辑（规范化读取、
//! 结构校验、另存/重命名/删除）完全共享。写回前用同一份 schema 校验，
//! 避免产生 cooker 无法消费的文档。

use std::path::{Path, PathBuf};

use prism_asset::GuidIndex;
use prism_scene::references::{sync_document, SyncMode};
use prism_scene::SceneJson;
use serde::Serialize;

/// 文档种类。作者 JSON 形状相同，只是所在目录与文件名后缀不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// 场景：`assets/scenes/*.scene.json`
    Scene,
    /// 预制体：`assets/prefabs/*.prefab.json`
    Prefab,
}

impl DocKind {
    /// 相对项目根的子目录。
    pub fn subdir(self) -> &'static str {
        match self {
            Self::Scene => "assets/scenes",
            Self::Prefab => "assets/prefabs",
        }
    }

    /// 文件名后缀（不含前导点）。
    pub fn ext(self) -> &'static str {
        match self {
            Self::Scene => "scene.json",
            Self::Prefab => "prefab.json",
        }
    }

    /// 中文标签（错误信息用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Scene => "场景",
            Self::Prefab => "预制体",
        }
    }

    /// 前端传字符串 → 种类。
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "scene" => Ok(Self::Scene),
            "prefab" => Ok(Self::Prefab),
            other => Err(format!("未知的文档类型 '{other}'")),
        }
    }

    /// 从路径推断种类（rename 只需要路径时用）。
    pub fn of_path(path: &Path) -> Self {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(&format!(".{}", Self::Prefab.ext())) {
            Self::Prefab
        } else {
            Self::Scene
        }
    }
}

/// `assets/` 下的场景子目录（相对项目根）。
pub const SCENES_SUBDIR: &str = "assets/scenes";

/// 场景文件扩展名。
pub const SCENE_EXT: &str = "scene.json";

/// 一个可在编辑器中打开的文档文件。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SceneEntry {
    /// 逻辑名（去掉后缀，如 `intro`）。
    pub name: String,
    /// 文件名（含后缀，如 `intro.scene.json`）。
    pub file_name: String,
    /// 绝对路径（前端原样回传给 read/write）。
    pub path: String,
}

/// 某类文档所在目录。
pub fn docs_dir(project_root: &Path, kind: DocKind) -> PathBuf {
    project_root.join(kind.subdir())
}

/// `assets/scenes` 目录。
pub fn scenes_dir(project_root: &Path) -> PathBuf {
    docs_dir(project_root, DocKind::Scene)
}

/// 从当前工作目录向上查找包含 `assets/scenes` 的项目根。
///
/// 编辑器 / MCP 的 CWD 可能不在仓库根，所以这里逐级上溯；
/// 找不到时返回错误，前端可提示用户手动指定。
pub fn find_project_root() -> Result<PathBuf, String> {
    let mut dir = std::env::current_dir().map_err(|e| format!("无法获取当前目录: {e}"))?;
    for _ in 0..12 {
        if scenes_dir(&dir).is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    Err("找不到包含 assets/scenes 的项目根目录".into())
}

/// 枚举某类文档，按逻辑名排序。目录不存在时视为空（预制体目录可以还没有）。
///
/// Prefabs may live anywhere under `assets/` (the hierarchy drag-and-drop
/// target is an arbitrary asset folder), so prefab discovery is recursive;
/// files under `assets/prefabs` keep their plain names, while files elsewhere
/// use their `assets/`-relative stem (e.g. `props/chair`).
pub fn list_docs(project_root: &Path, kind: DocKind) -> Result<Vec<SceneEntry>, String> {
    if kind == DocKind::Prefab {
        let assets = project_root.join("assets");
        if !assets.is_dir() {
            return Ok(Vec::new());
        }
        let mut docs = Vec::new();
        collect_docs(&assets, &assets, kind, &mut docs)?;
        docs.sort_by(|a, b| a.name.cmp(&b.name));
        return Ok(docs);
    }

    let dir = docs_dir(project_root, kind);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let read = std::fs::read_dir(&dir).map_err(|e| format!("无法读取 {}: {e}", dir.display()))?;

    let mut docs = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(name) = file_name.strip_suffix(&format!(".{}", kind.ext())) else {
            continue;
        };
        docs.push(SceneEntry {
            name: name.to_string(),
            file_name: file_name.to_string(),
            path: path.to_string_lossy().into_owned(),
        });
    }
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(docs)
}

fn collect_docs(
    assets_root: &Path,
    dir: &Path,
    kind: DocKind,
    docs: &mut Vec<SceneEntry>,
) -> Result<(), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("无法读取 {}: {e}", dir.display()))?;
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "cooked" | "heightmaps" | "target") || name.starts_with('.') {
                continue;
            }
            collect_docs(assets_root, &path, kind, docs)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(&format!(".{}", kind.ext())) else {
            continue;
        };
        let rel = path
            .strip_prefix(assets_root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| stem.to_string());
        let name = rel
            .trim_end_matches(&format!(".{}", kind.ext()))
            .trim_start_matches("prefabs/")
            .to_string();
        docs.push(SceneEntry {
            name,
            file_name: file_name.to_string(),
            path: path.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

/// 枚举所有 `.scene.json`。
pub fn list_scenes(project_root: &Path) -> Result<Vec<SceneEntry>, String> {
    list_docs(project_root, DocKind::Scene)
}

/// 读取作者文档并**规范化**为完整 JSON。
///
/// 经 [`prism_scene::SceneJson`] 往返一次，使缺失的可选字段（`transform` /
/// `components` 等）落到 serde 默认值，前端拿到的形状始终完整。
pub fn read_doc(path: &Path) -> Result<serde_json::Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("无法读取 {}: {e}", path.display()))?;
    // `SceneJson::parse` 会先把老文档迁移到当前 schema（例如 v1 的
    // `transform.translation` → `position`），再规范化往返。
    let scene =
        SceneJson::parse(&text).map_err(|e| format!("{} JSON 解析失败: {e}", path.display()))?;
    serde_json::to_value(&scene).map_err(|e| e.to_string())
}

/// [`read_doc`] 的场景别名。
pub fn read_scene_doc(path: &Path) -> Result<serde_json::Value, String> {
    read_doc(path)
}

/// Find the project root containing `path` by walking up to the directory
/// that has an `assets/` child.
pub fn project_root_for_doc(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_dir() { path } else { path.parent()? };
    loop {
        if dir.join("assets").is_dir() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

fn doc_relative_to_assets(assets_root: &Path, path: &Path) -> String {
    path.strip_prefix(assets_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        })
}

fn sync_doc_with_project(
    project_root: &Path,
    path: &Path,
    doc: &serde_json::Value,
    mode: SyncMode,
) -> serde_json::Value {
    let assets_root = project_root.join("assets");
    let index = GuidIndex::scan(&assets_root);
    let doc_rel = doc_relative_to_assets(&assets_root, path);
    let mut doc = doc.clone();
    sync_document(
        &mut doc,
        &doc_rel,
        mode,
        |guid| index.path_for_guid(guid).map(str::to_string),
        |asset| index.guid_for_path(asset).map(str::to_string),
    );
    doc
}

/// Read an authoring document and resolve its GUID references (GUID wins,
/// path fallback). Old documents without GUIDs are unchanged.
pub fn read_doc_in_project(project_root: &Path, path: &Path) -> Result<serde_json::Value, String> {
    let doc = read_doc(path)?;
    Ok(sync_doc_with_project(
        project_root,
        path,
        &doc,
        SyncMode::Load,
    ))
}

/// Resolve GUIDs and fill/refresh GUID fields before writing.
pub fn sync_doc_for_save(
    project_root: &Path,
    path: &Path,
    doc: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Run the same migration/normalisation as `write_doc` so GUID syncing sees
    // the final entity/component shape.
    let normalized = prism_scene::normalize_scene_doc(doc)?;
    Ok(sync_doc_with_project(
        project_root,
        path,
        &normalized,
        SyncMode::Save,
    ))
}

/// Write an authoring document with both paths and GUIDs.
pub fn write_doc_in_project(
    project_root: &Path,
    path: &Path,
    doc: &serde_json::Value,
) -> Result<(), String> {
    let synced = sync_doc_for_save(project_root, path, doc)?;
    write_doc(path, &synced)
}

/// 校验作者文档：形状 + 结构不变量（父索引越界 / 自父 / 空文档）。
pub fn validate_doc(doc: &serde_json::Value) -> Result<(), String> {
    // 先迁移再校验，使 v1 文档也能通过。
    let scene =
        SceneJson::from_value(doc.clone()).map_err(|e| format!("不是合法的作者文档结构: {e}"))?;
    scene.validate()
}

/// [`validate_doc`] 的场景别名。
pub fn validate_scene_doc(doc: &serde_json::Value) -> Result<(), String> {
    validate_doc(doc)
}

/// 校验并写回作者文档（2 空格缩进 + 结尾换行）。
///
/// 落盘的是**规范化后**的文档：补齐实体 `id`、升级到
/// [`prism_scene::SCENE_DOC_VERSION`]、剥掉派生组件，而不是原始输入。
pub fn write_doc(path: &Path, doc: &serde_json::Value) -> Result<(), String> {
    let doc = &prism_scene::normalize_scene_doc(doc)?;
    validate_doc(doc)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建 {}: {e}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// [`write_doc`] 的场景别名。
pub fn write_scene_doc(path: &Path, doc: &serde_json::Value) -> Result<(), String> {
    write_doc(path, doc)
}

// ---------------------------------------------------------------------------
// 文档名 / 路径
// ---------------------------------------------------------------------------

/// 校验逻辑名并拼出某类文档目录下的完整路径。
pub fn doc_path(root: &Path, kind: DocKind, name: &str) -> Result<PathBuf, String> {
    doc_file_in(&docs_dir(root, kind), kind, name)
}

fn doc_file_in(dir: &Path, kind: DocKind, name: &str) -> Result<PathBuf, String> {
    let trimmed = name.trim();
    let label = kind.label();
    if trimmed.is_empty() {
        return Err(format!("{label}名不能为空"));
    }
    const ILLEGAL: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if trimmed.contains("..") || trimmed.contains(ILLEGAL) {
        return Err(format!("非法的{label}名 '{trimmed}'"));
    }
    let ext = kind.ext();
    let file = if trimmed.ends_with(&format!(".{ext}")) {
        trimmed.to_string()
    } else if let Some(stem) = trimmed.strip_suffix(".json") {
        format!("{stem}.{ext}")
    } else {
        format!("{trimmed}.{ext}")
    };
    Ok(dir.join(file))
}

/// [`doc_path`] 的场景别名。
pub fn scene_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    doc_path(root, DocKind::Scene, name)
}

/// Write a document into an explicit directory (used by prefab drag-drop).
pub fn write_doc_in_dir(
    dir: &Path,
    kind: DocKind,
    name: &str,
    doc: &serde_json::Value,
) -> Result<PathBuf, String> {
    let path = doc_file_in(dir, kind, name)?;
    if path.exists() {
        return Err(format!("已存在同名{}：{}", kind.label(), path.display()));
    }
    write_doc(&path, doc)?;
    Ok(path)
}

/// 以新名字另存（不覆盖已存在的同名文件），返回新路径。
pub fn write_doc_as(
    root: &Path,
    kind: DocKind,
    name: &str,
    doc: &serde_json::Value,
) -> Result<PathBuf, String> {
    write_doc_in_dir(&docs_dir(root, kind), kind, name, doc)
}

/// [`write_doc_as`] 的场景别名。
pub fn write_scene_as(root: &Path, name: &str, doc: &serde_json::Value) -> Result<PathBuf, String> {
    write_doc_as(root, DocKind::Scene, name, doc)
}

/// 重命名文档（同目录内），返回新路径。
pub fn rename_doc(path: &Path, kind: DocKind, new_name: &str) -> Result<PathBuf, String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}路径没有父目录", kind.label()))?;
    let target = doc_file_in(dir, kind, new_name)?;
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        return Err(format!("已存在同名{}：{}", kind.label(), target.display()));
    }
    std::fs::rename(path, &target).map_err(|e| format!("重命名失败: {e}"))?;
    Ok(target)
}

/// [`rename_doc`] 的场景别名（种类按路径后缀推断）。
pub fn rename_scene(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    rename_doc(path, DocKind::of_path(path), new_name)
}

/// 删除文档文件。
pub fn delete_doc(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| format!("删除 {} 失败: {e}", path.display()))
}

/// [`delete_doc`] 的场景别名。
pub fn delete_scene(path: &Path) -> Result<(), String> {
    delete_doc(path)
}

// ---------------------------------------------------------------------------
// 场景模板
// ---------------------------------------------------------------------------

/// 一个内置场景模板。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SceneTemplate {
    pub id: String,
    pub label: String,
}

/// 所有内置模板（顺序即 UI 展示顺序）。
pub fn templates() -> Vec<SceneTemplate> {
    [
        ("camera", "相机"),
        ("lit", "相机 + 光照"),
        ("ui", "UI 叠层"),
        ("empty", "空实体"),
    ]
    .into_iter()
    .map(|(id, label)| SceneTemplate {
        id: id.to_string(),
        label: label.to_string(),
    })
    .collect()
}

/// 新建场景的初始文档（`camera` 模板：位于 `[0, 0, 10]` 的相机实体）。
pub fn empty_scene_doc() -> serde_json::Value {
    template_scene_doc("camera")
}

/// 按模板 id 生成场景文档；未知 id 回退到 `camera`。
///
/// 组件默认值来自 `prism-scene` 注册表（`default_json`），模板只覆盖差异字段，
/// 避免与组件定义脱节。
pub fn template_scene_doc(id: &str) -> serde_json::Value {
    use serde_json::json;

    let camera = || component_default("prism_engine::scene::Camera");

    let entities = match id {
        "empty" => vec![entity("Entity", None, [0.0, 0.0, 0.0], json!({}))],
        "lit" => vec![
            entity(
                "Camera",
                None,
                [0.0, 2.5, 18.0],
                json!({
                    "prism_engine::scene::Camera": camera(),
                }),
            ),
            entity(
                "Sun",
                None,
                [0.0, 6.0, 0.0],
                json!({
                    "prism_engine::scene::DirectionalLight":
                        component_default("prism_engine::scene::DirectionalLight"),
                }),
            ),
            entity(
                "PointLight",
                None,
                [3.0, 3.0, 3.0],
                json!({
                    "prism_engine::scene::PointLight":
                        component_default("prism_engine::scene::PointLight"),
                }),
            ),
        ],
        "ui" => vec![
            entity(
                "overlay",
                None,
                [0.0, 0.0, 0.0],
                json!({
                    "prism_engine::ui::Node": component_default("prism_engine::ui::Node"),
                    "prism_engine::ui::Style": merge(
                        component_default("prism_engine::ui::Style"),
                        json!({ "background": [0.0, 0.0, 0.0, 1.0] }),
                    ),
                }),
            ),
            entity(
                "title",
                Some(0),
                [0.0, 0.0, 0.0],
                json!({
                    "prism_engine::ui::Node": component_default("prism_engine::ui::Node"),
                    "prism_engine::ui::Style": merge(
                        component_default("prism_engine::ui::Style"),
                        json!({
                            "width": 900.0,
                            "height": 108.0,
                            "anchors": { "min_x": 0.5, "min_y": 0.5, "max_x": 0.5, "max_y": 0.5 },
                            "pivot": { "x": 0.5, "y": 0.5 },
                        }),
                    ),
                    "prism_engine::ui::Text": merge(
                        component_default("prism_engine::ui::Text"),
                        json!({
                            "content": "PrismaRev",
                            "font_size": 84.0,
                            "color": [0.91, 0.925, 0.957, 1.0],
                            "alignment": "Center",
                        }),
                    ),
                }),
            ),
        ],
        // "camera" 及未知 id
        _ => vec![entity(
            "Camera",
            None,
            [0.0, 0.0, 10.0],
            json!({
                "prism_engine::scene::Camera": camera(),
            }),
        )],
    };

    normalize(json!({ "version": prism_scene::SCENE_DOC_VERSION, "entities": entities }))
}

/// 给模板文档补上实体 `id`（v3 起作者文档必须带 id）。
fn normalize(doc: serde_json::Value) -> serde_json::Value {
    prism_scene::normalize_scene_doc(&doc).unwrap_or(doc)
}

fn component_default(name: &str) -> serde_json::Value {
    crate::build_schema()
        .type_by_name(name)
        .and_then(|ty| ty.default_json.clone())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn merge(base: serde_json::Value, patch: serde_json::Value) -> serde_json::Value {
    match (base, patch) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                base.insert(key, value);
            }
            serde_json::Value::Object(base)
        }
        (_, patch) => patch,
    }
}

fn transform(position: [f32; 3]) -> serde_json::Value {
    serde_json::json!({
        "position": position,
        "rotation": [0.0, 0.0, 0.0, 1.0],
        "scale": [1.0, 1.0, 1.0],
    })
}

fn entity(
    name: &str,
    parent: Option<u32>,
    position: [f32; 3],
    components: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "parent": parent,
        "transform": transform(position),
        "components": components,
    })
}

/// 某类文档的可用模板。
pub fn templates_for(kind: DocKind) -> Vec<SceneTemplate> {
    match kind {
        DocKind::Scene => templates(),
        // 预制体模板 = 单实体预设。
        DocKind::Prefab => crate::presets::entity_presets()
            .into_iter()
            .map(|preset| SceneTemplate {
                id: preset.id,
                label: preset.label,
            })
            .collect(),
    }
}

/// 按模板生成初始文档。
pub fn template_doc(kind: DocKind, template: &str) -> serde_json::Value {
    match kind {
        DocKind::Scene => template_scene_doc(template),
        DocKind::Prefab => prefab_template_doc(template),
    }
}

/// 单实体预制体文档（组件默认值来自注册表，与「快速添加实体」一致）。
pub fn prefab_template_doc(preset_id: &str) -> serde_json::Value {
    use serde_json::json;

    let presets = crate::presets::entity_presets();
    let preset = presets
        .iter()
        .find(|preset| preset.id == preset_id)
        .or_else(|| presets.iter().find(|preset| preset.id == "empty"));

    let Some(preset) = preset else {
        return json!({ "version": prism_scene::SCENE_DOC_VERSION, "entities": [] });
    };

    let mut components = serde_json::Map::new();
    for name in &preset.components {
        components.insert(name.clone(), component_default(name));
    }

    normalize(json!({
        "version": prism_scene::SCENE_DOC_VERSION,
        "entities": [
            entity(&preset.name, None, preset.position, serde_json::Value::Object(components)),
        ],
    }))
}

// ---------------------------------------------------------------------------
// 预制体：从子树提取 / 实例化
// ---------------------------------------------------------------------------

/// 预制体实例化的结果。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstantiateResult {
    /// 插入后的场景文档。
    pub doc: serde_json::Value,
    /// 新插入实体在场景里的下标（按预制体中的顺序）。
    pub inserted: Vec<usize>,
}

fn entities_of(doc: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    doc.get("entities")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "文档缺少 entities 数组".to_string())
}

fn id_of(entity: &serde_json::Value) -> u32 {
    entity
        .get("id")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as u32
}

/// `parent` 存的是父实体的 **id**（v3 起），不是数组下标。
fn parent_of(entity: &serde_json::Value) -> Option<u32> {
    entity
        .get("parent")
        .and_then(|value| value.as_u64())
        .map(|parent| parent as u32)
}

fn index_of_id(entities: &[serde_json::Value], id: u32) -> Option<usize> {
    entities.iter().position(|entity| id_of(entity) == id)
}

/// 编辑操作统一入口：先迁移到当前 schema（补 id / 升级版本 / 剥派生组件）。
fn normalized(doc: &serde_json::Value) -> Result<serde_json::Value, String> {
    prism_scene::normalize_scene_doc(doc)
}

fn next_entity_id(entities: &[serde_json::Value]) -> u32 {
    entities
        .iter()
        .map(id_of)
        .max()
        .map_or(0, |max| max.saturating_add(1))
}

/// 用新的实体数组重建文档（保留 `version`）。
fn doc_with_entities(
    doc: &serde_json::Value,
    entities: Vec<serde_json::Value>,
) -> serde_json::Value {
    let version = doc
        .get("version")
        .cloned()
        .unwrap_or(serde_json::json!(prism_scene::SCENE_DOC_VERSION));
    serde_json::json!({ "version": version, "entities": entities })
}

/// `node` 是否位于 `ancestor` 的子树中（沿 parent id 上溯）。
fn is_descendant(entities: &[serde_json::Value], node: usize, ancestor: usize) -> bool {
    let ancestor_id = id_of(&entities[ancestor]);
    let mut current = parent_of(&entities[node]);
    let mut guard = 0;
    while let Some(parent_id) = current {
        if parent_id == ancestor_id {
            return true;
        }
        guard += 1;
        if guard > entities.len() {
            return false; // 数据里存在环，保守放弃
        }
        current = index_of_id(entities, parent_id).and_then(|index| parent_of(&entities[index]));
    }
    false
}

/// 层级操作结果：新文档 + 实体在数组中的新下标。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MoveResult {
    pub doc: serde_json::Value,
    pub index: usize,
}

/// 改父级（不移动数组位置）。`parent = None` 表示变成根实体。
///
/// 拒绝自父与「挂到自己的后代下」（会成环）。
pub fn reparent_doc(
    doc: &serde_json::Value,
    child: usize,
    parent: Option<usize>,
) -> Result<serde_json::Value, String> {
    let result = move_entity(doc, child, child, parent)?;
    Ok(result.doc)
}

/// 把 `child` 移到数组的 `insert_at` 位置（旧下标空间，`len` = 末尾），
/// 同时把它的父级设为 `parent`（旧下标空间）。
///
/// 其余实体的父索引会随数组重排一起重映射，因此返回值是整份新文档。
pub fn move_entity(
    doc: &serde_json::Value,
    child: usize,
    insert_at: usize,
    parent: Option<usize>,
) -> Result<MoveResult, String> {
    let doc = &normalized(doc)?;
    validate_doc(doc)?;
    let entities = entities_of(doc)?;
    let count = entities.len();

    if child >= count {
        return Err(format!("实体下标 {child} 越界"));
    }
    if insert_at > count {
        return Err(format!("插入位置 {insert_at} 越界"));
    }
    if let Some(parent) = parent {
        if parent >= count {
            return Err(format!("父实体下标 {parent} 越界"));
        }
        if parent == child {
            return Err("不能以自己为父".into());
        }
        if is_descendant(entities, parent, child) {
            return Err("不能挂到自己的后代下（会成环）".into());
        }
    }

    // 先摘出 child，再插到目标位置。
    let mut order: Vec<usize> = (0..count).collect();
    order.remove(child);
    let target = if insert_at > child {
        insert_at - 1
    } else {
        insert_at
    };
    order.insert(target.min(order.len()), child);

    // 只改被移动实体的 `parent`：id 与数组位置解耦，其余实体完全不动。
    let new_parent_id = parent.map(|index| id_of(&entities[index]));
    let new_index = target.min(order.len() - 1);

    let mut out = Vec::with_capacity(count);
    for old_index in &order {
        let mut entity = entities[*old_index].clone();
        if *old_index == child {
            entity["parent"] = match new_parent_id {
                Some(parent) => serde_json::json!(parent),
                None => serde_json::json!(null),
            };
        }
        out.push(entity);
    }

    Ok(MoveResult {
        doc: doc_with_entities(doc, out),
        index: new_index,
    })
}

/// 批量删除实体，并把被删掉的父级改为根、其余父索引重映射。
///
/// 至少要留下一个实体（与校验一致）。
pub fn delete_entities(
    doc: &serde_json::Value,
    indices: &[usize],
) -> Result<serde_json::Value, String> {
    let doc = &normalized(doc)?;
    validate_doc(doc)?;
    let entities = entities_of(doc)?;
    let count = entities.len();

    let mut removed = vec![false; count];
    for index in indices {
        if *index >= count {
            return Err(format!("实体下标 {index} 越界"));
        }
        removed[*index] = true;
    }
    let remaining = removed.iter().filter(|flag| !**flag).count();
    if remaining == 0 {
        return Err("至少要保留一个实体".into());
    }

    let removed_ids: std::collections::HashSet<u32> = indices
        .iter()
        .map(|index| id_of(&entities[*index]))
        .collect();

    let mut out = Vec::with_capacity(remaining);
    for (old_index, entity) in entities.iter().enumerate() {
        if removed[old_index] {
            continue;
        }
        let mut entity = entity.clone();
        // 被删掉的父 → 变根；其余 parent 是 id，不受数组收缩影响。
        let new_parent = parent_of(&entity).filter(|parent| !removed_ids.contains(parent));
        entity["parent"] = match new_parent {
            Some(parent) => serde_json::json!(parent),
            None => serde_json::json!(null),
        };
        out.push(entity);
    }

    Ok(doc_with_entities(doc, out))
}

/// 预制体实例链接所处的组件名。
pub const PREFAB_LINK_COMPONENT: &str = "prism_engine::scene::PrefabInstance";

/// 子树下标（DFS，父先于子，`root` 在首位）。
/// 原数组下标在删除 `removed` 里的实体之后的新位置。
///
/// 只数排在它前面的被删下标；调用方必须保证 `index` 自己不在 `removed` 里。
fn index_after_removal(index: usize, removed: &[usize]) -> usize {
    index - removed.iter().filter(|removed| **removed < index).count()
}

fn subtree_indices(entities: &[serde_json::Value], root: usize) -> Vec<usize> {
    let mut order: Vec<usize> = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        if order.contains(&index) {
            continue;
        }
        order.push(index);
        let node_id = id_of(&entities[index]);
        for child in (0..entities.len()).rev() {
            if parent_of(&entities[child]) == Some(node_id) && !order.contains(&child) {
                stack.push(child);
            }
        }
    }
    order
}

/// 从场景中提取「根实体 + 全部后代」为一个预制体文档。
///
/// 提取出的根实体父级置为 `null`（实例化时再决定挂到哪里），内部父子索引重排到
/// 预制体自己的下标空间；根上的 [`PREFAB_LINK_COMPONENT`] 会被剥掉
/// （预制体不该记录「自己是哪个预制体的实例」）。
pub fn extract_subtree(
    scene: &serde_json::Value,
    root: usize,
) -> Result<serde_json::Value, String> {
    let scene = &normalized(scene)?;
    validate_doc(scene)?;
    let entities = entities_of(scene)?;
    if root >= entities.len() {
        return Err(format!("实体下标 {root} 越界"));
    }

    let order = subtree_indices(entities, root);
    // 预制体文件自成 id 空间：子树 id 重编号为 0..k，内部父子引用跟着改。
    let mapping: std::collections::HashMap<u32, u32> = order
        .iter()
        .enumerate()
        .map(|(new_id, old_index)| (id_of(&entities[*old_index]), new_id as u32))
        .collect();

    let mut extracted = Vec::with_capacity(order.len());
    for old_index in &order {
        let mut entity = entities[*old_index].clone();
        let parent = parent_of(&entity);
        // 不在子树内的父（含根实体原本的父）→ null。
        let new_parent = parent.and_then(|parent| mapping.get(&parent).copied());
        entity["id"] = serde_json::json!(mapping[&id_of(&entity)]);
        entity["parent"] = match new_parent {
            Some(parent) => serde_json::json!(parent),
            None => serde_json::json!(null),
        };
        extracted.push(entity);
    }

    // 剥掉根上的实例链接。
    if let Some(first) = extracted.first_mut() {
        if let Some(components) = first
            .get_mut("components")
            .and_then(|value| value.as_object_mut())
        {
            components.remove(PREFAB_LINK_COMPONENT);
        }
    }

    Ok(serde_json::json!({ "version": prism_scene::SCENE_DOC_VERSION, "entities": extracted }))
}

/// 预制体实例链接（编辑器「反向链接」用）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrefabLink {
    /// 场景中实例根实体的下标。
    pub index: usize,
    /// 预制体路径（相对项目根）。
    pub prefab: String,
}

/// 把预制体实例化进场景（追加到末尾），返回新文档与插入下标。
///
/// 预制体内部的父索引整体平移到新位置；预制体的根实体挂到 `parent`
/// （`None` = 作为场景根）。名字与场景已有实体去重（追加 `_2`、`_3`…）。
/// `link` 非空时给每个预制体根打上 [`PrefabLink`] 记录。
pub fn instantiate_prefab(
    scene: &serde_json::Value,
    prefab: &serde_json::Value,
    parent: Option<u32>,
    link: Option<&str>,
) -> Result<InstantiateResult, String> {
    let scene = &normalized(scene)?;
    let prefab = &normalized(prefab)?;
    validate_doc(scene)?;
    validate_doc(prefab)?;

    let scene_entities = entities_of(scene)?.clone();
    let prefab_entities = entities_of(prefab)?.clone();
    if let Some(parent) = parent {
        if parent as usize >= scene_entities.len() {
            return Err(format!("父实体下标 {parent} 越界"));
        }
    }

    let version = scene
        .get("version")
        .cloned()
        .unwrap_or(serde_json::json!(prism_scene::SCENE_DOC_VERSION));
    // 预制体自成 id 空间：整体分配一段新 id，并记住 旧 id → 新 id。
    let id_base = next_entity_id(&scene_entities);
    let id_map: std::collections::HashMap<u32, u32> = prefab_entities
        .iter()
        .enumerate()
        .map(|(offset, entity)| (id_of(entity), id_base + offset as u32))
        .collect();
    let root_parent_id = parent.map(|index| id_of(&scene_entities[index as usize]));

    let mut names: Vec<String> = scene_entities
        .iter()
        .filter_map(|entity| entity.get("name").and_then(|value| value.as_str()))
        .map(|name| name.to_string())
        .collect();

    let mut out = scene_entities;
    let mut inserted = Vec::with_capacity(prefab_entities.len());

    for entity in &prefab_entities {
        let mut entity = entity.clone();

        let base_name = entity
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("entity")
            .to_string();
        let mut name = base_name.clone();
        let mut suffix = 2;
        while names.contains(&name) {
            name = format!("{base_name}_{suffix}");
            suffix += 1;
        }
        names.push(name.clone());
        entity["name"] = serde_json::json!(name);

        let prefab_parent = parent_of(&entity);
        let new_parent = match prefab_parent {
            Some(prefab_parent) => id_map.get(&prefab_parent).copied(),
            None => root_parent_id,
        };
        entity["id"] = serde_json::json!(id_map[&id_of(&entity)]);
        entity["parent"] = match new_parent {
            Some(parent) => serde_json::json!(parent),
            None => serde_json::json!(null),
        };

        // 预制体的根实体记录来源（编辑器反向链接用）。
        if prefab_parent.is_none() {
            if let Some(path) = link {
                if let Some(components) = entity
                    .get_mut("components")
                    .and_then(|value| value.as_object_mut())
                {
                    components.insert(
                        PREFAB_LINK_COMPONENT.to_string(),
                        serde_json::json!({ "prefab": path }),
                    );
                }
            }
        }

        inserted.push(out.len());
        out.push(entity);
    }

    Ok(InstantiateResult {
        doc: serde_json::json!({ "version": version, "entities": out }),
        inserted,
    })
}

/// 列出文档里所有预制体实例根（反向链接）。
pub fn prefab_instances(doc: &serde_json::Value) -> Vec<PrefabLink> {
    let doc = &normalized(doc).unwrap_or_else(|_| doc.clone());
    let Ok(entities) = entities_of(doc) else {
        return Vec::new();
    };
    entities
        .iter()
        .enumerate()
        .filter_map(|(index, entity)| {
            let prefab = entity
                .get("components")?
                .get(PREFAB_LINK_COMPONENT)?
                .get("prefab")?
                .as_str()?;
            Some(PrefabLink {
                index,
                prefab: prefab.to_string(),
            })
        })
        .collect()
}

/// 把某个实例根**还原**成预制体的当前内容。
///
/// 删掉该实体的整棵子树，再用 `prefab` 重新实例化到原父级下，并保留原来的名字。
/// 重建后的子树**插回原来的数组位置**：数组顺序就是层级顺序，追加到末尾会让实例
/// 在层级树里跳到最后一个兄弟，并让它的下标（以及依赖下标的撤销/选中）失效。
pub fn revert_prefab_instance(
    scene: &serde_json::Value,
    root: usize,
    prefab: &serde_json::Value,
    link: Option<&str>,
) -> Result<MoveResult, String> {
    let scene = &normalized(scene)?;
    validate_doc(scene)?;
    let entities = entities_of(scene)?;
    if root >= entities.len() {
        return Err(format!("实体下标 {root} 越界"));
    }

    let keep_name = entities[root]
        .get("name")
        .and_then(|value| value.as_str())
        .map(|name| name.to_string());
    let subtree = subtree_indices(entities, root);
    // `parent` 是 id，删掉子树不会让它失效（这正是 v3 想要的效果）；
    // `instantiate_prefab` 的入参是**下标**，而且必须是**裁剪后**文档里的下标：
    // 数组顺序与层级无关（见 `SceneJson::topological_order`），被删的实体完全可能
    // 排在父实体前面，删除会让父实体整体前移。直接用原数组下标会挂错父级（或在
    // 越界检查里报错）。
    let parent = parent_of(&entities[root])
        .map(|id| {
            let index =
                index_of_id(entities, id).ok_or_else(|| format!("父实体 id {id} 不存在"))?;
            if subtree.contains(&index) {
                // 只有层级成环时才会出现（`validate` 只拒绝自父）。
                return Err(format!("父实体 id {id} 在待删除的子树内（层级成环）"));
            }
            Ok(index_after_removal(index, &subtree) as u32)
        })
        .transpose()?;
    // 根在裁剪后数组里的位置：它前面的实体减去同在本子树里的那些。
    let insert_at = index_after_removal(root, &subtree);

    let trimmed = delete_entities(scene, &subtree)?;
    let result = instantiate_prefab(&trimmed, prefab, parent, link)?;
    let first_new = *result
        .inserted
        .first()
        .ok_or_else(|| "预制体没有实体，无法还原".to_string())?;

    let mut out = entities_of(&result.doc)?.clone();
    let mut rebuilt = out.split_off(first_new);
    if let Some(name) = keep_name {
        if let Some(entity) = rebuilt.first_mut() {
            entity["name"] = serde_json::json!(name);
        }
    }
    out.splice(insert_at..insert_at, rebuilt);

    Ok(MoveResult {
        doc: doc_with_entities(&result.doc, out),
        index: insert_at,
    })
}

/// 解除实例链接（只去掉记录，不动实体内容）。
pub fn unlink_prefab_instance(
    scene: &serde_json::Value,
    root: usize,
) -> Result<serde_json::Value, String> {
    let scene = &normalized(scene)?;
    validate_doc(scene)?;
    let mut entities = entities_of(scene)?.clone();
    if root >= entities.len() {
        return Err(format!("实体下标 {root} 越界"));
    }
    if let Some(components) = entities[root]
        .get_mut("components")
        .and_then(|value| value.as_object_mut())
    {
        components.remove(PREFAB_LINK_COMPONENT);
    }
    Ok(doc_with_entities(scene, entities))
}

/// 把文档里所有引用 `prefab_path` 的实例都还原成 `prefab` 的内容。
///
/// 从大到小处理实例根：删除只会影响更靠后的下标，因此前面的根仍然有效。
pub fn sync_prefab_instances(
    scene: &serde_json::Value,
    prefab_path: &str,
    prefab: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut roots: Vec<usize> = prefab_instances(scene)
        .into_iter()
        .filter(|link| link.prefab == prefab_path)
        .map(|link| link.index)
        .collect();
    roots.sort_unstable_by(|a, b| b.cmp(a));

    let mut doc = scene.clone();
    for root in roots {
        doc = revert_prefab_instance(&doc, root, prefab, Some(prefab_path))?.doc;
    }
    Ok(doc)
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
        std::fs::create_dir_all(scenes_dir(&dir)).unwrap();
        dir
    }

    #[test]
    fn empty_scene_validates() {
        let doc = empty_scene_doc();
        validate_scene_doc(&doc).unwrap();
        assert_eq!(doc["entities"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn validate_rejects_empty_and_out_of_range_parents() {
        assert!(validate_scene_doc(
            &json!({ "version": prism_scene::SCENE_DOC_VERSION, "entities": [] })
        )
        .is_err());

        let bad = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "a", "parent": 5, "transform": {}, "components": {} }
            ]
        });
        assert!(validate_scene_doc(&bad).is_err());

        let self_parent = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "a", "parent": 0, "transform": {}, "components": {} }
            ]
        });
        assert!(validate_scene_doc(&self_parent).is_err());
    }

    #[test]
    fn list_read_write_roundtrip() {
        let root = temp_dir("roundtrip");
        let path = scenes_dir(&root).join("demo.scene.json");

        write_scene_doc(&path, &empty_scene_doc()).unwrap();

        let scenes = list_scenes(&root).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "demo");
        assert_eq!(scenes[0].file_name, "demo.scene.json");
        assert_eq!(Path::new(&scenes[0].path), path.as_path());

        let doc = read_scene_doc(&path).unwrap();
        validate_scene_doc(&doc).unwrap();

        // 写回非法文档必须被拒绝，且不落盘。
        let bad = json!({ "version": prism_scene::SCENE_DOC_VERSION, "entities": [] });
        assert!(write_scene_doc(&path, &bad).is_err());
        assert_eq!(read_scene_doc(&path).unwrap(), doc);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn all_templates_validate_and_round_trip() {
        let ids: Vec<String> = templates().into_iter().map(|t| t.id).collect();
        assert!(ids.contains(&"camera".to_string()));
        assert!(ids.contains(&"lit".to_string()));
        assert!(ids.contains(&"ui".to_string()));

        for id in ids {
            let doc = template_scene_doc(&id);
            validate_scene_doc(&doc).unwrap_or_else(|e| panic!("template '{id}' invalid: {e}"));

            // 每个实体都必须有完整的 transform / components 形状。
            for entity in doc["entities"].as_array().unwrap() {
                assert!(entity["transform"].is_object());
                assert!(entity["components"].is_object());
            }

            // 经 SceneJson 规范化后应保持不变（模板已是规范形状）。
            let normalized: serde_json::Value =
                serde_json::to_value(serde_json::from_value::<SceneJson>(doc.clone()).unwrap())
                    .unwrap();
            assert_eq!(normalized, doc, "template '{id}' is not normalized");
        }

        // 未知 id 回退到 camera。
        assert_eq!(template_scene_doc("nope"), template_scene_doc("camera"));
    }

    #[test]
    fn scene_names_are_validated() {
        let root = temp_dir("names");
        assert!(scene_path(&root, "My Scene").is_ok());
        assert!(scene_path(&root, "level1")
            .unwrap()
            .ends_with("level1.scene.json"));
        assert!(scene_path(&root, "level2.scene.json")
            .unwrap()
            .ends_with("level2.scene.json"));
        assert!(scene_path(&root, "").is_err());
        assert!(scene_path(&root, "../escape").is_err());
        assert!(scene_path(&root, "a/b").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn prefab_templates_validate() {
        for template in templates_for(DocKind::Prefab) {
            let doc = template_doc(DocKind::Prefab, &template.id);
            validate_doc(&doc).unwrap_or_else(|e| panic!("prefab template '{}': {e}", template.id));
            assert_eq!(doc["entities"].as_array().unwrap().len(), 1);
        }
        // 未知 id 回退到空实体。
        assert!(validate_doc(&template_doc(DocKind::Prefab, "nope")).is_ok());
        // 预制体有自己的目录。
        assert!(docs_dir(Path::new("/x"), DocKind::Prefab).ends_with("assets/prefabs"));
    }

    #[test]
    fn extract_subtree_keeps_children_and_rebases_parents() {
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "a", "parent": null, "transform": {}, "components": {} },
                { "name": "b", "parent": 0, "transform": {}, "components": {} },
                { "name": "root", "parent": 0, "transform": {}, "components": {} },
                { "name": "child", "parent": 2, "transform": {}, "components": {} },
                { "name": "grandchild", "parent": 3, "transform": {}, "components": {} },
                { "name": "other", "parent": null, "transform": {}, "components": {} }
            ]
        });

        let prefab = extract_subtree(&scene, 2).unwrap();
        validate_doc(&prefab).unwrap();
        let entities = prefab["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 3);
        assert_eq!(entities[0]["name"], "root");
        assert_eq!(entities[0]["parent"], serde_json::Value::Null); // 提取出的根断开父级
        assert_eq!(entities[1]["name"], "child");
        assert_eq!(entities[1]["parent"], 0); // 父是 root（新下标 0）
        assert_eq!(entities[2]["name"], "grandchild");
        assert_eq!(entities[2]["parent"], 1); // 父是 child（新下标 1）

        assert!(extract_subtree(&scene, 99).is_err());
    }

    #[test]
    fn instantiate_prefab_rebases_and_dedupes_names() {
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "host", "parent": null, "transform": {}, "components": {} },
                { "name": "Camera", "parent": null, "transform": {}, "components": {} }
            ]
        });
        let prefab = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "Camera", "parent": null, "transform": {}, "components": {} },
                { "name": "Lens", "parent": 0, "transform": {}, "components": {} }
            ]
        });

        let result = instantiate_prefab(&scene, &prefab, Some(0), None).unwrap();
        validate_doc(&result.doc).unwrap();
        assert_eq!(result.inserted, vec![2, 3]);

        let entities = result.doc["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 4);
        // 名字与已有实体去重。
        assert_eq!(entities[2]["name"], "Camera_2");
        // 预制体根挂到指定的父实体。
        assert_eq!(entities[2]["parent"], 0);
        // 内部父索引平移。
        assert_eq!(entities[3]["parent"], 2);

        // 越界父实体被拒绝。
        assert!(instantiate_prefab(&scene, &prefab, Some(9), None).is_err());
    }

    fn demo_scene_with_host() -> serde_json::Value {
        serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "host", "parent": null, "transform": {}, "components": {} }
            ]
        })
    }

    fn demo_prefab() -> serde_json::Value {
        serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "Door", "parent": null, "transform": {}, "components": {
                    "prism_engine::scene::Active": true } },
                { "name": "Handle", "parent": 0, "transform": {}, "components": {} }
            ]
        })
    }

    #[test]
    fn instantiate_records_and_extract_strips_the_link() {
        let scene = demo_scene_with_host();
        let prefab = demo_prefab();
        let path = "assets/prefabs/door.prefab.json";

        let result = instantiate_prefab(&scene, &prefab, Some(0), Some(path)).unwrap();
        let links = prefab_instances(&result.doc);
        assert_eq!(
            links,
            vec![PrefabLink {
                index: 1,
                prefab: path.to_string()
            }]
        );

        // 提取子树时剥掉链接（预制体不该记录自己是哪来的）。
        let extracted = extract_subtree(&result.doc, 1).unwrap();
        assert!(extracted["entities"][0]["components"]
            .get(PREFAB_LINK_COMPONENT)
            .is_none());

        // 解除链接。
        let unlinked = unlink_prefab_instance(&result.doc, 1).unwrap();
        assert!(prefab_instances(&unlinked).is_empty());
    }

    #[test]
    fn revert_restores_prefab_content_and_keeps_name_and_parent() {
        let scene = demo_scene_with_host();
        let prefab = demo_prefab();
        let path = "assets/prefabs/door.prefab.json";

        let instantiated = instantiate_prefab(&scene, &prefab, Some(0), Some(path)).unwrap();
        // 改坏实例内容（改名字 + 去掉组件）。
        let mut broken = instantiated.doc.clone();
        broken["entities"][1]["name"] = serde_json::json!("Broken");
        broken["entities"][1]["components"] = serde_json::json!({});

        let reverted = revert_prefab_instance(&broken, 1, &prefab, Some(path)).unwrap();
        validate_doc(&reverted.doc).unwrap();

        let entities = reverted.doc["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 3);
        // 名字与父级保留，内容来自预制体。
        assert_eq!(entities[reverted.index]["name"], "Broken");
        assert_eq!(entities[reverted.index]["parent"], 0);
        assert_eq!(
            entities[reverted.index]["components"]["prism_engine::scene::Active"],
            serde_json::json!(true)
        );
        // 链接仍在，且只有这一个实例。
        assert_eq!(prefab_instances(&reverted.doc).len(), 1);
    }

    #[test]
    fn revert_keeps_the_instance_at_its_original_position() {
        let path = "assets/prefabs/door.prefab.json";
        let mut door = serde_json::json!({
            "name": "Door",
            "parent": 0,
            "transform": {},
            "components": {}
        });
        door["components"][PREFAB_LINK_COMPONENT] = serde_json::json!({ "prefab": path });
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "host", "parent": null, "transform": {}, "components": {} },
                { "name": "filler", "parent": null, "transform": {}, "components": {} },
                door,
                { "name": "Handle", "parent": 2, "transform": {}, "components": {} },
                { "name": "tail", "parent": null, "transform": {}, "components": {} }
            ]
        });

        let reverted = revert_prefab_instance(&scene, 2, &demo_prefab(), Some(path)).unwrap();
        validate_doc(&reverted.doc).unwrap();

        // 重建的子树回到原位（不是追加到末尾），名字沿用实例原来的名字。
        assert_eq!(reverted.index, 2);
        let names: Vec<&str> = reverted.doc["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entity| entity["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["host", "filler", "Door", "Handle", "tail"]);
        assert_eq!(reverted.doc["entities"][2]["parent"], 0);
        assert_eq!(
            prefab_instances(&reverted.doc),
            vec![PrefabLink {
                index: 2,
                prefab: path.to_string()
            }]
        );
    }

    #[test]
    fn revert_maps_the_parent_id_to_its_current_index() {
        let path = "assets/prefabs/door.prefab.json";
        let mut door = serde_json::json!({
            "id": 30,
            "name": "Door",
            "parent": 10,
            "transform": {},
            "components": {}
        });
        door["components"][PREFAB_LINK_COMPONENT] = serde_json::json!({ "prefab": path });
        // v3 文档里 `parent` 是 id；这里 id 与下标刻意不同（10/30/40）。
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "id": 10, "name": "host", "parent": null, "transform": {}, "components": {} },
                door,
                { "id": 40, "name": "Handle", "parent": 30, "transform": {}, "components": {} }
            ]
        });

        let reverted = revert_prefab_instance(&scene, 1, &demo_prefab(), Some(path)).unwrap();
        validate_doc(&reverted.doc).unwrap();

        let entities = reverted.doc["entities"].as_array().unwrap();
        assert_eq!(reverted.index, 1);
        assert_eq!(entities.len(), 3);
        // 父级仍是 id（10），不是同值下标。
        assert_eq!(entities[1]["parent"], 10);
        let ids: Vec<u64> = entities
            .iter()
            .map(|entity| entity["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![10, 11, 12]);
    }

    #[test]
    fn revert_resolves_the_parent_index_after_deleting_the_subtree() {
        let path = "assets/prefabs/door.prefab.json";
        let mut door = serde_json::json!({
            "id": 1,
            "name": "Door",
            "parent": 0,
            "transform": {},
            "components": {}
        });
        door["components"][PREFAB_LINK_COMPONENT] = serde_json::json!({ "prefab": path });
        // 数组顺序刻意与层级顺序相反：
        //   idx0 `grandchild`（id 9）排在它的父 `Door` 前面，自己也在 Door 的子树里；
        //   idx1 `host`（id 0）是 Door 的父，却排在子树之后。
        // 删掉子树后 host 从下标 1 前移到 0：若沿用原数组下标（1），实例会被挂到
        // `tail` 上（静默挂错父级）。
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "id": 9, "name": "grandchild", "parent": 1, "transform": {}, "components": {} },
                { "id": 0, "name": "host", "parent": null, "transform": {}, "components": {} },
                door,
                { "id": 7, "name": "tail", "parent": null, "transform": {}, "components": {} }
            ]
        });

        let reverted = revert_prefab_instance(&scene, 2, &demo_prefab(), Some(path)).unwrap();
        validate_doc(&reverted.doc).unwrap();

        let entities = reverted.doc["entities"].as_array().unwrap();
        let names: Vec<&str> = entities
            .iter()
            .map(|entity| entity["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["host", "Door", "Handle", "tail"]);
        // 实例根挂回 host（id 0 / 裁剪后的下标 0），不是裁剪前的下标 1 所指的 tail。
        assert_eq!(entities[reverted.index]["parent"], 0);
        assert_eq!(reverted.index, 1);
        assert_eq!(prefab_instances(&reverted.doc).len(), 1);
    }

    #[test]
    fn revert_rejects_a_parent_that_is_inside_the_subtree() {
        let path = "assets/prefabs/door.prefab.json";
        // 层级成环（`validate` 只拒绝自父）：Door(id 1) 的父是它的后代 grandchild(id 9)。
        let mut door = serde_json::json!({
            "id": 1,
            "name": "Door",
            "parent": 9,
            "transform": {},
            "components": {}
        });
        door["components"][PREFAB_LINK_COMPONENT] = serde_json::json!({ "prefab": path });
        let scene = serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "id": 9, "name": "grandchild", "parent": 1, "transform": {}, "components": {} },
                { "id": 0, "name": "host", "parent": null, "transform": {}, "components": {} },
                door
            ]
        });

        let error = revert_prefab_instance(&scene, 2, &demo_prefab(), Some(path)).unwrap_err();
        assert!(
            error.contains("在待删除的子树内"),
            "成环时不能把父 id 换成凭空的裁剪后下标：{error}"
        );
    }

    #[test]
    fn sync_keeps_every_instance_in_its_own_slot() {
        let path = "assets/prefabs/door.prefab.json";
        let prefab = demo_prefab();

        let once = instantiate_prefab(&demo_scene_with_host(), &prefab, None, Some(path)).unwrap();
        let twice = instantiate_prefab(&once.doc, &prefab, None, Some(path)).unwrap();

        // 在两个实例之间插一个普通实体，检查同步后它和两个实例都留在原位。
        let mut broken = twice.doc.clone();
        broken["entities"].as_array_mut().unwrap().insert(
            3,
            serde_json::json!({
                "id": 99, "name": "mid", "parent": null, "transform": {}, "components": {}
            }),
        );
        for entity in broken["entities"].as_array_mut().unwrap().iter_mut() {
            if let Some(components) = entity["components"].as_object_mut() {
                components.remove("prism_engine::scene::Active");
            }
        }

        let synced = sync_prefab_instances(&broken, path, &prefab).unwrap();
        validate_doc(&synced).unwrap();

        let names: Vec<&str> = synced["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entity| entity["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["host", "Door", "Handle", "mid", "Door_2", "Handle_2"]
        );
        assert_eq!(
            prefab_instances(&synced)
                .into_iter()
                .map(|link| link.index)
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
    }

    #[test]
    fn sync_updates_every_instance_of_the_same_prefab() {
        let scene = demo_scene_with_host();
        let prefab = demo_prefab();
        let path = "assets/prefabs/door.prefab.json";

        let once = instantiate_prefab(&scene, &prefab, None, Some(path)).unwrap();
        let twice = instantiate_prefab(&once.doc, &prefab, None, Some(path)).unwrap();
        assert_eq!(prefab_instances(&twice.doc).len(), 2);

        // 把两个实例的内容改坏（但保留链接组件）。
        let mut broken = twice.doc.clone();
        for entity in broken["entities"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .skip(1)
        {
            if let Some(components) = entity["components"].as_object_mut() {
                components.remove("prism_engine::scene::Active");
            }
        }
        assert_eq!(prefab_instances(&broken).len(), 2);

        let synced = sync_prefab_instances(&broken, path, &prefab).unwrap();
        validate_doc(&synced).unwrap();
        let entities = synced["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 5); // host + 2×(Door + Handle)
        for entity in entities.iter().skip(1) {
            if entity["name"].as_str().unwrap_or("").starts_with("Door") {
                assert!(
                    entity["components"]
                        .get("prism_engine::scene::Active")
                        .is_some(),
                    "instance content was not restored: {entity}"
                );
            }
        }
        assert_eq!(prefab_instances(&synced).len(), 2);
    }

    /// a(0) → b(1) → c(2)，另有根 d(3)。
    fn hierarchy_doc() -> serde_json::Value {
        serde_json::json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [
                { "name": "a", "parent": null, "transform": {}, "components": {} },
                { "name": "b", "parent": 0, "transform": {}, "components": {} },
                { "name": "c", "parent": 1, "transform": {}, "components": {} },
                { "name": "d", "parent": null, "transform": {}, "components": {} }
            ]
        })
    }

    #[test]
    fn reparent_moves_a_subtree_and_rejects_cycles() {
        let doc = hierarchy_doc();

        // 把 c 挂到 d 下（并保持 c 的子树）。
        let out = reparent_doc(&doc, 2, Some(3)).unwrap();
        let entities = out["entities"].as_array().unwrap();
        assert_eq!(entities[2]["name"], "c");
        assert_eq!(entities[2]["parent"], 3);

        // 变成根。
        let out = reparent_doc(&doc, 1, None).unwrap();
        assert_eq!(out["entities"][1]["parent"], serde_json::Value::Null);
        // b 的子树保持不变（c 仍指向 b）。
        assert_eq!(out["entities"][2]["parent"], 1);

        // 拒绝：自父 / 挂到后代 / 越界。
        assert!(reparent_doc(&doc, 1, Some(1)).is_err());
        assert!(reparent_doc(&doc, 0, Some(2)).is_err()); // c 是 a 的后代
        assert!(reparent_doc(&doc, 9, Some(0)).is_err());
    }

    #[test]
    fn move_entity_reorders_without_rewriting_parents() {
        let doc = hierarchy_doc();
        validate_doc(&doc).unwrap();

        // 把 a(id 0) 移到末尾：顺序 b,c,d,a，但父引用是 id，不受位置影响。
        let result = move_entity(&doc, 0, 4, None).unwrap();
        validate_doc(&result.doc).unwrap();
        assert_eq!(result.index, 3);

        let entities = result.doc["entities"].as_array().unwrap();
        assert_eq!(entities[0]["name"], "b");
        assert_eq!(entities[0]["parent"], 0); // 仍指向 a（id 0，位置无关）
        assert_eq!(entities[1]["name"], "c");
        assert_eq!(entities[1]["parent"], 1); // 仍指向 b（id 1）
        assert_eq!(entities[3]["name"], "a");
        assert_eq!(entities[3]["parent"], serde_json::Value::Null);

        // 插到 b 之前，并挂到 d 下。
        let result = move_entity(&doc, 2, 1, Some(3)).unwrap();
        validate_doc(&result.doc).unwrap();
        let entities = result.doc["entities"].as_array().unwrap();
        assert_eq!(entities[1]["name"], "c");
        assert_eq!(result.index, 1);
        // c 的父变成 d 的 id（3），与被移动实体的位置无关。
        assert_eq!(entities[1]["parent"], 3);

        assert!(move_entity(&doc, 0, 0, Some(2)).is_err()); // 成环
    }

    #[test]
    fn delete_entities_roots_orphans_without_rewriting_ids() {
        let doc = hierarchy_doc();

        // 删掉 b：c 变根。
        let out = delete_entities(&doc, &[1]).unwrap();
        validate_doc(&out).unwrap();
        let entities = out["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 3);
        assert_eq!(entities[1]["name"], "c");
        assert_eq!(entities[1]["parent"], serde_json::Value::Null);

        // 同时删 a 和 d：b 的父 a 被删 → 变根。
        let out = delete_entities(&doc, &[0, 3]).unwrap();
        validate_doc(&out).unwrap();
        let entities = out["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0]["name"], "b");
        assert_eq!(entities[0]["parent"], serde_json::Value::Null);
        assert_eq!(entities[1]["parent"], 1); // c 仍指向 b（id 1，不随删除改变）

        assert!(delete_entities(&doc, &[0, 1, 2, 3]).is_err());
        assert!(delete_entities(&doc, &[9]).is_err());
    }

    #[test]
    fn write_as_rename_and_delete_roundtrip() {
        let root = temp_dir("ops");

        let path = write_scene_as(&root, "alpha", &empty_scene_doc()).unwrap();
        assert!(path.exists());
        // 同名不覆盖。
        assert!(write_scene_as(&root, "alpha", &empty_scene_doc()).is_err());

        let renamed = rename_scene(&path, "beta").unwrap();
        assert!(!path.exists());
        assert!(renamed.ends_with("beta.scene.json"));

        let scenes = list_scenes(&root).unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].name, "beta");

        delete_scene(&renamed).unwrap();
        assert!(list_scenes(&root).unwrap().is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    /// #466: a scene opened through the project API resolves GUIDs to paths.
    #[test]
    fn project_read_prefers_guid_and_save_writes_both() {
        let root = temp_dir("scene_guid");
        let models = root.join("assets/models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("box.gltf"), b"mesh").unwrap();
        let meta = prism_asset::AssetMeta::ensure(&root, "assets/models/box.gltf", "GltfImporter")
            .unwrap();

        let path = scene_path(&root, "arena").unwrap();
        let doc = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [{
                "id": 0,
                "name": "Box",
                "parent": null,
                "transform": {},
                "components": {
                    "prism_engine::scene::MeshRenderer": {
                        "mesh_path": "../models/renamed.gltf",
                        "mesh_guid": meta.guid
                    }
                }
            }]
        });
        write_doc(&path, &doc).unwrap();

        let opened = read_doc_in_project(&root, &path).unwrap();
        assert_eq!(
            opened["entities"][0]["components"]["prism_engine::scene::MeshRenderer"]["mesh_path"],
            "../models/box.gltf"
        );

        // Saving the resolved document keeps both fields.
        let saved = sync_doc_for_save(&root, &path, &opened).unwrap();
        write_doc(&path, &saved).unwrap();
        let raw = read_doc(&path).unwrap();
        let renderer = &raw["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
        assert_eq!(renderer["mesh_path"], "../models/box.gltf");
        assert_eq!(renderer["mesh_guid"], meta.guid);

        // Saving a path-only document adds the sidecar GUID.
        let path_only = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [{
                "id": 0,
                "name": "Box",
                "parent": null,
                "transform": {},
                "components": {
                    "prism_engine::scene::MeshRenderer": {
                        "mesh_path": "../models/box.gltf"
                    }
                }
            }]
        });
        write_doc(&path, &path_only).unwrap();
        let saved = sync_doc_for_save(&root, &path, &path_only).unwrap();
        write_doc(&path, &saved).unwrap();
        let raw = read_doc(&path).unwrap();
        assert_eq!(
            raw["entities"][0]["components"]["prism_engine::scene::MeshRenderer"]["mesh_guid"],
            meta.guid
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A document without GUIDs and without sidecars must be byte-for-byte
    /// unchanged by a load/save round trip.
    #[test]
    fn legacy_scene_without_sidecars_is_unchanged() {
        let root = temp_dir("scene_legacy");
        let path = scene_path(&root, "legacy").unwrap();
        let doc = json!({
            "version": prism_scene::SCENE_DOC_VERSION,
            "entities": [{
                "id": 0,
                "name": "Box",
                "parent": null,
                "transform": {},
                "components": {
                    "prism_engine::scene::MeshRenderer": {
                        "mesh_path": "../models/missing.gltf"
                    }
                }
            }]
        });
        write_doc(&path, &doc).unwrap();

        let opened = read_doc_in_project(&root, &path).unwrap();
        let saved = sync_doc_for_save(&root, &path, &opened).unwrap();
        let renderer = &saved["entities"][0]["components"]["prism_engine::scene::MeshRenderer"];
        assert_eq!(renderer["mesh_path"], "../models/missing.gltf");
        assert!(renderer.get("mesh_guid").is_none());

        std::fs::remove_dir_all(&root).ok();
    }
}
