//! 资产管理器数据层（`AssetBrowser.vue` / `editor_asset_*` 的等价物）。
//!
//! 输入是 `assets/` 下的扁平路径列表（`AssetInfo`），这里负责：
//! - 建目录树（`Project` 面板的两栏：目录树 + 列表）；
//! - 按路径 / 分类 / 导入器检索；
//! - 分类配色与体积格式化。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Model,
    Texture,
    Env,
    Scene,
    Prefab,
    Package,
    Material,
    Data,
    Audio,
    /// 工程自带字体（`.ttf` / `.otf` / `.ttc`，#583）。
    Font,
    Other,
}

impl AssetKind {
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::Model => "模型",
            AssetKind::Texture => "纹理",
            AssetKind::Env => "环境",
            AssetKind::Scene => "场景",
            AssetKind::Prefab => "预制体",
            AssetKind::Package => "包",
            AssetKind::Material => "材质",
            AssetKind::Data => "数据",
            AssetKind::Audio => "音频",
            AssetKind::Font => "字体",
            AssetKind::Other => "其他",
        }
    }

    pub fn parse(text: &str) -> AssetKind {
        match text {
            "model" => AssetKind::Model,
            "texture" => AssetKind::Texture,
            "env" => AssetKind::Env,
            "scene" => AssetKind::Scene,
            "prefab" => AssetKind::Prefab,
            "package" => AssetKind::Package,
            "material" => AssetKind::Material,
            "data" => AssetKind::Data,
            "audio" => AssetKind::Audio,
            "font" => AssetKind::Font,
            _ => AssetKind::Other,
        }
    }

    /// 与 `AssetBrowser.vue` 的分类色一致（`--accent-alt` 等强调色的用途）。
    pub fn is_text_like(self) -> bool {
        matches!(
            self,
            AssetKind::Scene | AssetKind::Prefab | AssetKind::Data | AssetKind::Material
        )
    }

    pub fn all() -> [AssetKind; 11] {
        [
            AssetKind::Model,
            AssetKind::Texture,
            AssetKind::Env,
            AssetKind::Scene,
            AssetKind::Prefab,
            AssetKind::Package,
            AssetKind::Material,
            AssetKind::Data,
            AssetKind::Audio,
            AssetKind::Font,
            AssetKind::Other,
        ]
    }
}

/// 组件字段是否是资产路径（检查器里显示「选择资产」按钮）。
///
/// 与 `prism-editor-api` 的 `ASSET_PATH_FIELDS` 保持同一判断口径：
/// `*_path` 后缀 + 少数没有后缀的历史字段（`env_map` / `volume` / `prefab`）。
pub fn is_asset_path_field(component: &str, field: &str) -> bool {
    let leaf = field.rsplit('.').next().unwrap_or(field);
    leaf.ends_with("_path")
        || matches!(leaf, "env_map" | "volume" | "prefab" | "font")
        || (component.ends_with("PrefabInstance") && leaf == "prefab")
}

/// 把文档里的一条引用解析成资产表里的路径。
///
/// 项目里的引用有三种合法写法（见 #465），都要能验证：
/// - `sprites/water.png` —— 相对 `assets/` 根（**最常见**的写法，没有 `assets/` 前缀）；
/// - `assets/textures/wood.png` —— 带前缀的项目路径；
/// - `../textures/wood.png` —— 相对**文档所在目录**；
/// - `wood.png` —— 纯文件名，按「唯一同名文件」匹配。
///
/// 返回 `None` 表示引用断掉了：文件被删、改名、或拼错。调用方据此给出可见提示
/// （检查器的 `Missing` 标记 / 层级树的警告图标，见 #467）。
pub fn resolve_reference<'a>(
    assets: &'a [String],
    doc_dir: &str,
    reference: &str,
) -> Option<&'a str> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    // 绝对路径 / 带盘符：不属于项目资产，别猜。
    if reference.starts_with('/') || reference.contains(':') {
        return None;
    }
    let exact = |path: &str| {
        assets
            .iter()
            .find(|asset| asset.as_str() == path)
            .map(String::as_str)
    };

    if reference.starts_with("assets/") {
        return exact(reference);
    }
    if reference.starts_with("./") || reference.starts_with("../") {
        let joined = if doc_dir.is_empty() {
            reference.to_string()
        } else {
            format!("{doc_dir}/{reference}")
        };
        return normalize_segments(&joined).and_then(|path| exact(&path));
    }
    // 相对 `assets/` 根：`sprites/water.png` / `textures/wood.png`。
    if let Some(hit) = exact(&format!("assets/{reference}")) {
        return Some(hit);
    }
    // 纯文件名：同名资产必须唯一，否则不猜。
    let mut hits = assets
        .iter()
        .filter(|asset| file_name(asset) == reference)
        .map(String::as_str);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// 消掉 `.` / `..` 段；`..` 越界（跑出 `assets/` 之外）时返回 `None`。
fn normalize_segments(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    (!out.is_empty()).then(|| out.join("/"))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    pub path: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default = "other_kind")]
    pub kind: AssetKind,
    #[serde(default)]
    pub importer: String,
}

fn other_kind() -> AssetKind {
    AssetKind::Other
}

impl AssetInfo {
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub fn folder(&self) -> &str {
        self.path
            .rsplit_once('/')
            .map(|(folder, _)| folder)
            .unwrap_or("")
    }

    pub fn extension(&self) -> &str {
        self.file_name()
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .unwrap_or("")
    }
}

pub fn parse_assets(value: &serde_json::Value) -> Vec<AssetInfo> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetNode {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub kind: Option<AssetKind>,
    pub size_bytes: u64,
    pub children: Vec<AssetNode>,
}

impl AssetNode {
    pub fn child_names(&self) -> Vec<&str> {
        self.children
            .iter()
            .map(|child| child.name.as_str())
            .collect()
    }

    pub fn find(&self, path: &str) -> Option<&AssetNode> {
        if self.path == path {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(path))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetTree {
    pub roots: Vec<AssetNode>,
    pub count: usize,
}

impl AssetTree {
    pub fn build(assets: &[AssetInfo]) -> Self {
        // path -> 子节点（目录用 BTreeMap 保证稳定顺序：目录在前、同类按名字）。
        #[derive(Default)]
        struct Dir {
            name: String,
            path: String,
            dirs: BTreeMap<String, Dir>,
            files: Vec<AssetInfo>,
        }

        let mut root = Dir::default();
        for asset in assets {
            let mut parts: Vec<&str> = asset
                .path
                .split('/')
                .filter(|part| !part.is_empty())
                .collect();
            // `assets/` 是项目资产根，不作为一个可折叠目录显示。
            if parts.first() == Some(&"assets") {
                parts.remove(0);
            }
            if parts.is_empty() {
                continue;
            }
            let mut current = &mut root;
            for (depth, part) in parts.iter().enumerate() {
                let is_file = depth + 1 == parts.len();
                if is_file {
                    current.files.push(asset.clone());
                } else {
                    let path = format!("assets/{}", parts[..=depth].join("/"));
                    current = current.dirs.entry(part.to_string()).or_insert_with(|| Dir {
                        name: part.to_string(),
                        path,
                        ..Default::default()
                    });
                }
            }
        }

        fn convert(dir: Dir) -> Vec<AssetNode> {
            let mut nodes: Vec<AssetNode> = dir
                .dirs
                .into_values()
                .map(|dir| AssetNode {
                    name: dir.name.clone(),
                    path: dir.path.clone(),
                    is_dir: true,
                    kind: None,
                    size_bytes: 0,
                    children: convert(dir),
                })
                .collect();
            let mut files: Vec<AssetNode> = dir
                .files
                .into_iter()
                .map(|asset| AssetNode {
                    name: asset.file_name().to_string(),
                    path: asset.path.clone(),
                    is_dir: false,
                    kind: Some(asset.kind),
                    size_bytes: asset.size_bytes,
                    children: Vec::new(),
                })
                .collect();
            files.sort_by_key(|a| a.name.to_lowercase());
            nodes.extend(files);
            nodes
        }

        Self {
            roots: convert(root),
            count: assets.len(),
        }
    }

    pub fn find(&self, path: &str) -> Option<&AssetNode> {
        self.roots.iter().find_map(|root| root.find(path))
    }

    /// 目录列表（`assets/` 下的直属目录）。
    pub fn top_level(&self) -> Vec<&AssetNode> {
        self.roots.iter().filter(|node| node.is_dir).collect()
    }

    /// 资产根目录下的散落文件（`assets/settings.ini` 之类）。
    pub fn root_files(&self) -> Vec<&AssetNode> {
        self.roots.iter().filter(|node| !node.is_dir).collect()
    }

    /// 某个目录的直接子项（`None` = 根）。
    pub fn children_of(&self, folder: Option<&str>) -> Vec<&AssetNode> {
        match folder {
            None | Some("assets") | Some("") => self.roots.iter().collect(),
            Some(folder) => self
                .find(folder)
                .map(|node| node.children.iter().collect())
                .unwrap_or_default(),
        }
    }

    /// 深度优先的行（目录树面板）。
    pub fn visible_rows<'a>(
        &'a self,
        expanded: &std::collections::HashSet<String>,
    ) -> Vec<(&'a AssetNode, u32)> {
        fn walk<'a>(
            nodes: &'a [AssetNode],
            depth: u32,
            expanded: &std::collections::HashSet<String>,
            out: &mut Vec<(&'a AssetNode, u32)>,
        ) {
            for node in nodes {
                out.push((node, depth));
                if node.is_dir && expanded.contains(&node.path) {
                    walk(&node.children, depth + 1, expanded, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, 0, expanded, &mut out);
        out
    }

    /// 插入一个空目录节点（`scan_assets` 只返回文件，空目录需要单独补）。
    pub fn insert_dir(&mut self, path: &str) {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let Some(start) = parts.iter().position(|part| *part != "assets") else {
            return;
        };
        let mut nodes = &mut self.roots;
        let mut current = String::from("assets");
        for part in &parts[start..] {
            current.push('/');
            current.push_str(part);
            let position = nodes
                .iter()
                .position(|node| node.is_dir && node.path == current);
            let index = if let Some(position) = position {
                position
            } else {
                let node = AssetNode {
                    name: (*part).to_string(),
                    path: current.clone(),
                    is_dir: true,
                    kind: None,
                    size_bytes: 0,
                    children: Vec::new(),
                };
                let first_file = nodes
                    .iter()
                    .position(|node| !node.is_dir)
                    .unwrap_or(nodes.len());
                let key = part.to_lowercase();
                let insert_at = nodes[..first_file]
                    .iter()
                    .position(|node| node.name.to_lowercase() > key)
                    .unwrap_or(first_file);
                nodes.insert(insert_at, node);
                insert_at
            };
            nodes = &mut nodes[index].children;
        }
    }

    /// 某个目录下（递归）的全部文件。
    pub fn files_under<'a>(&'a self, folder: Option<&str>) -> Vec<&'a AssetNode> {
        let mut out = Vec::new();
        let mut stack: Vec<&AssetNode> = match folder {
            None | Some("assets") | Some("") => self.roots.iter().collect(),
            Some(folder) => self
                .find(folder)
                .map(|node| node.children.iter().collect())
                .unwrap_or_default(),
        };
        while let Some(node) = stack.pop() {
            if node.is_dir {
                stack.extend(node.children.iter());
            } else {
                out.push(node);
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

/// 稳定的资产 GUID（32 位十六进制，形状与 `.meta` 里的 guid 一致）。
///
/// 用 FNV-1a 64 位哈希路径：不引入 uuid 依赖，同一路径跨进程稳定。
pub fn asset_guid(path: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}{hash:016x}")
}

/// 复制资产时生成不冲突的路径：`rock.png` → `rock-2.png`。
///
/// `.scene.json` / `.prefab.json` 的双后缀会整体保留（`a-2.scene.json`），
/// 不会生成 `a.scene-2.json`。
pub fn unique_asset_path(assets: &[AssetInfo], path: &str) -> String {
    let taken: std::collections::HashSet<&str> =
        assets.iter().map(|asset| asset.path.as_str()).collect();
    if !taken.contains(path) {
        return path.to_string();
    }
    let (stem, suffix) = split_asset_suffix(path);
    for index in 2..1000 {
        let candidate = format!("{stem}-{index}{suffix}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    format!("{stem}-copy{suffix}")
}

fn split_asset_suffix(path: &str) -> (&str, &str) {
    for suffix in [".scene.json", ".prefab.json"] {
        if let Some(stem) = path.strip_suffix(suffix) {
            return (stem, suffix);
        }
    }
    match path.rsplit_once('.') {
        Some((stem, _extension)) if !stem.is_empty() => (stem, &path[stem.len()..]),
        _ => (path, ""),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssetPreview {
    Text {
        text: String,
        truncated: bool,
    },
    Image {
        width: Option<u32>,
        height: Option<u32>,
        bytes: u64,
    },
    Document,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetReference {
    pub document: String,
    pub entity: u32,
    pub entity_name: String,
    pub field: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetDetails {
    pub asset: AssetInfo,
    pub guid: String,
    pub preview: AssetPreview,
    pub references: Vec<AssetReference>,
}

/// 读文本预览（最多 `max_bytes`，按 UTF-8 边界截断）。
pub fn text_preview(
    root: &std::path::Path,
    relative: &str,
    max_bytes: usize,
) -> Option<(String, bool)> {
    let bytes = std::fs::read(root.join(relative)).ok()?;
    let truncated = bytes.len() > max_bytes;
    let slice = &bytes[..bytes.len().min(max_bytes)];
    let mut text = String::from_utf8_lossy(slice).to_string();
    while !text.is_char_boundary(text.len()) {
        text.pop();
    }
    Some((text, truncated))
}

/// 图片尺寸：支持 PNG / GIF / BMP / JPEG（只读文件头，不引入解码库）。
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((width, height));
    }
    if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 10 {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return Some((width, height));
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?) as u32;
        let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?).unsigned_abs();
        return Some((width, height));
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2;
        while offset + 9 < bytes.len() {
            if bytes[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            let is_sof = matches!(
                marker,
                0xc0 | 0xc1
                    | 0xc2
                    | 0xc3
                    | 0xc5
                    | 0xc6
                    | 0xc7
                    | 0xc9
                    | 0xca
                    | 0xcb
                    | 0xcd
                    | 0xce
                    | 0xcf
            );
            if is_sof && offset + 9 < bytes.len() {
                let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
                let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
                return Some((width, height));
            }
            offset += 2 + length;
        }
    }
    None
}

pub fn asset_preview(
    root: Option<&std::path::Path>,
    asset: &AssetInfo,
    max_text_bytes: usize,
) -> AssetPreview {
    if asset.kind == AssetKind::Scene || asset.kind == AssetKind::Prefab {
        return AssetPreview::Document;
    }
    let Some(root) = root else {
        return AssetPreview::None;
    };
    if asset.kind == AssetKind::Texture || asset.kind == AssetKind::Env {
        let bytes = std::fs::read(root.join(&asset.path)).unwrap_or_default();
        let dimensions = image_dimensions(&bytes);
        return AssetPreview::Image {
            width: dimensions.map(|(width, _)| width),
            height: dimensions.map(|(_, height)| height),
            bytes: bytes.len() as u64,
        };
    }
    if asset.kind.is_text_like() || asset.kind == AssetKind::Audio || asset.kind == AssetKind::Other
    {
        if let Some((text, truncated)) = text_preview(root, &asset.path, max_text_bytes) {
            if !text.contains('\u{0}') {
                return AssetPreview::Text { text, truncated };
            }
        }
    }
    AssetPreview::None
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetQuery {
    pub search: String,
    pub kinds: Vec<AssetKind>,
}

impl AssetQuery {
    pub fn matches(&self, asset: &AssetInfo) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&asset.kind) {
            return false;
        }
        if self.search.is_empty() {
            return true;
        }
        asset
            .path
            .to_ascii_lowercase()
            .contains(&self.search.to_ascii_lowercase())
    }
}

pub fn filter_assets<'a>(assets: &'a [AssetInfo], query: &AssetQuery) -> Vec<&'a AssetInfo> {
    assets.iter().filter(|asset| query.matches(asset)).collect()
}

/// 人类可读体积（与 `format.ts::formatBytes` 一致：1024 进制、1 位小数）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<AssetInfo> {
        vec![
            AssetInfo {
                path: "assets/scenes/default.scene.json".into(),
                size_bytes: 2048,
                kind: AssetKind::Scene,
                importer: "TextScriptImporter".into(),
            },
            AssetInfo {
                path: "assets/scenes/rocket.scene.json".into(),
                size_bytes: 1024,
                kind: AssetKind::Scene,
                importer: "TextScriptImporter".into(),
            },
            AssetInfo {
                path: "assets/prefabs/camera.prefab.json".into(),
                size_bytes: 512,
                kind: AssetKind::Prefab,
                importer: "TextScriptImporter".into(),
            },
            AssetInfo {
                path: "assets/textures/brick.png".into(),
                size_bytes: 5_242_880,
                kind: AssetKind::Texture,
                importer: "TextureImporter".into(),
            },
            AssetInfo {
                path: "assets/settings.ini".into(),
                size_bytes: 64,
                kind: AssetKind::Data,
                importer: "DefaultImporter".into(),
            },
        ]
    }

    #[test]
    fn builds_directory_tree_with_dirs_first() {
        let tree = AssetTree::build(&sample());
        assert_eq!(tree.count, 5);
        let top: Vec<&str> = tree
            .top_level()
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(top, vec!["prefabs", "scenes", "textures"]);
        let root_files: Vec<&str> = tree
            .root_files()
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(root_files, vec!["settings.ini"]);

        let scenes = tree.find("assets/scenes").expect("scenes dir");
        let names: Vec<&str> = scenes.child_names();
        assert_eq!(names, vec!["default.scene.json", "rocket.scene.json"]);

        // 根目录的文件也在根节点里。
        assert!(tree.find("assets/settings.ini").is_some());
    }

    #[test]
    fn visible_rows_respect_expansion() {
        let tree = AssetTree::build(&sample());
        let mut expanded = std::collections::HashSet::new();
        assert_eq!(tree.visible_rows(&expanded).len(), 4, "3 目录 + 1 根文件");
        assert_eq!(tree.children_of(Some("assets")).len(), 4);

        expanded.insert("assets/scenes".to_string());
        let rows = tree.visible_rows(&expanded);
        assert_eq!(rows.len(), 6);
        assert!(rows
            .iter()
            .any(|(node, depth)| node.name == "rocket.scene.json" && *depth == 1));
    }

    #[test]
    fn files_under_is_recursive_and_sorted() {
        let tree = AssetTree::build(&sample());
        let assets = tree.files_under(Some("assets"));
        assert_eq!(assets.len(), 5);
        assert_eq!(assets[0].path, "assets/prefabs/camera.prefab.json");

        let scenes = tree.files_under(Some("assets/scenes"));
        assert_eq!(scenes.len(), 2);
    }

    #[test]
    fn query_filters_by_kind_and_search() {
        let assets = sample();
        let mut query = AssetQuery::default();
        assert_eq!(filter_assets(&assets, &query).len(), 5);

        query.search = "SCENE".into();
        let hits = filter_assets(&assets, &query);
        assert_eq!(hits.len(), 2, "搜索不区分大小写");

        query.search.clear();
        query.kinds.push(AssetKind::Texture);
        let hits = filter_assets(&assets, &query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name(), "brick.png");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(5_242_880), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn asset_metadata_helpers() {
        let asset = &sample()[0];
        assert_eq!(asset.file_name(), "default.scene.json");
        assert_eq!(asset.folder(), "assets/scenes");
        assert_eq!(asset.extension(), "json");
        assert!(AssetKind::Scene.is_text_like());
        assert!(!AssetKind::Texture.is_text_like());
        assert_eq!(AssetKind::parse("texture"), AssetKind::Texture);
        assert_eq!(AssetKind::parse("weird"), AssetKind::Other);
    }

    #[test]
    fn guid_is_stable_and_path_specific() {
        let first = asset_guid("assets/rock.png");
        assert_eq!(first.len(), 32);
        assert_eq!(first, asset_guid("assets/rock.png"));
        assert_ne!(first, asset_guid("assets/rock2.png"));
    }

    #[test]
    fn unique_asset_path_keeps_document_suffix() {
        let assets = sample();
        assert_eq!(
            unique_asset_path(&assets, "assets/scenes/new.scene.json"),
            "assets/scenes/new.scene.json"
        );
        assert_eq!(
            unique_asset_path(&assets, "assets/scenes/default.scene.json"),
            "assets/scenes/default-2.scene.json"
        );
        assert_eq!(
            unique_asset_path(&assets, "assets/textures/brick.png"),
            "assets/textures/brick-2.png"
        );
    }

    #[test]
    fn reads_image_dimensions_from_headers() {
        let mut png = vec![0_u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&640_u32.to_be_bytes());
        png[20..24].copy_from_slice(&480_u32.to_be_bytes());
        assert_eq!(image_dimensions(&png), Some((640, 480)));

        let mut gif = vec![0_u8; 10];
        gif[..6].copy_from_slice(b"GIF89a");
        gif[6..8].copy_from_slice(&32_u16.to_le_bytes());
        gif[8..10].copy_from_slice(&16_u16.to_le_bytes());
        assert_eq!(image_dimensions(&gif), Some((32, 16)));
        assert_eq!(image_dimensions(b"not an image"), None);
    }

    #[test]
    fn text_preview_truncates_on_char_boundary() {
        let root = std::env::temp_dir().join(format!(
            "prism-asset-preview-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/note.txt"), "hello world").unwrap();
        let (text, truncated) = text_preview(&root, "assets/note.txt", 5).unwrap();
        assert_eq!(text, "hello");
        assert!(truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn paths() -> Vec<String> {
        vec![
            "assets/textures/wood.png".to_string(),
            "assets/scenes/level.scene.json".to_string(),
            "assets/prefabs/box.prefab.json".to_string(),
            "assets/a/shared.png".to_string(),
            "assets/b/shared.png".to_string(),
        ]
    }

    #[test]
    fn references_resolve_in_all_three_writing_styles() {
        let assets = paths();
        // 相对项目根
        assert_eq!(
            resolve_reference(&assets, "assets/scenes", "assets/textures/wood.png"),
            Some("assets/textures/wood.png")
        );
        // 相对文档目录（#465 允许的写法）
        assert_eq!(
            resolve_reference(&assets, "assets/scenes/deep", "../../textures/wood.png"),
            Some("assets/textures/wood.png")
        );
        // 相对 assets/ 根（项目里最常见的写法）
        assert_eq!(
            resolve_reference(&assets, "assets/scenes", "textures/wood.png"),
            Some("assets/textures/wood.png")
        );
        // 纯文件名（唯一时）
        assert_eq!(
            resolve_reference(&assets, "assets/scenes", "wood.png"),
            Some("assets/textures/wood.png")
        );
    }

    #[test]
    fn a_broken_reference_resolves_to_nothing() {
        let assets = paths();
        assert_eq!(
            resolve_reference(&assets, "assets/scenes", "assets/textures/stone.png"),
            None
        );
        // 同名文件不唯一时不能瞎猜。
        assert_eq!(resolve_reference(&assets, "assets", "shared.png"), None);
        // 空值 = 没设，不算断引用。
        assert_eq!(resolve_reference(&assets, "assets", "   "), None);
        // 相对路径跑出 assets/ 之外。
        assert_eq!(resolve_reference(&assets, "assets", "../outside.png"), None);
    }
}
