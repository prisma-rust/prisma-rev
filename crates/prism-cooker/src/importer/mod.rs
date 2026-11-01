//! # prism-cooker::importer
//!
//! PrismaRev 资源管道的导入器框架
//!
//! 导入器将源文件（`.png`、`.gltf`、`.obj`、`.wav` 等）转换为中间数据，
//! 随后由烹饪器转换为运行时格式。
//!
//! 导入管线如下：
//!
//! ```text
//! 源文件 → [Importer] → ImportResult（中间数据）
//!   ↓
//! [AssetDatabase] 记录已创建/更新
//! ```

// CI: clippy lints, fix when time permits.
#![allow(
    clippy::doc_lazy_continuation,
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo
)]

mod skinned;

use crate::db::{AssetDatabase, AssetRecord, ImportCache};
use prism_asset::core::{AssetId, AssetType};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("No importer found for: {0}")]
    NoImporter(PathBuf),

    #[error("Importer {0} rejected file: {1}")]
    ImporterRejected(String, PathBuf),

    #[error("Import failed: {0}")]
    ImportFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Database(#[from] crate::db::DatabaseError),

    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// 导入 Context
// ---------------------------------------------------------------------------

/// Context provided to an importer during the 导入 进程
pub struct ImportContext {
    /// 绝对 path to the 源 file being imported.
    pub source_path: PathBuf,
    /// xxh3 哈希 of the 源 file contents.
    pub source_hash: u64,
    /// JSON settings passed to the importer.
    pub settings: Value,
    /// 引用 to the 资源 database (for dependency lookups).
    pub db: Arc<AssetDatabase>,
}

impl std::fmt::Debug for ImportContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportContext")
            .field("source_path", &self.source_path)
            .field("source_hash", &format!("{:#x}", self.source_hash))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// 导入 结果
// ---------------------------------------------------------------------------

/// The 结果 of a successful 导入
pub struct ImportResult {
    /// The 类型 of 资源 produced.
    pub asset_type: AssetType,
    /// IDs of other assets this one depends on.
    pub dependencies: Vec<AssetId>,
    /// Intermediate 二进制 data 输入 to the cooker).
    pub output_data: Vec<u8>,
    /// Optional JSON metadata stored alongside the 资源
    pub metadata: Option<Value>,
}

impl std::fmt::Debug for ImportResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportResult")
            .field("asset_type", &self.asset_type)
            .field("deps", &self.dependencies.len())
            .field("data_size", &self.output_data.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// 导入 File 结果 (returned by ImportPipeline::import_file)
// ---------------------------------------------------------------------------

/// 结果 of a single `import_file` 调用
pub struct ImportFileResult {
    /// `true` when the file was actually imported; `false` when cache hit.
    pub was_imported: bool,
    /// When `was_imported == true`, the importer's intermediate 输出 data.
    /// `None` when the file was cached.
    pub intermediate_data: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Importer trait
// ---------------------------------------------------------------------------

/// A 可插拔 importer that converts 源 files into intermediate data.
///
/// Implementations must be `Send + Sync` so they can be registered in a 全局
/// registry and run on any 线程
pub trait Importer: Send + Sync {
    /// 唯一 name for this importer (e.g. `"texture-importer"`).
    fn name(&self) -> &'static str;

    /// Version of this importer. Increment when the 输出 格式 changes
    /// to 力 re-import.
    fn version(&self) -> u32;

    /// Return `true` if this impporter can handle the given 源 file.
    fn can_import(&self, path: &Path) -> bool;

    /// 执行 the 导入
    ///
    /// This may be called on a background 线程 / 异步 任务
    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError>;
}

// ---------------------------------------------------------------------------
// Importer Registry
// ---------------------------------------------------------------------------

/// Registry of all available importers, keyed by name.
pub struct ImporterRegistry {
    importers: Vec<Box<dyn Importer>>,
    by_name: HashMap<&'static str, usize>,
}

impl ImporterRegistry {
    /// 创建 an 空 registry.
    pub fn new() -> Self {
        Self {
            importers: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Register an importer.
    pub fn register(&mut self, importer: Box<dyn Importer>) {
        let name = importer.name();
        let idx = self.importers.len();
        self.importers.push(importer);
        self.by_name.insert(name, idx);
        tracing::info!(
            "Registered importer: {name} v{}",
            self.importers[idx].version()
        );
    }

    /// Number of registered importers.
    pub fn len(&self) -> usize {
        self.importers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.importers.is_empty()
    }

    /// 查找 an importer by name.
    pub fn get(&self, name: &str) -> Option<&dyn Importer> {
        self.by_name
            .get(name)
            .map(|&idx| self.importers[idx].as_ref())
    }

    /// 查找 the 第一个 importer that can handle a given file.
    pub fn find_for_path(&self, path: &Path) -> Option<&dyn Importer> {
        self.importers
            .iter()
            .find(|imp| imp.can_import(path))
            .map(|b| b.as_ref())
    }

    /// Iterate all registered importers.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Importer> {
        self.importers.iter().map(|b| b.as_ref())
    }
}

impl Default for ImporterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ImporterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImporterRegistry({} importers)", self.importers.len())
    }
}

// ---------------------------------------------------------------------------
// 导入 管线
// ---------------------------------------------------------------------------

/// High-level 导入 管线 that coordinates importers, the database, and
/// the 导入 cache.
pub struct ImportPipeline {
    registry: Arc<ImporterRegistry>,
}

impl ImportPipeline {
    /// 创建 a new 管线 using the given importer registry.
    pub fn new(registry: Arc<ImporterRegistry>) -> Self {
        Self { registry }
    }

    /// 引用 to the underlying registry.
    pub fn registry(&self) -> &ImporterRegistry {
        &self.registry
    }

    /// 导入 a single file.
    ///
    /// If the file is unchanged (matching 哈希 in 导入 cache), the 导入
    /// is skipped. Returns [`ImportFileResult`] with `was_imported` indicating
    /// whether the file was actually processed, and `intermediate_data` carrying
    /// the importer's 输出 for downstream cooking.
    pub fn import_file(
        &self,
        source_path: &Path,
        db: &mut AssetDatabase,
        cache: &mut ImportCache,
        settings: Option<Value>,
    ) -> Result<ImportFileResult, ImportError> {
        let normalized = normalize_relative_path(source_path);
        self.import_file_with_key(source_path, &normalized, db, cache, settings, false)
    }

    /// 导入 a single file under an explicit asset key.
    ///
    /// `relative_key` is the path relative to the project's `Assets/` root.
    /// CLI callers that already know the root should prefer this over
    /// [`Self::import_file`], whose key is derived from the (possibly
    /// absolute) source path and would otherwise duplicate the record.
    ///
    /// When `force` is `true` the import cache is ignored and the importer is
    /// run even if the source hash and settings are unchanged.
    pub fn import_file_with_key(
        &self,
        source_path: &Path,
        relative_key: &str,
        db: &mut AssetDatabase,
        cache: &mut ImportCache,
        settings: Option<Value>,
        force: bool,
    ) -> Result<ImportFileResult, ImportError> {
        let normalized = normalize_relative_path(Path::new(relative_key));
        let data = std::fs::read(source_path)?;
        let hash = xxhash_rust::xxh3::xxh3_64(&data);
        let settings = settings.unwrap_or(Value::Null);
        let settings_hash =
            xxhash_rust::xxh3::xxh3_64(serde_json::to_string(&settings)?.as_bytes());

        // 查找 importer.
        let importer = self
            .registry
            .find_for_path(source_path)
            .ok_or_else(|| ImportError::NoImporter(source_path.to_path_buf()))?;

        // Check cache (unless the caller asked for a forced re-import).
        if !force && cache.is_up_to_date(&normalized, hash, settings_hash, importer.version()) {
            tracing::debug!("  ~ cached: {normalized}");
            return Ok(ImportFileResult {
                was_imported: false,
                intermediate_data: None,
            });
        }

        // Run 导入
        let ctx = ImportContext {
            source_path: source_path.to_path_buf(),
            source_hash: hash,
            settings,
            db: Arc::new(db.clone()),
        };

        let result = importer.import(&ctx)?;

        // 更新 database.
        let id = db
            .id_by_path(&normalized)
            .unwrap_or_else(|| db.generate_id());
        let mut record =
            AssetRecord::new(id, normalized.clone(), result.asset_type, importer.name());
        record.source_hash = hash;
        record.import_settings_hash = settings_hash;
        record.dependencies = result.dependencies;
        record.version = importer.version();
        db.insert(record)?;

        // 更新 cache.
        cache.record(&normalized, hash, settings_hash, id, importer.version());

        Ok(ImportFileResult {
            was_imported: true,
            intermediate_data: Some(result.output_data),
        })
    }

    /// 导入 all files in a directory 树
    pub fn import_directory(
        &self,
        dir: &Path,
        db: &mut AssetDatabase,
        cache: &mut ImportCache,
    ) -> ImportSummary {
        let mut summary = ImportSummary::default();
        walk_directory(
            dir,
            &mut |path| match self.import_file(&path, db, cache, None) {
                Ok(r) if r.was_imported => summary.imported += 1,
                Ok(_) => summary.cached += 1,
                Err(ImportError::NoImporter(_)) => summary.skipped += 1,
                Err(e) => {
                    tracing::warn!("  ! {}: {e}", path.display());
                    summary.errors += 1;
                }
            },
        );
        summary
    }
}

/// 摘要 of an 导入 run.
#[derive(Debug, Default, Clone)]
pub struct ImportSummary {
    pub imported: u32,
    pub cached: u32,
    pub skipped: u32,
    pub errors: u32,
}

// ===========================================================================
// Built-in Importers
// ===========================================================================

// ---------------------------------------------------------------------------
// Raw / 二进制 Importer
// ---------------------------------------------------------------------------

/// Imports any unrecognized file as a raw 二进制 blob.
pub struct RawImporter;

impl Importer for RawImporter {
    fn name(&self) -> &'static str {
        "raw-importer"
    }

    fn version(&self) -> u32 {
        1
    }

    fn can_import(&self, _path: &Path) -> bool {
        true // catch-all
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let data = std::fs::read(&ctx.source_path)?;
        Ok(ImportResult {
            asset_type: AssetType::Binary,
            dependencies: Vec::new(),
            output_data: data,
            metadata: None,
        })
    }
}

// ---------------------------------------------------------------------------
// 纹理 Importer (real 解码
// ---------------------------------------------------------------------------

/// 导入 格式 tag stored in the intermediate 二进制 data.
const TEXTURE_INTERMEDIATE_MAGIC: &[u8; 4] = b"RTXI";

/// 纹理 像素 格式 枚举 for intermediate 存储
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum TexIntermediateFormat {
    Rgba8 = 0,
}

/// Imports 图像 files by decoding them to RGBA8 and storing a 标准
/// intermediate representation: `[magic:4][width:4][height:4][channels:1][format:1][pixels:N]`
pub struct TextureImporter;

impl TextureImporter {
    fn write_intermediate(width: u32, height: u32, channels: u8, rgba_pixels: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + rgba_pixels.len());
        buf.extend_from_slice(TEXTURE_INTERMEDIATE_MAGIC);
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.push(channels);
        buf.push(TexIntermediateFormat::Rgba8 as u8);
        buf.extend_from_slice(rgba_pixels);
        buf
    }

    fn read_raw_pixels(img: &image::DynamicImage) -> (u32, u32, u8, Vec<u8>) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let pixels = rgba.into_raw();
        (w, h, 4, pixels)
    }
}

impl Importer for TextureImporter {
    fn name(&self) -> &'static str {
        "texture-importer"
    }

    fn version(&self) -> u32 {
        2 // switched from raw pass-through to real decode
    }

    fn can_import(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "tga" | "bmp" | "hdr" | "exr"
        )
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let img = image::open(&ctx.source_path)
            .map_err(|e| ImportError::ImportFailed(format!("Failed to decode image: {e}")))?;

        let (w, h, channels, rgba) = Self::read_raw_pixels(&img);

        let output_data = Self::write_intermediate(w, h, channels, &rgba);

        let ext = ctx
            .source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("unknown")
            .to_lowercase();

        let metadata = serde_json::json!({
            "original_name": ctx.source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown"),
            "format": ext,
            "width": w,
            "height": h,
            "channels": channels,
            "decoded_size": output_data.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Texture,
            dependencies: Vec::new(),
            output_data,
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// glTF 网格 Importer
// ---------------------------------------------------------------------------

/// Intermediate 网格 格式 magic: "RMXI" 资源 网格 Intermediate)
const MESH_INTERMEDIATE_MAGIC: &[u8; 4] = b"RMXI";

/// Imports .gltf / .glb files: **整棵场景树**里的所有三角形 primitive 都被烘成一个
/// 世界空间的网格资产（节点 TRS 已经乘进顶点，法线用逆转置矩阵变换）。
///
/// 一个 primitive 缺法线 / UV 时整份网格就不带该属性（不拿零值冒充）；
/// 非索引 primitive 会补一条顺序索引。每个 primitive 的索引区间与源材质索引写进
/// RMXI v2 的 submesh 表；运行时按 `MeshRenderer.material_paths` 逐 submesh 绘制。
/// 源文件里的材质 / 图片可通过 [`GltfImporter::side_assets`] 落成 RMAT / RTEX
/// 旁路资产；材质参数与图片像素只保存在内存中间结果里，不写进 JSON metadata。
///
/// Intermediate 格式
/// ```text
/// [magic:4][version:1][vert_count:4][idx_count:4][uv_count:4]
/// [positions: f32*3*vert_count][normals: f32*3*vert_count or 空
/// [uv0: f32*2*vert_count or empty][indices: u32*idx_count]
/// v2 only: [submesh_count:4][index_start:4][index_count:4][material_index:4]...
/// ```
pub struct GltfImporter;

/// 一个由模型导入器附带生成、已经 cook 好的旁路资产（#520）。
///
/// `path` 是项目相对的打包路径（同时也是 `AssetId::from_path` 的输入）；
/// 材质里的纹理 ID 与这里写进包内的路径完全一致。
#[derive(Debug, Clone)]
pub struct ImportedSideAsset {
    /// Project-relative path inside the asset package.
    pub path: String,
    /// Cooked runtime bytes (RMAT for materials, RTEX for textures).
    pub cooked_data: Vec<u8>,
}

/// glTF/GLB 附带的材质与贴图资产（#520）。
#[derive(Debug, Clone, Default)]
pub struct GltfSideAssets {
    /// One material path per glTF material, in source order.
    pub material_paths: Vec<String>,
    /// One material path per mesh submesh, in draw order. Entries fall back to
    /// a generated default material when the submesh has no source material.
    pub submesh_material_paths: Vec<String>,
    /// Cooked side assets to stage at their declared paths.
    pub assets: Vec<ImportedSideAsset>,
    /// `.rskel` path when the source file carries a skeleton (#560).
    pub skeleton_path: Option<String>,
    /// `.rskin` path when the source file carries skin weights (#560).
    pub skinned_mesh_path: Option<String>,
    /// One `.ranim` path per glTF animation, in source order (#560).
    pub clip_paths: Vec<String>,
}

/// 把一套已经拆好的顶点属性写成 RMXI 中间格式。
///
/// glTF 与 OBJ 导入器共用；`normals` / `uv0` 为 `None` 时该属性整份省略
/// （cooker 会补默认值），不会写零值冒充。
fn write_mesh_intermediate(
    positions: &[[f32; 3]],
    normals: Option<&[[f32; 3]]>,
    uv0: Option<&[[f32; 2]]>,
    indices: &[u32],
    submeshes: &[SubmeshInfo],
) -> Vec<u8> {
    let verts = positions.len() as u32;
    let idxs = indices.len() as u32;
    let uv_channels: u32 = if uv0.is_some() { 1 } else { 0 };

    // Estimate 容量
    let table_len = if submeshes.is_empty() {
        0
    } else {
        4 + submeshes.len() * 12
    };
    let cap = 4 + 1 + 4 + 4 + 4
            + verts as usize * 3 * 4   // positions
            + normals.map_or(0, |n| n.len() * 3 * 4)
            + uv0.map_or(0, |u| u.len() * 2 * 4)
            + idxs as usize * 4
            + table_len;
    let mut buf = Vec::with_capacity(cap);

    buf.extend_from_slice(MESH_INTERMEDIATE_MAGIC);
    // v2: index block 后追加 submesh 表（#520）。没有 submesh 的导入器
    // （OBJ / 手工网格）继续写 v1，cooker 两种都接受。
    buf.push(if submeshes.is_empty() { 1 } else { 2 });
    buf.extend_from_slice(&verts.to_le_bytes());
    buf.extend_from_slice(&idxs.to_le_bytes());
    buf.extend_from_slice(&uv_channels.to_le_bytes());

    for pos in positions {
        buf.extend_from_slice(&pos[0].to_le_bytes());
        buf.extend_from_slice(&pos[1].to_le_bytes());
        buf.extend_from_slice(&pos[2].to_le_bytes());
    }
    if let Some(n) = normals {
        for nrm in n {
            buf.extend_from_slice(&nrm[0].to_le_bytes());
            buf.extend_from_slice(&nrm[1].to_le_bytes());
            buf.extend_from_slice(&nrm[2].to_le_bytes());
        }
    }
    if let Some(uv) = uv0 {
        for t in uv {
            buf.extend_from_slice(&t[0].to_le_bytes());
            buf.extend_from_slice(&t[1].to_le_bytes());
        }
    }
    for idx in indices {
        buf.extend_from_slice(&idx.to_le_bytes());
    }
    if !submeshes.is_empty() {
        buf.extend_from_slice(&(submeshes.len() as u32).to_le_bytes());
        for submesh in submeshes {
            buf.extend_from_slice(&submesh.index_start.to_le_bytes());
            buf.extend_from_slice(&submesh.index_count.to_le_bytes());
            buf.extend_from_slice(
                &submesh
                    .material_index
                    .map_or(u32::MAX, |index| index as u32)
                    .to_le_bytes(),
            );
        }
    }
    buf
}

impl GltfImporter {
    #[allow(clippy::type_complexity)]
    fn read_gltf(path: &Path) -> Result<MeshData, ImportError> {
        let (document, buffers, images) = gltf::import(path)
            .map_err(|e| ImportError::ImportFailed(format!("glTF parse failed: {e}")))?;

        let mut mesh = MeshData {
            material_count: document.materials().count(),
            has_skins: document.skins().next().is_some(),
            ..Default::default()
        };

        // 蒙皮骨骼 + 动画先算出来：顶点流里的 JOINTS_0 需要「skin 本地索引 →
        // 全局骨架索引」的重映射表（#560）。
        let skin_result = skinned::read_skinning(&document, &buffers);
        if !skin_result.skinning.is_empty() {
            tracing::info!(
                "glTF: {} joint(s), {} clip(s)",
                skin_result.skinning.joint_count(),
                skin_result.clips.len()
            );
        }
        mesh.skinning = skin_result.skinning;
        mesh.clips = skin_result.clips;

        // 材质 / 图片先落 metadata（#520）：多材质渲染和贴图资产落库要改
        // `.scene.json` 与 DrawItem，这一版只把源文件里的信息完整保留下来。
        mesh.materials = document
            .materials()
            .map(|material| {
                let pbr = material.pbr_metallic_roughness();
                MaterialInfo {
                    name: material.name().map(str::to_string),
                    base_color: pbr.base_color_factor(),
                    metallic: pbr.metallic_factor(),
                    roughness: pbr.roughness_factor(),
                    emissive: material.emissive_factor(),
                    alpha_mode: format!("{:?}", material.alpha_mode()),
                    double_sided: material.double_sided(),
                    base_color_texture: pbr
                        .base_color_texture()
                        .map(|t| t.texture().source().index()),
                    metallic_roughness_texture: pbr
                        .metallic_roughness_texture()
                        .map(|t| t.texture().source().index()),
                    normal_texture: material
                        .normal_texture()
                        .map(|t| t.texture().source().index()),
                    normal_scale: material.normal_texture().map(|t| t.scale()).unwrap_or(1.0),
                    occlusion_texture: material
                        .occlusion_texture()
                        .map(|t| t.texture().source().index()),
                    occlusion_strength: material
                        .occlusion_texture()
                        .map(|t| t.strength())
                        .unwrap_or(1.0),
                    emissive_texture: material
                        .emissive_texture()
                        .map(|t| t.texture().source().index()),
                }
            })
            .collect();
        mesh.image_pixels = images.iter().map(rgba8_from_gltf_image).collect();
        mesh.images = document
            .images()
            .map(|image| {
                let (uri, mime_type, source_bytes) = match image.source() {
                    gltf::image::Source::Uri { uri, mime_type } => {
                        (Some(uri.to_string()), mime_type.map(str::to_string), 0)
                    }
                    gltf::image::Source::View { view, mime_type } => {
                        (None, Some(mime_type.to_string()), view.length())
                    }
                };
                let decoded = images.get(image.index());
                ImageInfo {
                    name: image.name().map(str::to_string),
                    mime_type,
                    uri,
                    width: decoded.map(|data| data.width).unwrap_or(0),
                    height: decoded.map(|data| data.height).unwrap_or(0),
                    source_bytes,
                    format: decoded
                        .map(|data| format!("{:?}", data.format))
                        .unwrap_or_else(|| "unknown".to_string()),
                }
            })
            .collect();

        // 场景图：节点层级里的 TRS 直接烘进顶点，这样「一个网格资产」就是整棵树
        // 在世界空间里的样子（不再只取第一个 mesh 的第一个 primitive，#520）。
        let mut roots: Vec<gltf::Node> = Vec::new();
        match document
            .default_scene()
            .or_else(|| document.scenes().next())
        {
            Some(scene) => roots.extend(scene.nodes()),
            None => {
                // 没有 scene：把所有「没有父节点」的节点当根。
                let mut children = std::collections::HashSet::new();
                for node in document.nodes() {
                    for child in node.children() {
                        children.insert(child.index());
                    }
                }
                roots.extend(
                    document
                        .nodes()
                        .filter(|node| !children.contains(&node.index())),
                );
            }
        }

        let parent = IDENTITY;
        for node in &roots {
            collect_node(node, &parent, &buffers, &mut mesh);
        }

        // 没有节点但文件里有网格：退回到「所有网格、单位变换」，别让合法文件白读。
        if mesh.primitive_count == 0 && !roots.is_empty() {
            return Err(ImportError::ImportFailed(
                "glTF has nodes but no triangle primitives".into(),
            ));
        }
        if mesh.primitive_count == 0 {
            // 没有节点信息时无法绑定 skin：顶点保持未蒙皮（网格照常可渲染）。
            for gltf_mesh in document.meshes() {
                for primitive in gltf_mesh.primitives() {
                    push_primitive(&primitive, &IDENTITY, &buffers, None, &mut mesh);
                }
            }
        }
        if mesh.primitive_count == 0 {
            return Err(ImportError::ImportFailed(
                "No triangle primitives found in glTF".into(),
            ));
        }

        if mesh.positions.is_empty() {
            return Err(ImportError::ImportFailed(
                "glTF mesh has no vertices".into(),
            ));
        }
        // 有任何一个 primitive 缺 JOINTS_0/WEIGHTS_0：整份网格不带蒙皮，
        // 免得半蒙皮的数据把整条骨架拖歪。
        if !mesh.all_skin_attrs || mesh.joints.len() != mesh.positions.len() {
            mesh.joints.clear();
            mesh.weights.clear();
        }

        Ok(mesh)
    }

    /// Return the material paths a scene should assign to each submesh of a
    /// glTF/GLB mesh, in draw order (#520).
    ///
    /// This parses the source file but does not cook anything, so the scene
    /// packer can rewrite `MeshRenderer.material_paths` before cooking the
    /// scene. Use [`Self::side_assets`] to obtain the actual cooked RMAT/RTEX
    /// blobs.
    pub fn submesh_material_paths(
        path: &Path,
        mesh_pak_path: &str,
    ) -> Result<Vec<String>, ImportError> {
        let mesh = Self::read_gltf(path)?;
        Ok(gltf_material_plan(&mesh, mesh_pak_path).submesh_material_paths)
    }

    /// Cook all materials and textures referenced by a glTF/GLB file (#520).
    ///
    /// Paths are derived deterministically from `mesh_pak_path`:
    /// `{mesh}.materials/{index}_{name}.mat.json` and
    /// `{mesh}.textures/{index}_{name}.png`. The latter contain RTEX bytes;
    /// keeping the source-style extension matches the rest of the package
    /// layout, where a `.png` path is a cooked RTEX asset at runtime.
    pub fn side_assets(path: &Path, mesh_pak_path: &str) -> Result<GltfSideAssets, ImportError> {
        use crate::cooker::profile::CookSettings;
        use crate::cooker::{CookContext, Cooker as _, TextureCooker};

        let mesh = Self::read_gltf(path)?;
        let plan = gltf_material_plan(&mesh, mesh_pak_path);
        let mut assets = Vec::new();

        // One RTEX per glTF image. Materials point at these by image index.
        let mut image_paths: Vec<Option<String>> = vec![None; mesh.images.len()];
        for (index, info) in mesh.images.iter().enumerate() {
            let Some(pixels) = mesh.image_pixels.get(index) else {
                continue;
            };
            if pixels.is_empty() || info.width == 0 || info.height == 0 {
                tracing::warn!(
                    "glTF image {index} has no decodable RGBA8 pixels; skipping texture asset"
                );
                continue;
            }
            let tex_path = gltf_texture_path(mesh_pak_path, index, info.name.as_deref());
            let intermediate =
                TextureImporter::write_intermediate(info.width, info.height, 4, pixels);
            let record = AssetRecord::new(
                AssetId::from_path(&tex_path),
                tex_path.clone(),
                AssetType::Texture,
                "texture-importer",
            );
            let cooked = TextureCooker
                .cook(&CookContext {
                    record: &record,
                    imported_data: &intermediate,
                    settings: &CookSettings::default(),
                })
                .map_err(|error| {
                    ImportError::ImportFailed(format!(
                        "cook glTF texture '{tex_path}' failed: {error}"
                    ))
                })?;
            assets.push(ImportedSideAsset {
                path: tex_path.clone(),
                cooked_data: cooked.cooked_data,
            });
            image_paths[index] = Some(tex_path);
        }

        let tex_path = |slot: Option<usize>| -> Option<String> {
            slot.and_then(|index| image_paths.get(index).cloned().flatten())
        };

        for (index, info) in mesh.materials.iter().enumerate() {
            let mat_path = plan.material_paths.get(index).cloned().ok_or_else(|| {
                ImportError::ImportFailed(format!(
                    "glTF material {index} has no generated path for '{mesh_pak_path}'"
                ))
            })?;
            let material = MaterialJson {
                name: info.name.clone(),
                base_color: info.base_color,
                metallic: info.metallic,
                roughness: info.roughness,
                emissive: info.emissive,
                emissive_strength: 1.0,
                normal_scale: info.normal_scale,
                occlusion_strength: info.occlusion_strength,
                transmission: 0.0,
                ior: 1.5,
                translucency: 0.0,
                anisotropy: 0.0,
                clearcoat: 0.0,
                clearcoat_roughness: 0.0,
                albedo_tex: tex_path(info.base_color_texture),
                normal_tex: tex_path(info.normal_texture),
                metallic_roughness_tex: tex_path(info.metallic_roughness_texture),
                emissive_tex: tex_path(info.emissive_texture),
                occlusion_tex: tex_path(info.occlusion_texture),
            };
            assets.push(cook_gltf_material(&material, &mat_path)?);
        }

        if plan.needs_default {
            let material = MaterialJson {
                name: Some("glTF default".to_string()),
                base_color: [0.8, 0.8, 0.8, 1.0],
                metallic: 0.0,
                roughness: 0.5,
                emissive: [0.0; 3],
                emissive_strength: 1.0,
                normal_scale: 1.0,
                occlusion_strength: 1.0,
                transmission: 0.0,
                ior: 1.5,
                translucency: 0.0,
                anisotropy: 0.0,
                clearcoat: 0.0,
                clearcoat_roughness: 0.0,
                albedo_tex: None,
                normal_tex: None,
                metallic_roughness_tex: None,
                emissive_tex: None,
                occlusion_tex: None,
            };
            assets.push(cook_gltf_material(&material, &plan.default_path)?);
        }

        // 蒙皮 / 动画（#560）：与材质一样按「主资产路径 + 后缀」派生，
        // 保证同一个源文件每次 cook 都落到同样的路径。
        let mut skeleton_path = None;
        let mut skinned_mesh_path = None;
        let mut clip_paths = Vec::new();
        if !mesh.skinning.is_empty() {
            let joint_count = mesh.skinning.joint_count() as u32;
            let skel_path = format!("{mesh_pak_path}.skinning/skeleton.rskel");
            match crate::cooker::skinned::encode_rskel(&mesh.skinning.skeleton) {
                Some(bytes) => {
                    assets.push(ImportedSideAsset {
                        path: skel_path.clone(),
                        cooked_data: bytes,
                    });
                    skeleton_path = Some(skel_path);
                }
                None => tracing::warn!(
                    "glTF skeleton for '{mesh_pak_path}' could not be encoded; skipping .rskel"
                ),
            }

            if mesh.joints.len() == mesh.positions.len() && !mesh.joints.is_empty() {
                let rskin_path = format!("{mesh_pak_path}.skinning/mesh.rskin");
                match skinned::skinned_mesh_info(&mesh, joint_count) {
                    Some(info) => match crate::cooker::skinned::encode_rskin(&info) {
                        Some(bytes) => {
                            assets.push(ImportedSideAsset {
                                path: rskin_path.clone(),
                                cooked_data: bytes,
                            });
                            skinned_mesh_path = Some(rskin_path);
                        }
                        None => tracing::warn!(
                            "glTF skinned mesh for '{mesh_pak_path}' could not be encoded; skipping .rskin"
                        ),
                    },
                    None => tracing::warn!(
                        "glTF skinned mesh for '{mesh_pak_path}' has no usable joint/weight data; skipping .rskin"
                    ),
                }
            } else if !mesh.skinning.is_empty() {
                tracing::warn!(
                    "glTF '{mesh_pak_path}' has a skeleton but no complete JOINTS_0/WEIGHTS_0 stream; no .rskin"
                );
            }

            for (index, clip) in mesh.clips.iter().enumerate() {
                let name = sanitize_asset_name(Some(&clip.name), "clip");
                let clip_path = format!("{mesh_pak_path}.skinning/clips/{index}_{name}.ranim");
                match crate::cooker::skinned::encode_ranim(clip) {
                    Some(bytes) => {
                        assets.push(ImportedSideAsset {
                            path: clip_path.clone(),
                            cooked_data: bytes,
                        });
                        clip_paths.push(clip_path);
                    }
                    None => {
                        tracing::warn!("glTF clip '{}' could not be encoded; skipped", clip.name)
                    }
                }
            }
        }

        Ok(GltfSideAssets {
            material_paths: plan.material_paths,
            submesh_material_paths: plan.submesh_material_paths,
            assets,
            skeleton_path,
            skinned_mesh_path,
            clip_paths,
        })
    }
}

/// Deterministic material/texture plan for one glTF mesh.
struct GltfMaterialPlan {
    /// One path per source material, in glTF material order.
    material_paths: Vec<String>,
    /// One path per submesh, in draw order.
    submesh_material_paths: Vec<String>,
    default_path: String,
    /// True when at least one submesh has no source material (or the file has
    /// no materials at all) and therefore needs the generated default.
    needs_default: bool,
}

fn gltf_material_plan(mesh: &MeshData, mesh_pak_path: &str) -> GltfMaterialPlan {
    let material_paths: Vec<String> = mesh
        .materials
        .iter()
        .enumerate()
        .map(|(index, info)| {
            let name = sanitize_asset_name(info.name.as_deref(), "material");
            format!("{mesh_pak_path}.materials/{index}_{name}.mat.json")
        })
        .collect();
    let default_path = format!("{mesh_pak_path}.materials/default.mat.json");

    let mut needs_default = mesh.materials.is_empty() && !mesh.submeshes.is_empty();
    let submesh_material_paths = mesh
        .submeshes
        .iter()
        .map(|submesh| {
            submesh
                .material_index
                .and_then(|index| material_paths.get(index).cloned())
                .unwrap_or_else(|| {
                    needs_default = true;
                    default_path.clone()
                })
        })
        .collect();

    GltfMaterialPlan {
        material_paths,
        submesh_material_paths,
        default_path,
        needs_default,
    }
}

fn gltf_texture_path(mesh_pak_path: &str, index: usize, name: Option<&str>) -> String {
    let name = sanitize_asset_name(name, "image");
    format!("{mesh_pak_path}.textures/{index}_{name}.png")
}

fn sanitize_asset_name(name: Option<&str>, fallback: &str) -> String {
    let mut out = String::new();
    for character in name.unwrap_or(fallback).chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    out = if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    };
    out.truncate(48);
    out
}

/// Cook a generated material. `tex_paths` are project-relative pak paths in
/// RMAT slot order; dependencies are derived with the same `AssetId::from_path`
/// the packer uses for the staged texture files.
fn cook_gltf_material(
    material: &MaterialJson,
    material_path: &str,
) -> Result<ImportedSideAsset, ImportError> {
    use crate::cooker::profile::CookSettings;
    use crate::cooker::{CookContext, Cooker as _, MaterialCooker};

    let raw_paths: [Option<String>; 5] = [
        material.albedo_tex.clone(),
        material.normal_tex.clone(),
        material.metallic_roughness_tex.clone(),
        material.emissive_tex.clone(),
        material.occlusion_tex.clone(),
    ];
    let mut resolved: [Option<String>; 5] = [None, None, None, None, None];
    let mut dependencies = Vec::new();
    for (index, path) in raw_paths.iter().enumerate() {
        if let Some(path) = path {
            resolved[index] = Some(path.clone());
            dependencies.push(AssetId::from_path(path));
        }
    }
    let intermediate = MaterialImporter::write_intermediate(material, &resolved);
    let mut record = AssetRecord::new(
        AssetId::from_path(material_path),
        material_path.to_string(),
        AssetType::Material,
        "material-importer",
    );
    record.dependencies = dependencies;
    let cooked = MaterialCooker
        .cook(&CookContext {
            record: &record,
            imported_data: &intermediate,
            settings: &CookSettings::default(),
        })
        .map_err(|error| {
            ImportError::ImportFailed(format!(
                "cook glTF material '{material_path}' failed: {error}"
            ))
        })?;
    Ok(ImportedSideAsset {
        path: material_path.to_string(),
        cooked_data: cooked.cooked_data,
    })
}

/// Convert a decoded `gltf` image to tightly packed RGBA8.
fn rgba8_from_gltf_image(data: &gltf::image::Data) -> Vec<u8> {
    use gltf::image::Format;

    let byte = |value: f32| -> u8 { (value.clamp(0.0, 1.0) * 255.0).round() as u8 };
    let wide = |value: u16| -> u8 { (value as f32 / 65535.0 * 255.0).round() as u8 };
    let float = |bytes: &[u8]| -> f32 { f32::from_le_bytes(bytes.try_into().unwrap()) };

    match data.format {
        Format::R8 => data.pixels.iter().flat_map(|&r| [r, r, r, 255]).collect(),
        Format::R8G8 => data
            .pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], 0, 255])
            .collect(),
        Format::R8G8B8 => data
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        Format::R8G8B8A8 => data.pixels.clone(),
        Format::R16 => data
            .pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| {
                let r = wide(u16::from_le_bytes([p[0], p[1]]));
                [r, r, r, 255]
            })
            .collect(),
        Format::R16G16 => data
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let r = wide(u16::from_le_bytes([p[0], p[1]]));
                let g = wide(u16::from_le_bytes([p[2], p[3]]));
                [r, g, 0, 255]
            })
            .collect(),
        Format::R16G16B16 => data
            .pixels
            .as_chunks::<6>()
            .0
            .iter()
            .flat_map(|p| {
                let r = wide(u16::from_le_bytes([p[0], p[1]]));
                let g = wide(u16::from_le_bytes([p[2], p[3]]));
                let b = wide(u16::from_le_bytes([p[4], p[5]]));
                [r, g, b, 255]
            })
            .collect(),
        Format::R16G16B16A16 => data
            .pixels
            .as_chunks::<8>()
            .0
            .iter()
            .flat_map(|p| {
                [
                    wide(u16::from_le_bytes([p[0], p[1]])),
                    wide(u16::from_le_bytes([p[2], p[3]])),
                    wide(u16::from_le_bytes([p[4], p[5]])),
                    wide(u16::from_le_bytes([p[6], p[7]])),
                ]
            })
            .collect(),
        Format::R32G32B32FLOAT => data
            .pixels
            .as_chunks::<12>()
            .0
            .iter()
            .flat_map(|p| {
                [
                    byte(float(&p[0..4])),
                    byte(float(&p[4..8])),
                    byte(float(&p[8..12])),
                    255,
                ]
            })
            .collect(),
        Format::R32G32B32A32FLOAT => data
            .pixels
            .as_chunks::<16>()
            .0
            .iter()
            .flat_map(|p| {
                [
                    byte(float(&p[0..4])),
                    byte(float(&p[4..8])),
                    byte(float(&p[8..12])),
                    byte(float(&p[12..16])),
                ]
            })
            .collect(),
    }
}

/// 累积结果：整棵场景树烘焙成一个世界空间的三角形网格。
#[derive(Debug)]
struct MeshData {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uv0: Vec<[f32; 2]>,
    indices: Vec<u32>,
    /// 只要有一个 primitive 缺法线，整份网格就不带法线（交给 #515 的占位切线/法线流程）。
    all_normals: bool,
    all_uv0: bool,
    primitive_count: usize,
    node_count: usize,
    material_count: usize,
    has_skins: bool,
    /// 每个 primitive 在合并后的 index buffer 里的范围与材质（#520）。
    submeshes: Vec<SubmeshInfo>,
    /// glTF 里声明的材质参数（#520）；这一版只进 metadata，不生成资产。
    materials: Vec<MaterialInfo>,
    /// glTF 里的图片尺寸 / 来源（#520）；同样只进 metadata。
    images: Vec<ImageInfo>,
    /// Decoded RGBA8 pixels for each image, in glTF image order. Empty entries
    /// mean the decoder could not produce RGBA8 (unsupported format).
    image_pixels: Vec<Vec<u8>>,
    /// 每顶点 4 个骨骼下标（已重映射到全局骨架），#560。
    joints: Vec<[u16; 4]>,
    /// 每顶点 4 个权重（原始值，cooker 负责归一化），#560。
    weights: Vec<[f32; 4]>,
    /// 每个 primitive 都带 JOINTS_0/WEIGHTS_0 时才为 true。
    all_skin_attrs: bool,
    /// 骨架 + 每个 skin 的 joint 重映射（没有 skin 时为空），#560。
    skinning: skinned::GltfSkinning,
    /// glTF 里的动画剪辑（已重映射到全局骨架），#560。
    clips: Vec<prism_asset::formats_skinned::AnimClipInfo>,
}

/// 合并网格里的一个 submesh：index 区间 + 它使用的材质。
#[derive(Debug, Clone, serde::Serialize)]
struct SubmeshInfo {
    index_start: u32,
    index_count: u32,
    material_index: Option<usize>,
    material: Option<String>,
}

/// glTF 材质的 PBR 参数（只保留现有 RMAT 能表达 / 将来能映射的字段）。
#[derive(Debug, Clone, serde::Serialize)]
struct MaterialInfo {
    name: Option<String>,
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    emissive: [f32; 3],
    alpha_mode: String,
    double_sided: bool,
    base_color_texture: Option<usize>,
    metallic_roughness_texture: Option<usize>,
    normal_texture: Option<usize>,
    normal_scale: f32,
    occlusion_texture: Option<usize>,
    occlusion_strength: f32,
    emissive_texture: Option<usize>,
}

/// glTF 图片的元信息。像素本身没有写进 mesh metadata（避免 JSON 膨胀）。
#[derive(Debug, Clone, serde::Serialize)]
struct ImageInfo {
    name: Option<String>,
    mime_type: Option<String>,
    uri: Option<String>,
    width: u32,
    height: u32,
    source_bytes: usize,
    format: String,
}

impl Default for MeshData {
    fn default() -> Self {
        Self {
            positions: Vec::new(),
            normals: Vec::new(),
            uv0: Vec::new(),
            indices: Vec::new(),
            all_normals: true,
            all_uv0: true,
            primitive_count: 0,
            node_count: 0,
            material_count: 0,
            has_skins: false,
            submeshes: Vec::new(),
            materials: Vec::new(),
            images: Vec::new(),
            image_pixels: Vec::new(),
            joints: Vec::new(),
            weights: Vec::new(),
            all_skin_attrs: true,
            skinning: skinned::GltfSkinning::default(),
            clips: Vec::new(),
        }
    }
}

/// 列主序 4×4（与 glTF 的 `node.transform().matrix()` 一致）。
type Mat4 = [[f32; 4]; 4];

const IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn matrix_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for (col, slot) in out.iter_mut().enumerate() {
        for (row, value) in slot.iter_mut().enumerate() {
            *value = (0..4).map(|k| a[k][row] * b[col][k]).sum();
        }
    }
    out
}

fn transform_point(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for (row, value) in out.iter_mut().enumerate() {
        *value = m[0][row] * p[0] + m[1][row] * p[1] + m[2][row] * p[2] + m[3][row];
    }
    out
}

/// 法线矩阵 = 左上 3×3 的逆转置（有非均匀缩放时仍然正确）。
fn normal_matrix(m: &Mat4) -> [[f32; 3]; 3] {
    let a = [
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ];
    let cofactor = |r: usize, c: usize| -> f32 {
        let rows: Vec<usize> = (0..3).filter(|i| *i != r).collect();
        let cols: Vec<usize> = (0..3).filter(|i| *i != c).collect();
        let minor =
            a[rows[0]][cols[0]] * a[rows[1]][cols[1]] - a[rows[0]][cols[1]] * a[rows[1]][cols[0]];
        if (r + c).is_multiple_of(2) {
            minor
        } else {
            -minor
        }
    };
    let det = a[0][0] * cofactor(0, 0) + a[0][1] * cofactor(0, 1) + a[0][2] * cofactor(0, 2);
    if det.abs() < 1e-12 {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    // adj(A)[i][j] = cofactor(j, i)；逆转置再做一次转置 → 直接用 adj。
    let mut out = [[0.0f32; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, value) in row.iter_mut().enumerate() {
            *value = cofactor(j, i) / det;
        }
    }
    out
}

fn apply_normal(m: &[[f32; 3]; 3], n: [f32; 3]) -> [f32; 3] {
    let x = m[0][0] * n[0] + m[1][0] * n[1] + m[2][0] * n[2];
    let y = m[0][1] * n[0] + m[1][1] * n[1] + m[2][1] * n[2];
    let z = m[0][2] * n[0] + m[1][2] * n[1] + m[2][2] * n[2];
    let length = (x * x + y * y + z * z).sqrt();
    if length > 1e-6 {
        [x / length, y / length, z / length]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn collect_node(
    node: &gltf::Node,
    parent: &Mat4,
    buffers: &[gltf::buffer::Data],
    out: &mut MeshData,
) {
    out.node_count += 1;
    let world = matrix_mul(parent, &node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        // `node.skin()` 只作用于这个节点自己的网格；子节点各带各的 skin。
        let skin = node.skin();
        for primitive in mesh.primitives() {
            push_primitive(&primitive, &world, buffers, skin.as_ref(), out);
        }
    }
    for child in node.children() {
        collect_node(&child, &world, buffers, out);
    }
}

fn push_primitive(
    primitive: &gltf::Primitive,
    world: &Mat4,
    buffers: &[gltf::buffer::Data],
    skin: Option<&gltf::Skin>,
    out: &mut MeshData,
) {
    // 只烤三角形；线/点之类先跳过并计数（不在这一版的支持范围内）。
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        tracing::warn!(
            "gltf: skipping a {:?} primitive (only triangles are supported)",
            primitive.mode()
        );
        return;
    }

    let reader = primitive.reader(|buffer| Some(&buffers[buffer.index()]));
    let Some(positions) = reader.read_positions() else {
        tracing::warn!("gltf: skipping a primitive without POSITION");
        return;
    };
    let positions: Vec<[f32; 3]> = positions.collect();
    if positions.is_empty() {
        return;
    }

    let base = out.positions.len() as u32;
    let index_start = out.indices.len() as u32;
    let normals = normal_matrix(world);
    out.positions
        .extend(positions.iter().map(|p| transform_point(world, *p)));

    match reader.read_normals() {
        Some(normals_iter) => {
            let collected: Vec<[f32; 3]> = normals_iter.collect();
            if collected.len() == positions.len() {
                out.normals
                    .extend(collected.iter().map(|n| apply_normal(&normals, *n)));
            } else {
                out.all_normals = false;
                out.normals
                    .resize(out.normals.len() + positions.len(), [0.0, 0.0, 0.0]);
            }
        }
        None => {
            out.all_normals = false;
            out.normals
                .resize(out.normals.len() + positions.len(), [0.0, 0.0, 0.0]);
        }
    }

    match reader.read_tex_coords(0) {
        Some(uvs) => {
            let collected: Vec<[f32; 2]> = uvs.into_f32().collect();
            if collected.len() == positions.len() {
                out.uv0.extend(collected);
            } else {
                out.all_uv0 = false;
                out.uv0.resize(out.uv0.len() + positions.len(), [0.0, 0.0]);
            }
        }
        None => {
            out.all_uv0 = false;
            out.uv0.resize(out.uv0.len() + positions.len(), [0.0, 0.0]);
        }
    }

    // JOINTS_0 / WEIGHTS_0（#560）：只有整份网格都有蒙皮时才保留。
    if out.all_skin_attrs {
        let remap =
            skin.and_then(|skin| out.skinning.remap(skin.index()).map(|remap| (skin, remap)));
        match (remap, reader.read_joints(0), reader.read_weights(0)) {
            (Some((_skin, remap)), Some(joints), Some(weights)) => {
                let joints: Vec<[u16; 4]> = joints.into_u16().collect();
                let weights: Vec<[f32; 4]> = weights.into_f32().collect();
                if joints.len() == positions.len() && weights.len() == positions.len() {
                    out.joints.extend(joints.iter().map(|joint| {
                        let mut mapped = [0u16; 4];
                        for (slot, index) in joint.iter().enumerate() {
                            // skin 之外的下标一律夹到 0：坏数据不该让整份网格失效。
                            mapped[slot] = remap.get(*index as usize).copied().unwrap_or(0);
                        }
                        mapped
                    }));
                    out.weights.extend(weights);
                } else {
                    out.all_skin_attrs = false;
                    out.joints
                        .resize(out.joints.len() + positions.len(), [0; 4]);
                    out.weights
                        .resize(out.weights.len() + positions.len(), [0.0; 4]);
                }
            }
            _ => {
                out.all_skin_attrs = false;
                out.joints
                    .resize(out.joints.len() + positions.len(), [0; 4]);
                out.weights
                    .resize(out.weights.len() + positions.len(), [0.0; 4]);
            }
        }
    }

    match reader.read_indices() {
        Some(indices) => out.indices.extend(
            indices
                .into_u32()
                .filter(|index| (*index as usize) < positions.len())
                .map(|index| index + base),
        ),
        // 非索引网格：补一条顺序索引，而不是直接拒绝（合法的 glTF）。
        None => out
            .indices
            .extend((0..positions.len() as u32).map(|index| index + base)),
    }

    let index_count = out.indices.len() as u32 - index_start;
    let material = primitive.material();
    out.submeshes.push(SubmeshInfo {
        index_start,
        index_count,
        material_index: material.index(),
        material: material.name().map(str::to_string),
    });

    out.primitive_count += 1;
}

impl Importer for GltfImporter {
    fn name(&self) -> &'static str {
        "gltf-importer"
    }

    fn version(&self) -> u32 {
        // v2: metadata 增加 submeshes / materials / images（#520），
        // 旧的 import cache 要重新跑一遍才能拿到这些字段。
        2
    }

    fn can_import(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("gltf") || e.eq_ignore_ascii_case("glb"))
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let src = &ctx.source_path;

        let mesh = Self::read_gltf(src)?;

        let normals = mesh.all_normals.then_some(mesh.normals.as_slice());
        let texcoords = mesh.all_uv0.then_some(mesh.uv0.as_slice());
        let output_data = write_mesh_intermediate(
            &mesh.positions,
            normals,
            texcoords,
            &mesh.indices,
            &mesh.submeshes,
        );

        let metadata = serde_json::json!({
            "original_name": src
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown"),
            "vertex_count": mesh.positions.len(),
            "index_count": mesh.indices.len(),
            "has_normals": normals.is_some(),
            "has_texcoords": texcoords.is_some(),
            "primitive_count": mesh.primitive_count,
            "node_count": mesh.node_count,
            "material_count": mesh.material_count,
            "submesh_count": mesh.submeshes.len(),
            "submeshes": mesh.submeshes,
            "materials": mesh.materials,
            "images": mesh.images,
            "has_skins": mesh.has_skins,
            "intermediate_size": output_data.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Mesh,
            dependencies: Vec::new(),
            output_data,
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// OBJ 网格 Importer
// ---------------------------------------------------------------------------

/// 导入 Wavefront `.obj`。
///
/// 支持 `v` / `vt` / `vn` / `f`，面索引的三种写法（`v`、`v/vt`、`v//vn`、
/// `v/vt/vn`）以及**负索引**；多边形按扇形三角化。顶点按
/// `(position, uv, normal)` 三元组去重，因此共享边的三角形只存一份顶点。
///
/// 与 glTF 导入器一致：只要有一个面缺 UV / 法线，整份网格就不导出该属性
/// （不拿零值冒充）；`usemtl` / `o` / `g` / `s` 目前只做统计或忽略。
pub struct ObjImporter;

impl ObjImporter {
    fn read_obj(path: &Path) -> Result<MeshData, ImportError> {
        let text = std::fs::read_to_string(path)?;
        let mut positions: Vec<[f32; 3]> = Vec::new();
        let mut texcoords: Vec<[f32; 2]> = Vec::new();
        let mut normals: Vec<[f32; 3]> = Vec::new();

        let mut vertex_positions: Vec<[f32; 3]> = Vec::new();
        let mut vertex_uvs: Vec<Option<[f32; 2]>> = Vec::new();
        let mut vertex_normals: Vec<Option<[f32; 3]>> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut dedup: HashMap<(usize, Option<usize>, Option<usize>), u32> = HashMap::new();

        let mut all_normals = true;
        let mut all_uv0 = true;
        let mut face_count = 0usize;
        let mut materials = std::collections::HashSet::new();

        for (line_no, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next() else {
                continue;
            };
            match kind {
                "v" => {
                    let x = parse_obj_float(fields.next(), line_no, "vertex x")?;
                    let y = parse_obj_float(fields.next(), line_no, "vertex y")?;
                    let z = parse_obj_float(fields.next(), line_no, "vertex z")?;
                    positions.push([x, y, z]);
                }
                "vt" => {
                    let u = parse_obj_float(fields.next(), line_no, "texcoord u")?;
                    let v = fields
                        .next()
                        .map(|value| parse_obj_float(Some(value), line_no, "texcoord v"))
                        .transpose()?
                        .unwrap_or(0.0);
                    texcoords.push([u, v]);
                }
                "vn" => {
                    let x = parse_obj_float(fields.next(), line_no, "normal x")?;
                    let y = parse_obj_float(fields.next(), line_no, "normal y")?;
                    let z = parse_obj_float(fields.next(), line_no, "normal z")?;
                    normals.push([x, y, z]);
                }
                "f" => {
                    let face: Vec<(usize, Option<usize>, Option<usize>)> = fields
                        .map(|token| {
                            parse_obj_face_ref(
                                token,
                                positions.len(),
                                texcoords.len(),
                                normals.len(),
                                line_no,
                            )
                        })
                        .collect::<Result<_, _>>()?;
                    if face.len() < 3 {
                        return Err(ImportError::ImportFailed(format!(
                            "OBJ line {}: face needs at least 3 vertices",
                            line_no + 1
                        )));
                    }
                    for triangle in 1..face.len() - 1 {
                        for &(position, uv, normal) in
                            [&face[0], &face[triangle], &face[triangle + 1]]
                        {
                            if uv.is_none() {
                                all_uv0 = false;
                            }
                            if normal.is_none() {
                                all_normals = false;
                            }
                            let key = (position, uv, normal);
                            let index = match dedup.get(&key) {
                                Some(&index) => index,
                                None => {
                                    let index = vertex_positions.len() as u32;
                                    dedup.insert(key, index);
                                    vertex_positions.push(positions[position]);
                                    vertex_uvs.push(uv.map(|i| texcoords[i]));
                                    vertex_normals.push(normal.map(|i| normals[i]));
                                    index
                                }
                            };
                            indices.push(index);
                        }
                    }
                    face_count += 1;
                }
                "usemtl" => {
                    if let Some(name) = fields.next() {
                        materials.insert(name.to_string());
                    }
                }
                // `o` / `g` / `s` / `mtllib` / 注释等：OBJ 的节点与材质分组
                // 目前不进入中间格式，忽略即可。
                _ => {}
            }
        }

        if face_count == 0 {
            return Err(ImportError::ImportFailed(
                "OBJ contains no faces (`f` lines)".into(),
            ));
        }

        // `all_*` 为真时每个顶点都有该属性；`unwrap` 前的长度/内容由构造过程保证。
        let out_normals = if all_normals {
            vertex_normals
                .iter()
                .map(|normal| normal.unwrap_or([0.0, 0.0, 1.0]))
                .collect()
        } else {
            Vec::new()
        };
        let out_uvs = if all_uv0 {
            vertex_uvs
                .iter()
                .map(|uv| uv.unwrap_or([0.0, 0.0]))
                .collect()
        } else {
            Vec::new()
        };

        Ok(MeshData {
            positions: vertex_positions,
            normals: out_normals,
            uv0: out_uvs,
            indices,
            all_normals,
            all_uv0,
            primitive_count: 1,
            node_count: 1,
            material_count: materials.len(),
            has_skins: false,
            ..Default::default()
        })
    }
}

fn parse_obj_float(value: Option<&str>, line: usize, what: &str) -> Result<f32, ImportError> {
    let Some(value) = value else {
        return Err(ImportError::ImportFailed(format!(
            "OBJ line {}: missing {what}",
            line + 1
        )));
    };
    value
        .parse::<f32>()
        .map_err(|_| {
            ImportError::ImportFailed(format!("OBJ line {}: invalid {what} `{value}`", line + 1))
        })
        .and_then(|parsed| {
            if parsed.is_finite() {
                Ok(parsed)
            } else {
                Err(ImportError::ImportFailed(format!(
                    "OBJ line {}: {what} `{value}` is not finite",
                    line + 1
                )))
            }
        })
}

/// 解析一个 1-based（或负 = 相对当前末尾）OBJ 索引。
fn parse_obj_index(value: &str, len: usize, what: &str, line: usize) -> Result<usize, ImportError> {
    let raw: i64 = value.parse().map_err(|_| {
        ImportError::ImportFailed(format!(
            "OBJ line {}: invalid {what} index `{value}`",
            line + 1
        ))
    })?;
    let index = match raw {
        0 => {
            return Err(ImportError::ImportFailed(format!(
                "OBJ line {}: {what} index 0 is not valid (OBJ is 1-based)",
                line + 1
            )))
        }
        value if value > 0 => value - 1,
        value => len as i64 + value,
    };
    if index < 0 || index as usize >= len {
        return Err(ImportError::ImportFailed(format!(
            "OBJ line {}: {what} index `{value}` is out of range (have {len})",
            line + 1
        )));
    }
    Ok(index as usize)
}

fn parse_obj_face_ref(
    token: &str,
    positions: usize,
    texcoords: usize,
    normals: usize,
    line: usize,
) -> Result<(usize, Option<usize>, Option<usize>), ImportError> {
    let mut parts = token.split('/');
    let position = parse_obj_index(
        parts.next().unwrap_or_default(),
        positions,
        "position",
        line,
    )?;
    let uv = match parts.next() {
        None | Some("") => None,
        Some(value) => Some(parse_obj_index(value, texcoords, "texcoord", line)?),
    };
    let normal = match parts.next() {
        None | Some("") => None,
        Some(value) => Some(parse_obj_index(value, normals, "normal", line)?),
    };
    if parts.next().is_some() {
        return Err(ImportError::ImportFailed(format!(
            "OBJ line {}: face reference `{token}` has too many components",
            line + 1
        )));
    }
    Ok((position, uv, normal))
}

impl Importer for ObjImporter {
    fn name(&self) -> &'static str {
        "obj-importer"
    }

    fn version(&self) -> u32 {
        1
    }

    fn can_import(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("obj"))
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let src = &ctx.source_path;
        let mesh = Self::read_obj(src)?;

        let normals = mesh.all_normals.then_some(mesh.normals.as_slice());
        let texcoords = mesh.all_uv0.then_some(mesh.uv0.as_slice());
        let output_data =
            write_mesh_intermediate(&mesh.positions, normals, texcoords, &mesh.indices, &[]);

        let metadata = serde_json::json!({
            "original_name": src
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown"),
            "source_format": "obj",
            "vertex_count": mesh.positions.len(),
            "index_count": mesh.indices.len(),
            "has_normals": normals.is_some(),
            "has_texcoords": texcoords.is_some(),
            "primitive_count": mesh.primitive_count,
            "node_count": mesh.node_count,
            "material_count": mesh.material_count,
            "has_skins": mesh.has_skins,
            "intermediate_size": output_data.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Mesh,
            dependencies: Vec::new(),
            output_data,
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// JSON Importer
// ---------------------------------------------------------------------------

/// Imports JSON files, validating syntax, and registers them as 二进制 assets
/// with structured metadata.
pub struct JsonImporter;

impl Importer for JsonImporter {
    fn name(&self) -> &'static str {
        "json-importer"
    }

    fn version(&self) -> u32 {
        1
    }

    fn can_import(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let text = std::fs::read_to_string(&ctx.source_path)?;
        // Validate JSON.
        let parsed: Value = serde_json::from_str(&text)?;

        let metadata = serde_json::json!({
            "is_object": parsed.is_object(),
            "is_array": parsed.is_array(),
            "size_bytes": text.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Binary,
            dependencies: Vec::new(),
            output_data: text.into_bytes(),
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// 材质 Importer (.mat.json -> RMATI intermediate)
// ---------------------------------------------------------------------------

/// Intermediate 材质 格式 magic: "RMATI" 资源 材质 Intermediate).
/// 5 字节 so it is 不同 from the 4-byte RMAT 运行时 magic.
const MATERIAL_INTERMEDIATE_MAGIC: &[u8; 5] = b"RMATI";

/// 纹理 slots referenced by a 材质 The order is fixed and matches the
/// RMATI/RMAT 二进制 布局 (5 slots). Names 匹配 the `MaterialJson` fields.
const MATERIAL_TEX_SLOTS: [&str; 5] = [
    "albedo_tex",
    "normal_tex",
    "metallic_roughness_tex",
    "emissive_tex",
    "occlusion_tex",
];

/// Authoring schema for `.mat.json` files.
///
/// 纹理 fields are 相对 资源 paths (resolved to `AssetId` dependencies
/// at 导入 时间 All 标量 fields are optional with sensible defaults.
#[derive(Debug, Clone, serde::Deserialize)]
struct MaterialJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_base_color")]
    base_color: [f32; 4],
    #[serde(default)]
    metallic: f32,
    #[serde(default = "default_roughness")]
    roughness: f32,
    #[serde(default)]
    emissive: [f32; 3],
    #[serde(default)]
    emissive_strength: f32,
    #[serde(default = "default_one")]
    normal_scale: f32,
    #[serde(default = "default_one")]
    occlusion_strength: f32,
    #[serde(default)]
    transmission: f32,
    #[serde(default = "default_ior")]
    ior: f32,
    #[serde(default)]
    translucency: f32,
    #[serde(default)]
    anisotropy: f32,
    #[serde(default)]
    clearcoat: f32,
    #[serde(default)]
    clearcoat_roughness: f32,
    #[serde(default)]
    albedo_tex: Option<String>,
    #[serde(default)]
    normal_tex: Option<String>,
    #[serde(default)]
    metallic_roughness_tex: Option<String>,
    #[serde(default)]
    emissive_tex: Option<String>,
    #[serde(default)]
    occlusion_tex: Option<String>,
}

fn default_base_color() -> [f32; 4] {
    [0.8, 0.8, 0.8, 1.0]
}
fn default_roughness() -> f32 {
    0.5
}
fn default_one() -> f32 {
    1.0
}
fn default_ior() -> f32 {
    1.5
}

/// Imports `.mat.json` 材质 定义 files.
///
/// Produces an RMATI intermediate blob (magic + version + scalars + 5 纹理
/// path records) and resolves the 纹理 paths to `AssetId` dependencies via
/// `ctx.db.id_by_path`. Textures that don't 解析 are dropped with a 警告
/// (the 材质 still imports; the 槽 is 左 空 at 运行时
///
/// Intermediate 格式 (all little-endian):
/// ```text
/// [magic:5]   b"RMATI"
/// [version:1] 1
/// [scalars]   base_color[4] + metallic + roughness + emissive[3]
///             + emissive_strength + normal_scale + occlusion_strength
/// + transmission + ior + translucency + 各向异性
/// + clearcoat + clearcoat_roughness (each f32 LE, 18 floats 总计
/// per 槽 (5x):
///   [present:1]   0 or 1
/// [if present] [path_len:u16][path 字节 UTF-8]
/// ```
pub struct MaterialImporter;

impl MaterialImporter {
    /// 序列化 scalars + 5 texture-path records into the RMATI blob.
    fn write_intermediate(mat: &MaterialJson, tex_paths: &[Option<String>; 5]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(MATERIAL_INTERMEDIATE_MAGIC);
        buf.push(1); // version

        // 16 标量 floats (64 字节
        buf.extend_from_slice(&mat.base_color[0].to_le_bytes());
        buf.extend_from_slice(&mat.base_color[1].to_le_bytes());
        buf.extend_from_slice(&mat.base_color[2].to_le_bytes());
        buf.extend_from_slice(&mat.base_color[3].to_le_bytes());
        buf.extend_from_slice(&mat.metallic.to_le_bytes());
        buf.extend_from_slice(&mat.roughness.to_le_bytes());
        buf.extend_from_slice(&mat.emissive[0].to_le_bytes());
        buf.extend_from_slice(&mat.emissive[1].to_le_bytes());
        buf.extend_from_slice(&mat.emissive[2].to_le_bytes());
        buf.extend_from_slice(&mat.emissive_strength.to_le_bytes());
        buf.extend_from_slice(&mat.normal_scale.to_le_bytes());
        buf.extend_from_slice(&mat.occlusion_strength.to_le_bytes());
        buf.extend_from_slice(&mat.transmission.to_le_bytes());
        buf.extend_from_slice(&mat.ior.to_le_bytes());
        buf.extend_from_slice(&mat.translucency.to_le_bytes());
        buf.extend_from_slice(&mat.anisotropy.to_le_bytes());
        buf.extend_from_slice(&mat.clearcoat.to_le_bytes());
        buf.extend_from_slice(&mat.clearcoat_roughness.to_le_bytes());

        // 5 纹理 path records.
        for slot in tex_paths {
            match slot {
                Some(path) => {
                    let bytes = path.as_bytes();
                    buf.push(1); // present
                    let len = bytes.len().min(u16::MAX as usize) as u16;
                    buf.extend_from_slice(&len.to_le_bytes());
                    buf.extend_from_slice(&bytes[..len as usize]);
                }
                None => buf.push(0), // absent
            }
        }

        buf
    }
}

impl Importer for MaterialImporter {
    fn name(&self) -> &'static str {
        "material-importer"
    }

    fn version(&self) -> u32 {
        1
    }

    fn can_import(&self, path: &Path) -> bool {
        // `.mat.json` or `.mat` files. We 匹配 `.mat.json` so plain JSON files
        // still fall through to JsonImporter; bare `.mat` is also accepted.
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if name.ends_with(".mat.json") {
            return true;
        }
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mat"))
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let text = std::fs::read_to_string(&ctx.source_path)?;
        let mat: MaterialJson = serde_json::from_str(&text)
            .map_err(|e| ImportError::ImportFailed(format!("material JSON parse failed: {e}")))?;

        // Collect the 5 纹理 paths in 槽 order.
        let raw_paths: [Option<String>; 5] = [
            mat.albedo_tex.clone(),
            mat.normal_tex.clone(),
            mat.metallic_roughness_tex.clone(),
            mat.emissive_tex.clone(),
            mat.occlusion_tex.clone(),
        ];

        // 解析 each path to an AssetId dependency via the database.
        let mut dependencies: Vec<AssetId> = Vec::new();
        let mut resolved_paths: [Option<String>; 5] = [None, None, None, None, None];
        for (i, path_opt) in raw_paths.iter().enumerate() {
            if let Some(path) = path_opt {
                let normalized = path.replace('\\', "/");
                match ctx.db.id_by_path(&normalized) {
                    Some(id) => {
                        dependencies.push(id);
                        resolved_paths[i] = Some(normalized);
                    }
                    None => {
                        // The 纹理 资源 isn't registered yet; warn and leave
                        // the 槽 空 The 材质 still imports.
                        tracing::warn!(
                            "material importer: texture '{}' not in DB (slot '{}'); leaving empty",
                            normalized,
                            MATERIAL_TEX_SLOTS[i]
                        );
                    }
                }
            }
        }

        let output_data = Self::write_intermediate(&mat, &resolved_paths);

        let metadata = serde_json::json!({
            "name": mat.name.clone().unwrap_or_default(),
            "texture_slots": resolved_paths.iter().map(|p| p.is_some()).collect::<Vec<_>>(),
            "dependency_count": dependencies.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Material,
            dependencies,
            output_data,
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// 着色器 Importer Slang -> RSLI intermediate)
// ---------------------------------------------------------------------------

/// Intermediate 着色器 格式 magic: "RSLI" 资源 Slang Intermediate).
const SHADER_INTERMEDIATE_MAGIC: &[u8; 4] = b"RSLI";

/// Infer the Slang entry-point name and 阶段 from a 源 filename.
///
/// Convention (matches `shaders/compile.sh`):
/// - `*_vert.slang` -> `vertexMain` / 顶点
/// - `*_frag.slang` -> `fragmentMain` / 片元
/// - `*_comp.slang` -> `computeMain` / 计算
/// - `*_geom.slang`  -> `geometryMain` / `geometry`
/// - `*_hull.slang`  -> `hullMain`     / `hull`
/// - `*_domain.slang`-> `domainMain`   / `domain`
/// - `pt_*.slang` / `gi_*.slang` 计算 -> `ptMain` / 计算
///
/// Returns `None` for unrecognized names; the 调用者 falls 后 to defaults.
fn infer_entry_stage_from_name(file_stem: &str) -> Option<(&'static str, &'static str)> {
    let stem = file_stem.to_lowercase();
    if stem.ends_with("_vert") {
        Some(("vertexMain", "vertex"))
    } else if stem.ends_with("_frag") {
        Some(("fragmentMain", "fragment"))
    } else if stem.ends_with("_comp") {
        Some(("computeMain", "compute"))
    } else if stem.ends_with("_geom") {
        Some(("geometryMain", "geometry"))
    } else if stem.ends_with("_hull") {
        Some(("hullMain", "hull"))
    } else if stem.ends_with("_domain") {
        Some(("domainMain", "domain"))
    } else if stem.starts_with("pt_") {
        // Path-tracing 计算 shaders use `ptMain` per compile.sh.
        Some(("ptMain", "compute"))
    } else {
        None
    }
}

/// Look 上 调 in the importer settings JSON (an 对象 returning its
/// 字符串 value if present.
fn setting_str(settings: &Value, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

/// Imports Slang 着色器 源 files.
///
/// Produces an RSLI intermediate blob that carries the entry-point name,
/// 阶段 配置 and raw 源 字节 The cooker later feeds these to
/// `slangc` to produce SPIR-V
///
/// Entry-point / 阶段 / 配置 are resolved in this priority order:
/// 1. `settings["slang_entry"]` / `["slang_stage"]` / `["slang_profile"]`
///   (per-asset 导入 overrides passed by the 编辑器 / CLI).
/// 2. Filename convention (see [`infer_entry_stage_from_name`]).
/// 3. Defaults: entry = `vertexMain`, 阶段 = 顶点 配置 = `spirv_1_5`.
///
/// Intermediate 格式 (all little-endian):
/// ```text
/// [magic:4]        b"RSLI"
/// [version:1]      1
/// [entry_len:u16] + entry 字节 (UTF-8)
/// [stage_len:u16] + 阶段 字节 (UTF-8)
/// [profile_len:u16]+ 配置 字节 (UTF-8)
/// [source_len:u32] + 源 字节 (raw Slang content)
/// ```
pub struct ShaderImporter;

impl ShaderImporter {
    fn write_intermediate(entry: &str, stage: &str, profile: &str, source: &[u8]) -> Vec<u8> {
        let entry_b = entry.as_bytes();
        let stage_b = stage.as_bytes();
        let profile_b = profile.as_bytes();
        let cap =
            4 + 1 + 2 + entry_b.len() + 2 + stage_b.len() + 2 + profile_b.len() + 4 + source.len();
        let mut buf = Vec::with_capacity(cap);
        buf.extend_from_slice(SHADER_INTERMEDIATE_MAGIC);
        buf.push(1); // version

        let entry_len = entry_b.len().min(u16::MAX as usize) as u16;
        buf.extend_from_slice(&entry_len.to_le_bytes());
        buf.extend_from_slice(&entry_b[..entry_len as usize]);

        let stage_len = stage_b.len().min(u16::MAX as usize) as u16;
        buf.extend_from_slice(&stage_len.to_le_bytes());
        buf.extend_from_slice(&stage_b[..stage_len as usize]);

        let profile_len = profile_b.len().min(u16::MAX as usize) as u16;
        buf.extend_from_slice(&profile_len.to_le_bytes());
        buf.extend_from_slice(&profile_b[..profile_len as usize]);

        buf.extend_from_slice(&(source.len() as u32).to_le_bytes());
        buf.extend_from_slice(source);
        buf
    }
}

impl Importer for ShaderImporter {
    fn name(&self) -> &'static str {
        "shader-importer"
    }

    fn version(&self) -> u32 {
        1
    }

    fn can_import(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("slang"))
    }

    fn import(&self, ctx: &ImportContext) -> Result<ImportResult, ImportError> {
        let source = std::fs::read(&ctx.source_path)?;

        let file_stem = ctx
            .source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        // 解析 entry / 阶段 / 配置
        let (default_entry, default_stage) =
            infer_entry_stage_from_name(file_stem).unwrap_or(("vertexMain", "vertex"));
        let entry =
            setting_str(&ctx.settings, "slang_entry").unwrap_or_else(|| default_entry.to_owned());
        let stage =
            setting_str(&ctx.settings, "slang_stage").unwrap_or_else(|| default_stage.to_owned());
        let profile =
            setting_str(&ctx.settings, "slang_profile").unwrap_or_else(|| "spirv_1_5".to_owned());

        let output_data = Self::write_intermediate(&entry, &stage, &profile, &source);

        let metadata = serde_json::json!({
            "entry": entry,
            "stage": stage,
            "profile": profile,
            "source_size": source.len(),
        });

        Ok(ImportResult {
            asset_type: AssetType::Shader,
            dependencies: Vec::new(),
            output_data,
            metadata: Some(metadata),
        })
    }
}

// ---------------------------------------------------------------------------
// 默认 Registry
// ---------------------------------------------------------------------------

/// 构建 the 默认 importer registry with all built-in importers.
pub fn default_importer_registry() -> ImporterRegistry {
    let mut reg = ImporterRegistry::new();
    reg.register(Box::new(TextureImporter));
    reg.register(Box::new(GltfImporter));
    reg.register(Box::new(ObjImporter));
    reg.register(Box::new(MaterialImporter));
    reg.register(Box::new(ShaderImporter));
    reg.register(Box::new(JsonImporter));
    reg.register(Box::new(RawImporter)); // catch-all last
    reg
}

// ===========================================================================
// Helpers
// ===========================================================================

fn normalize_relative_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn walk_directory(dir: &Path, cb: &mut impl FnMut(PathBuf)) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_directory(&path, cb);
            } else if path.is_file() {
                cb(path);
            }
        }
    }
}

#[cfg(test)]
mod tests;
