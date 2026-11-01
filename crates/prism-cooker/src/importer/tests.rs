// ===========================================================================
// Tests
// ===========================================================================

use super::*;
use crate::db::AssetDatabase;

#[test]
fn raw_importer_accepts_anything() {
    let imp = RawImporter;
    assert!(imp.can_import(Path::new("foo.bin")));
    assert!(imp.can_import(Path::new("foo.xyz")));
    assert!(imp.can_import(Path::new("foo")));
}

#[test]
fn texture_importer_accepts_image_extensions() {
    let imp = TextureImporter;
    assert!(imp.can_import(Path::new("tex.png")));
    assert!(imp.can_import(Path::new("tex.jpg")));
    assert!(imp.can_import(Path::new("tex.jpeg")));
    assert!(imp.can_import(Path::new("tex.hdr")));
    assert!(imp.can_import(Path::new("tex.exr")));
    assert!(!imp.can_import(Path::new("tex.txt")));
    assert!(!imp.can_import(Path::new("tex.gltf")));
}

#[test]
fn json_importer_accepts_json() {
    let imp = JsonImporter;
    assert!(imp.can_import(Path::new("data.json")));
    assert!(imp.can_import(Path::new("data.JSON")));
    assert!(!imp.can_import(Path::new("data.txt")));
    assert!(!imp.can_import(Path::new("data.xml")));
}

#[test]
fn raw_importer_imports_bytes() {
    let imp = RawImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_import.bin");
    std::fs::write(&path, b"hello importer").unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Binary);
    assert_eq!(result.output_data, b"hello importer");
    std::fs::remove_file(&path).ok();
}

#[test]
fn texture_importer_imports_with_metadata() {
    let imp = TextureImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_tex.png");

    // 写入 a real 2×2 PNG via the 图像 crate.
    let img = image::RgbaImage::from_raw(
        2,
        2,
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
    )
    .unwrap();
    img.save(&path).unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Texture);
    assert!(result.metadata.is_some());
    let meta = result.metadata.unwrap();
    assert_eq!(meta["format"], "png");
    assert_eq!(meta["original_name"], "test_tex");
    assert_eq!(meta["width"], 2);
    assert_eq!(meta["height"], 2);
    assert_eq!(meta["channels"], 4);
    std::fs::remove_file(&path).ok();
}

#[test]
fn json_importer_validates_json() {
    let imp = JsonImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test.json");
    std::fs::write(&path, b"{\"key\": \"value\"}").unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert!(result.metadata.unwrap()["is_object"].as_bool().unwrap());
    std::fs::remove_file(&path).ok();
}

#[test]
fn json_importer_rejects_bad_json() {
    let imp = JsonImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("bad.json");
    std::fs::write(&path, b"not json").unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let err = imp.import(&ctx);
    assert!(err.is_err(), "bad JSON should be rejected");
    std::fs::remove_file(&path).ok();
}

#[test]
fn registry_finds_importer_by_path() {
    let mut reg = ImporterRegistry::new();
    reg.register(Box::new(TextureImporter));
    reg.register(Box::new(RawImporter));

    assert!(reg.find_for_path(Path::new("tex.png")).is_some());
    assert!(reg.find_for_path(Path::new("tex.jpg")).is_some());
    // RawImporter is catch-all
    assert!(reg.find_for_path(Path::new("foo.xyz")).is_some());
}

#[test]
fn registry_get_by_name() {
    let mut reg = ImporterRegistry::new();
    reg.register(Box::new(TextureImporter));
    let imp = reg.get("texture-importer").unwrap();
    assert_eq!(imp.name(), "texture-importer");
}

#[test]
fn import_pipeline_skips_cached_files() {
    let reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(reg);

    let dir = std::env::temp_dir();
    let path = dir.join("test_cached.bin");
    std::fs::write(&path, b"data").unwrap();

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();

    // 第一个 导入
    let r1 = pipeline
        .import_file(&path, &mut db, &mut cache, None)
        .unwrap();
    assert!(r1.was_imported);

    // 秒 导入 (cached).
    let r2 = pipeline
        .import_file(&path, &mut db, &mut cache, None)
        .unwrap();
    assert!(!r2.was_imported);

    std::fs::remove_file(&path).ok();
}

#[test]
fn force_import_bypasses_the_cache() {
    let reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(reg);

    let dir = std::env::temp_dir().join("prism_force_import_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scanned.bin");
    std::fs::write(&path, b"force me").unwrap();

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();

    let first = pipeline
        .import_file_with_key(&path, "scanned.bin", &mut db, &mut cache, None, false)
        .unwrap();
    assert!(first.was_imported);

    let cached = pipeline
        .import_file_with_key(&path, "scanned.bin", &mut db, &mut cache, None, false)
        .unwrap();
    assert!(!cached.was_imported);

    // The source is unchanged, but `force` must re-run the importer.
    let forced = pipeline
        .import_file_with_key(&path, "scanned.bin", &mut db, &mut cache, None, true)
        .unwrap();
    assert!(forced.was_imported);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn import_pipeline_updates_database() {
    let reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(reg);

    let dir = std::env::temp_dir();
    let path = dir.join("test_db.png");

    // 写入 a real 1×1 red PNG.
    let img = image::RgbaImage::from_raw(1, 1, vec![255, 0, 0, 255]).unwrap();
    img.save(&path).unwrap();

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();

    pipeline
        .import_file(&path, &mut db, &mut cache, None)
        .unwrap();
    assert_eq!(db.len(), 1);
    let r = db.records().next().unwrap();
    assert_eq!(r.asset_type, AssetType::Texture);
    assert_eq!(r.importer_name, "texture-importer");

    std::fs::remove_file(&path).ok();
}

#[test]
fn explicit_relative_key_reuses_scanned_record() {
    let reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(reg);

    let dir = std::env::temp_dir().join("prism_relative_key_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scanned.bin");
    std::fs::write(&path, b"relative key").unwrap();

    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    db.insert(crate::db::AssetRecord::new(
        id,
        "scanned.bin".into(),
        AssetType::Binary,
        "raw",
    ))
    .unwrap();
    let mut cache = ImportCache::new();

    let result = pipeline
        .import_file_with_key(&path, "scanned.bin", &mut db, &mut cache, None, false)
        .unwrap();

    assert!(result.was_imported);
    assert_eq!(
        db.len(),
        1,
        "absolute source path must not add a second record"
    );
    assert_eq!(db.id_by_path("scanned.bin"), Some(id));
    assert!(db.get_by_path("scanned.bin").is_some());

    std::fs::remove_dir_all(&dir).ok();
}

// ── Real glTF / GLB 导入 test ──────────────────────────────────

/// 构建 a minimal 有效 GLB file in 内存
///
/// 包含 one triangle 网格 (3 顶点 3 unsigned-short indices),
/// no 材质 no textures.
fn create_minimal_glb_bytes() -> Vec<u8> {
    // Positions: 右 triangle in XY 平面 Z=0.
    let positions: &[f32] = &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    let indices: &[u16] = &[0, 1, 2];

    let bin_data_size = positions.len() * 4 + indices.len() * 2; // 36 + 6 = 42
    let bin_padding = (4 - (bin_data_size % 4)) % 4;
    let bin_chunk_total = 8 + bin_data_size + bin_padding; // includes chunk-header

    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "prisma-test" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{
            "primitives": [{
                "attributes": { "POSITION": 0 },
                "indices": 1
            }]
        }],
        "accessors": [
            {
                "bufferView": 0,
                "componentType": 5126, // FLOAT
                "count": 3,
                "type": "VEC3",
                "min": [0.0, 0.0, 0.0],
                "max": [1.0, 1.0, 0.0]
            },
            {
                "bufferView": 1,
                "componentType": 5123, // UNSIGNED_SHORT
                "count": 3,
                "type": "SCALAR"
            }
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 6 }
        ],
        "buffers": [{ "byteLength": 42 }]
    });

    let json_string = serde_json::to_string(&json).unwrap();
    let json_bytes = json_string.as_bytes();
    let json_padding = (4 - (json_bytes.len() % 4)) % 4;
    let json_chunk_total = 8 + json_bytes.len() + json_padding;

    let total_len = 12 + json_chunk_total + bin_chunk_total;

    let mut glb = Vec::with_capacity(total_len);

    // GLB header
    glb.extend_from_slice(b"glTF"); // magic
    glb.extend_from_slice(&2u32.to_le_bytes()); // version
    glb.extend_from_slice(&(total_len as u32).to_le_bytes()); // length

    // JSON chunk
    glb.extend_from_slice(&((json_bytes.len() + json_padding) as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(json_bytes);
    glb.extend_from_slice(&vec![0x20; json_padding]); // space padding

    // BIN chunk
    glb.extend_from_slice(&((bin_data_size + bin_padding) as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    for &p in positions {
        glb.extend_from_slice(&p.to_le_bytes());
    }
    for &i in indices {
        glb.extend_from_slice(&i.to_le_bytes());
    }
    glb.extend_from_slice(&vec![0x00; bin_padding]);

    glb
}

#[test]
fn gltf_importer_imports_real_glb() {
    let imp = GltfImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_triangle.glb");

    let glb_bytes = create_minimal_glb_bytes();
    std::fs::write(&path, &glb_bytes).unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: xxhash_rust::xxh3::xxh3_64(&glb_bytes),
        settings: serde_json::Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Mesh);
    assert!(
        !result.output_data.is_empty(),
        "intermediate should have data"
    );

    // Validate RMXI header in 输出
    assert_eq!(&result.output_data[..4], b"RMXI");
    let verts = u32::from_le_bytes(result.output_data[5..9].try_into().unwrap());
    let idxs = u32::from_le_bytes(result.output_data[9..13].try_into().unwrap());
    assert_eq!(verts, 3, "real .glb should yield 3 vertices");
    assert_eq!(idxs, 3, "real .glb should yield 3 indices");

    let meta = result.metadata.unwrap();
    assert_eq!(meta["vertex_count"], 3);
    assert_eq!(meta["index_count"], 3);
    assert!(!meta["has_normals"].as_bool().unwrap_or(false));
    assert!(!meta["has_texcoords"].as_bool().unwrap_or(false));

    std::fs::remove_file(&path).ok();
}

// -------------------------------------------------------------------
// glTF 场景图（#520）
// -------------------------------------------------------------------

/// 把任意 JSON + 二进制块打成 GLB（chunk 按 4 字节对齐）。
fn pack_glb(json: serde_json::Value, bin: &[u8]) -> Vec<u8> {
    let json_bytes = serde_json::to_string(&json).unwrap();
    let json_padding = (4 - (json_bytes.len() % 4)) % 4;
    let bin_padding = (4 - (bin.len() % 4)) % 4;
    let json_chunk = 8 + json_bytes.len() + json_padding;
    let bin_chunk = if bin.is_empty() {
        0
    } else {
        8 + bin.len() + bin_padding
    };
    let total = 12 + json_chunk + bin_chunk;

    let mut glb = Vec::with_capacity(total);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&((json_bytes.len() + json_padding) as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(json_bytes.as_bytes());
    glb.extend(std::iter::repeat_n(b' ', json_padding));
    if !bin.is_empty() {
        glb.extend_from_slice(&((bin.len() + bin_padding) as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(bin);
        glb.extend(std::iter::repeat_n(0u8, bin_padding));
    }
    glb
}

fn import_bytes(name: &str, glb: Vec<u8>) -> ImportResult {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, &glb).unwrap();
    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: xxhash_rust::xxh3::xxh3_64(&glb),
        settings: serde_json::Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = GltfImporter.import(&ctx).unwrap();
    std::fs::remove_file(&path).ok();
    result
}

/// 读 RMXI 的头部与顶点/索引数量。
fn rmxi_counts(data: &[u8]) -> (usize, usize) {
    assert_eq!(&data[..4], b"RMXI");
    let verts = u32::from_le_bytes(data[5..9].try_into().unwrap()) as usize;
    let idxs = u32::from_le_bytes(data[9..13].try_into().unwrap()) as usize;
    (verts, idxs)
}

/// 读 RMXI 的第 `index` 个顶点位置（顶点区紧跟 17 字节头部）。
fn rmxi_position(data: &[u8], index: usize) -> [f32; 3] {
    let base = 17 + index * 12;
    [
        f32::from_le_bytes(data[base..base + 4].try_into().unwrap()),
        f32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap()),
        f32::from_le_bytes(data[base + 8..base + 12].try_into().unwrap()),
    ]
}

/// 一个三角形的位置 + 索引（u16），返回 (bin 片段, bufferView/accessor 的偏移信息)。
fn triangle_bytes(scale: f32) -> (Vec<u8>, Vec<u8>) {
    let positions: &[f32] = &[0.0, 0.0, 0.0, scale, 0.0, 0.0, 0.0, scale, 0.0];
    let indices: &[u16] = &[0, 1, 2];
    let mut positions_bytes = Vec::new();
    for p in positions {
        positions_bytes.extend_from_slice(&p.to_le_bytes());
    }
    let mut index_bytes = Vec::new();
    for i in indices {
        index_bytes.extend_from_slice(&i.to_le_bytes());
    }
    (positions_bytes, index_bytes)
}

fn accessor(
    buffer_view: usize,
    component_type: u32,
    count: usize,
    kind: &str,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "bufferView": buffer_view,
        "componentType": component_type,
        "count": count,
        "type": kind,
    });
    // glTF 要求 POSITION 访问器带 min/max。
    if kind == "VEC3" && component_type == 5126 {
        value["min"] = serde_json::json!([-1000.0, -1000.0, -1000.0]);
        value["max"] = serde_json::json!([1000.0, 1000.0, 1000.0]);
    }
    value
}

#[test]
fn gltf_import_bakes_every_node_and_primitive_into_one_mesh() {
    // 一个网格含两个 primitive，被两个带平移的节点引用 —— 旧实现只会留下
    // 「第一个网格的第一个 primitive」在原点（#520）。
    let (pos_a, idx_a) = triangle_bytes(1.0);
    let (pos_b, idx_b) = triangle_bytes(2.0);
    let mut bin = Vec::new();
    bin.extend_from_slice(&pos_a); // 0..36
    let idx_a_offset = bin.len();
    bin.extend_from_slice(&idx_a); // 36..42
    let pad = (4 - (bin.len() % 4)) % 4;
    bin.extend(std::iter::repeat_n(0u8, pad));
    let pos_b_offset = bin.len();
    bin.extend_from_slice(&pos_b);
    let idx_b_offset = bin.len();
    bin.extend_from_slice(&idx_b);

    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "prisma-test" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 1] }],
        "nodes": [
            { "mesh": 0, "translation": [5.0, 0.0, 0.0] },
            { "mesh": 0, "translation": [0.0, 0.0, 7.0] }
        ],
        "meshes": [{
            "primitives": [
                { "attributes": { "POSITION": 0 }, "indices": 1, "material": 0 },
                { "attributes": { "POSITION": 2 }, "indices": 3, "material": 1 }
            ]
        }],
        "materials": [
            {
                "name": "Red",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 0.0, 0.0, 1.0],
                    "metallicFactor": 0.1,
                    "roughnessFactor": 0.8
                }
            },
            {
                "name": "Blue",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [0.0, 0.0, 1.0, 1.0]
                },
                "doubleSided": true
            }
        ],
        "accessors": [
            accessor(0, 5126, 3, "VEC3"),
            accessor(1, 5123, 3, "SCALAR"),
            accessor(2, 5126, 3, "VEC3"),
            accessor(3, 5123, 3, "SCALAR")
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_a_offset, "byteLength": 6 },
            { "buffer": 0, "byteOffset": pos_b_offset, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_b_offset, "byteLength": 6 }
        ],
        "buffers": [{ "byteLength": bin.len() }]
    });

    let result = import_bytes("test_multi_primitive.glb", pack_glb(json, &bin));
    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!(verts, 12, "两个节点 × 两个 primitive × 3 顶点");
    assert_eq!(idxs, 12);
    assert_eq!(
        result.output_data[4], 2,
        "多 primitive 网格必须写 RMXI v2 submesh 表"
    );

    let meta = result.metadata.unwrap();
    assert_eq!(meta["primitive_count"], 4);
    assert_eq!(meta["node_count"], 2);
    // 每个 primitive 的 index range + 材质都要留下（#520）。
    assert_eq!(meta["submesh_count"], 4);
    assert_eq!(meta["submeshes"][0]["index_start"], 0);
    assert_eq!(meta["submeshes"][0]["index_count"], 3);
    assert_eq!(meta["submeshes"][0]["material_index"], 0);
    assert_eq!(meta["submeshes"][0]["material"], "Red");
    assert_eq!(meta["submeshes"][1]["index_start"], 3);
    assert_eq!(meta["submeshes"][1]["material_index"], 1);
    assert_eq!(meta["submeshes"][1]["material"], "Blue");
    assert_eq!(meta["submeshes"][2]["index_start"], 6);
    assert_eq!(meta["material_count"], 2);
    assert_eq!(meta["materials"][0]["name"], "Red");
    assert_eq!(meta["materials"][0]["base_color"][0], 1.0);
    assert!((meta["materials"][0]["metallic"].as_f64().unwrap() - 0.1).abs() < 1e-6);
    assert!((meta["materials"][0]["roughness"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    assert_eq!(meta["materials"][1]["double_sided"], true);
    assert_eq!(meta["materials"][1]["alpha_mode"], "Opaque");

    // 第一个节点的顶点被平移了 +5，第二个节点在 z=7。
    let first = rmxi_position(&result.output_data, 0);
    assert!((first[0] - 5.0).abs() < 1e-5, "got {first:?}");
    // 第 7..9 个顶点属于第二个节点（每个节点的网格有 6 个顶点）。
    let second_node = rmxi_position(&result.output_data, 6);
    assert!((second_node[0]).abs() < 1e-5, "got {second_node:?}");
    assert!((second_node[2] - 7.0).abs() < 1e-5, "got {second_node:?}");
}

fn one_pixel_png() -> Vec<u8> {
    use image::ImageEncoder as _;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    png
}

#[test]
fn gltf_import_records_image_metadata_and_material_texture_slots() {
    let (positions, indices) = triangle_bytes(1.0);
    let png = one_pixel_png();
    let mut bin = Vec::new();
    bin.extend_from_slice(&positions); // 0..36
    let idx_offset = bin.len();
    bin.extend_from_slice(&indices); // 36..42
    let pad = (4 - (bin.len() % 4)) % 4;
    bin.extend(std::iter::repeat_n(0u8, pad));
    let image_offset = bin.len();
    bin.extend_from_slice(&png);

    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "images": [{ "bufferView": 2, "mimeType": "image/png", "name": "red" }],
        "textures": [{ "source": 0 }],
        "materials": [{
            "name": "Textured",
            "pbrMetallicRoughness": {
                "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                "baseColorTexture": { "index": 0 }
            }
        }],
        "meshes": [{
            "primitives": [
                { "attributes": { "POSITION": 0 }, "indices": 1, "material": 0 }
            ]
        }],
        "accessors": [
            accessor(0, 5126, 3, "VEC3"),
            accessor(1, 5123, 3, "SCALAR")
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6 },
            { "buffer": 0, "byteOffset": image_offset, "byteLength": png.len() }
        ],
        "buffers": [{ "byteLength": bin.len() }]
    });

    let result = import_bytes("test_material_image.glb", pack_glb(json, &bin));
    let meta = result.metadata.unwrap();
    assert_eq!(meta["images"][0]["name"], "red");
    assert_eq!(meta["images"][0]["mime_type"], "image/png");
    assert_eq!(meta["images"][0]["width"], 1);
    assert_eq!(meta["images"][0]["height"], 1);
    assert_eq!(meta["images"][0]["source_bytes"], png.len());
    assert_eq!(meta["materials"][0]["base_color_texture"], 0);
    assert_eq!(meta["submeshes"][0]["material"], "Textured");
}

#[test]
fn gltf_side_assets_cook_rmat_rtex_and_submesh_paths() {
    let (positions, indices) = triangle_bytes(1.0);
    let png = one_pixel_png();
    let mut bin = Vec::new();
    bin.extend_from_slice(&positions); // 0..36
    let idx_offset = bin.len();
    bin.extend_from_slice(&indices); // 36..42
    let pad = (4 - (bin.len() % 4)) % 4;
    bin.extend(std::iter::repeat_n(0u8, pad));
    let image_offset = bin.len();
    bin.extend_from_slice(&png);

    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "images": [{ "bufferView": 2, "mimeType": "image/png", "name": "red" }],
        "textures": [{ "source": 0 }],
        "materials": [{
            "name": "Textured",
            "pbrMetallicRoughness": {
                "baseColorFactor": [1.0, 0.0, 0.0, 1.0],
                "metallicFactor": 0.25,
                "roughnessFactor": 0.75,
                "baseColorTexture": { "index": 0 }
            }
        }],
        "meshes": [{
            "primitives": [
                { "attributes": { "POSITION": 0 }, "indices": 1, "material": 0 }
            ]
        }],
        "accessors": [
            accessor(0, 5126, 3, "VEC3"),
            accessor(1, 5123, 3, "SCALAR")
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6 },
            { "buffer": 0, "byteOffset": image_offset, "byteLength": png.len() }
        ],
        "buffers": [{ "byteLength": bin.len() }]
    });

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("textured.glb");
    std::fs::write(&source, pack_glb(json, &bin)).unwrap();

    let mesh_path = "models/textured.glb";
    let submesh_paths = GltfImporter::submesh_material_paths(&source, mesh_path).unwrap();
    assert_eq!(
        submesh_paths,
        vec!["models/textured.glb.materials/0_Textured.mat.json"]
    );

    let side = GltfImporter::side_assets(&source, mesh_path).unwrap();
    assert_eq!(side.material_paths, submesh_paths);
    assert_eq!(side.submesh_material_paths, submesh_paths);
    assert_eq!(side.assets.len(), 2, "one RMAT + one RTEX");

    let texture = side
        .assets
        .iter()
        .find(|asset| asset.path.ends_with(".png"))
        .expect("generated texture");
    assert_eq!(&texture.cooked_data[..4], b"RTEX");
    assert_eq!(texture.path, "models/textured.glb.textures/0_red.png");

    let material = side
        .assets
        .iter()
        .find(|asset| asset.path.ends_with(".mat.json"))
        .expect("generated material");
    assert_eq!(&material.cooked_data[..4], b"RMAT");
    let decoded = crate::cooker::decode_rmat(&material.cooked_data).unwrap();
    assert_eq!(decoded.scalars[0], 1.0, "base color red");
    assert_eq!(decoded.scalars[4], 0.25, "metallic");
    assert_eq!(decoded.scalars[5], 0.75, "roughness");
    assert_eq!(
        decoded.texture_ids[0],
        Some(AssetId::from_path(&texture.path)),
        "albedo slot must embed the staged texture path id"
    );
}

#[test]
fn gltf_side_assets_generate_a_default_material_for_untagged_primitives() {
    let (positions, _) = triangle_bytes(1.0);
    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 } }] }],
        "accessors": [accessor(0, 5126, 3, "VEC3")],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": 36 }],
        "buffers": [{ "byteLength": positions.len() }]
    });

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("plain.glb");
    std::fs::write(&source, pack_glb(json, &positions)).unwrap();

    let side = GltfImporter::side_assets(&source, "models/plain.glb").unwrap();
    assert_eq!(
        side.submesh_material_paths,
        vec!["models/plain.glb.materials/default.mat.json"]
    );
    assert!(side.material_paths.is_empty());
    assert_eq!(side.assets.len(), 1);
    assert_eq!(&side.assets[0].cooked_data[..4], b"RMAT");
}

#[test]
fn gltf_import_synthesises_indices_for_non_indexed_primitives() {
    let (positions, _) = triangle_bytes(1.0);
    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 } }] }],
        "accessors": [accessor(0, 5126, 3, "VEC3")],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": 36 }],
        "buffers": [{ "byteLength": positions.len() }]
    });

    let result = import_bytes("test_non_indexed.glb", pack_glb(json, &positions));
    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!(verts, 3);
    assert_eq!(idxs, 3, "非索引网格要补一条顺序索引，而不是报错");
}

#[test]
fn gltf_import_transforms_normals_with_the_inverse_transpose() {
    // 节点绕 X 轴转 90°、X 方向再放大 4 倍：法线 (0,0,1) 应该变成 (0,-1,0)，
    // 而且不能因为缩放被拉长或反向。
    let (positions, indices) = triangle_bytes(1.0);
    let normals: Vec<u8> = [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let mut bin = Vec::new();
    bin.extend_from_slice(&positions);
    let idx_offset = bin.len();
    bin.extend_from_slice(&indices);
    let pad = (4 - (bin.len() % 4)) % 4;
    bin.extend(std::iter::repeat_n(0u8, pad));
    let normal_offset = bin.len();
    bin.extend_from_slice(&normals);

    let half = std::f32::consts::FRAC_1_SQRT_2;
    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{
            "mesh": 0,
            "rotation": [half, 0.0, 0.0, half],
            "scale": [4.0, 1.0, 1.0]
        }],
        "meshes": [{
            "primitives": [{ "attributes": { "POSITION": 0, "NORMAL": 1 }, "indices": 2 }]
        }],
        "accessors": [
            accessor(0, 5126, 3, "VEC3"),
            accessor(2, 5126, 3, "VEC3"),
            accessor(1, 5123, 3, "SCALAR")
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6 },
            { "buffer": 0, "byteOffset": normal_offset, "byteLength": 36 }
        ],
        "buffers": [{ "byteLength": bin.len() }]
    });

    let result = import_bytes("test_rotated_normals.glb", pack_glb(json, &bin));
    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!((verts, idxs), (3, 3));
    assert!(result.metadata.unwrap()["has_normals"].as_bool().unwrap());

    // 头部 17 字节 + 顶点区（3 * 12）+ UV（0 通道没有，所以跳过）= 法线区。
    let normal_base = 17 + 3 * 12;
    let read = |offset: usize| {
        f32::from_le_bytes(result.output_data[offset..offset + 4].try_into().unwrap())
    };
    let normal = [
        read(normal_base),
        read(normal_base + 4),
        read(normal_base + 8),
    ];
    assert!(normal[0].abs() < 1e-5, "got {normal:?}");
    assert!((normal[1] + 1.0).abs() < 1e-5, "got {normal:?}");
    assert!(normal[2].abs() < 1e-5, "got {normal:?}");
}

#[test]
fn gltf_import_drops_normals_when_any_primitive_lacks_them() {
    let (positions, indices) = triangle_bytes(1.0);
    let normals: Vec<u8> = [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let mut bin = Vec::new();
    bin.extend_from_slice(&positions); // 0..36
    let idx_offset = bin.len();
    bin.extend_from_slice(&indices); // 36..42
    let pad = (4 - (bin.len() % 4)) % 4;
    bin.extend(std::iter::repeat_n(0u8, pad));
    let normal_offset = bin.len();
    bin.extend_from_slice(&normals);

    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{
            "primitives": [
                { "attributes": { "POSITION": 0, "NORMAL": 1 }, "indices": 2 },
                { "attributes": { "POSITION": 0 }, "indices": 2 }
            ]
        }],
        "accessors": [
            accessor(0, 5126, 3, "VEC3"),
            accessor(2, 5126, 3, "VEC3"),
            accessor(1, 5123, 3, "SCALAR")
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6 },
            { "buffer": 0, "byteOffset": normal_offset, "byteLength": 36 }
        ],
        "buffers": [{ "byteLength": bin.len() }]
    });

    let result = import_bytes("test_partial_normals.glb", pack_glb(json, &bin));
    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!(verts, 6);
    assert_eq!(idxs, 6);
    let meta = result.metadata.unwrap();
    assert_eq!(
        meta["has_normals"], false,
        "有一个 primitive 缺法线时整份网格不带法线（不拿零值冒充）"
    );
}

// -------------------------------------------------------------------
// 材质 Importer
// -------------------------------------------------------------------

#[test]
fn material_importer_accepts_mat_extensions() {
    let imp = MaterialImporter;
    assert!(imp.can_import(Path::new("plastic.mat.json")));
    assert!(imp.can_import(Path::new("plastic.mat")));
    assert!(imp.can_import(Path::new("PLASTIC.MAT")));
    // Plain .json falls through to JsonImporter.
    assert!(!imp.can_import(Path::new("data.json")));
    assert!(!imp.can_import(Path::new("data.txt")));
}

#[test]
fn material_importer_roundtrip_with_textures() {
    let imp = MaterialImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_material.mat.json");

    // Register two 纹理 资源 records in the DB so the importer can
    // 解析 their paths to AssetId dependencies.
    let mut db = AssetDatabase::new();
    let albedo_id = db.generate_id();
    let occ_id = db.generate_id();
    db.insert(crate::db::AssetRecord::new(
        albedo_id,
        "textures/albedo.png".into(),
        AssetType::Texture,
        "texture-importer",
    ))
    .unwrap();
    db.insert(crate::db::AssetRecord::new(
        occ_id,
        "textures/occlusion.png".into(),
        AssetType::Texture,
        "texture-importer",
    ))
    .unwrap();

    let json = r#"{
            "name": "test_plastic",
            "base_color": [0.9, 0.1, 0.1, 1.0],
            "metallic": 0.0,
            "roughness": 0.6,
            "emissive": [0.05, 0.0, 0.0],
            "emissive_strength": 2.0,
            "normal_scale": 1.2,
            "occlusion_strength": 0.9,
            "transmission": 0.1,
            "ior": 1.45,
            "clearcoat": 0.5,
            "albedo_tex": "textures/albedo.png",
            "occlusion_tex": "textures/occlusion.png"
        }"#;
    std::fs::write(&path, json).unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(db),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Material);
    // Two 纹理 deps resolved.
    assert_eq!(result.dependencies.len(), 2);
    assert_eq!(result.dependencies[0], albedo_id);
    assert_eq!(result.dependencies[1], occ_id);

    // Intermediate must start with RMATI magic.
    assert_eq!(&result.output_data[..5], b"RMATI");
    assert_eq!(result.output_data[5], 1); // version

    // Metadata carries the 槽 presence flags.
    let meta = result.metadata.unwrap();
    let slots = meta["texture_slots"].as_array().unwrap();
    assert_eq!(slots.len(), 5);
    assert!(slots[0].as_bool().unwrap()); // albedo
    assert!(!slots[1].as_bool().unwrap()); // normal
    assert!(slots[4].as_bool().unwrap()); // occlusion

    std::fs::remove_file(&path).ok();
}

#[test]
fn material_importer_handles_unresolved_texture() {
    // A 纹理 path not present in the DB should be dropped (warn), not
    // abort the 导入 The 材质 still imports with that 槽 空
    let imp = MaterialImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_material_missing_tex.mat.json");

    let json = r#"{
            "base_color": [0.5, 0.5, 0.5, 1.0],
            "albedo_tex": "textures/missing.png"
        }"#;
    std::fs::write(&path, json).unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Material);
    // Unresolved -> 0 deps, 材质 still imports.
    assert!(result.dependencies.is_empty());

    std::fs::remove_file(&path).ok();
}

#[test]
fn material_importer_uses_defaults_for_missing_scalars() {
    let imp = MaterialImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_material_defaults.mat.json");

    // 空 对象 -> all defaults.
    std::fs::write(&path, "{}").unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Material);
    // 78 字节 最小 (5 magic + 1 version + 72 scalars), no textures.
    assert!(result.output_data.len() >= 78);

    std::fs::remove_file(&path).ok();
}

// -------------------------------------------------------------------
// 着色器 Importer
// -------------------------------------------------------------------

#[test]
fn shader_importer_accepts_slang() {
    let imp = ShaderImporter;
    assert!(imp.can_import(Path::new("mesh_vert.slang")));
    assert!(imp.can_import(Path::new("scene_frag.SLANG")));
    assert!(!imp.can_import(Path::new("data.json")));
    assert!(!imp.can_import(Path::new("data.txt")));
}

#[test]
fn shader_importer_infers_entry_from_filename() {
    let imp = ShaderImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_vert.slang");
    std::fs::write(&path, b"// dummy shader\n").unwrap();

    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings: Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    assert_eq!(result.asset_type, AssetType::Shader);
    assert_eq!(&result.output_data[..4], b"RSLI");

    let meta = result.metadata.unwrap();
    assert_eq!(meta["entry"], "vertexMain");
    assert_eq!(meta["stage"], "vertex");
    assert_eq!(meta["profile"], "spirv_1_5");

    std::fs::remove_file(&path).ok();
}

#[test]
fn shader_importer_pt_prefix_uses_pt_main() {
    // pt_* shaders use the `ptMain` entry per compile.sh convention.
    assert_eq!(
        infer_entry_stage_from_name("pt_render"),
        Some(("ptMain", "compute"))
    );
    assert_eq!(
        infer_entry_stage_from_name("gi_bake_comp"),
        Some(("computeMain", "compute"))
    );
    assert_eq!(
        infer_entry_stage_from_name("scene_frag"),
        Some(("fragmentMain", "fragment"))
    );
    assert_eq!(infer_entry_stage_from_name("unknown"), None);
}

#[test]
fn shader_importer_respects_settings_overrides() {
    let imp = ShaderImporter;
    let dir = std::env::temp_dir();
    let path = dir.join("test_custom.slang");
    std::fs::write(&path, b"// dummy").unwrap();

    let settings = serde_json::json!({
        "slang_entry": "myEntry",
        "slang_stage": "compute",
        "slang_profile": "spirv_1_4"
    });
    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: 0,
        settings,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = imp.import(&ctx).unwrap();
    let meta = result.metadata.unwrap();
    assert_eq!(meta["entry"], "myEntry");
    assert_eq!(meta["stage"], "compute");
    assert_eq!(meta["profile"], "spirv_1_4");

    std::fs::remove_file(&path).ok();
}

// -------------------------------------------------------------------
// OBJ 导入器（#520）
// -------------------------------------------------------------------

fn import_obj(name: &str, source: &str) -> Result<ImportResult, ImportError> {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, source).unwrap();
    let ctx = ImportContext {
        source_path: path.clone(),
        source_hash: xxhash_rust::xxh3::xxh3_64(source.as_bytes()),
        settings: serde_json::Value::Null,
        db: Arc::new(AssetDatabase::new()),
    };
    let result = ObjImporter.import(&ctx);
    std::fs::remove_file(&path).ok();
    result
}

#[test]
fn obj_importer_triangulates_a_quad_and_keeps_uvs_and_normals() {
    let result = import_obj(
        "test_quad.obj",
        "\
# a quad with shared normal/uv streams
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
vt 0 0
vt 1 0
vt 1 1
vt 0 1
vn 0 0 1
f 1/1/1 2/2/1 3/3/1 4/4/1
",
    )
    .unwrap();

    assert_eq!(result.asset_type, AssetType::Mesh);
    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!((verts, idxs), (4, 6), "quad → 4 vertices, 2 triangles");
    assert_eq!(rmxi_position(&result.output_data, 0), [0.0, 0.0, 0.0]);
    assert_eq!(rmxi_position(&result.output_data, 2), [1.0, 1.0, 0.0]);

    let meta = result.metadata.unwrap();
    assert_eq!(meta["source_format"], "obj");
    assert_eq!(meta["vertex_count"], 4);
    assert_eq!(meta["index_count"], 6);
    assert_eq!(meta["has_normals"], true);
    assert_eq!(meta["has_texcoords"], true);
    assert_eq!(meta["material_count"], 0);
}

#[test]
fn obj_importer_supports_negative_indices_without_uvs() {
    let result = import_obj(
        "test_negative.obj",
        "\
v 0 0 0
v 1 0 0
v 0 1 0
vn 0 0 1
f -3//1 -2//1 -1//1
",
    )
    .unwrap();

    let (verts, idxs) = rmxi_counts(&result.output_data);
    assert_eq!((verts, idxs), (3, 3));
    let meta = result.metadata.unwrap();
    assert_eq!(meta["has_normals"], true);
    assert_eq!(meta["has_texcoords"], false);
}

#[test]
fn obj_importer_deduplicates_shared_vertices() {
    let result = import_obj(
        "test_shared.obj",
        "\
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
vn 0 0 1
f 1//1 2//1 3//1
f 1//1 3//1 4//1
",
    )
    .unwrap();

    let (verts, idxs) = rmxi_counts(&result.output_data);
    // 四个位置各出现一次；两个三角形共享 1 和 3。
    assert_eq!((verts, idxs), (4, 6));
}

#[test]
fn obj_importer_rejects_files_without_faces_and_out_of_range_indices() {
    let empty = import_obj("test_empty.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\n").unwrap_err();
    assert!(
        matches!(&empty, ImportError::ImportFailed(message) if message.contains("no faces")),
        "{empty:?}"
    );

    let out_of_range = import_obj(
        "test_out_of_range.obj",
        "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 4\n",
    )
    .unwrap_err();
    assert!(
        matches!(&out_of_range, ImportError::ImportFailed(message) if message.contains("out of range")),
        "{out_of_range:?}"
    );

    let nan = import_obj("test_nan.obj", "v NaN 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap_err();
    assert!(
        matches!(&nan, ImportError::ImportFailed(message) if message.contains("not finite")),
        "{nan:?}"
    );
}

#[test]
fn default_registry_routes_obj_to_the_obj_importer() {
    let registry = default_importer_registry();
    let path = Path::new("model.OBJ");
    let importer = registry
        .find_for_path(path)
        .expect("OBJ should have an importer");
    assert_eq!(importer.name(), "obj-importer");
    assert!(ObjImporter.can_import(path));
    assert!(!ObjImporter.can_import(Path::new("model.gltf")));
}
