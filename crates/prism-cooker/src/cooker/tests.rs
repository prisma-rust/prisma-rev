// ===========================================================================
// Tests
// ===========================================================================

use super::*;
use crate::db::AssetDatabase;
use prism_asset::core::AssetId;
use prism_asset::package::PackageBuilder;

fn make_record(id: AssetId, deps: Vec<AssetId>, path: &str) -> crate::db::AssetRecord {
    let mut r = crate::db::AssetRecord::new(id, path.into(), AssetType::Binary, "raw");
    r.dependencies = deps;
    r
}

#[test]
fn binary_cooker_passes_through() {
    let cooker = BinaryCooker;
    assert!(cooker.can_cook(AssetType::Binary));
    assert!(!cooker.can_cook(AssetType::Texture));

    let id = AssetId::from_raw((1u64 << 32) | 1);
    let record = make_record(id, vec![], "test.bin");
    let settings = profile::CookSettings::default();
    let ctx = CookContext {
        record: &record,
        imported_data: b"hello cooker",
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();
    assert_eq!(result.cooked_data, b"hello cooker");
    assert!(result.compress);
}

#[test]
fn texture_cooker_handles_texture() {
    let cooker = TextureCooker;
    assert!(cooker.can_cook(AssetType::Texture));
    assert!(!cooker.can_cook(AssetType::Audio));
}

#[test]
fn topological_sort_simple() {
    let mut db = AssetDatabase::new();

    let id_a = db.generate_id();
    let id_b = db.generate_id();
    let id_c = db.generate_id();

    // A depends on B. B depends on C.
    db.insert(make_record(id_a, vec![id_b], "a.bin")).unwrap();
    db.insert(make_record(id_b, vec![id_c], "b.bin")).unwrap();
    db.insert(make_record(id_c, vec![], "c.bin")).unwrap();

    let order = topological_sort(&db);
    // C must come before B, B before A.
    let pos_c = order.iter().position(|&id| id == id_c).unwrap();
    let pos_b = order.iter().position(|&id| id == id_b).unwrap();
    let pos_a = order.iter().position(|&id| id == id_a).unwrap();
    assert!(pos_c < pos_b, "C before B");
    assert!(pos_b < pos_a, "B before A");
}

#[test]
fn topological_sort_cycle_does_not_panic() {
    let mut db = AssetDatabase::new();
    let id_a = db.generate_id();
    let id_b = db.generate_id();

    // A depends on B, B depends on A (cycle).
    db.insert(make_record(id_a, vec![id_b], "a.bin")).unwrap();
    db.insert(make_record(id_b, vec![id_a], "b.bin")).unwrap();

    let order = topological_sort(&db);
    // Both should be present despite the cycle.
    assert!(order.contains(&id_a));
    assert!(order.contains(&id_b));
}

#[test]
fn topological_sort_empty_db() {
    let db = AssetDatabase::new();
    let order = topological_sort(&db);
    assert!(order.is_empty());
}

#[test]
fn cooker_registry_basics() {
    let mut reg = CookerRegistry::new();
    reg.register(Box::new(BinaryCooker));
    reg.register(Box::new(TextureCooker));
    assert_eq!(reg.len(), 2);

    assert!(reg.find_for_type(AssetType::Binary).is_some());
    assert!(reg.find_for_type(AssetType::Texture).is_some());
    assert!(reg.find_for_type(AssetType::Audio).is_none());

    let b = reg.get("binary-cooker").unwrap();
    assert_eq!(b.name(), "binary-cooker");
}

#[test]
fn full_cook_pipeline() {
    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    let record = crate::db::AssetRecord::new(id, "test.bin".into(), AssetType::Binary, "raw");
    db.insert(record).unwrap();

    let reg = default_cooker_registry();
    let pipeline = CookPipeline::new(reg);
    let settings = profile::CookSettings::default();

    let mut asset_data = HashMap::new();
    asset_data.insert(id, b"cook me".to_vec());

    let mut builder = PackageBuilder::new();
    let summary = pipeline
        .cook_all(&db, &asset_data, &mut builder, &settings)
        .unwrap();
    assert_eq!(summary.cooked, 1);
    assert_eq!(summary.skipped, 0);

    let pak = builder.build().unwrap();
    assert!(!pak.is_empty());
}

#[test]
fn cook_pipeline_skips_missing_data() {
    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    db.insert(crate::db::AssetRecord::new(
        id,
        "missing.bin".into(),
        AssetType::Binary,
        "raw",
    ))
    .unwrap();

    let reg = default_cooker_registry();
    let pipeline = CookPipeline::new(reg);
    let settings = profile::CookSettings::default();

    // No data for the 资源
    let asset_data = HashMap::new();
    let mut builder = PackageBuilder::new();
    let summary = pipeline
        .cook_all(&db, &asset_data, &mut builder, &settings)
        .unwrap();
    assert_eq!(summary.cooked, 0);
    assert_eq!(summary.skipped, 1);
}

// ── 纹理 Cooker new tests ─────────────────────────────────────

fn make_texture_intermediate(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + rgba.len());
    buf.extend_from_slice(b"RTXI");
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.push(4); // channels
    buf.push(0); // format RGBA8
    buf.extend_from_slice(rgba);
    buf
}

#[test]
fn texture_cooker_generates_mips() {
    // 4×4 RGBA red 图像
    let pixels = std::iter::repeat_n([255u8, 0, 0, 255], 4 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let cooker = TextureCooker;

    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let settings = profile::CookSettings::default();
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    // 验证 RTEX magic.
    assert_eq!(&result.cooked_data[..4], b"RTEX");
    assert_eq!(result.cooked_data[4], 1); // version

    // Base width/height.
    let bw = u32::from_le_bytes(result.cooked_data[5..9].try_into().unwrap());
    let bh = u32::from_le_bytes(result.cooked_data[9..13].try_into().unwrap());
    assert_eq!(bw, 4);
    assert_eq!(bh, 4);

    // Mip level count: 4→2→1 = 3 levels.
    let levels = u32::from_le_bytes(result.cooked_data[13..17].try_into().unwrap());
    assert_eq!(levels, 3);

    // 格式
    assert_eq!(result.cooked_data[17], RTEX_FORMAT_RGBA8); // RGBA8

    // Offsets 表 (levels * 4 字节 after header).
    let off_pos = 18usize;
    let mip0_off = u32::from_le_bytes(result.cooked_data[off_pos..off_pos + 4].try_into().unwrap());
    let mip1_off = u32::from_le_bytes(
        result.cooked_data[off_pos + 4..off_pos + 8]
            .try_into()
            .unwrap(),
    );
    let mip2_off = u32::from_le_bytes(
        result.cooked_data[off_pos + 8..off_pos + 12]
            .try_into()
            .unwrap(),
    );

    // Mip0: 4*4*4 = 64 字节 starting at header (18 + 12 = 30)
    assert_eq!(mip0_off, 30);
    assert_eq!(mip1_off, 30 + 64);
    // Mip1: 2*2*4 = 16 字节
    assert_eq!(mip2_off, 30 + 64 + 16);

    // RGBA8 mip 链仍然值得交给 pak 的 zstd（#519）。
    assert!(result.compress);
}

#[test]
fn rgba8_textures_are_marked_for_pak_compression() {
    // 未压缩的像素很好压（zstd 一般省 30~50%）；只有块压缩的那些才应该跳过
    // 外层压缩。以前一律 `compress: false` 是 #519 记的那条问题。
    let pixels = std::iter::repeat_n([16u8, 128, 240, 255], 4 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let id = AssetId::from_raw((1u64 << 32) | 100);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");

    let settings = profile::CookSettings::default();
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = TextureCooker.cook(&ctx).unwrap();
    assert!(
        result.compress,
        "RGBA8 payloads should go through the pak zstd path"
    );
}

#[test]
fn an_unimplemented_compression_profile_still_falls_back_to_rgba8() {
    // profile 说 BC7（desktop 默认），但编码器还没实现：必须是 RGBA8 + 可压缩，
    // 而不是产出运行时读不懂的块数据。
    let pixels = std::iter::repeat_n([200u8, 40, 90, 255], 4 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let id = AssetId::from_raw((1u64 << 32) | 101);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let mut settings = profile::CookSettings::default();
    settings.texture.compression = profile::TextureCompression::Bc7;
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = TextureCooker.cook(&ctx).unwrap();
    // RTEX format byte 在 header 的固定偏移上（4 magic + 1 version + 4 w + 4 h + 4 levels）。
    assert_eq!(result.cooked_data[17], 0, "RGBA8 format byte");
    assert!(result.compress);
}

fn rtex_header(data: &[u8]) -> (u32, u32, u32) {
    assert_eq!(&data[..4], b"RTEX");
    let width = u32::from_le_bytes(data[5..9].try_into().unwrap());
    let height = u32::from_le_bytes(data[9..13].try_into().unwrap());
    let levels = u32::from_le_bytes(data[13..17].try_into().unwrap());
    (width, height, levels)
}

#[test]
fn texture_cooker_honours_max_size_and_keeps_the_aspect_ratio() {
    // 8×4 红图，最长边压到 4 → 4×2；mip 链 4×2 → 2×1 → 1×1 = 3 级。
    let pixels = std::iter::repeat_n([255u8, 0, 0, 255], 8 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(8, 4, &pixels);
    let id = AssetId::from_raw((1u64 << 32) | 102);
    let record = crate::db::AssetRecord::new(
        id,
        "wide.png".into(),
        AssetType::Texture,
        "texture-importer",
    );
    let mut settings = profile::CookSettings::default();
    settings.texture.max_size = 4;
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = TextureCooker.cook(&ctx).unwrap();
    assert_eq!(rtex_header(&result.cooked_data), (4, 2, 3));

    // 小图不会被放大。
    let intermediate = make_texture_intermediate(2, 2, &pixels[..16]);
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = TextureCooker.cook(&ctx).unwrap();
    assert_eq!(rtex_header(&result.cooked_data), (2, 2, 2));
}

#[test]
fn texture_cooker_can_skip_the_mip_chain() {
    let pixels = std::iter::repeat_n([10u8, 20, 30, 255], 4 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let id = AssetId::from_raw((1u64 << 32) | 103);
    let record = crate::db::AssetRecord::new(
        id,
        "flat.png".into(),
        AssetType::Texture,
        "texture-importer",
    );
    let mut settings = profile::CookSettings::default();
    settings.texture.generate_mips = false;
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = TextureCooker.cook(&ctx).unwrap();
    assert_eq!(rtex_header(&result.cooked_data), (4, 4, 1));
}

#[test]
fn texture_cooker_rejects_bad_magic() {
    let cooker = TextureCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: b"garbage data",
        settings: &settings,
    };
    assert!(cooker.cook(&ctx).is_err());
}

#[test]
fn texture_cooker_rejects_zero_dimensions() {
    let cooker = TextureCooker;
    let intermediate = make_texture_intermediate(0, 0, &[]);
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    assert!(cooker.cook(&ctx).is_err());
}

// ── 网格 Cooker new tests ────────────────────────────────────────

fn make_mesh_intermediate(verts: u32, idxs: u32, uv_channels: u32) -> Vec<u8> {
    let stride = (3 + 3 + uv_channels * 2) as usize;
    let vert_size = verts as usize * stride * 4;
    let idx_size = idxs as usize * 4;

    let mut buf = Vec::with_capacity(17 + vert_size + idx_size);
    buf.extend_from_slice(b"RMXI");
    buf.push(1); // version
    buf.extend_from_slice(&verts.to_le_bytes());
    buf.extend_from_slice(&idxs.to_le_bytes());
    buf.extend_from_slice(&uv_channels.to_le_bytes());
    // Fill 顶点 data (positions + normals + uv
    for _ in 0..verts {
        for _ in 0..stride {
            buf.extend_from_slice(&0.0f32.to_le_bytes());
        }
    }
    for _ in 0..idxs {
        buf.extend_from_slice(&0u32.to_le_bytes());
    }
    buf
}

#[test]
fn mesh_cooker_writes_rmes() {
    let intermediate = make_mesh_intermediate(12, 36, 1);
    let cooker = MeshCooker;
    let settings = profile::CookSettings::default();

    let id = AssetId::from_raw((1u64 << 32) | 200);
    let record =
        crate::db::AssetRecord::new(id, "mesh.gltf".into(), AssetType::Mesh, "gltf-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    // 验证 RMES magic.
    assert_eq!(&result.cooked_data[..4], b"RMES");
    assert_eq!(result.cooked_data[4], 1); // version

    let vert_count = u32::from_le_bytes(result.cooked_data[5..9].try_into().unwrap());
    let idx_count = u32::from_le_bytes(result.cooked_data[9..13].try_into().unwrap());
    assert_eq!(vert_count, 12);
    assert_eq!(idx_count, 36);

    let uv_count = u32::from_le_bytes(result.cooked_data[13..17].try_into().unwrap());
    assert_eq!(uv_count, 1);

    let stride = u32::from_le_bytes(result.cooked_data[17..21].try_into().unwrap());
    assert_eq!(stride, (3 + 3 + 2) * 4); // pos + nrm + uv = 8 floats * 4

    // Offsets.
    let pos_off = u32::from_le_bytes(result.cooked_data[21..25].try_into().unwrap());
    assert_eq!(pos_off, 33); // after 33-byte header

    assert!(result.compress);
}

#[test]
fn mesh_cooker_rejects_bad_magic() {
    let cooker = MeshCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 200);
    let record =
        crate::db::AssetRecord::new(id, "mesh.gltf".into(), AssetType::Mesh, "gltf-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: b"garbage",
        settings: &settings,
    };
    assert!(cooker.cook(&ctx).is_err());
}

#[test]
fn mesh_cooker_rejects_empty_mesh() {
    let cooker = MeshCooker;
    let intermediate = make_mesh_intermediate(0, 0, 0);
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 200);
    let record =
        crate::db::AssetRecord::new(id, "mesh.gltf".into(), AssetType::Mesh, "gltf-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    assert!(cooker.cook(&ctx).is_err());
}

#[test]
fn mesh_cooker_registry_integration() {
    let mut reg = CookerRegistry::new();
    reg.register(Box::new(MeshCooker));
    assert_eq!(reg.len(), 1);

    let found = reg.find_for_type(AssetType::Mesh);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name(), "mesh-cooker");
    assert!(reg.find_for_type(AssetType::Texture).is_none());
}

#[test]
fn texture_cooker_registry_integration() {
    let mut reg = CookerRegistry::new();
    reg.register(Box::new(TextureCooker));
    let found = reg.find_for_type(AssetType::Texture);
    assert!(found.is_some());
    assert_eq!(found.unwrap().name(), "texture-cooker");
}

// ── Round-trip: 烹饪 → 解码 → assert ───────────────────────────

#[test]
fn binary_cooker_roundtrip() {
    let input = b"some binary payload";
    let cooker = BinaryCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 1);
    let record = crate::db::AssetRecord::new(id, "data.bin".into(), AssetType::Binary, "raw");
    let ctx = CookContext {
        record: &record,
        imported_data: input,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();
    // 二进制 cooker is pass-through; cooked data must be 相同
    assert_eq!(result.cooked_data, input);
}

#[test]
fn texture_cooker_roundtrip() {
    // 构建 a small 8×6 gradient RGBA8 图像
    let w = 8u32;
    let h = 6u32;
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            pixels.push((x * 32) as u8); // R varies with x
            pixels.push((y * 42) as u8); // G varies with y
            pixels.push(128u8); // B constant
            pixels.push(255u8); // A opaque
        }
    }

    let intermediate = make_texture_intermediate(w, h, &pixels);
    let cooker = TextureCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    // 解码 RTEX 后
    let rtex = decode_rtex(&result.cooked_data).expect("should decode RTEX");
    assert_eq!(rtex.width, w);
    assert_eq!(rtex.height, h);
    assert_eq!(rtex.format, RTEX_FORMAT_RGBA8); // RGBA8
    assert!(rtex.mip_levels >= 1);

    // Mip0 must be byte-identical to the 输入 pixels (cooker copies mip0 verbatim).
    assert_eq!(
        rtex.mip_data[0], pixels,
        "mip0 must match input pixels exactly"
    );

    // Mip 链 must be non-empty and each successive level must be
    // smaller (or 等于 at 1×1).
    for i in 1..rtex.mip_levels as usize {
        assert!(
            rtex.mip_data[i].len() < rtex.mip_data[i - 1].len(),
            "mip{} ({}B) must be smaller than mip{} ({}B)",
            i,
            rtex.mip_data[i].len(),
            i - 1,
            rtex.mip_data[i - 1].len(),
        );
    }
}

#[test]
fn texture_decoder_rejects_bad_data() {
    assert!(decode_rtex(b"garbage").is_none());
    assert!(decode_rtex(b"RTEX").is_none()); // too short
                                             // Wrong version.
    let mut bad = vec![b'R', b'T', b'E', b'X', 99];
    bad.resize(20, 0);
    assert!(decode_rtex(&bad).is_none());
}

#[test]
fn mesh_cooker_roundtrip() {
    // 构建 an RMXI intermediate with 3 顶点 (a triangle).
    let verts = 3u32;
    let idxs = 3u32;
    let uv_channels = 1u32;
    let stride_floats = (3 + 3 + 2) as usize; // pos + nrm + uv

    let mut intermediate = Vec::new();
    intermediate.extend_from_slice(b"RMXI");
    intermediate.push(1); // version
    intermediate.extend_from_slice(&verts.to_le_bytes());
    intermediate.extend_from_slice(&idxs.to_le_bytes());
    intermediate.extend_from_slice(&uv_channels.to_le_bytes());

    // Positions: a simple triangle
    let pos: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    // Normals: all pointing 上
    let nrm = [0.0f32, 0.0, 1.0];
    // UVs
    let uv: [[f32; 2]; 3] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];

    for i in 0..verts as usize {
        intermediate.extend_from_slice(&pos[i][0].to_le_bytes());
        intermediate.extend_from_slice(&pos[i][1].to_le_bytes());
        intermediate.extend_from_slice(&pos[i][2].to_le_bytes());
        intermediate.extend_from_slice(&nrm[0].to_le_bytes());
        intermediate.extend_from_slice(&nrm[1].to_le_bytes());
        intermediate.extend_from_slice(&nrm[2].to_le_bytes());
        intermediate.extend_from_slice(&uv[i][0].to_le_bytes());
        intermediate.extend_from_slice(&uv[i][1].to_le_bytes());
    }
    // Indices
    for i in 0..idxs {
        intermediate.extend_from_slice(&i.to_le_bytes());
    }

    let pw = &intermediate;
    let expected_vert_size = verts as usize * stride_floats * 4;
    let expected_idx_size = idxs as usize * 4;

    // 烹饪
    let cooker = MeshCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 200);
    let record =
        crate::db::AssetRecord::new(id, "mesh.gltf".into(), AssetType::Mesh, "gltf-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: pw,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    // 解码 RMES.
    let rmes = decode_rmes(&result.cooked_data).expect("should decode RMES");
    assert_eq!(rmes.vert_count, verts);
    assert_eq!(rmes.idx_count, idxs);
    assert_eq!(rmes.uv_channels, uv_channels);

    // 顶点 data must 匹配 the intermediate (after its 17-byte header).
    let expected_vert = &intermediate[17..17 + expected_vert_size];
    assert_eq!(
        rmes.vertex_data, expected_vert,
        "RMES vertex data must match RMXI vertex data"
    );

    // 索引 data must 匹配
    let expected_idx =
        &intermediate[17 + expected_vert_size..17 + expected_vert_size + expected_idx_size];
    assert_eq!(
        rmes.index_data, expected_idx,
        "RMES index data must match RMXI index data"
    );
}

#[test]
fn mesh_decoder_rejects_bad_data() {
    assert!(decode_rmes(b"garbage").is_none());
    // Wrong version.
    let mut bad = vec![b'R', b'M', b'E', b'S', 99];
    bad.resize(40, 0);
    assert!(decode_rmes(&bad).is_none());
}

#[test]
fn decode_rtex_handles_known_asset() {
    // Use the same 模式 as texture_cooker_generates_mips test.
    let pixels = std::iter::repeat_n([255u8, 0, 0, 255], 4 * 4)
        .flatten()
        .collect::<Vec<_>>();
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let cooker = TextureCooker;
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    let rtex = decode_rtex(&result.cooked_data).unwrap();
    assert_eq!(rtex.width, 4);
    assert_eq!(rtex.height, 4);
    assert_eq!(rtex.mip_levels, 3);
    assert_eq!(rtex.format, RTEX_FORMAT_RGBA8);
    assert_eq!(rtex.mip_data.len(), 3);
    // mip0 = 4*4*4 = 64 字节
    assert_eq!(rtex.mip_data[0].len(), 64);
    // mip1 = 2*2*4 = 16 字节
    assert_eq!(rtex.mip_data[1].len(), 16);
    // mip2 = 1*1*4 = 4 字节
    assert_eq!(rtex.mip_data[2].len(), 4);
}

#[test]
fn parse_rtexi_pixels_roundtrip() {
    let mut pixels = Vec::new();
    for i in 0..16 {
        pixels.push(i as u8);
    }
    // 4 channels, so 2×2 图像 with 4 字节 per 像素 = 16 字节
    let intermediate = make_texture_intermediate(2, 2, &pixels);

    let (w, h, parsed) = parse_rtexi_pixels(&intermediate).unwrap();
    assert_eq!(w, 2);
    assert_eq!(h, 2);
    assert_eq!(parsed, pixels);
}

#[test]
fn texture_cooker_rgba8_still_default() {
    // 默认 配置 压缩 (Rgba8) must produce RGBA8 输出
    let pixels = vec![255u8; 4 * 4 * 4];
    let intermediate = make_texture_intermediate(4, 4, &pixels);
    let cooker = TextureCooker;

    let id = AssetId::from_raw((1u64 << 32) | 99);
    let record =
        crate::db::AssetRecord::new(id, "tex.png".into(), AssetType::Texture, "texture-importer");
    let settings = profile::CookSettings::default(); // Rgba8
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();

    // Must be RGBA8 格式
    assert_eq!(&result.cooked_data[..4], b"RTEX");
    assert_eq!(result.cooked_data[17], RTEX_FORMAT_RGBA8);
}

// -------------------------------------------------------------------
// 材质 cooker round-trip
// -------------------------------------------------------------------

/// 构建 an RMATI intermediate blob by hand (mirrors the importer 输出
fn make_rmati(scalars: &[f32; MATERIAL_SCALAR_COUNT], tex_paths: &[Option<String>; 5]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RMATI");
    buf.push(1); // version
    for s in scalars {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    for slot in tex_paths {
        match slot {
            Some(p) => {
                buf.push(1);
                let b = p.as_bytes();
                let len = b.len().min(u16::MAX as usize) as u16;
                buf.extend_from_slice(&len.to_le_bytes());
                buf.extend_from_slice(&b[..len as usize]);
            }
            None => buf.push(0),
        }
    }
    buf
}

#[test]
fn material_cooker_roundtrip_no_textures() {
    let scalars = [
        0.8, 0.8, 0.8, 1.0, // base_color
        0.2, 0.5, // metallic, roughness
        0.0, 0.0, 0.0, // emissive
        1.0, 1.0, 1.0, // emissive_strength, normal_scale, occlusion_strength
        0.0, 1.5, 0.0, 0.0, // transmission, ior, translucency, anisotropy
        0.0, 0.0, // clearcoat, clearcoat_roughness
    ];
    let tex_paths: [Option<String>; 5] = [None, None, None, None, None];
    let intermediate = make_rmati(&scalars, &tex_paths);

    let cooker = MaterialCooker;
    assert!(cooker.can_cook(AssetType::Material));
    assert!(!cooker.can_cook(AssetType::Mesh));

    let id = AssetId::from_raw((1u64 << 32) | 7);
    let record = make_record(id, vec![], "test.mat.json");
    let settings = profile::CookSettings::default();
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = cooker.cook(&ctx).unwrap();
    assert!(result.compress);

    // 解码 and 验证
    let info = decode_rmat(&result.cooked_data).expect("decode_rmat");
    assert_eq!(info.scalars, scalars);
    for slot in &info.texture_ids {
        assert!(slot.is_none());
    }
}

#[test]
fn material_cooker_roundtrip_with_textures() {
    let scalars = [
        1.0, 0.0, 0.0, 1.0, // base_color (red)
        0.0, 1.0, // metallic, roughness
        0.1, 0.0, 0.0, // emissive
        2.0, 1.5, 0.8, // emissive_strength, normal_scale, occlusion_strength
        0.0, 1.45, 0.0, 0.0, // transmission, ior, translucency, anisotropy
        1.0, 0.1, // clearcoat, clearcoat_roughness
    ];
    // Only albedo + 遮挡 textures present.
    let tex_paths: [Option<String>; 5] = [
        Some("textures/albedo.png".into()),
        None,
        None,
        None,
        Some("textures/occlusion.png".into()),
    ];
    let intermediate = make_rmati(&scalars, &tex_paths);

    // The importer would have resolved these two paths to two AssetId deps
    // stored on the record, in 槽 order (albedo 第一个 遮挡 秒
    let tex_id_albedo = AssetId::from_raw((1u64 << 32) | 100);
    let tex_id_occlusion = AssetId::from_raw((1u64 << 32) | 101);
    let id = AssetId::from_raw((1u64 << 32) | 7);
    let record = make_record(id, vec![tex_id_albedo, tex_id_occlusion], "test.mat.json");
    let settings = profile::CookSettings::default();
    let ctx = CookContext {
        record: &record,
        imported_data: &intermediate,
        settings: &settings,
    };
    let result = MaterialCooker.cook(&ctx).unwrap();

    let info = decode_rmat(&result.cooked_data).expect("decode_rmat");
    assert_eq!(info.scalars, scalars);
    assert_eq!(info.texture_ids[0], Some(tex_id_albedo));
    assert!(info.texture_ids[1].is_none());
    assert!(info.texture_ids[2].is_none());
    assert!(info.texture_ids[3].is_none());
    assert_eq!(info.texture_ids[4], Some(tex_id_occlusion));
}

#[test]
fn material_cooker_rejects_bad_magic() {
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 7);
    let record = make_record(id, vec![], "test.mat.json");
    let ctx = CookContext {
        record: &record,
        imported_data: b"XXXXgarbage",
        settings: &settings,
    };
    assert!(MaterialCooker.cook(&ctx).is_err());
}

#[test]
fn decode_rmat_rejects_bad_magic() {
    assert!(decode_rmat(b"XXXX").is_none());
}

// -------------------------------------------------------------------
// 着色器 cooker (intermediate parsing only; 编译 needs slangc)
// -------------------------------------------------------------------

/// 构建 an RSLI intermediate blob by hand (mirrors the importer 输出
fn make_rsli(entry: &str, stage: &str, profile: &str, source: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RSLI");
    buf.push(1); // version
    let e = entry.as_bytes();
    buf.extend_from_slice(&(e.len() as u16).to_le_bytes());
    buf.extend_from_slice(e);
    let s = stage.as_bytes();
    buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
    buf.extend_from_slice(s);
    let p = profile.as_bytes();
    buf.extend_from_slice(&(p.len() as u16).to_le_bytes());
    buf.extend_from_slice(p);
    buf.extend_from_slice(&(source.len() as u32).to_le_bytes());
    buf.extend_from_slice(source);
    buf
}

#[test]
fn shader_cooker_parses_intermediate() {
    let intermediate = make_rsli("vertexMain", "vertex", "spirv_1_5", b"// dummy");
    let info = ShaderCooker::parse_intermediate(&intermediate).expect("parse RSLI");
    assert_eq!(info.entry, "vertexMain");
    assert_eq!(info.stage, "vertex");
    assert_eq!(info.profile, "spirv_1_5");
    assert_eq!(info.source, b"// dummy");
}

#[test]
fn shader_cooker_rejects_bad_magic() {
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 9);
    let record = make_record(id, vec![], "test.slang");
    let ctx = CookContext {
        record: &record,
        imported_data: b"XXXXgarbage",
        settings: &settings,
    };
    assert!(ShaderCooker.cook(&ctx).is_err());
}

#[test]
fn shader_cooker_rejects_bad_intermediate() {
    // Truncated RSLI: magic + version + 部分 header.
    let settings = profile::CookSettings::default();
    let id = AssetId::from_raw((1u64 << 32) | 9);
    let record = make_record(id, vec![], "test.slang");
    let ctx = CookContext {
        record: &record,
        imported_data: b"RSLI\x01\x01\x00",
        settings: &settings,
    };
    assert!(ShaderCooker.cook(&ctx).is_err());
}

#[test]
fn shader_cooker_can_cook_shader_only() {
    let cooker = ShaderCooker;
    assert!(cooker.can_cook(AssetType::Shader));
    assert!(!cooker.can_cook(AssetType::Mesh));
    assert!(!cooker.can_cook(AssetType::Material));
}
