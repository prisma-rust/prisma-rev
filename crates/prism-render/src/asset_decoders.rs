//! Asset decoders — RTEX/RMES cooked formats → GPU upload inputs.
//!
//! The byte layouts are the ones written by the cookers in `prism-cooker`
//! (`TextureCooker::write_rtex`, `MeshCooker::write_rmes`), all scalars
//! little-endian:
//!
//! ```text
//! RTEX  [magic "RTEX"][version 1][width 4][height 4][mip_levels 4][format 1]
//!       [offset table: mip_levels × u32][mip 0][mip 1]…[mip n]
//!
//! RMES  [magic "RMES"][version 1 or 2][vert_count 4][idx_count 4][uv_channels 4]
//!       [stride 4][positions_offset 4][normals_offset 4][uv0_offset 4]
//!       [vertex block][index block]
//!       v2: [submesh_count 4][index_start 4][index_count 4][material_index 4]…
//! ```
//!
//! RMES attribute blocks are **planar** (positions → normals → uv) and located
//! by the three absolute offset fields; `stride` is the interleaved size the
//! cooker records and is deliberately *not* used to locate attributes. The v2
//! submesh table is ignored here: render draw ranges arrive through
//! `DrawItem::index_start` / `index_count` from the engine (this decoder only
//! uploads the buffers).
//!
//! Both decoders are pure data transforms: they are not wired into
//! `BatchUploader` yet (that is the remaining part of DESIGN §10.11 G1).

use crate::managers::{MeshUploadInput, TextureFormat, TextureUploadInput};
use anyhow::{anyhow, bail, Result};

/// RTEX format codes (mirrors `prism-cooker`'s `RTEX_FORMAT_*` constants).
const RTEX_FORMAT_RGBA8: u8 = 0;
const RTEX_FORMAT_BC5: u8 = 2;
const RTEX_FORMAT_BC1: u8 = 3;
const RTEX_FORMAT_BC3: u8 = 4;
const RTEX_FORMAT_BC6H: u8 = 5;

/// Fixed part of the RTEX header: magic + version + w/h + mip count + format.
const RTEX_HEADER_SIZE: usize = 4 + 1 + 4 + 4 + 4 + 1;
/// Fixed part of the RMES header: magic + version + counts + stride + offsets.
const RMES_HEADER_SIZE: usize = 4 + 1 + 4 + 4 + 4 + 4 + 4 + 4 + 4;
const MAX_MIP_LEVELS: u32 = 16;

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| anyhow!("offset overflow at {offset}"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| anyhow!("truncated: need 4 bytes at {offset}"))?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

/// Read `count` `[f32; N]` values starting at an absolute byte offset.
fn read_f32_vectors<const N: usize>(
    bytes: &[u8],
    offset: u32,
    count: u32,
    what: &str,
) -> Result<Vec<[f32; N]>> {
    let start = offset as usize;
    let len = (count as usize)
        .checked_mul(N * 4)
        .ok_or_else(|| anyhow!("{what}: element count {count} overflows"))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| anyhow!("{what}: offset {start} + {len} overflows"))?;
    if end > bytes.len() {
        bail!(
            "{what}: {start}..{end} is past the end of the buffer ({})",
            bytes.len()
        );
    }
    Ok(bytes[start..end]
        .chunks_exact(N * 4)
        .map(|chunk| {
            let mut value = [0.0f32; N];
            for (i, component) in value.iter_mut().enumerate() {
                let at = i * 4;
                *component = f32::from_le_bytes(chunk[at..at + 4].try_into().unwrap());
            }
            value
        })
        .collect())
}

/// 解析 RTEX 字节 → TextureUploadInput。
///
/// Only the base mip level is returned: `TextureUploadInput` carries one tightly
/// packed buffer, and pre-baked mip chains are not uploaded yet (tracked by
/// issue #106). Compressed formats keep working because
/// `RenderTextureManager::reserve` accepts a base level for them as well.
pub fn decode_rtex(bytes: &[u8]) -> Result<TextureUploadInput> {
    if bytes.len() < RTEX_HEADER_SIZE {
        bail!("RTEX too short: {} < {RTEX_HEADER_SIZE}", bytes.len());
    }
    if &bytes[0..4] != b"RTEX" {
        bail!("RTEX bad magic");
    }
    if bytes[4] != 1 {
        bail!("RTEX unsupported version {}", bytes[4]);
    }
    let width = read_u32(bytes, 5)?;
    let height = read_u32(bytes, 9)?;
    let mip_levels = read_u32(bytes, 13)?;
    let format_code = bytes[17];
    if width == 0 || height == 0 {
        bail!("RTEX has zero extent: {width}x{height}");
    }
    if mip_levels == 0 || mip_levels > MAX_MIP_LEVELS {
        bail!("RTEX has invalid mip level count {mip_levels}");
    }

    let table_end = RTEX_HEADER_SIZE
        .checked_add(mip_levels as usize * 4)
        .ok_or_else(|| anyhow!("RTEX mip table overflows"))?;
    if bytes.len() < table_end {
        bail!(
            "RTEX mip offset table truncated: {} < {table_end}",
            bytes.len()
        );
    }
    let mut offsets = Vec::with_capacity(mip_levels as usize);
    for i in 0..mip_levels as usize {
        let offset = read_u32(bytes, RTEX_HEADER_SIZE + i * 4)? as usize;
        if offset < table_end || offset > bytes.len() {
            bail!(
                "RTEX mip {i} offset {offset} outside data region {table_end}..{}",
                bytes.len()
            );
        }
        if let Some(&previous) = offsets.last() {
            if offset <= previous {
                bail!("RTEX mip offsets are not strictly increasing at level {i}");
            }
        }
        offsets.push(offset);
    }

    let format = rtex_texture_format(format_code)?;
    let base_mip_end = offsets.get(1).copied().unwrap_or(bytes.len());
    let pixels = bytes[offsets[0]..base_mip_end].to_vec();
    let expected = format.compressed_byte_len(width, height);
    if pixels.len() != expected {
        bail!(
            "RTEX base mip is {} bytes, expected {expected} for {width}x{height} {:?}",
            pixels.len(),
            format
        );
    }

    Ok(TextureUploadInput {
        width,
        height,
        format,
        // The base level is the only level in `pixels`; asking the manager to
        // derive the chain would misinterpret the count.
        mip_levels: 1,
        pixels,
    })
}

fn rtex_texture_format(code: u8) -> Result<TextureFormat> {
    match code {
        // The cooked format does not record colour space yet; RGBA8 textures are
        // treated as sRGB like every other albedo path in the engine.
        RTEX_FORMAT_RGBA8 => Ok(TextureFormat::Rgba8Srgb),
        RTEX_FORMAT_BC5 => Ok(TextureFormat::Bc5Unorm),
        RTEX_FORMAT_BC6H => Ok(TextureFormat::Bc6HUfloat),
        // BC1 / BC3 不带颜色空间信息；与 RGBA8 一致按 sRGB 处理（albedo / emissive 是
        // 主要用途。需要线性时上层用 `texture_format_for`，它知道槽位语义）（#519）。
        RTEX_FORMAT_BC1 => Ok(TextureFormat::Bc1Srgb),
        RTEX_FORMAT_BC3 => Ok(TextureFormat::Bc3Srgb),
        other => bail!("RTEX unknown format code {other}"),
    }
}

/// 解析 RMES 字节 → MeshUploadInput。
///
/// `uv_channels > 1` is accepted, but only the first uv channel is uploaded
/// (`MeshUploadInput` has a single uv set); tangents/colors stay empty because
/// the cooked format does not carry them.
pub fn decode_rmes(bytes: &[u8]) -> Result<MeshUploadInput> {
    if bytes.len() < RMES_HEADER_SIZE {
        bail!("RMES too short: {} < {RMES_HEADER_SIZE}", bytes.len());
    }
    if &bytes[0..4] != b"RMES" {
        bail!("RMES bad magic");
    }
    if bytes[4] != 1 && bytes[4] != 2 {
        bail!("RMES unsupported version {}", bytes[4]);
    }
    let vert_count = read_u32(bytes, 5)?;
    let idx_count = read_u32(bytes, 9)?;
    let uv_channels = read_u32(bytes, 13)?;
    let stride = read_u32(bytes, 17)?;
    let positions_offset = read_u32(bytes, 21)?;
    let normals_offset = read_u32(bytes, 25)?;
    let uv0_offset = read_u32(bytes, 29)?;

    if stride == 0 {
        bail!("RMES vertex stride is zero");
    }
    let vertex_block = (vert_count as usize)
        .checked_mul(stride as usize)
        .ok_or_else(|| anyhow!("RMES vertex block size overflows"))?;
    let index_start = RMES_HEADER_SIZE
        .checked_add(vertex_block)
        .ok_or_else(|| anyhow!("RMES index block offset overflows"))?;
    let index_size = (idx_count as usize)
        .checked_mul(4)
        .ok_or_else(|| anyhow!("RMES index block size overflows"))?;
    let index_end = index_start
        .checked_add(index_size)
        .ok_or_else(|| anyhow!("RMES index block end overflows"))?;
    if index_end > bytes.len() {
        bail!(
            "RMES index block {index_start}..{index_end} is past the end of the buffer ({})",
            bytes.len()
        );
    }

    let positions = if vert_count == 0 {
        Vec::new()
    } else {
        if positions_offset == 0 {
            bail!("RMES has {vert_count} vertices but positions_offset is zero");
        }
        read_f32_vectors::<3>(bytes, positions_offset, vert_count, "RMES positions")?
    };
    let normals = if normals_offset == 0 {
        Vec::new()
    } else {
        read_f32_vectors::<3>(bytes, normals_offset, vert_count, "RMES normals")?
    };
    let uvs = if uv0_offset == 0 || uv_channels == 0 {
        Vec::new()
    } else {
        // Channel 0 of an interleaved uv block: `uv_channels` pairs per vertex.
        let uv_pair_count = vert_count
            .checked_mul(uv_channels)
            .ok_or_else(|| anyhow!("RMES uv channel count overflows"))?;
        let uv_pairs = read_f32_vectors::<2>(bytes, uv0_offset, uv_pair_count, "RMES uv channels")?;
        uv_pairs.into_iter().step_by(uv_channels as usize).collect()
    };
    let indices = bytes[index_start..index_end]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&chunk| u32::from_le_bytes(chunk))
        .collect();

    Ok(MeshUploadInput {
        positions,
        normals,
        colors: Vec::new(),
        uvs,
        tangents: Vec::new(),
        indices,
    })
}

#[cfg(test)]
#[path = "asset_decoders_tests.rs"]
mod tests;
