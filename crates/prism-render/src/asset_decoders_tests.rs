use super::*;

/// Build an RTEX blob exactly like `TextureCooker::write_rtex` does.
fn rtex_blob(width: u32, height: u32, format: u8, mips: &[Vec<u8>]) -> Vec<u8> {
    let levels = mips.len() as u32;
    let header_size = RTEX_HEADER_SIZE + levels as usize * 4;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RTEX");
    buf.push(1);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&levels.to_le_bytes());
    buf.push(format);
    let mut offset = header_size as u32;
    for mip in mips {
        buf.extend_from_slice(&offset.to_le_bytes());
        offset += mip.len() as u32;
    }
    for mip in mips {
        buf.extend_from_slice(mip);
    }
    buf
}

/// Build an RMES blob exactly like `MeshCooker::write_rmes` does: 33-byte
/// header, planar position/normal/uv blocks, then the index block.
fn rmes_blob(
    positions: &[[f32; 3]],
    normals: Option<&[[f32; 3]]>,
    uvs: &[[f32; 2]],
    indices: &[u32],
) -> Vec<u8> {
    let vert_count = positions.len() as u32;
    let uv_channels: u32 = if uvs.is_empty() { 0 } else { 1 };
    let nrm_floats: u32 = if normals.is_some() { 3 } else { 0 };
    let stride = (3 + nrm_floats + uv_channels * 2) * 4;

    let mut buf = Vec::new();
    buf.extend_from_slice(b"RMES");
    buf.push(1);
    buf.extend_from_slice(&vert_count.to_le_bytes());
    buf.extend_from_slice(&(indices.len() as u32).to_le_bytes());
    buf.extend_from_slice(&uv_channels.to_le_bytes());
    buf.extend_from_slice(&stride.to_le_bytes());

    let pos_off = RMES_HEADER_SIZE as u32;
    let nrm_off = if normals.is_some() {
        pos_off + vert_count * 12
    } else {
        0
    };
    let uv_off = if uv_channels > 0 {
        if normals.is_some() {
            nrm_off + vert_count * 12
        } else {
            pos_off + vert_count * 12
        }
    } else {
        0
    };
    buf.extend_from_slice(&pos_off.to_le_bytes());
    buf.extend_from_slice(&nrm_off.to_le_bytes());
    buf.extend_from_slice(&uv_off.to_le_bytes());

    for position in positions {
        for component in position {
            buf.extend_from_slice(&component.to_le_bytes());
        }
    }
    if let Some(normals) = normals {
        for normal in normals {
            for component in normal {
                buf.extend_from_slice(&component.to_le_bytes());
            }
        }
    }
    for uv in uvs {
        for component in uv {
            buf.extend_from_slice(&component.to_le_bytes());
        }
    }
    for index in indices {
        buf.extend_from_slice(&index.to_le_bytes());
    }
    buf
}

#[test]
fn decode_rtex_uses_the_real_cooked_header() {
    // 2x2 RGBA8 with a 1x1 follow-up mip.
    let mip0 = vec![7u8; 2 * 2 * 4];
    let mip1 = vec![9u8; 4];
    let blob = rtex_blob(2, 2, RTEX_FORMAT_RGBA8, &[mip0.clone(), mip1]);

    let input = decode_rtex(&blob).expect("valid RTEX");

    assert_eq!(input.width, 2, "width must come from the cooked header");
    assert_eq!(input.height, 2);
    assert_eq!(input.format, TextureFormat::Rgba8Srgb);
    assert_eq!(input.mip_levels, 1, "only the base level is uploaded");
    assert_eq!(input.pixels, mip0, "the mip offset table must be skipped");
    // The buffer has to satisfy RenderTextureManager's exact-size check.
    assert_eq!(
        input.pixels.len(),
        input.format.compressed_byte_len(input.width, input.height)
    );
}

#[test]
fn decode_rtex_maps_block_formats() {
    let bc5 = rtex_blob(4, 4, RTEX_FORMAT_BC5, &[vec![0u8; 16]]);
    assert_eq!(decode_rtex(&bc5).unwrap().format, TextureFormat::Bc5Unorm);

    let bc6h = rtex_blob(4, 4, RTEX_FORMAT_BC6H, &[vec![0u8; 16]]);
    assert_eq!(
        decode_rtex(&bc6h).unwrap().format,
        TextureFormat::Bc6HUfloat
    );
}

#[test]
fn decode_rtex_accepts_bc1_and_bc3_block_data() {
    // 块压缩贴图能直接上传（#519）：BC1 是 8 字节 / 块，BC3 是 16 字节 / 块。
    let bc1 = rtex_blob(4, 4, RTEX_FORMAT_BC1, &[vec![0u8; 8]]);
    let decoded = decode_rtex(&bc1).expect("BC1 payload decodes");
    assert_eq!(decoded.width, 4);
    assert_eq!(
        decoded.pixels.len(),
        8,
        "BC1 keeps the 8-byte block payload"
    );
    assert_eq!(decoded.format, TextureFormat::Bc1Srgb);

    let bc3 = rtex_blob(4, 4, RTEX_FORMAT_BC3, &[vec![0u8; 16]]);
    assert!(decode_rtex(&bc3).is_ok(), "BC3 payload decodes");
}

#[test]
fn decode_rtex_rejects_unknown_format_codes() {
    let unknown = rtex_blob(4, 4, 99, &[vec![0u8; 8]]);
    let err = decode_rtex(&unknown).unwrap_err().to_string();
    assert!(err.contains("unknown format"), "unexpected error: {err}");
}

#[test]
fn decode_rtex_rejects_mip_offsets_inside_the_table() {
    let mut blob = rtex_blob(2, 2, RTEX_FORMAT_RGBA8, &[vec![0u8; 16], vec![0u8; 4]]);
    // Point the first mip at byte 0: inside the header/offset table.
    blob[RTEX_HEADER_SIZE..RTEX_HEADER_SIZE + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(decode_rtex(&blob).is_err());
}

#[test]
fn decode_rtex_rejects_base_mip_size_mismatch() {
    // 2x2 RGBA8 needs 16 bytes; hand it 12 so the manager would reject it later.
    let blob = rtex_blob(2, 2, RTEX_FORMAT_RGBA8, &[vec![0u8; 12]]);
    assert!(decode_rtex(&blob).is_err());
}

#[test]
fn decode_rmes_returns_vertices_not_an_empty_mesh() {
    let positions = [[0.0, 1.0, 2.0], [3.0, 4.0, 5.0], [6.0, 7.0, 8.0]];
    let normals = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]];
    let indices = [0u32, 1, 2];
    let blob = rmes_blob(&positions, Some(&normals), &uvs, &indices);

    let input = decode_rmes(&blob).expect("valid RMES");

    assert_eq!(input.positions, positions.to_vec());
    assert_eq!(input.normals, normals.to_vec());
    assert_eq!(input.uvs, uvs.to_vec());
    assert_eq!(input.indices, indices.to_vec());
    assert!(input.colors.is_empty() && input.tangents.is_empty());
    assert!(!input.positions.is_empty(), "mesh must not decode as empty");
}

#[test]
fn decode_rmes_handles_vertex_streams_without_normals_or_uvs() {
    let positions = [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]];
    let blob = rmes_blob(&positions, None, &[], &[0u32, 1]);

    let input = decode_rmes(&blob).unwrap();

    assert_eq!(input.positions, positions.to_vec());
    assert!(input.normals.is_empty());
    assert!(input.uvs.is_empty());
    assert_eq!(input.indices, vec![0, 1]);
}

#[test]
fn decode_rmes_accepts_v2_and_ignores_the_submesh_table() {
    let positions = [[0.0, 0.0, 0.0]];
    let mut blob = rmes_blob(&positions, None, &[], &[0u32]);
    blob[4] = 2;
    // v2 表：一个 submesh，覆盖全部索引。
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&u32::MAX.to_le_bytes());
    let input = decode_rmes(&blob).expect("RMES v2 must decode");
    assert_eq!(input.indices, vec![0]);
}

#[test]
fn decode_rmes_rejects_a_truncated_index_block() {
    let positions = [[0.0, 0.0, 0.0]];
    let mut blob = rmes_blob(&positions, None, &[], &[0u32]);
    blob.truncate(blob.len() - 2);
    assert!(decode_rmes(&blob).is_err());
}
