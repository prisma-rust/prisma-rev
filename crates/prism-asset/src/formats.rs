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
/// BC7 (RGBA, 16 bytes per 4×4 block).
pub const RTEX_FORMAT_BC7: u8 = 6;
/// ASTC 4×4 (16 bytes per block).
pub const RTEX_FORMAT_ASTC_4X4: u8 = 7;
/// ASTC 6×6 (16 bytes per block).
pub const RTEX_FORMAT_ASTC_6X6: u8 = 8;
/// ASTC 8×8 (16 bytes per block).
pub const RTEX_FORMAT_ASTC_8X8: u8 = 9;
/// ASTC 12×12 (16 bytes per block).
pub const RTEX_FORMAT_ASTC_12X12: u8 = 10;
/// ETC2 RGBA8 (16 bytes per 4×4 block).
pub const RTEX_FORMAT_ETC2_RGBA8: u8 = 11;

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

// ---------------------------------------------------------------------------
// RATL (.ratlas 2D Sprite Atlas)
// ---------------------------------------------------------------------------

pub const RATL_MAGIC: &[u8; 4] = b"RATL";
pub const RATL_VERSION: u8 = 1;

/// Metadata for a single sprite entry within an atlas.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasSpriteEntry {
    /// Sprite name (e.g. `idle_0` or `character/run`).
    pub name: String,
    /// 64-bit FNV-1a hash of the name for fast runtime lookup.
    pub name_hash: u64,
    /// Pixel rectangle `[x, y, width, height]` on the atlas texture.
    pub pixel_rect: [u32; 4],
    /// Normalized UV rectangle `[u0, v0, u1, v1]`.
    pub uv_rect: [f32; 4],
    /// Original pixel dimensions `[width, height]` before alpha trimming.
    pub original_size: [u32; 2],
    /// Offset `[x, y]` of the trimmed region relative to original sprite top-left.
    pub trimmed_offset: [i32; 2],
    /// Pivot position `[pivot_x, pivot_y]` (0..1, defaults to `[0.5, 0.5]`).
    pub pivot: [f32; 2],
    /// 9-slice borders `[top, right, bottom, left]` in pixels (0 = no 9-slice).
    pub nine_slice: [u32; 4],
}

/// Decoded sprite atlas metadata (.ratlas file).
#[derive(Debug, Clone, PartialEq)]
pub struct RatlInfo {
    /// Path to the underlying texture asset (typically an .rtex file).
    pub texture_path: String,
    /// Total width of the atlas texture.
    pub width: u32,
    /// Total height of the atlas texture.
    pub height: u32,
    /// All sprite entries packed into this atlas.
    pub sprites: Vec<AtlasSpriteEntry>,
}

impl RatlInfo {
    /// Find a sprite entry by name (matches hash first, falling back to name comparison).
    pub fn find_sprite(&self, name: &str) -> Option<&AtlasSpriteEntry> {
        let hash = hash_sprite_name(name);
        self.sprites
            .iter()
            .find(|s| s.name_hash == hash || s.name == name)
    }

    /// Find a sprite entry by 64-bit name hash.
    pub fn find_sprite_by_hash(&self, hash: u64) -> Option<&AtlasSpriteEntry> {
        self.sprites.iter().find(|s| s.name_hash == hash)
    }
}

/// 64-bit FNV-1a hash function for sprite names.
pub fn hash_sprite_name(name: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in name.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Encode [`RatlInfo`] into binary `.ratlas` bytes.
pub fn encode_ratl(info: &RatlInfo) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RATL_MAGIC);
    out.push(RATL_VERSION);
    let tex_bytes = info.texture_path.as_bytes();
    out.extend_from_slice(&(tex_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(tex_bytes);
    out.extend_from_slice(&info.width.to_le_bytes());
    out.extend_from_slice(&info.height.to_le_bytes());
    out.extend_from_slice(&(info.sprites.len() as u32).to_le_bytes());
    for s in &info.sprites {
        let name_bytes = s.name.as_bytes();
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&s.name_hash.to_le_bytes());
        for &val in &s.pixel_rect {
            out.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &s.uv_rect {
            out.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &s.original_size {
            out.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &s.trimmed_offset {
            out.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &s.pivot {
            out.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &s.nine_slice {
            out.extend_from_slice(&val.to_le_bytes());
        }
    }
    out
}

/// Decode binary `.ratlas` bytes into [`RatlInfo`].
pub fn decode_ratl(data: &[u8]) -> Option<RatlInfo> {
    if data.len() < 7 || &data[..4] != RATL_MAGIC || data[4] != RATL_VERSION {
        return None;
    }
    let mut pos = 5;
    let tex_len = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
    pos += 2;
    let tex_bytes = data.get(pos..pos + tex_len)?;
    pos += tex_len;
    let texture_path = std::str::from_utf8(tex_bytes).ok()?.to_string();

    let width = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
    pos += 4;
    let height = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
    pos += 4;
    let sprite_count = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;

    let mut sprites = Vec::with_capacity(sprite_count);
    for _ in 0..sprite_count {
        let name_len = u16::from_le_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
        pos += 2;
        let name_bytes = data.get(pos..pos + name_len)?;
        pos += name_len;
        let name = std::str::from_utf8(name_bytes).ok()?.to_string();

        let name_hash = u64::from_le_bytes(data.get(pos..pos + 8)?.try_into().ok()?);
        pos += 8;

        let mut pixel_rect = [0u32; 4];
        for v in &mut pixel_rect {
            *v = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        let mut uv_rect = [0.0f32; 4];
        for v in &mut uv_rect {
            *v = f32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        let mut original_size = [0u32; 2];
        for v in &mut original_size {
            *v = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        let mut trimmed_offset = [0i32; 2];
        for v in &mut trimmed_offset {
            *v = i32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        let mut pivot = [0.0f32; 2];
        for v in &mut pivot {
            *v = f32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        let mut nine_slice = [0u32; 4];
        for v in &mut nine_slice {
            *v = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
            pos += 4;
        }

        sprites.push(AtlasSpriteEntry {
            name,
            name_hash,
            pixel_rect,
            uv_rect,
            original_size,
            trimmed_offset,
            pivot,
            nine_slice,
        });
    }

    if pos != data.len() {
        return None;
    }

    Some(RatlInfo {
        texture_path,
        width,
        height,
        sprites,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratl_encode_decode_roundtrip() {
        let entry1 = AtlasSpriteEntry {
            name: "hero_idle".into(),
            name_hash: hash_sprite_name("hero_idle"),
            pixel_rect: [4, 4, 32, 48],
            uv_rect: [0.0078125, 0.0078125, 0.0703125, 0.1015625],
            original_size: [32, 50],
            trimmed_offset: [0, 2],
            pivot: [0.5, 0.0],
            nine_slice: [0, 0, 0, 0],
        };
        let entry2 = AtlasSpriteEntry {
            name: "ui_button".into(),
            name_hash: hash_sprite_name("ui_button"),
            pixel_rect: [40, 4, 64, 32],
            uv_rect: [0.078125, 0.0078125, 0.203125, 0.0703125],
            original_size: [64, 32],
            trimmed_offset: [0, 0],
            pivot: [0.5, 0.5],
            nine_slice: [8, 8, 8, 8],
        };
        let info = RatlInfo {
            texture_path: "textures/atlas.rtex".into(),
            width: 512,
            height: 512,
            sprites: vec![entry1.clone(), entry2.clone()],
        };

        let encoded = encode_ratl(&info);
        assert_eq!(&encoded[..4], RATL_MAGIC);
        assert_eq!(encoded[4], RATL_VERSION);

        let decoded = decode_ratl(&encoded).expect("should decode valid RATL");
        assert_eq!(decoded.texture_path, "textures/atlas.rtex");
        assert_eq!(decoded.width, 512);
        assert_eq!(decoded.height, 512);
        assert_eq!(decoded.sprites.len(), 2);
        assert_eq!(decoded.sprites[0], entry1);
        assert_eq!(decoded.sprites[1], entry2);

        let found = decoded.find_sprite("hero_idle").expect("hero_idle must be found");
        assert_eq!(found.name, "hero_idle");
        assert_eq!(found.pixel_rect, [4, 4, 32, 48]);

        let not_found = decoded.find_sprite("hero_attack");
        assert!(not_found.is_none());
    }

    #[test]
    fn ratl_decode_rejects_corrupted() {
        assert!(decode_ratl(b"").is_none());
        assert!(decode_ratl(b"GARBAGE").is_none());
        assert!(decode_ratl(b"RATL\x02").is_none()); // wrong version
    }
}
