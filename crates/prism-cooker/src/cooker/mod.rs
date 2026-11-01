//! # prism-cooker
//!
//! PrismaRev 资源管线的烹饪器框架
//!
//! 烹饪器将中间导入数据转换为运行时就绪的二进制格式，
//! 然后打包为 .pak 存档。
//!
//! 烹饪管线如下：
//!
//! ```text
//! ImportResult（中间数据）→ [Cooker] → .pak data → [PackageBuilder]
//! ```

#![allow(clippy::pedantic, clippy::nursery, clippy::cargo)]

use crate::db::AssetRecord;
use prism_asset::core::{AssetId, AssetType};
// RTEX format bytes are part of the wire format; the single source of truth
// lives in `prism-asset::formats` (shared with the runtime decoder).
use prism_asset::formats::{RMES_VERSION, RTEX_FORMAT_RGBA8};
use prism_asset::package::PackageBuilder;
use std::collections::HashMap;
use thiserror::Error;

pub mod profile;
pub mod scene;
pub mod skinned;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CookError {
    #[error("No cooker found for asset type {0:?}")]
    NoCooker(AssetType),

    #[error("Cook failed: {0}")]
    CookFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Package error: {0}")]
    Package(#[from] prism_asset::package::PackageError),
}

// ---------------------------------------------------------------------------
// 烹饪 Context & 结果
// ---------------------------------------------------------------------------

/// Context provided to a cooker.
pub struct CookContext<'a> {
    /// The 资源 record from the database.
    pub record: &'a AssetRecord,
    /// The imported intermediate data.
    pub imported_data: &'a [u8],
    /// Final merged cooking settings for this 构建
    pub settings: &'a profile::CookSettings,
}

impl std::fmt::Debug for CookContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookContext")
            .field("record", &self.record.path)
            .field("data_size", &self.imported_data.len())
            .finish()
    }
}

/// 结果 of a cooking 操作
pub struct CookResult {
    /// The cooked 二进制 data ready for packaging.
    pub cooked_data: Vec<u8>,
    /// Whether to 压缩 this 资源 in the .pak.
    pub compress: bool,
}

impl std::fmt::Debug for CookResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookResult")
            .field("data_size", &self.cooked_data.len())
            .field("compress", &self.compress)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Cooker trait
// ---------------------------------------------------------------------------

/// A 可插拔 cooker that converts intermediate data into 运行时 格式
pub trait Cooker: Send + Sync {
    /// 唯一 name for this cooker.
    fn name(&self) -> &'static str;

    /// Return `true` if this cooker can handle the given 资源 类型
    fn can_cook(&self, asset_type: AssetType) -> bool;

    /// 执行 the cooking step.
    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError>;
}

// ---------------------------------------------------------------------------
// Cooker Registry
// ---------------------------------------------------------------------------

/// Registry of all available cookers, keyed by name.
pub struct CookerRegistry {
    cookers: Vec<Box<dyn Cooker>>,
    by_name: HashMap<&'static str, usize>,
}

impl CookerRegistry {
    pub fn new() -> Self {
        Self {
            cookers: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Register a cooker.
    pub fn register(&mut self, cooker: Box<dyn Cooker>) {
        let name = cooker.name();
        let idx = self.cookers.len();
        self.cookers.push(cooker);
        self.by_name.insert(name, idx);
        tracing::info!("Registered cooker: {name}");
    }

    pub fn len(&self) -> usize {
        self.cookers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookers.is_empty()
    }

    /// 查找 a cooker by name.
    pub fn get(&self, name: &str) -> Option<&dyn Cooker> {
        self.by_name
            .get(name)
            .map(|&idx| self.cookers[idx].as_ref())
    }

    /// 查找 the 第一个 cooker that can handle a given 资源 类型
    pub fn find_for_type(&self, asset_type: AssetType) -> Option<&dyn Cooker> {
        self.cookers
            .iter()
            .find(|c| c.can_cook(asset_type))
            .map(|b| b.as_ref())
    }

    /// Iterate all cookers.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Cooker> {
        self.cookers.iter().map(|b| b.as_ref())
    }
}

impl Default for CookerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Built-in Cookers
// ===========================================================================

// ---------------------------------------------------------------------------
// 二进制 Cooker (pass-through)
// ---------------------------------------------------------------------------

/// Cooks 二进制 assets by passing data through unchanged.
pub struct BinaryCooker;

impl Cooker for BinaryCooker {
    fn name(&self) -> &'static str {
        "binary-cooker"
    }

    fn can_cook(&self, asset_type: AssetType) -> bool {
        matches!(asset_type, AssetType::Binary)
    }

    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError> {
        Ok(CookResult {
            cooked_data: ctx.imported_data.to_vec(),
            compress: true,
        })
    }
}

// ---------------------------------------------------------------------------
// 纹理 Cooker — decodes intermediate RTXI → generates mip 链 → RTEX
// ---------------------------------------------------------------------------

/// RTEX header magic (cooked 运行时 纹理
const RTEX_MAGIC: &[u8; 4] = b"RTEX";
/// RTXI magic from the importer intermediate.
const RTXI_MAGIC: &[u8; 4] = b"RTXI";
/// 最大 mip levels for a single 纹理
const MAX_MIP_LEVELS: u32 = 16;

/// Cooks 纹理 data by reconstructing the RGBA 图像 generating a mip
/// 链 (box-filtered), and packing into a runtime-ready 二进制
///
/// ```text
/// [magic:4][version:1][width:4][height:4][mip_levels:4][format:1]
/// [mip0_offset:4][mip1_offset:4]...[mip0_data][mip1_data]...
/// ```
pub struct TextureCooker;

impl TextureCooker {
    fn parse_intermediate(data: &[u8]) -> Option<(u32, u32, &[u8])> {
        if data.len() < 12 || &data[..4] != RTXI_MAGIC {
            return None;
        }
        let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
        // Skip byte 12 (channels) and byte 13 格式
        let pixels_start = 14usize;
        let expected = w as usize * h as usize * 4;
        if data.len() < pixels_start + expected {
            return None;
        }
        Some((w, h, &data[pixels_start..pixels_start + expected]))
    }

    /// 把最长边压到 `max_size` 以内（0 = 不限制），保持宽高比。
    ///
    /// 用面积平均（box filter）而不是最近邻：缩小后不会丢掉整体亮度，
    /// 也不会产生摩尔纹。只做**缩小**，小图不会被放大。
    fn clamp_max_size<'a>(
        width: u32,
        height: u32,
        rgba: &'a [u8],
        max_size: u32,
    ) -> (u32, u32, std::borrow::Cow<'a, [u8]>) {
        if max_size == 0 {
            return (width, height, std::borrow::Cow::Borrowed(rgba));
        }
        let longest = width.max(height);
        if longest <= max_size {
            return (width, height, std::borrow::Cow::Borrowed(rgba));
        }
        let scale = max_size as f32 / longest as f32;
        let new_w = ((width as f32 * scale).round() as u32).max(1);
        let new_h = ((height as f32 * scale).round() as u32).max(1);
        (
            new_w,
            new_h,
            std::borrow::Cow::Owned(Self::resize_rgba(width, height, rgba, new_w, new_h)),
        )
    }

    /// 面积平均缩放（只用于缩小）。
    fn resize_rgba(width: u32, height: u32, rgba: &[u8], new_w: u32, new_h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(new_w as usize * new_h as usize * 4);
        for y in 0..new_h {
            let y0 = y * height / new_h;
            let y1 = ((y + 1) * height / new_h).max(y0 + 1).min(height);
            for x in 0..new_w {
                let x0 = x * width / new_w;
                let x1 = ((x + 1) * width / new_w).max(x0 + 1).min(width);
                let mut acc = [0u32; 4];
                let mut count = 0u32;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let idx = ((sy * width) + sx) as usize * 4;
                        for channel in 0..4 {
                            acc[channel] += rgba[idx + channel] as u32;
                        }
                        count += 1;
                    }
                }
                for channel in acc {
                    out.push((channel / count.max(1)) as u8);
                }
            }
        }
        out
    }

    /// Generate a mip 链 using simple 2×2 盒 filtering.
    fn generate_mips(width: u32, height: u32, rgba: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
        let mut mips = Vec::new();
        mips.push((width, height, rgba.to_vec()));

        let mut src_w = width;
        let mut src_h = height;
        let mut prev = rgba.to_vec();

        loop {
            let dst_w = (src_w / 2).max(1);
            let dst_h = (src_h / 2).max(1);
            let mut next = Vec::with_capacity(dst_w as usize * dst_h as usize * 4);

            for y in 0..dst_h {
                for x in 0..dst_w {
                    // 2×2 盒 滤波器
                    let mut r = 0u32;
                    let mut g = 0u32;
                    let mut b = 0u32;
                    let mut a = 0u32;
                    let mut count = 0u32;

                    for dy in 0..2 {
                        for dx in 0..2 {
                            let sx = x * 2 + dx;
                            let sy = y * 2 + dy;
                            // Guard against 源 dimensions (not the 目标
                            if sx < src_w && sy < src_h {
                                let idx = ((sy * src_w) + sx) as usize * 4;
                                r += prev[idx] as u32;
                                g += prev[idx + 1] as u32;
                                b += prev[idx + 2] as u32;
                                a += prev[idx + 3] as u32;
                                count += 1;
                            }
                        }
                    }

                    next.push((r / count) as u8);
                    next.push((g / count) as u8);
                    next.push((b / count) as u8);
                    next.push((a / count) as u8);
                }
            }

            mips.push((dst_w, dst_h, next.clone()));
            prev = next;
            src_w = dst_w;
            src_h = dst_h;

            if mips.len() >= MAX_MIP_LEVELS as usize || (dst_w == 1 && dst_h == 1) {
                break;
            }
        }

        mips
    }

    fn write_rtex(mips: &[(u32, u32, Vec<u8>)], format: u8) -> Vec<u8> {
        let levels = mips.len() as u32;
        // Header: magic(4) + version(1) + w(4) + h(4) + levels(4) + format(1) + offsets(levels*4)
        let header_size = 4 + 1 + 4 + 4 + 4 + 1 + (levels as usize * 4);
        let data_size: usize = mips.iter().map(|m| m.2.len()).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        buf.extend_from_slice(RTEX_MAGIC);
        buf.push(1); // version
        buf.extend_from_slice(&mips[0].0.to_le_bytes()); // base width
        buf.extend_from_slice(&mips[0].1.to_le_bytes()); // base height
        buf.extend_from_slice(&levels.to_le_bytes());
        buf.push(format); // 0 = RGBA8

        // 预留 空间 for offsets.
        let offset_pos = buf.len();
        buf.resize(offset_pos + levels as usize * 4, 0);

        // 写入 mip data and record offsets.
        let mut mip_start = header_size as u32;
        for (i, mip) in mips.iter().enumerate() {
            let off = &mut buf[offset_pos + i * 4..offset_pos + (i + 1) * 4];
            off.copy_from_slice(&mip_start.to_le_bytes());
            buf.extend_from_slice(&mip.2);
            mip_start += mip.2.len() as u32;
        }

        buf
    }
}

impl Cooker for TextureCooker {
    fn name(&self) -> &'static str {
        "texture-cooker"
    }

    fn can_cook(&self, asset_type: AssetType) -> bool {
        matches!(asset_type, AssetType::Texture)
    }

    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError> {
        let (w, h, rgba) = Self::parse_intermediate(ctx.imported_data).ok_or_else(|| {
            CookError::CookFailed("Invalid texture intermediate: missing RTXI header".into())
        })?;

        if w == 0 || h == 0 {
            return Err(CookError::CookFailed("Zero-dimension texture".into()));
        }

        // 每资产导入设置（`<asset>.import.json`）可以覆盖 profile 的 max_size / mips：
        // 先把最长边压到 max_size 以内，再按需生成 mip 链。
        let (w, h, rgba) = Self::clamp_max_size(w, h, rgba, ctx.settings.texture.max_size);
        let mips_rgba = if ctx.settings.texture.generate_mips {
            Self::generate_mips(w, h, &rgba)
        } else {
            vec![(w, h, rgba.to_vec())]
        };

        // 格式 byte：只有 RGBA8 有编码器。块压缩（BC / ASTC / ETC2）还没实现，
        // 所以 profile 里声明的桌面 / 移动格式仍然回退 —— 但**运行时**已经能读
        // BC1 / BC3 / BC5 / BC6H（#519），编码器接上就能直接用。
        let (format, mips, block_compressed) = match ctx.settings.texture.compression {
            profile::TextureCompression::Rgba8 | profile::TextureCompression::None => {
                (RTEX_FORMAT_RGBA8, mips_rgba, false)
            }
            other => {
                tracing::warn!(
                    "Compression format {other:?} not implemented yet, falling back to RGBA8"
                );
                (RTEX_FORMAT_RGBA8, mips_rgba, false)
            }
        };

        let cooked_data = Self::write_rtex(&mips, format);

        // 块压缩的 mip 数据已经够紧凑，再套一层 zstd 收益很小；未压缩的 RGBA8
        // 像素很好压（一般能省 30~50% 体积），所以让它进 pak 的压缩流程（#519）。
        Ok(CookResult {
            cooked_data,
            compress: !block_compressed,
        })
    }
}

// ---------------------------------------------------------------------------
// RTEX decoder — cooked 运行时 纹理 → structured mip data
// ---------------------------------------------------------------------------

/// Parsed RTEX cooked 纹理 data.
#[derive(Debug, Clone)]
pub struct RtexInfo {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub format: u8,
    pub mip_data: Vec<Vec<u8>>,
}

/// 解码 a cooked RTEX blob 后 into structured mip data.
///
/// This is the inverse of [`TextureCooker::write_rtex`]. Returns `None` if
/// the data is malformed.
pub fn decode_rtex(data: &[u8]) -> Option<RtexInfo> {
    if data.len() < 18 || &data[..4] != RTEX_MAGIC {
        return None;
    }
    if data[4] != 1 {
        return None; // unknown version
    }
    let width = u32::from_le_bytes(data[5..9].try_into().ok()?);
    let height = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let mip_levels = u32::from_le_bytes(data[13..17].try_into().ok()?);
    let format = data[17];
    if mip_levels == 0 || mip_levels > MAX_MIP_LEVELS {
        return None;
    }

    let offset_table_start = 18usize;
    let offset_table_size = mip_levels as usize * 4;
    let header_size = offset_table_start + offset_table_size;
    if data.len() < header_size {
        return None;
    }

    // 读取 偏移 表
    let mut offsets = Vec::with_capacity(mip_levels as usize);
    for i in 0..mip_levels as usize {
        let off = u32::from_le_bytes(data[offset_table_start + i * 4..][..4].try_into().ok()?);
        offsets.push(off as usize);
    }

    // Extract each mip's data.
    let mut mip_data = Vec::with_capacity(mip_levels as usize);
    for i in 0..mip_levels as usize {
        let start = offsets[i];
        let end = if i + 1 < mip_levels as usize {
            offsets[i + 1]
        } else {
            data.len()
        };
        if start >= end || end > data.len() {
            return None;
        }
        mip_data.push(data[start..end].to_vec());
    }

    Some(RtexInfo {
        width,
        height,
        mip_levels,
        format,
        mip_data,
    })
}

/// Parse an RTXI intermediate blob and return the raw RGBA8 像素 data (mip 0).
///
/// Returns 宽度 高度 pixels)` where `pixels` is tightly-packed RGBA8.
pub fn parse_rtexi_pixels(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    TextureCooker::parse_intermediate(data).map(|(w, h, px)| (w, h, px.to_vec()))
}

// ---------------------------------------------------------------------------
// 网格 Cooker — validates RMXI intermediate → serialises RMES 运行时 格式
// ---------------------------------------------------------------------------

/// RMES cooked 网格 magic.
const RMES_MAGIC: &[u8; 4] = b"RMES";
/// RMXI intermediate 网格 magic.
const RMXI_MAGIC: &[u8; 4] = b"RMXI";

/// 从 RMXI 头/长度推导出的布局。
struct RmxiLayout {
    version: u8,
    vert_count: u32,
    idx_count: u32,
    uv_channels: u32,
    has_normals: bool,
    vert_data_start: usize,
    vert_data_size: usize,
    idx_data_size: usize,
    submeshes: Vec<prism_asset::formats::RmesSubmesh>,
}

/// 读取 RMXI v2 的 submesh 表。
fn read_submesh_table(
    data: &[u8],
    offset: usize,
    idx_count: u32,
) -> Option<Vec<prism_asset::formats::RmesSubmesh>> {
    let count = u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?) as usize;
    let table_start = offset.checked_add(4)?;
    let table_end = table_start.checked_add(count.checked_mul(12)?)?;
    if table_end != data.len() {
        return None;
    }
    let mut submeshes = Vec::with_capacity(count);
    for i in 0..count {
        let base = table_start + i * 12;
        let index_start = u32::from_le_bytes(data[base..base + 4].try_into().ok()?);
        let index_count = u32::from_le_bytes(data[base + 4..base + 8].try_into().ok()?);
        let material_index = u32::from_le_bytes(data[base + 8..base + 12].try_into().ok()?);
        let end = index_start.checked_add(index_count)?;
        if index_count == 0 || end > idx_count {
            return None;
        }
        submeshes.push(prism_asset::formats::RmesSubmesh {
            index_start,
            index_count,
            material_index: (material_index != u32::MAX).then_some(material_index),
        });
    }
    Some(submeshes)
}

/// 解析 RMXI 头并确定 `[positions][normals?][uv0][indices][submeshes?]` 布局。
///
/// RMXI v1 没有 submesh 表；v2 在 index block 后追加
/// `[count:u32][index_start:u32][index_count:u32][material_index:u32]...`。
/// 没有显式的 `has_normals` 位，所以 v2 用「候选 base + 表长度 == 文件长度」
/// 来判定，避免把 submesh 表误当成法线数据。
fn rmxi_layout(data: &[u8]) -> Option<RmxiLayout> {
    if data.len() < 17 || &data[..4] != RMXI_MAGIC {
        return None;
    }
    let version = data[4];
    if version != 1 && version != 2 {
        return None;
    }
    let vert_count = u32::from_le_bytes(data[5..9].try_into().ok()?);
    let idx_count = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let uv_channels = u32::from_le_bytes(data[13..17].try_into().ok()?);

    let pos_size = (vert_count as usize).checked_mul(3 * 4)?;
    let nrm_size = (vert_count as usize).checked_mul(3 * 4)?;
    let uv_size = (uv_channels as usize)
        .checked_mul(vert_count as usize)?
        .checked_mul(2 * 4)?;
    let idx_size = (idx_count as usize).checked_mul(4)?;
    let base_without_normals = 17usize
        .checked_add(pos_size)?
        .checked_add(uv_size)?
        .checked_add(idx_size)?;
    let base_with_normals = base_without_normals.checked_add(nrm_size)?;

    let (has_normals, submeshes) = if version >= 2 {
        let mut found = None;
        for has_normals in [false, true] {
            let base = if has_normals {
                base_with_normals
            } else {
                base_without_normals
            };
            if data.len() < base + 4 {
                continue;
            }
            if let Some(submeshes) = read_submesh_table(data, base, idx_count) {
                found = Some((has_normals, submeshes));
                break;
            }
        }
        found?
    } else if data.len() == base_without_normals {
        (false, Vec::new())
    } else if data.len() == base_with_normals {
        (true, Vec::new())
    } else {
        return None;
    };

    let nrm_floats = if has_normals { 3 } else { 0 };
    let stride_floats = 3 + nrm_floats + uv_channels as usize * 2;
    let vert_data_size = (vert_count as usize).checked_mul(stride_floats * 4)?;

    Some(RmxiLayout {
        version,
        vert_count,
        idx_count,
        uv_channels,
        has_normals,
        vert_data_start: 17,
        vert_data_size,
        idx_data_size: idx_size,
        submeshes,
    })
}

/// Cooks 网格 data by validating the intermediate 格式 and packing into a
/// runtime-ready 二进制
///
/// ```text
/// [rmes_magic:4][version:1][vert_count:4][idx_count:4][uv_count:4]
/// [stride:4][positions_offset:4][normals_offset:4][uv0_offset:4]
/// [vertex_data][index_data]
/// [submesh_count:4][index_start:4][index_count:4][material_index:4]...
/// ```
pub struct MeshCooker;

impl MeshCooker {
    fn parse_intermediate(data: &[u8]) -> Option<(u32, u32, u32, u32)> {
        let layout = rmxi_layout(data)?;
        Some((
            layout.vert_count,
            layout.idx_count,
            layout.uv_channels,
            layout.version as u32,
        ))
    }

    fn write_rmes(intermediate: &[u8]) -> Option<Vec<u8>> {
        let layout = rmxi_layout(intermediate)?;
        let vert_count = layout.vert_count;
        let idx_count = layout.idx_count;
        let uv_channels = layout.uv_channels;

        let nrm_floats: u32 = if layout.has_normals { 3 } else { 0 };
        let stride = 3 + nrm_floats + uv_channels * 2; // floats per vertex
        let header_size = 33usize;
        let vert_data_size = layout.vert_data_size;
        let idx_data_size = layout.idx_data_size;

        let mut buf = Vec::with_capacity(
            header_size + vert_data_size + idx_data_size + 4 + layout.submeshes.len() * 12,
        );

        buf.extend_from_slice(RMES_MAGIC);
        buf.push(RMES_VERSION);
        buf.extend_from_slice(&vert_count.to_le_bytes());
        buf.extend_from_slice(&idx_count.to_le_bytes());
        buf.extend_from_slice(&uv_channels.to_le_bytes());
        buf.extend_from_slice(&(stride * 4).to_le_bytes()); // stride in bytes

        let pos_off = header_size as u32;
        let nrm_off = if layout.has_normals {
            pos_off + vert_count * 3 * 4
        } else {
            0
        };
        let uv0_off = if uv_channels > 0 {
            if layout.has_normals {
                nrm_off + vert_count * 3 * 4
            } else {
                pos_off + vert_count * 3 * 4
            }
        } else {
            0
        };

        buf.extend_from_slice(&pos_off.to_le_bytes());
        buf.extend_from_slice(&nrm_off.to_le_bytes());
        buf.extend_from_slice(&uv0_off.to_le_bytes());

        let vert_start = layout.vert_data_start;
        let vert_end = vert_start.checked_add(vert_data_size)?;
        let idx_end = vert_end.checked_add(idx_data_size)?;
        if idx_end > intermediate.len() {
            return None;
        }
        buf.extend_from_slice(&intermediate[vert_start..vert_end]);
        buf.extend_from_slice(&intermediate[vert_end..idx_end]);

        buf.extend_from_slice(&(layout.submeshes.len() as u32).to_le_bytes());
        for submesh in &layout.submeshes {
            buf.extend_from_slice(&submesh.index_start.to_le_bytes());
            buf.extend_from_slice(&submesh.index_count.to_le_bytes());
            buf.extend_from_slice(&submesh.material_index.unwrap_or(u32::MAX).to_le_bytes());
        }

        Some(buf)
    }
}

impl Cooker for MeshCooker {
    fn name(&self) -> &'static str {
        "mesh-cooker"
    }

    fn can_cook(&self, asset_type: AssetType) -> bool {
        matches!(asset_type, AssetType::Mesh)
    }

    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError> {
        let (vert_count, idx_count, _uv_channels, _ver) =
            Self::parse_intermediate(ctx.imported_data).ok_or_else(|| {
                CookError::CookFailed("Invalid mesh intermediate: missing RMXI header".into())
            })?;

        if vert_count == 0 || idx_count == 0 {
            return Err(CookError::CookFailed(
                "Empty mesh (no vertices or indices)".into(),
            ));
        }

        let cooked_data = Self::write_rmes(ctx.imported_data).ok_or_else(|| {
            CookError::CookFailed("Invalid mesh intermediate: truncated data".into())
        })?;

        Ok(CookResult {
            cooked_data,
            compress: true,
        })
    }
}

// ---------------------------------------------------------------------------
// RMES decoder — cooked 运行时 网格 → structured vertex/index data
// ---------------------------------------------------------------------------

/// Parsed RMES cooked 网格 data.
#[derive(Debug, Clone)]
pub struct RmesInfo {
    pub vert_count: u32,
    pub idx_count: u32,
    pub uv_channels: u32,
    pub stride_bytes: u32,
    pub vertex_data: Vec<u8>,
    pub index_data: Vec<u8>,
    /// RMES v2 submesh table（v1 为空）。
    pub submeshes: Vec<prism_asset::formats::RmesSubmesh>,
}

/// 解码 a cooked RMES blob 后 into structured vertex/index data.
///
/// This is the inverse of [`MeshCooker::write_rmes`]. Returns `None` if the
/// data is malformed. v1（没有 submesh 表）仍然可读。
pub fn decode_rmes(data: &[u8]) -> Option<RmesInfo> {
    if data.len() < 33 || &data[..4] != RMES_MAGIC {
        return None;
    }
    let version = data[4];
    if version != 1 && version != RMES_VERSION {
        return None; // unknown version
    }
    let vert_count = u32::from_le_bytes(data[5..9].try_into().ok()?);
    let idx_count = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let uv_channels = u32::from_le_bytes(data[13..17].try_into().ok()?);
    let stride_bytes = u32::from_le_bytes(data[17..21].try_into().ok()?) as usize;
    let _pos_offset = u32::from_le_bytes(data[21..25].try_into().ok()?);
    let _nrm_offset = u32::from_le_bytes(data[25..29].try_into().ok()?);
    let _uv0_offset = u32::from_le_bytes(data[29..33].try_into().ok()?);

    let vert_data_size = (vert_count as usize).checked_mul(stride_bytes)?;
    let idx_data_size = (idx_count as usize).checked_mul(4)?;
    let header_size = 33usize;
    let index_end = header_size
        .checked_add(vert_data_size)?
        .checked_add(idx_data_size)?;
    if index_end > data.len() {
        return None;
    }

    let vertex_data = data[header_size..header_size + vert_data_size].to_vec();
    let index_data = data[header_size + vert_data_size..index_end].to_vec();
    let submeshes = if version >= 2 {
        read_submesh_table(data, index_end, idx_count)?
    } else {
        Vec::new()
    };

    Some(RmesInfo {
        vert_count,
        idx_count,
        uv_channels,
        stride_bytes: stride_bytes as u32,
        vertex_data,
        index_data,
        submeshes,
    })
}

/// Parse an RMXI intermediate header and return vertex/index metadata plus
/// the raw vertex-data and index-data slices.
///
/// Returns `(vert_count, idx_count, uv_channels, vertex_bytes, index_bytes)`.
pub type RmxiInfo = (u32, u32, u32, Vec<u8>, Vec<u8>);

pub fn parse_rmxi_info(data: &[u8]) -> Option<RmxiInfo> {
    let layout = rmxi_layout(data)?;
    let vert_end = layout.vert_data_start.checked_add(layout.vert_data_size)?;
    let idx_end = vert_end.checked_add(layout.idx_data_size)?;
    if idx_end > data.len() {
        return None;
    }
    let vert_data = data[layout.vert_data_start..vert_end].to_vec();
    let idx_data = data[vert_end..idx_end].to_vec();

    Some((
        layout.vert_count,
        layout.idx_count,
        layout.uv_channels,
        vert_data,
        idx_data,
    ))
}

// ---------------------------------------------------------------------------
// 材质 Cooker (RMATI intermediate -> RMAT 运行时 格式
// ---------------------------------------------------------------------------

/// RMATI intermediate 材质 magic (from the importer). 5 字节 so it is
/// 不同 from the 4-byte RMAT 运行时 magic.
const RMATI_MAGIC: &[u8; 5] = b"RMATI";
/// RMAT 运行时 材质 magic (cooked, packed into .pak).
pub const RMAT_MAGIC: &[u8; 4] = b"RMAT";

/// Number of 标量 floats in the 材质 header (18 floats = 72 字节
///
/// 布局 base_color[4], metallic, roughness, emissive[3], emissive_strength,
/// normal_scale, occlusion_strength, transmission, ior, translucency,
/// 各向异性 clearcoat, clearcoat_roughness.
pub const MATERIAL_SCALAR_COUNT: usize = 18;
const MATERIAL_SCALAR_SIZE: usize = MATERIAL_SCALAR_COUNT * 4;

/// Cooks 材质 data by translating the RMATI intermediate 纹理 paths)
/// into the RMAT 运行时 格式 纹理 `AssetId` dependencies).
///
/// 运行时 格式 (all little-endian):
/// ```text
/// [magic:4]       b"RMAT"
/// [version:1]     1
/// [scalars] 18 f32 LE (72 字节 - same 布局 as RMATI
/// per 槽 (5x):
///   [present:1]   0 or 1
/// [if present] [asset_id:u64 LE] - 纹理 AssetId
/// ```
pub struct MaterialCooker;

impl MaterialCooker {
    /// Parse the RMATI header + scalars + 5 纹理 path records.
    ///
    /// Returns `(scalars: [f32; 18], tex_paths: [Option<String>; 5])` or `None`
    /// on malformed 输入
    fn parse_intermediate(
        data: &[u8],
    ) -> Option<([f32; MATERIAL_SCALAR_COUNT], [Option<String>; 5])> {
        // Header: magic(5) + version(1) + scalars(72) = 78 字节 最小
        const MAGIC_LEN: usize = 5;
        if data.len() < MAGIC_LEN + 1 + MATERIAL_SCALAR_SIZE || &data[..MAGIC_LEN] != RMATI_MAGIC {
            return None;
        }
        let _version = data[MAGIC_LEN];
        let mut scalars = [0f32; MATERIAL_SCALAR_COUNT];
        for (i, scalar) in scalars.iter_mut().enumerate() {
            let off = MAGIC_LEN + 1 + i * 4;
            *scalar = f32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        }

        let mut tex_paths: [Option<String>; 5] = [None, None, None, None, None];
        let mut pos = MAGIC_LEN + 1 + MATERIAL_SCALAR_SIZE;
        for slot in tex_paths.iter_mut() {
            if pos >= data.len() {
                return None;
            }
            let present = data[pos];
            pos += 1;
            if present == 1 {
                if pos + 2 > data.len() {
                    return None;
                }
                let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                if pos + len > data.len() {
                    return None;
                }
                let s = std::str::from_utf8(&data[pos..pos + len]).ok()?.to_owned();
                pos += len;
                *slot = Some(s);
            }
        }

        Some((scalars, tex_paths))
    }

    fn write_rmat(
        scalars: &[f32; MATERIAL_SCALAR_COUNT],
        tex_ids: &[Option<AssetId>; 5],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(5 + MATERIAL_SCALAR_SIZE + 5 * 9);
        buf.extend_from_slice(RMAT_MAGIC);
        buf.push(1); // version
        for s in scalars {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        for id in tex_ids {
            match id {
                Some(id) => {
                    buf.push(1);
                    buf.extend_from_slice(&id.into_raw().to_le_bytes());
                }
                None => buf.push(0),
            }
        }
        buf
    }
}

impl Cooker for MaterialCooker {
    fn name(&self) -> &'static str {
        "material-cooker"
    }

    fn can_cook(&self, asset_type: AssetType) -> bool {
        matches!(asset_type, AssetType::Material)
    }

    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError> {
        let (scalars, tex_paths) =
            Self::parse_intermediate(ctx.imported_data).ok_or_else(|| {
                CookError::CookFailed("Invalid material intermediate: missing RMATI header".into())
            })?;

        // The importer already resolved 纹理 paths to AssetId dependencies
        // stored on the record. We walk the path records in 槽 order and
        // look each 上 in the dependency 列表 by matching path -> id via db.
        // However, dependencies is just Vec<AssetId> without path 信息 so we
        // re-resolve paths against the database here (the db is not on
        // CookContext). Instead, we rely on record.dependencies preserving the
        // 槽 order from the importer. 映射表 them positionally.
        let deps = &ctx.record.dependencies;
        let mut tex_ids: [Option<AssetId>; 5] = [None, None, None, None, None];
        let mut dep_idx = 0;
        for (i, path_opt) in tex_paths.iter().enumerate() {
            if path_opt.is_some() {
                if dep_idx < deps.len() {
                    tex_ids[i] = Some(deps[dep_idx]);
                    dep_idx += 1;
                } else {
                    tracing::warn!(
                        "material cooker: slot {} expected a dependency but record has only {} (path='{}'); leaving empty",
                        i,
                        deps.len(),
                        path_opt.as_deref().unwrap_or("")
                    );
                }
            }
        }

        let cooked_data = Self::write_rmat(&scalars, &tex_ids);

        Ok(CookResult {
            cooked_data,
            compress: true,
        })
    }
}

// ---------------------------------------------------------------------------
// RMAT decoder - cooked 运行时 材质 -> structured data
// ---------------------------------------------------------------------------

/// Parsed RMAT cooked 材质 data.
#[derive(Debug, Clone)]
pub struct RmatInfo {
    /// 18 标量 floats in 槽 order (see [`MATERIAL_SCALAR_COUNT`] docs).
    pub scalars: [f32; MATERIAL_SCALAR_COUNT],
    /// 5 纹理 slots, each `Some(AssetId)` or `None`.
    pub texture_ids: [Option<AssetId>; 5],
}

/// 解码 a cooked RMAT blob 后 into structured 材质 data.
///
/// This is the inverse of [`MaterialCooker::write_rmat`]. Returns `None` if the
/// data is malformed.
pub fn decode_rmat(data: &[u8]) -> Option<RmatInfo> {
    let header = 5 + MATERIAL_SCALAR_SIZE;
    if data.len() < header || &data[..4] != RMAT_MAGIC {
        return None;
    }
    if data[4] != 1 {
        return None; // unknown version
    }
    let mut scalars = [0f32; MATERIAL_SCALAR_COUNT];
    for (i, scalar) in scalars.iter_mut().enumerate() {
        let off = 5 + i * 4;
        *scalar = f32::from_le_bytes(data[off..off + 4].try_into().ok()?);
    }

    let mut texture_ids: [Option<AssetId>; 5] = [None, None, None, None, None];
    let mut pos = header;
    for slot in texture_ids.iter_mut() {
        if pos >= data.len() {
            return None;
        }
        let present = data[pos];
        pos += 1;
        if present == 1 {
            if pos + 8 > data.len() {
                return None;
            }
            let raw = u64::from_le_bytes(data[pos..pos + 8].try_into().ok()?);
            pos += 8;
            *slot = Some(AssetId::from_raw(raw));
        }
    }

    Some(RmatInfo {
        scalars,
        texture_ids,
    })
}

// ---------------------------------------------------------------------------
// 着色器 Cooker (RSLI intermediate -> SPIR-V via slangc)
// ---------------------------------------------------------------------------

/// RSLI intermediate 着色器 magic (from the importer).
const RSLI_MAGIC: &[u8; 4] = b"RSLI";
/// SPIR-V magic number (little-endian 第一个 word). Used to sanity-check the
/// compiler 输出
const SPIRV_MAGIC_LE: u32 = 0x0723_0203;

/// Parsed RSLI intermediate header + 源
#[derive(Debug, Clone)]
struct RsliInfo {
    entry: String,
    stage: String,
    profile: String,
    source: Vec<u8>,
}

/// Cooks Slang 着色器 sources into SPIR-V by invoking `slangc` at 烹饪
/// 时间
///
/// The cooker receives the RSLI intermediate (entry / 阶段 / 配置 +
/// 源 字节 produced by [`ShaderImporter`]. It writes the 源 to a
/// temporary file (so `#include` 分辨率 works), invokes `slangc`, and
/// returns the raw SPIR-V 字节 as the cooked data.
///
/// `slangc` is located via the `SLANGC` env var, falling 后 to `slangc` on
/// `PATH`. 编译 flags mirror `shaders/compile.sh`:
/// `-profile 配置 -target SPIR-V -entry <entry> -stage 阶段
///    -fvk-use-entrypoint-name -o <out.spv>`
///
/// The cooked data is the raw SPIR-V bytecode (no 包装器 - the 运行时
/// loads it directly via `vkCreateShaderModule`.
pub struct ShaderCooker;

impl ShaderCooker {
    fn parse_intermediate(data: &[u8]) -> Option<RsliInfo> {
        // Header: magic(4) + version(1) = 5 字节 最小
        if data.len() < 5 || &data[..4] != RSLI_MAGIC {
            return None;
        }
        let _version = data[4];
        let mut pos = 5;

        let read_str = |buf: &[u8], pos: &mut usize| -> Option<String> {
            if *pos + 2 > buf.len() {
                return None;
            }
            let len = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]) as usize;
            *pos += 2;
            if *pos + len > buf.len() {
                return None;
            }
            let s = std::str::from_utf8(&buf[*pos..*pos + len]).ok()?.to_owned();
            *pos += len;
            Some(s)
        };

        let entry = read_str(data, &mut pos)?;
        let stage = read_str(data, &mut pos)?;
        let profile = read_str(data, &mut pos)?;

        if pos + 4 > data.len() {
            return None;
        }
        let source_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + source_len > data.len() {
            return None;
        }
        let source = data[pos..pos + source_len].to_vec();

        Some(RsliInfo {
            entry,
            stage,
            profile,
            source,
        })
    }

    /// 解析 the slangc 二进制 path: `SLANGC` env var, else `slangc` on PATH.
    fn slangc_path() -> String {
        std::env::var("SLANGC").unwrap_or_else(|_| "slangc".to_owned())
    }

    /// 调用 slangc on 源 written to a temp file, returning the SPIR-V
    /// 字节 on 成功
    fn compile(rsli: &RsliInfo) -> Result<Vec<u8>, CookError> {
        // 写入 源 to a temp file so #include / 模块 分辨率 works.
        let tmp_dir = std::env::temp_dir();
        let source_path = tmp_dir.join(format!("prisma_shader_cook_{}.slang", std::process::id()));
        let out_path = tmp_dir.join(format!("prisma_shader_cook_{}.spv", std::process::id()));

        // RAII guard: removes temp files on 放置 成功 or 错误
        struct TempGuard {
            paths: Vec<std::path::PathBuf>,
        }
        impl Drop for TempGuard {
            fn drop(&mut self) {
                for p in &self.paths {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        let _guard = TempGuard {
            paths: vec![source_path.clone(), out_path.clone()],
        };

        std::fs::write(&source_path, &rsli.source)
            .map_err(|e| CookError::CookFailed(format!("write temp shader source: {e}")))?;

        let slangc = Self::slangc_path();
        let output = std::process::Command::new(&slangc)
            .arg(&source_path)
            .arg("-profile")
            .arg(&rsli.profile)
            .arg("-target")
            .arg("spirv")
            .arg("-entry")
            .arg(&rsli.entry)
            .arg("-stage")
            .arg(&rsli.stage)
            .arg("-fvk-use-entrypoint-name")
            .arg("-o")
            .arg(&out_path)
            .output()
            .map_err(|e| {
                CookError::CookFailed(format!(
                    "failed to invoke slangc ({slangc}): {e}. \
                     Set SLANGC env var or ensure slangc is on PATH."
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CookError::CookFailed(format!(
                "slangc failed (entry='{}', stage='{}', profile='{}'): {stderr}",
                rsli.entry, rsli.stage, rsli.profile
            )));
        }

        let spv = std::fs::read(&out_path).map_err(|e| {
            CookError::CookFailed(format!(
                "slangc succeeded but output .spv missing at {}: {e}",
                out_path.display()
            ))
        })?;

        // Sanity check: SPIR-V starts with the magic 0x07230203 (LE).
        if spv.len() < 4 {
            return Err(CookError::CookFailed(
                "slangc produced empty/short SPIR-V".into(),
            ));
        }
        let magic = u32::from_le_bytes([spv[0], spv[1], spv[2], spv[3]]);
        if magic != SPIRV_MAGIC_LE {
            return Err(CookError::CookFailed(format!(
                "slangc output is not valid SPIR-V (magic={:#010x}, expected {:#010x})",
                magic, SPIRV_MAGIC_LE
            )));
        }

        Ok(spv)
    }
}

impl Cooker for ShaderCooker {
    fn name(&self) -> &'static str {
        "shader-cooker"
    }

    fn can_cook(&self, asset_type: AssetType) -> bool {
        matches!(asset_type, AssetType::Shader)
    }

    fn cook(&self, ctx: &CookContext) -> Result<CookResult, CookError> {
        let rsli = Self::parse_intermediate(ctx.imported_data).ok_or_else(|| {
            CookError::CookFailed("Invalid shader intermediate: missing RSLI header".into())
        })?;

        let spv = Self::compile(&rsli)?;
        tracing::info!(
            "shader-cooker: compiled {} (entry='{}', stage='{}', profile='{}') -> {} bytes SPIR-V",
            ctx.record.path,
            rsli.entry,
            rsli.stage,
            rsli.profile,
            spv.len()
        );

        Ok(CookResult {
            cooked_data: spv,
            // SPIR-V doesn't 压缩 well; skip zstd 开销
            compress: false,
        })
    }
}

// ---------------------------------------------------------------------------
// 默认 Registry
// ---------------------------------------------------------------------------

/// 构建 the 默认 cooker registry with all built-in cookers.
pub fn default_cooker_registry() -> CookerRegistry {
    let mut reg = CookerRegistry::new();
    reg.register(Box::new(BinaryCooker));
    reg.register(Box::new(TextureCooker));
    reg.register(Box::new(MeshCooker));
    reg.register(Box::new(MaterialCooker));
    reg.register(Box::new(ShaderCooker));
    reg.register(Box::new(scene::SceneCooker::default()));
    reg
}

// ---------------------------------------------------------------------------
// 烹饪 管线
// ===========================================================================

/// High-level cooking 管线 that processes all assets through cookers and
/// builds a .pak 包
pub struct CookPipeline {
    registry: CookerRegistry,
}

impl CookPipeline {
    pub fn new(registry: CookerRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &CookerRegistry {
        &self.registry
    }

    /// 烹饪 all assets from a database and 构建 a .pak file.
    ///
    /// `asset_data` is a 映射表 from AssetId to the raw imported 字节
    /// The 烹饪 管线 handles topological sorting of dependencies.
    pub fn cook_all(
        &self,
        db: &crate::db::AssetDatabase,
        asset_data: &HashMap<AssetId, Vec<u8>>,
        builder: &mut PackageBuilder,
        settings: &profile::CookSettings,
    ) -> Result<CookSummary, CookError> {
        let mut summary = CookSummary::default();

        // Collect records in dependency order (topological 排序
        let order = topological_sort(db);

        for &id in &order {
            let record = db
                .get(id)
                .ok_or_else(|| CookError::CookFailed(format!("Record not found: {id}")))?;

            let data = match asset_data.get(&id) {
                Some(d) => d,
                None => {
                    tracing::warn!("  ! no data for {id}");
                    summary.skipped += 1;
                    continue;
                }
            };

            let cooker = match self.registry.find_for_type(record.asset_type) {
                Some(c) => c,
                None => {
                    tracing::warn!("  ! no cooker for {:?}", record.asset_type);
                    summary.skipped += 1;
                    continue;
                }
            };

            let ctx = CookContext {
                record,
                imported_data: data,
                settings,
            };
            let result = cooker.cook(&ctx)?;

            let deps: Vec<AssetId> = record.dependencies.clone();
            if settings.should_stream(result.cooked_data.len()) {
                builder.add_streamed_asset(id, record.asset_type, result.cooked_data, &deps);
                summary.streamed += 1;
            } else {
                builder.add_asset(id, record.asset_type, result.cooked_data, &deps);
            }
            summary.cooked += 1;
        }

        Ok(summary)
    }
}

/// 摘要 of a cooking run.
#[derive(Debug, Default, Clone)]
pub struct CookSummary {
    pub cooked: u32,
    pub skipped: u32,
    /// Assets marked `FLAG_STREAMED` by the active profile.
    pub streamed: u32,
}

// ===========================================================================
// Topological 排序
// ===========================================================================

/// 计算 a topological ordering of assets based on their dependencies.
///
/// Returns 资源 IDs in an order where all dependencies appear before their
/// dependents. Cycles are broken by emitting a 警告 and still including
/// the 资源 (the cycle participants are appended at the 结束
pub fn topological_sort(db: &crate::db::AssetDatabase) -> Vec<AssetId> {
    let all_ids: Vec<AssetId> = db.records().map(|r| r.id).collect();
    if all_ids.is_empty() {
        return Vec::new();
    }

    // DFS-based topological 排序 with cycle detection.
    let mut visited = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(all_ids.len());
    let mut temp_mark = std::collections::HashSet::new();

    fn visit(
        id: AssetId,
        db: &crate::db::AssetDatabase,
        visited: &mut std::collections::HashSet<AssetId>,
        temp_mark: &mut std::collections::HashSet<AssetId>,
        result: &mut Vec<AssetId>,
    ) {
        if visited.contains(&id) {
            return;
        }
        if temp_mark.contains(&id) {
            tracing::warn!("Cycle detected involving asset {id}, breaking dependency");
            return;
        }
        temp_mark.insert(id);

        if let Some(record) = db.get(id) {
            for dep in &record.dependencies {
                visit(*dep, db, visited, temp_mark, result);
            }
        }

        temp_mark.remove(&id);
        visited.insert(id);
        result.push(id);
    }

    for &id in &all_ids {
        if !visited.contains(&id) {
            visit(id, db, &mut visited, &mut temp_mark, &mut result);
        }
    }

    result
}

#[cfg(test)]
mod tests;
