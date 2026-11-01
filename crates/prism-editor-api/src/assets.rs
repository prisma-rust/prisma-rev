//! 资产目录浏览与引用分析：给编辑器的「路径字段」与「资产管理器」提供数据。
//!
//! 只做文件名/大小枚举与 `.scene.json` 里的字符串扫描，不解析 `.pak`，
//! 因此不需要链接运行时资产层。

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::asset_ops::normalize_asset_path;

/// 单次返回的最大条目数（避免超大资源目录拖垮 UI）。
pub const MAX_ASSETS: usize = 4000;

/// 编辑器内联预览的字节上限（图片等二进制）。
pub const MAX_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;

/// 文本预览的字节上限。
pub const MAX_TEXT_PREVIEW_BYTES: u64 = 512 * 1024;

/// 枚举时跳过的子目录：构建产物 / 超大中间数据。
const SKIP_DIRS: [&str; 3] = ["cooked", "heightmaps", "target"];

/// 项目根下的 `assets/` 目录。
pub fn assets_dir(project_root: &Path) -> PathBuf {
    project_root.join("assets")
}

/// 递归列出 `assets/` 下的文件，返回**相对项目根**的路径（`/` 分隔，已排序）。
///
/// 目录遍历有深度与数量上限；隐藏目录、[`SKIP_DIRS`] 会被跳过。
pub fn list_assets(project_root: &Path) -> Result<Vec<String>, String> {
    let root = assets_dir(project_root);
    if !root.is_dir() {
        return Err(format!("找不到资产目录 {}", root.display()));
    }

    let mut out = Vec::new();
    walk_files(&root, project_root, &mut out);
    out.sort();
    out.dedup();
    Ok(out)
}

/// 递归列出 `assets/` 下的目录（包含空目录），相对项目根、`/` 分隔。
///
/// 资产管理器用它渲染目录树：仅靠文件路径无法表示刚创建的空文件夹。
pub fn list_asset_dirs(project_root: &Path) -> Result<Vec<String>, String> {
    let root = assets_dir(project_root);
    if !root.is_dir() {
        return Err(format!("找不到资产目录 {}", root.display()));
    }

    let mut out = Vec::new();
    walk_dirs(&root, project_root, &mut out);
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_dirs(dir: &Path, project_root: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_ASSETS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if out.len() >= MAX_ASSETS {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || !path.is_dir() || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(project_root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
        walk_dirs(&path, project_root, out);
    }
}

fn walk_files(dir: &Path, project_root: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_ASSETS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // 稳定顺序：按文件名排序后再递归。
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if out.len() >= MAX_ASSETS {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_files(&path, project_root, out);
        } else if let Ok(relative) = path.strip_prefix(project_root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

// ---------------------------------------------------------------------------
// 资产元信息
// ---------------------------------------------------------------------------

/// 一个资产的元信息（编辑器「资产管理器」用）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    /// 相对项目根的路径（`/` 分隔）。
    pub path: String,
    /// 文件大小（字节；读取失败为 0）。
    pub size_bytes: u64,
    /// 粗分类：`model` / `texture` / `env` / `scene` / `package` / `data` / `other`。
    pub kind: &'static str,
    /// 负责导入该文件的导入器类名（`ImageImporter` / `ModelImporter` …）。
    pub importer: &'static str,
}

/// 按扩展名给资产粗分类。
pub fn asset_kind(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".scene.json") {
        return "scene";
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "gltf" | "glb" | "obj" | "fbx" | "mesh" | "glb2" => "model",
        "png" | "jpg" | "jpeg" | "ktx2" | "dds" | "tga" | "webp" => "texture",
        "hdr" | "exr" => "env",
        "rscn" | "scene" => "scene",
        "pak" => "package",
        "json" | "toml" | "txt" | "md" | "bin" => "data",
        "ttf" | "otf" | "ttc" => "font",
        _ => "other",
    }
}

/// 枚举资产并附带大小与分类。
pub fn list_assets_info(project_root: &Path) -> Result<Vec<AssetInfo>, String> {
    let paths = list_assets(project_root)?;
    Ok(paths
        .into_iter()
        .map(|path| {
            // `path` 相对项目根（不是相对 assets/）。
            let size_bytes = std::fs::metadata(project_root.join(&path))
                .map(|meta| meta.len())
                .unwrap_or(0);
            AssetInfo {
                importer: crate::asset_importer::ImporterClass::from_path(&path).importer_name(),
                kind: asset_kind(&path),
                path,
                size_bytes,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 资产预览
// ---------------------------------------------------------------------------

/// 文本资产的预览内容。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetTextPreview {
    /// 相对项目根的路径（`/` 分隔）。
    pub path: String,
    /// 文件内容（最多 `max_bytes` 字节，按 UTF-8 字符边界截断）。
    pub text: String,
    /// 是否因为超过上限而截断。
    pub truncated: bool,
    /// 文件总大小（字节）。
    pub size_bytes: u64,
}

fn resolve_asset_file(project_root: &Path, raw: &str) -> Result<PathBuf, String> {
    let normalized = normalize_asset_path(raw)?;
    let path = project_root.join(&normalized);
    if !path.is_file() {
        return Err(format!("资产不存在或不是文件：{normalized}"));
    }
    Ok(path)
}

/// 读取资产的前 `max_bytes` 字节（图片预览用）。
///
/// 只允许 `assets/` 下的路径，且拒绝超过上限的文件——避免一次 IPC
/// 把几十 MB 的 HDR/模型塞进 WebView。
pub fn read_asset_bytes(project_root: &Path, raw: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    let path = resolve_asset_file(project_root, raw)?;
    let size = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .map_err(|e| format!("无法读取 {}: {e}", path.display()))?;
    if size > max_bytes {
        return Err(format!(
            "文件过大：{:.1} MB（预览上限 {:.1} MB）",
            size as f64 / (1024.0 * 1024.0),
            max_bytes as f64 / (1024.0 * 1024.0)
        ));
    }
    std::fs::read(&path).map_err(|e| format!("无法读取 {}: {e}", path.display()))
}

/// 读取文本资产（按 UTF-8 解码，超限时截断）。
pub fn read_asset_text(
    project_root: &Path,
    raw: &str,
    max_bytes: u64,
) -> Result<AssetTextPreview, String> {
    let path = resolve_asset_file(project_root, raw)?;
    let size = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .map_err(|e| format!("无法读取 {}: {e}", path.display()))?;

    // 多读 1 字节用于判断是否截断。
    let mut file =
        std::fs::File::open(&path).map_err(|e| format!("无法打开 {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法读取 {}: {e}", path.display()))?;

    let truncated = bytes.len() as u64 > max_bytes;
    if truncated {
        bytes.truncate(max_bytes as usize);
    }
    if bytes.contains(&0) {
        return Err("该文件是二进制内容，无法作为文本预览".into());
    }

    // 截断可能落在多字节字符中间；只回退到最近的字符边界，
    // 文件其余部分不是 UTF-8 时仍然报错，而不是静默吞掉内容。
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            let valid = err.utf8_error().valid_up_to();
            let bytes = err.into_bytes();
            if truncated && bytes.len() - valid <= 3 {
                String::from_utf8(bytes[..valid].to_vec())
                    .map_err(|_| "该文件不是 UTF-8 文本，无法预览".to_string())?
            } else {
                return Err("该文件不是 UTF-8 文本，无法预览".into());
            }
        }
    };

    Ok(AssetTextPreview {
        path: raw.replace('\\', "/"),
        text,
        truncated,
        size_bytes: size,
    })
}

// ---------------------------------------------------------------------------
// 引用分析
// ---------------------------------------------------------------------------

/// 一条「某场景的某组件字段引用了该资产」的记录。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetRef {
    /// 引用来源：`"field"` = 文档里的组件字段；`"manifest"` = `assets/scenes.json` 条目。
    pub kind: String,
    /// 场景逻辑名（清单条目固定为 `scenes.json`）。
    pub scene: String,
    /// 场景文件绝对路径。
    pub scene_path: String,
    /// 实体名（无名时为 `#<index>`；清单条目为条目名）。
    pub entity: String,
    pub entity_index: usize,
    /// 组件规范名（清单条目为 `assets/scenes.json`）。
    pub component: String,
    /// 组件内字段路径（点号连接，数组下标用 `[i]`；清单条目为 `path`）。
    pub field: String,
    /// 实际写在该字段里的值（可能是相对路径）。
    pub value: String,
}

/// 扫描所有 `.scene.json` / `.prefab.json`，找出引用 `target` 的字段，
/// 再补上 `assets/scenes.json` 里指向该资产的构建清单条目。
///
/// `target` 是**相对项目根**的路径。字段里的引用会按两种基准解析后比较：
/// 1. 相对场景文件所在目录（引擎约定，如 `default.scene.json` 里的 `../valley.hdr`）；
/// 2. 相对项目根（作者偶尔写成 `assets/valley.hdr`）。
///
/// 两者都归一到绝对路径再判等，因此不会因为文件名相同而误报。
pub fn asset_usage(project_root: &Path, target: &str) -> Result<Vec<AssetRef>, String> {
    let target_abs = resolve(project_root, target);
    if target_abs.is_empty() {
        return Err("目标资产路径为空".into());
    }
    let mut refs = Vec::new();

    // 场景与预制体共用同一套文档形状，都要扫；预制体在 `assets/` 下是递归枚举的。
    let mut documents = crate::scene::list_scenes(project_root)?;
    documents.extend(crate::scene::list_docs(
        project_root,
        crate::scene::DocKind::Prefab,
    )?);

    for entry in documents {
        let doc = match crate::scene::read_scene_doc(Path::new(&entry.path)) {
            Ok(doc) => doc,
            Err(_) => continue,
        };
        let Some(entities) = doc.get("entities").and_then(|v| v.as_array()) else {
            continue;
        };

        for (index, entity) in entities.iter().enumerate() {
            let entity_name = entity
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("#{index}"));
            let Some(components) = entity.get("components").and_then(|v| v.as_object()) else {
                continue;
            };

            let scene_dir = Path::new(&entry.path)
                .parent()
                .unwrap_or(project_root)
                .to_path_buf();

            for (component, payload) in components {
                let mut found = Vec::new();
                collect_strings(payload, "", &mut found);
                for (field, value) in found {
                    if value.trim().is_empty() {
                        continue;
                    }
                    let referenced = resolve(&scene_dir, &value) == target_abs
                        || resolve(project_root, &value) == target_abs;
                    if referenced {
                        refs.push(AssetRef {
                            kind: "field".into(),
                            scene: entry.name.clone(),
                            scene_path: entry.path.clone(),
                            entity: entity_name.clone(),
                            entity_index: index,
                            component: component.clone(),
                            field,
                            value,
                        });
                    }
                }
            }
        }
    }

    refs.extend(manifest_usage(project_root, &target_abs)?);
    Ok(refs)
}

/// `assets/scenes.json` 里指向该资产的条目。
///
/// 清单路径可以是 `.rscn`（cook 产物）或 `.scene.json`（作者文档），两者按同一个
/// stem 比较——和 `build-scenes` 的约定一致；所以删除/重命名任一形式都能被看到。
fn manifest_usage(project_root: &Path, target_abs: &str) -> Result<Vec<AssetRef>, String> {
    let Some(path) = crate::scene_manifest::existing_manifest(project_root) else {
        return Ok(Vec::new());
    };
    let Ok(manifest) = crate::scene_manifest::read(project_root) else {
        return Ok(Vec::new());
    };

    let base = assets_dir(project_root);
    let mut refs = Vec::new();
    for (index, entry) in manifest.iter().enumerate() {
        if entry.path.trim().is_empty() {
            continue;
        }
        let trimmed = entry.path.trim_start_matches("./");
        let stem = trimmed
            .strip_suffix(".scene.json")
            .or_else(|| trimmed.strip_suffix(".rscn"));
        let mut candidates = vec![resolve(&base, trimmed)];
        if let Some(stem) = stem {
            candidates.push(resolve(&base, &format!("{stem}.scene.json")));
            candidates.push(resolve(&base, &format!("{stem}.rscn")));
        }
        if candidates.iter().any(|candidate| candidate == target_abs) {
            refs.push(AssetRef {
                kind: "manifest".into(),
                scene: "scenes.json".into(),
                scene_path: path.to_string_lossy().into_owned(),
                entity: entry.name.clone(),
                entity_index: index,
                component: "assets/scenes.json".into(),
                field: "path".into(),
                value: entry.path.clone(),
            });
        }
    }
    Ok(refs)
}

/// 用 `base` 解析 `value`，并折叠 `.` / `..`，返回 `/` 分隔的归一化路径。
fn resolve(base: &Path, value: &str) -> String {
    use std::path::Component;

    let joined = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        base.join(value)
    };

    let mut stack: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::RootDir => stack.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Prefix(prefix) => {
                stack.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            Component::Normal(part) => stack.push(part.to_string_lossy().into_owned()),
        }
    }
    stack.join("/")
}

/// 递归收集 JSON 里的字符串，字段路径形如 `a.b[0].c`。
fn collect_strings(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::String(text) => out.push((prefix.to_string(), text.clone())),
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_strings(item, &format!("{prefix}[{index}]"), out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_strings(item, &path, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "prism_editor_assets_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    #[test]
    fn lists_relative_paths_and_skips_build_dirs() {
        let root = temp_root("list");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(assets.join("external/models")).unwrap();
        std::fs::create_dir_all(assets.join("cooked/scenes")).unwrap();
        std::fs::create_dir_all(assets.join("heightmaps")).unwrap();
        std::fs::write(assets.join("valley.hdr"), b"x").unwrap();
        std::fs::write(assets.join("external/models/a.gltf"), b"x").unwrap();
        std::fs::write(assets.join("cooked/scenes/secret.rscn"), b"x").unwrap();
        std::fs::write(assets.join("heightmaps/h.png"), b"x").unwrap();

        let listed = list_assets(&root).unwrap();
        assert_eq!(
            listed,
            vec![
                "assets/external/models/a.gltf".to_string(),
                "assets/valley.hdr".to_string(),
            ]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lists_directories_including_empty_ones() {
        let root = temp_root("dirs");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(assets.join("models/empty")).unwrap();
        std::fs::create_dir_all(assets.join("cooked/scenes")).unwrap();
        std::fs::write(assets.join("models/a.gltf"), b"x").unwrap();

        let dirs = list_asset_dirs(&root).unwrap();
        assert!(dirs.contains(&"assets/models".to_string()));
        assert!(dirs.contains(&"assets/models/empty".to_string()));
        assert!(!dirs.iter().any(|dir| dir.contains("cooked")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_assets_dir_is_an_error() {
        let root = temp_root("missing");
        assert!(list_assets(&root).is_err());
    }

    #[test]
    fn classifies_assets_by_extension() {
        assert_eq!(asset_kind("assets/m/a.gltf"), "model");
        assert_eq!(asset_kind("assets/t/a.PNG"), "texture");
        assert_eq!(asset_kind("assets/e/valley.hdr"), "env");
        assert_eq!(asset_kind("assets/scenes/intro.scene.json"), "scene");
        assert_eq!(asset_kind("assets/scenes.pak"), "package");
        assert_eq!(asset_kind("assets/data/x.toml"), "data");
        assert_eq!(asset_kind("assets/gi/probe_volume.bin"), "data");
        assert_eq!(asset_kind("assets/weird.xyz"), "other");
    }

    #[test]
    fn asset_info_reports_size_and_kind() {
        let root = temp_root("info");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(assets.join("scenes")).unwrap();
        std::fs::write(assets.join("valley.hdr"), b"abcd").unwrap();
        std::fs::write(assets.join("scenes/demo.scene.json"), b"{}").unwrap();

        let info = list_assets_info(&root).unwrap();
        assert_eq!(info.len(), 2);
        assert_eq!(info[0].path, "assets/scenes/demo.scene.json");
        assert_eq!(info[0].kind, "scene");
        assert_eq!(info[0].importer, "DefaultImporter");
        assert_eq!(info[0].size_bytes, 2);
        assert_eq!(info[1].path, "assets/valley.hdr");
        assert_eq!(info[1].kind, "env");
        assert_eq!(info[1].importer, "ImageImporter");
        assert_eq!(info[1].size_bytes, 4);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finds_asset_references_including_relative_ones() {
        let root = temp_root("usage");
        let scenes = crate::scene::scenes_dir(&root);
        std::fs::create_dir_all(&scenes).unwrap();

        let doc = serde_json::json!({
            "version": 1,
            "entities": [
                {
                    "name": "Sky",
                    "parent": null,
                    "transform": { "translation": [0, 0, 0], "rotation": [0, 0, 0, 1], "scale": [1, 1, 1] },
                    "components": {
                        "prism_engine::scene::Skybox": { "hdr_path": "../valley.hdr", "enabled": true },
                        "prism_engine::scene::EnvironmentLighting": { "env_map": "../valley.hdr" },
                        "prism_engine::ui::Text": { "content": "not an asset", "font_size": 12.0, "color": [1, 1, 1, 1], "alignment": "Center" }
                    }
                },
                {
                    "name": "Mesh",
                    "parent": 0,
                    "transform": { "translation": [0, 0, 0], "rotation": [0, 0, 0, 1], "scale": [1, 1, 1] },
                    "components": {
                        "prism_engine::scene::MeshRenderer": { "mesh_path": "assets/valley.hdr" }
                    }
                }
            ]
        });
        std::fs::write(
            scenes.join("demo.scene.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();

        let refs = asset_usage(&root, "assets/valley.hdr").unwrap();
        assert_eq!(refs.len(), 3, "got {refs:#?}");

        let fields: Vec<&str> = refs.iter().map(|r| r.field.as_str()).collect();
        assert!(fields.contains(&"hdr_path")); // 场景目录相对：../valley.hdr
        assert!(fields.contains(&"env_map"));
        assert!(fields.contains(&"mesh_path")); // 项目根相对：assets/valley.hdr

        let sky = refs.iter().find(|r| r.field == "hdr_path").unwrap();
        assert_eq!(sky.entity, "Sky");
        assert_eq!(sky.entity_index, 0);
        assert_eq!(sky.component, "prism_engine::scene::Skybox");
        assert_eq!(sky.scene, "demo");

        // 无关字符串不产生引用。
        assert!(asset_usage(&root, "not_an_asset").unwrap().is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finds_references_in_prefabs() {
        let root = temp_root("usage_prefab");
        std::fs::create_dir_all(root.join("assets/models")).unwrap();
        std::fs::create_dir_all(root.join("assets/prefabs")).unwrap();
        std::fs::write(root.join("assets/models/a.gltf"), b"x").unwrap();
        let doc = serde_json::json!({
            "version": 1,
            "entities": [{
                "name": "Chair",
                "parent": null,
                "transform": { "translation": [0, 0, 0], "rotation": [0, 0, 0, 1], "scale": [1, 1, 1] },
                "components": {
                    "prism_engine::scene::MeshRenderer": { "mesh_path": "../models/a.gltf" }
                }
            }]
        });
        std::fs::write(
            root.join("assets/prefabs/chair.prefab.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();

        let refs = asset_usage(&root, "assets/models/a.gltf").unwrap();
        assert_eq!(refs.len(), 1, "got {refs:#?}");
        assert_eq!(refs[0].kind, "field");
        assert_eq!(refs[0].scene, "chair");
        assert_eq!(refs[0].entity, "Chair");
        assert!(refs[0].scene_path.ends_with("chair.prefab.json"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finds_references_in_scene_manifest() {
        let root = temp_root("usage_manifest");
        std::fs::create_dir_all(root.join("assets/scenes")).unwrap();
        crate::scene_manifest::write(
            &root,
            &[crate::scene_manifest::SceneListEntry {
                name: "intro".into(),
                path: "scenes/intro.rscn".into(),
                enabled: true,
            }],
        )
        .unwrap();

        // `.rscn` 与它的作者文档 `.scene.json` 视为同一个场景。
        for target in ["assets/scenes/intro.rscn", "assets/scenes/intro.scene.json"] {
            let refs = asset_usage(&root, target).unwrap();
            assert_eq!(refs.len(), 1, "{target}: {refs:#?}");
            assert_eq!(refs[0].kind, "manifest");
            assert_eq!(refs[0].scene, "scenes.json");
            assert_eq!(refs[0].entity, "intro");
            assert_eq!(refs[0].field, "path");
            assert!(refs[0].scene_path.ends_with("scenes.json"));
        }

        // 未登记的资产不产生清单引用。
        assert!(asset_usage(&root, "assets/scenes/other.rscn")
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_asset_bytes_and_rejects_oversized_files() {
        let root = temp_root("preview_bytes");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(assets.join("textures")).unwrap();
        std::fs::write(assets.join("textures/a.png"), b"PNGDATA").unwrap();

        let bytes = read_asset_bytes(&root, "assets/textures/a.png", 64).unwrap();
        assert_eq!(bytes, b"PNGDATA");
        assert!(read_asset_bytes(&root, "assets/textures/a.png", 4).is_err());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_text_with_truncation_on_char_boundary() {
        let root = temp_root("preview_text");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(assets.join("data")).unwrap();
        std::fs::write(assets.join("data/a.txt"), "你好，世界").unwrap();

        let full = read_asset_text(&root, "assets/data/a.txt", 1024).unwrap();
        assert_eq!(full.text, "你好，世界");
        assert!(!full.truncated);
        assert_eq!(full.size_bytes, "你好，世界".len() as u64);

        // 截断到 7 字节会落在“世”的中间，应回退到 6 字节（两个汉字）。
        let cut = read_asset_text(&root, "assets/data/a.txt", 7).unwrap();
        assert_eq!(cut.text, "你好");
        assert!(cut.truncated);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn preview_rejects_paths_outside_assets() {
        let root = temp_root("preview_safe");
        let assets = assets_dir(&root);
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("a.txt"), b"x").unwrap();

        assert!(read_asset_text(&root, "../a.txt", 64).is_err());
        assert!(read_asset_bytes(&root, "assets/../a.txt", 64).is_err());
        assert!(read_asset_text(&root, "assets/missing.txt", 64).is_err());

        std::fs::remove_dir_all(&root).ok();
    }
}
