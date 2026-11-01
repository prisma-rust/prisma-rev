//! 运行时二进制格式定义与解码器。
//! 此模块只依赖 core，供 runtime 与 cooker 共享，避免运行时反向依赖烘焙器。

use crate::core::AssetId;

const RTEX_MAGIC: &[u8; 4] = b"RTEX";
const RMES_MAGIC: &[u8; 4] = b"RMES";
const MAX_MIP_LEVELS: u32 = 16;

// ---------------------------------------------------------------------------
// RTEX `format` byte values
// ---------------------------------------------------------------------------
//
// The cooker writes one of these into the header; runtime consumers use them to
// interpret the mip payload. RTEX does **not** encode colour space: an RGBA8
// payload can be sRGB (albedo/emissive) or linear (normal/MR/AO), so the caller
// picks sRGB-ness per texture slot.

/// Uncompressed RGBA8 (4 bytes per pixel).
pub const RTEX_FORMAT_RGBA8: u8 = 0;
/// BC5 (two-channel RG normal map, 16 bytes per 4×4 block).
pub const RTEX_FORMAT_BC5: u8 = 2;
/// BC1 / DXT1 (RGB, optional 1-bit alpha, 8 bytes per 4×4 block).
pub const RTEX_FORMAT_BC1: u8 = 3;
/// BC3 / DXT5 (RGBA, 16 bytes per 4×4 block).
pub const RTEX_FORMAT_BC3: u8 = 4;
/// BC6H (HDR RGB, 16 bytes per 4×4 block).
pub const RTEX_FORMAT_BC6H: u8 = 5;

#[derive(Debug, Clone)]
pub struct RtexInfo {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub format: u8,
    pub mip_data: Vec<Vec<u8>>,
}

pub fn decode_rtex(data: &[u8]) -> Option<RtexInfo> {
    if data.len() < 18 || &data[..4] != RTEX_MAGIC || data[4] != 1 {
        return None;
    }
    let width = u32::from_le_bytes(data[5..9].try_into().ok()?);
    let height = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let mip_levels = u32::from_le_bytes(data[13..17].try_into().ok()?);
    if mip_levels == 0 || mip_levels > MAX_MIP_LEVELS {
        return None;
    }
    let format = data[17];
    let table_end = 18usize.checked_add(mip_levels as usize * 4)?;
    if data.len() < table_end {
        return None;
    }
    let mut offsets = Vec::with_capacity(mip_levels as usize);
    for i in 0..mip_levels as usize {
        offsets.push(u32::from_le_bytes(data[18 + i * 4..22 + i * 4].try_into().ok()?) as usize);
    }
    let mut mip_data = Vec::with_capacity(offsets.len());
    for (i, &start) in offsets.iter().enumerate() {
        let end = offsets.get(i + 1).copied().unwrap_or(data.len());
        if start < table_end || start >= end || end > data.len() {
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

#[derive(Debug, Clone)]
pub struct RmesInfo {
    pub vert_count: u32,
    pub idx_count: u32,
    pub uv_channels: u32,
    pub stride_bytes: u32,
    pub vertex_data: Vec<u8>,
    pub index_data: Vec<u8>,
    /// RMES v2 submesh table（v1 为空）。
    pub submeshes: Vec<RmesSubmesh>,
}

/// RMES v2 的一个 submesh：索引区间 + 源文件材质索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RmesSubmesh {
    pub index_start: u32,
    pub index_count: u32,
    /// glTF `material.index()`；`None` 表示源文件没有材质。
    pub material_index: Option<u32>,
}

/// RMES 当前写出的版本（v1 仍可读；v2 在 index block 后追加 submesh 表）。
pub const RMES_VERSION: u8 = 2;

/// Parse the trailing RMES v2 submesh table.
fn decode_rmes_submeshes(data: &[u8], offset: usize, idx_count: u32) -> Option<Vec<RmesSubmesh>> {
    let count = u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?) as usize;
    let table_start = offset.checked_add(4)?;
    let table_end = table_start.checked_add(count.checked_mul(12)?)?;
    if table_end > data.len() {
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
        submeshes.push(RmesSubmesh {
            index_start,
            index_count,
            material_index: (material_index != u32::MAX).then_some(material_index),
        });
    }
    if table_end != data.len() {
        return None;
    }
    Some(submeshes)
}

pub fn decode_rmes(data: &[u8]) -> Option<RmesInfo> {
    if data.len() < 33 || &data[..4] != RMES_MAGIC {
        return None;
    }
    let version = data[4];
    if version != 1 && version != RMES_VERSION {
        return None;
    }
    let vert_count = u32::from_le_bytes(data[5..9].try_into().ok()?);
    let idx_count = u32::from_le_bytes(data[9..13].try_into().ok()?);
    let uv_channels = u32::from_le_bytes(data[13..17].try_into().ok()?);
    let stride_bytes = u32::from_le_bytes(data[17..21].try_into().ok()?);
    let vertex_size = (vert_count as usize).checked_mul(stride_bytes as usize)?;
    let index_size = (idx_count as usize).checked_mul(4)?;
    let vertex_start = 33usize;
    let index_start = vertex_start.checked_add(vertex_size)?;
    let end = index_start.checked_add(index_size)?;
    if end > data.len() {
        return None;
    }
    let submeshes = if version >= 2 {
        decode_rmes_submeshes(data, end, idx_count)?
    } else {
        Vec::new()
    };
    Some(RmesInfo {
        vert_count,
        idx_count,
        uv_channels,
        stride_bytes,
        vertex_data: data[vertex_start..index_start].to_vec(),
        index_data: data[index_start..end].to_vec(),
        submeshes,
    })
}

pub const MATERIAL_SCALAR_COUNT: usize = 18;
const RMAT_MAGIC: &[u8; 4] = b"RMAT";

#[derive(Debug, Clone)]
pub struct RmatInfo {
    pub scalars: [f32; MATERIAL_SCALAR_COUNT],
    pub texture_ids: [Option<AssetId>; 5],
}

pub fn decode_rmat(data: &[u8]) -> Option<RmatInfo> {
    let header = 5 + MATERIAL_SCALAR_COUNT * 4;
    if data.len() < header || &data[..4] != RMAT_MAGIC || data[4] != 1 {
        return None;
    }
    let mut scalars = [0.0; MATERIAL_SCALAR_COUNT];
    for (i, value) in scalars.iter_mut().enumerate() {
        let off = 5 + i * 4;
        *value = f32::from_le_bytes(data[off..off + 4].try_into().ok()?);
    }
    let mut texture_ids = [None; 5];
    let mut pos = header;
    for slot in &mut texture_ids {
        if pos >= data.len() {
            return None;
        }
        let present = data[pos];
        pos += 1;
        if present == 1 {
            if pos + 8 > data.len() {
                return None;
            }
            *slot = Some(AssetId::from_raw(u64::from_le_bytes(
                data[pos..pos + 8].try_into().ok()?,
            )));
            pos += 8;
        }
    }
    Some(RmatInfo {
        scalars,
        texture_ids,
    })
}
