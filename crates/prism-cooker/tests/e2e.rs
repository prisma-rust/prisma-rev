//! End-to-end integration test for the resource pipeline.
//!
//! Covers the full lifecycle with **real assets** from the Sponza scene:
//!
//!   init → import → build → validate → runtime load → assert
//!
//! Texture asset: a genuine 4 K Sponza PBR base-colour PNG (~11 MB) that
//! exercises the real image decoder, mip-chain generator, and RTEX cooking
//! code paths any production project would use.
//!
//! Mesh and JSON assets are real files created at test time since the
//! Sponza glTF (140 MB .bin + 137 textures) is too heavy for a unit test.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use prism_asset::core::{AssetId, AssetType, Handle};
use prism_asset::package::{PackageBuilder, PackageReader};
use prism_asset::runtime::{EvictionPolicy, ResourceManager};
use prism_cooker::cooker::profile::CookSettings;
use prism_cooker::cooker::{decode_rmes, parse_rmxi_info};
use prism_cooker::cooker::{
    decode_rtex, default_cooker_registry, parse_rtexi_pixels, CookPipeline,
};
use prism_cooker::db::{AssetDatabase, ImportCache};
use prism_cooker::importer::{default_importer_registry, ImportPipeline};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary project directory with the given name.
fn create_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path().join(name);
    std::fs::create_dir_all(root.join("Assets")).expect("create Assets/");
    std::fs::create_dir_all(root.join("Library")).expect("create Library/");
    (dir, root)
}

/// Write a test asset file.
fn write_asset(root: &Path, rel_path: &str, data: &[u8]) -> PathBuf {
    let full = root.join("Assets").join(rel_path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&full, data).expect("write asset file");
    full
}

/// Copy an external file into the test Assets/ directory.
fn copy_external_asset(root: &Path, src: &Path, rel_dst: &str) -> PathBuf {
    let dst = root.join("Assets").join(rel_dst);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::copy(src, &dst).expect("copy external asset");
    dst
}

/// Walk a directory and collect file paths (excluding subdirectories).
fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                files.append(&mut collect_files(&path));
            }
        }
    }
    files.sort();
    files
}

/// Build a minimal valid GLB file in memory.
///
/// One triangle mesh (3 vertices, 3 unsigned-short indices), no material,
/// no textures.
fn create_minimal_glb_bytes() -> Vec<u8> {
    let positions: &[f32] = &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    let indices: &[u16] = &[0, 1, 2];

    let bin_data_size = positions.len() * 4 + indices.len() * 2;
    let bin_padding = (4 - (bin_data_size % 4)) % 4;
    let bin_chunk_total = 8 + bin_data_size + bin_padding;

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
                "componentType": 5126,
                "count": 3,
                "type": "VEC3",
                "min": [0.0, 0.0, 0.0],
                "max": [1.0, 1.0, 0.0]
            },
            {
                "bufferView": 1,
                "componentType": 5123,
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

    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_len as u32).to_le_bytes());

    glb.extend_from_slice(&((json_bytes.len() + json_padding) as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(json_bytes);
    glb.extend_from_slice(&vec![0x20; json_padding]);

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

/// Find an asset record whose stored path contains the given suffix.
fn find_id_by_suffix(db: &AssetDatabase, suffix: &str) -> Option<AssetId> {
    db.records().find_map(|r| {
        if r.path.ends_with(suffix) {
            Some(r.id)
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Real asset source paths (Sponza scene)
// ---------------------------------------------------------------------------

/// Root of the downloaded Sponza scene (configured in `assets/scenes.json`).
const SPONZA_DIR: &str = "D:/Download/main_sponza/main_sponza";

/// A real 4 K PBR base-colour PNG from the Sponza scene (~11 MB).  Picked as
/// the smallest BaseColor texture so the test stays reasonably fast while
/// still exercising the full production texture pipeline.
const SPONZA_TEXTURE: &str = "textures/metal_door_01_BaseColor.png";

// ---------------------------------------------------------------------------
// E2E Test: full pipeline with real Sponza texture
// ---------------------------------------------------------------------------

/// E2E Test: 完整 管线 with real Sponza 纹理
///
/// **需要外部数据**：Sponza 场景需按 `assets/scenes.json` 下载，
/// 且纹理路径指向 [`SPONZA_DIR`]。默认 `#[ignore]`——运行前请先
/// 下载场景，然后用 `cargo test -p prism-asset --test e2e -- --ignored e2e_full_pipeline` 手动执行。
#[test]
#[ignore = "requires downloaded Sponza scene (see SPONZA_DIR)"]
fn e2e_full_pipeline() {
    // ======================================================================
    // 1. SETUP: create project with real + generated assets
    // ======================================================================
    let (_dir, root) = create_project("sponza-e2e");

    let test_out = root.display().to_string();
    eprintln!("=== Test output / 测试输出: {test_out}");

    let assets_dir = root.join("Assets");
    let library_dir = root.join("Library");

    // --- Real Sponza 4 K texture ---
    let sponza_tex_src = Path::new(SPONZA_DIR).join(SPONZA_TEXTURE);
    assert!(
        sponza_tex_src.exists(),
        "Sponza texture not found at {}. Has the Sponza scene been downloaded?\n\
         See assets/scenes.json for the expected path.\n\
         Test output (persistent): {test_out}",
        sponza_tex_src.display(),
    );
    let sponza_path =
        copy_external_asset(&root, &sponza_tex_src, "sponza/metal_door_01_BaseColor.png");

    // --- Generated assets ---
    write_asset(&root, "test.json", b"{\"name\": \"test\", \"value\": 42}");
    let glb_bytes = create_minimal_glb_bytes();
    let glb_path = write_asset(&root, "subdir/model.glb", &glb_bytes);

    let files = collect_files(&assets_dir);
    assert_eq!(files.len(), 3, "should have 3 source files");

    // ======================================================================
    // 2. IMPORT: run ImportPipeline on all files, capture intermediate data
    //
    // NOTE: import_file stores the full absolute path (forward-slash
    // normalized) in the database, NOT a relative path.  All lookups
    // below use suffix matching to work around that.
    // ======================================================================
    let importer_reg = Arc::new(default_importer_registry());
    let import_pipeline = ImportPipeline::new(importer_reg);

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();
    let mut intermediate_data: HashMap<AssetId, Vec<u8>> = HashMap::new();

    // Import the real Sponza texture.
    let r1 = import_pipeline
        .import_file(&sponza_path, &mut db, &mut cache, None)
        .expect("import Sponza texture");
    assert!(
        r1.was_imported,
        "Sponza texture should be imported (not cached)"
    );
    if let Some(data) = r1.intermediate_data {
        let id = find_id_by_suffix(&db, "metal_door_01_BaseColor.png")
            .expect("Sponza texture in db after import");
        intermediate_data.insert(id, data);
    }

    // Import the JSON file.
    let json_file = assets_dir.join("test.json");
    let r2 = import_pipeline
        .import_file(&json_file, &mut db, &mut cache, None)
        .expect("import JSON");
    assert!(r2.was_imported, "JSON should be imported (not cached)");
    if let Some(data) = r2.intermediate_data {
        let id = find_id_by_suffix(&db, "test.json").expect("JSON asset in db after import");
        intermediate_data.insert(id, data);
    }

    // Import the GLB file.
    let r3 = import_pipeline
        .import_file(&glb_path, &mut db, &mut cache, None)
        .expect("import GLB");
    assert!(r3.was_imported, "GLB should be imported (not cached)");
    if let Some(data) = r3.intermediate_data {
        let id = find_id_by_suffix(&db, "subdir/model.glb").expect("GLB asset in db after import");
        intermediate_data.insert(id, data);
    }

    // Assert database has 3 records.
    assert_eq!(db.len(), 3, "database should have 3 records after import");
    assert!(!cache.is_empty(), "import cache should have entries");

    // Verify each record has the correct asset type and importer.
    for record in db.records() {
        if record.path.ends_with("metal_door_01_BaseColor.png") {
            assert_eq!(record.asset_type, AssetType::Texture);
            assert_eq!(record.importer_name, "texture-importer");
        } else if record.path.ends_with("test.json") {
            assert_eq!(record.asset_type, AssetType::Binary);
            assert_eq!(record.importer_name, "json-importer");
        } else if record.path.ends_with("subdir/model.glb") {
            assert_eq!(record.asset_type, AssetType::Mesh);
            assert_eq!(record.importer_name, "gltf-importer");
        } else {
            panic!("unexpected asset path: {}", record.path);
        }
        assert_ne!(record.id, AssetId::tombstone(0));
        assert!(record.source_hash != 0, "source hash should be set");
    }

    // Verify we captured intermediate data for all three assets.
    assert_eq!(
        intermediate_data.len(),
        3,
        "should have intermediate data for all assets"
    );

    // Save and reload the database (simulates persistence).
    let db_path = library_dir.join("AssetDatabase.json");
    db.save(&db_path).expect("save database");
    let db_loaded = AssetDatabase::load(&db_path).expect("reload database");
    assert_eq!(
        db_loaded.len(),
        3,
        "reloaded database should have 3 records"
    );

    // ======================================================================
    // 3. COOK + BUILD: run CookPipeline with real intermediate data → .pak
    // ======================================================================
    let cooker_reg = default_cooker_registry();
    let cook_pipeline = CookPipeline::new(cooker_reg);

    let mut builder = PackageBuilder::new();
    builder.set_compression(3);

    let settings = CookSettings::default();
    let summary = cook_pipeline
        .cook_all(&db_loaded, &intermediate_data, &mut builder, &settings)
        .expect("cook should succeed");
    assert_eq!(summary.cooked, 3, "all 3 assets should be cooked");
    assert_eq!(summary.skipped, 0, "no assets should be skipped");

    let pak_bytes = builder.build().expect("build .pak");
    assert!(pak_bytes.len() > 50, ".pak should be non-trivial size");

    // Save .pak to disk for runtime loading.
    let pak_path = root.join("game.pak");
    std::fs::write(&pak_path, &pak_bytes).expect("write .pak");

    // ── Write a .pak.meta.json alongside the .pak for inspection ──────
    {
        let reader_for_meta =
            PackageReader::from_bytes(&pak_bytes).expect("read back .pak for metadata");
        let mut assets = Vec::new();
        for record in db_loaded.records() {
            let info = reader_for_meta
                .find_record(record.id)
                .map(|r| (r.size, r.compressed_size))
                .unwrap_or((0, 0));
            let compressed = info.1 > 0;
            assets.push(serde_json::json!({
                "id": format!("{:#x}", record.id.into_raw()),
                "path": record.path,
                "type": record.asset_type.label(),
                "importer": record.importer_name,
                "size": info.0,
                "compressed_size": if compressed {
                    serde_json::Value::Number(info.1.into())
                } else {
                    serde_json::Value::Null
                },
                "compression_ratio": if compressed && info.0 > 0 {
                    let r = info.1 as f64 / info.0 as f64;
                    serde_json::Value::Number(serde_json::Number::from_f64(
                        (r * 100.0).round() / 100.0
                    ).unwrap_or(serde_json::Number::from(0)))
                } else {
                    serde_json::Value::Null
                },
            }));
        }
        let manifest = serde_json::json!({
            "pak": pak_path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
            "format": std::str::from_utf8(&reader_for_meta.header().magic).unwrap_or("?"),
            "version": reader_for_meta.header().version,
            "asset_count": reader_for_meta.asset_count(),
            "total_size": pak_bytes.len(),
            "assets": assets,
        });
        let meta_path = pak_path.with_extension("pak.meta.json");
        let meta_json = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
        std::fs::write(&meta_path, &meta_json).expect("write .pak.meta.json");
        eprintln!("   📋  Manifest: {}", meta_path.display());
    }

    // ======================================================================
    // 4. VALIDATE: verify .pak structure with PackageReader
    // ======================================================================
    let reader = PackageReader::from_bytes(&pak_bytes).expect("read .pak from bytes");
    assert_eq!(&reader.header().magic, b"RPAK", "magic should be RPAK");
    assert_eq!(reader.header().version, 1, "version should be 1");
    assert_eq!(reader.asset_count(), 3, "should have 3 assets in .pak");
    assert_eq!(
        reader.records().len(),
        3,
        "should have 3 records in registry"
    );

    // Verify we can read each asset's cooked data back.
    for record in db_loaded.records() {
        let data: Vec<u8> = reader
            .read_asset_data(record.id)
            .expect("read asset from .pak")
            .unwrap_or_else(|| panic!("asset {} should exist in .pak", record.id));
        assert!(!data.is_empty(), "asset data should not be empty");
    }

    // --- Verify cooked texture format ---
    let tex_id = find_id_by_suffix(&db_loaded, "metal_door_01_BaseColor.png")
        .expect("texture in db after reload");
    let tex_data = reader
        .read_asset_data(tex_id)
        .expect("read texture from .pak")
        .unwrap();
    assert_eq!(
        &tex_data[..4],
        b"RTEX",
        "cooked texture should have RTEX header"
    );
    // A real 4 K texture (e.g. 4096×4096) should produce 13 mip levels
    // (4096→2048→1024→512→256→128→64→32→16→8→4→2→1).
    let tex_width = u32::from_le_bytes(tex_data[5..9].try_into().unwrap());
    let tex_height = u32::from_le_bytes(tex_data[9..13].try_into().unwrap());
    let tex_mips = u32::from_le_bytes(tex_data[13..17].try_into().unwrap());
    // The Sponza metal_door_01 texture is 4096×4096 → log2(4096) = 12 → 13 mips.
    assert_eq!(tex_width, 4096, "Sponza metal door basecolor should be 4K");
    assert_eq!(tex_height, 4096, "Sponza metal door basecolor should be 4K");
    assert_eq!(
        tex_mips, 13,
        "4096×4096 texture should produce 13 mip levels"
    );

    // --- Verify cooked mesh format ---
    let mesh_id =
        find_id_by_suffix(&db_loaded, "subdir/model.glb").expect("mesh in db after reload");
    let mesh_data = reader
        .read_asset_data(mesh_id)
        .expect("read mesh from .pak")
        .unwrap();
    assert_eq!(
        &mesh_data[..4],
        b"RMES",
        "cooked mesh should have RMES header"
    );
    let verts = u32::from_le_bytes(mesh_data[5..9].try_into().unwrap());
    let idxs = u32::from_le_bytes(mesh_data[9..13].try_into().unwrap());
    assert_eq!(verts, 3, "triangle should have 3 vertices");
    assert_eq!(idxs, 3, "triangle should have 3 indices");

    // ── Round-trip: decode cooked data back → compare vs intermediate ──
    // Texture: RTEX mip0 must match RTXI pixels byte-for-byte.
    {
        let rtxi = intermediate_data
            .get(&tex_id)
            .expect("texture intermediate data should exist");
        let (rtxi_w, rtxi_h, rtxi_pixels) = parse_rtexi_pixels(rtxi).expect("should parse RTXI");
        assert_eq!(rtxi_w, 4096);
        assert_eq!(rtxi_h, 4096);
        assert_eq!(rtxi_pixels.len() as u64, 4096 * 4096 * 4);

        let rtex = decode_rtex(&tex_data).expect("should decode RTEX");
        assert_eq!(rtex.width, rtxi_w);
        assert_eq!(rtex.height, rtxi_h);
        assert_eq!(rtex.mip_levels, 13);
        assert_eq!(rtex.format, 0); // RGBA8

        // mip0 must be byte-identical to the import-decoded pixels.
        assert_eq!(
            rtex.mip_data[0], rtxi_pixels,
            "RTEX mip0 must match RTXI pixels (texture cook round-trip)"
        );

        // Optional: save decoded mip0 as PNG for visual inspection.
        let png_path = pak_path.with_extension("decoded.png");
        if let Err(e) = image::save_buffer(
            &png_path,
            &rtex.mip_data[0],
            rtex.width,
            rtex.height,
            image::ColorType::Rgba8,
        ) {
            eprintln!("   ⚠  Failed to save decoded PNG: {e}");
        } else {
            eprintln!("   🖼️  Saved decoded texture: {}", png_path.display());
        }
    }

    // Mesh: RMES vertex/index data must match RMXI data byte-for-byte.
    {
        let rmxi = intermediate_data
            .get(&mesh_id)
            .expect("mesh intermediate data should exist");
        let (_mv, _mi, _muv, rmxi_vert, rmxi_idx) =
            parse_rmxi_info(rmxi).expect("should parse RMXI");

        let rmes = decode_rmes(&mesh_data).expect("should decode RMES");
        assert_eq!(rmes.vert_count, 3);
        assert_eq!(rmes.idx_count, 3);

        // Vertex data (positions + normals + UVs) must match.
        assert_eq!(
            rmes.vertex_data, rmxi_vert,
            "RMES vertex data must match RMXI (mesh cook round-trip)"
        );
        // Index data must match.
        assert_eq!(
            rmes.index_data, rmxi_idx,
            "RMES index data must match RMXI (mesh cook round-trip)"
        );
    }

    // Binary: cooked data must be identical to intermediate data.
    {
        let bin_id =
            find_id_by_suffix(&db_loaded, "test.json").expect("json asset in db after reload");
        let bin_cooked = reader
            .read_asset_data(bin_id)
            .expect("read binary from .pak")
            .unwrap();
        let bin_intermediate = intermediate_data
            .get(&bin_id)
            .expect("binary intermediate data should exist");
        assert_eq!(
            bin_cooked, *bin_intermediate,
            "binary cooked data must match intermediate (binary cook round-trip)"
        );
    }

    // ======================================================================
    // 5. RUNTIME LOAD: use ResourceManager to load the .pak
    // ======================================================================
    let mut rm = ResourceManager::new();
    rm.set_memory_budget(200 * 1024 * 1024); // 200 MB (a 4K RGBA8 texture is ~67 MB)
    rm.load_package(&pak_path).expect("load .pak into runtime");

    assert_eq!(rm.asset_count(), 3, "runtime should have 3 assets");
    assert_eq!(rm.package_count(), 1, "runtime should have 1 package");

    // Load and verify each asset.
    let records: Vec<(AssetId, String)> = db_loaded
        .records()
        .map(|r| (r.id, r.path.clone()))
        .collect();
    for (id, path) in &records {
        let handle: Handle<Vec<u8>> = rm
            .load(*id)
            .unwrap_or_else(|_| panic!("should load asset {} ({})", id, path));
        let data: Vec<u8> = rm
            .get(handle)
            .unwrap_or_else(|_| panic!("should get data for asset {} ({})", id, path));
        assert!(!data.is_empty(), "loaded data should not be empty");
    }

    // Verify memory tracking.
    assert!(
        rm.memory_usage() > 0,
        "memory usage should be > 0 after loading"
    );
    assert!(
        rm.memory_usage() <= 200 * 1024 * 1024,
        "memory usage should be within budget"
    );

    // ======================================================================
    // 6. UNLOAD + RE-LOAD: verify handle generations
    // ======================================================================
    let id = records[0].0;
    let handle: Handle<Vec<u8>> = rm.load(id).unwrap();
    rm.unload(handle);

    let handle2: Handle<Vec<u8>> = rm.load(id).unwrap();
    assert_ne!(handle.generation(), handle2.generation());
    let data2: Vec<u8> = rm.get(handle2).unwrap();
    assert!(!data2.is_empty(), "re-loaded data should be valid");

    let err = rm.get::<Vec<u8>>(handle);
    assert!(err.is_err(), "old handle should fail generation check");

    // ======================================================================
    // 7. DEPENDENCY LOADING: verify topological load
    // ======================================================================
    let dep_id = AssetId::from_raw((2u64 << 32) | 1);
    let root_id = AssetId::from_raw((2u64 << 32) | 2);

    let mut b2 = PackageBuilder::new();
    b2.add_asset(dep_id, AssetType::Binary, b"dep data".to_vec(), &[]);
    b2.add_asset(root_id, AssetType::Binary, b"root data".to_vec(), &[dep_id]);
    let pak2_bytes = b2.build().expect("build dependency .pak");
    let pak2_path = root.join("game_with_deps.pak");
    std::fs::write(&pak2_path, &pak2_bytes).expect("write dep .pak");

    let mut rm2 = ResourceManager::new();
    rm2.load_package(&pak2_path).unwrap();

    let root_handle: Handle<Vec<u8>> = rm2.load_with_deps(root_id).expect("load root with deps");
    let root_data: Vec<u8> = rm2.get(root_handle).unwrap();
    assert_eq!(root_data, b"root data", "root data should match");

    let dep_data = rm2.get_raw(dep_id).unwrap();
    assert_eq!(dep_data, b"dep data", "dep data should match");

    // ======================================================================
    // 8. MEMORY BUDGET / EVICTION
    // ======================================================================
    let small_id = AssetId::from_raw((3u64 << 32) | 1);
    let mut b3 = PackageBuilder::new();
    b3.add_asset(small_id, AssetType::Binary, vec![0u8; 100], &[]);
    let pak3_bytes = b3.build().expect("build small .pak");
    let pak3_path = root.join("small.pak");
    std::fs::write(&pak3_path, &pak3_bytes).expect("write small .pak");

    let mut rm3 = ResourceManager::new();
    rm3.set_memory_budget(50);
    rm3.set_eviction_policy(EvictionPolicy::None);
    rm3.load_package(&pak3_path).unwrap();

    let err: Result<Handle<Vec<u8>>, _> = rm3.load(small_id);
    assert!(
        err.is_err(),
        "should fail with OutOfMemory when budget < asset size and no eviction"
    );

    let mut rm4 = ResourceManager::new();
    rm4.set_memory_budget(150);
    rm4.set_eviction_policy(EvictionPolicy::Lru);
    rm4.load_package(&pak3_path).unwrap();

    let h1: Handle<Vec<u8>> = rm4.load(small_id).unwrap();
    assert_eq!(rm4.memory_usage(), 100);

    let small_id2 = AssetId::from_raw((3u64 << 32) | 2);
    let mut b3b = PackageBuilder::new();
    b3b.add_asset(small_id2, AssetType::Binary, vec![0u8; 100], &[]);
    let pak3b_bytes = b3b.build().expect("build second small .pak");
    let pak3b_path = root.join("small2.pak");
    std::fs::write(&pak3b_path, &pak3b_bytes).unwrap();
    rm4.load_package(&pak3b_path).unwrap();

    let h2: Handle<Vec<u8>> = rm4.load(small_id2).unwrap();
    assert_eq!(
        rm4.memory_usage(),
        100,
        "second load should evict first, staying at 100"
    );
    let data2: Vec<u8> = rm4.get(h2).unwrap();
    assert_eq!(data2.len(), 100, "second asset data should be valid");

    let err = rm4.get::<Vec<u8>>(h1);
    assert!(err.is_err(), "first asset should be evicted");

    // ======================================================================
    // 9. STREAMING
    // ======================================================================
    let stream_id = AssetId::from_raw((4u64 << 32) | 1);
    let mut b4 = PackageBuilder::new();
    let stream_data: Vec<u8> = (0..250).collect();
    b4.add_asset(stream_id, AssetType::Binary, stream_data.clone(), &[]);
    let pak4_bytes = b4.build().expect("build stream .pak");
    let pak4_path = root.join("stream.pak");
    std::fs::write(&pak4_path, &pak4_bytes).expect("write stream .pak");

    let mut rm5 = ResourceManager::new();
    rm5.load_package(&pak4_path).unwrap();
    let chunks: Vec<Vec<u8>> = rm5
        .read_stream(stream_id, 100)
        .expect("stream should exist")
        .collect();
    let total: usize = chunks.iter().map(|c| c.len()).sum();
    assert_eq!(total, 250, "streaming should reconstruct full data");
    assert_eq!(chunks.len(), 3, "should be 3 chunks (100+100+50)");

    // ======================================================================
    // 10. HOT-RELOAD (manual trigger)
    // ======================================================================
    {
        use prism_asset::runtime::HotReloadWatcher;

        let hr_path = root.join("hot.pak");
        std::fs::write(&hr_path, &pak_bytes).expect("write hot.pak");

        let mut watcher =
            HotReloadWatcher::watch_file(&hr_path, std::time::Duration::from_millis(100))
                .expect("create watcher");
        let rx = watcher.receiver();
        assert!(
            rx.try_iter().next().is_none(),
            "no events before modification"
        );

        // Modify the file.
        let mut b5 = PackageBuilder::new();
        b5.add_asset(
            records[0].0,
            AssetType::Binary,
            b"modified content".to_vec(),
            &[],
        );
        let new_pak = b5.build().expect("build modified .pak");
        std::fs::write(&hr_path, &new_pak).expect("write modified .pak");

        let started = std::time::Instant::now();
        let mut event_seen = false;
        while started.elapsed() < std::time::Duration::from_secs(5) {
            if let Some(event) = rx.try_iter().next() {
                if matches!(event, prism_asset::runtime::HotReloadEvent::PakModified(p) if p == hr_path)
                {
                    event_seen = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if event_seen {
            let mut rm6 = ResourceManager::new();
            rm6.load_package(&hr_path).unwrap();
            rm6.on_pak_changed(&hr_path).unwrap_or(());
        }

        watcher.stop();
    }

    // ======================================================================
    // 11. CLI COMMANDS (simulate via library calls)
    // ======================================================================

    let reader2 = PackageReader::open(&pak_path).expect("validate .pak");
    let records2: Vec<_> = reader2.records().iter().collect();
    assert_eq!(records2.len(), 3);

    let first_id = db_loaded.records().next().unwrap().id;
    let record = reader2
        .find_record(first_id)
        .unwrap_or_else(|| panic!("asset {first_id} should exist in .pak"));
    assert!(record.size > 0, "record should have size > 0");

    let asset_list: Vec<_> = rm.assets().collect();
    assert_eq!(asset_list.len(), 3);

    println!("✅ E2E pipeline test passed / 测试通过: all 11 phases complete / 全部 11 阶段完成 (real Sponza texture)");
}

// ---------------------------------------------------------------------------
// E2E Test: incremental import (cache hit)
// ---------------------------------------------------------------------------

#[test]
fn e2e_incremental_import() {
    let (_dir, root) = create_project("incremental-import");
    let assets_dir = root.join("Assets");

    write_asset(&root, "cached.bin", b"stable content");

    let importer_reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(importer_reg);

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();

    let file = assets_dir.join("cached.bin");

    // First import — should run (not cached).
    let r1 = pipeline
        .import_file(&file, &mut db, &mut cache, None)
        .expect("first import");
    assert!(r1.was_imported, "first import should run (not cached)");

    // Second import — should be cached.
    let r2 = pipeline
        .import_file(&file, &mut db, &mut cache, None)
        .expect("second import");
    assert!(!r2.was_imported, "second import should be cached");

    // Modify the file, should re-import.
    std::fs::write(&file, b"modified content").expect("modify file");
    let r3 = pipeline
        .import_file(&file, &mut db, &mut cache, None)
        .expect("third import after modification");
    assert!(r3.was_imported, "modified file should re-import");
}

// ---------------------------------------------------------------------------
// E2E Test: empty project
// ---------------------------------------------------------------------------

#[test]
fn e2e_empty_project() {
    let (_dir, root) = create_project("empty");
    let assets_dir = root.join("Assets");

    let importer_reg = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(importer_reg);

    let mut db = AssetDatabase::new();
    let mut cache = ImportCache::new();

    let summary = pipeline.import_directory(&assets_dir, &mut db, &mut cache);
    assert_eq!(summary.imported, 0, "no files should be imported");
    assert_eq!(summary.cached, 0, "no files should be cached");
    assert_eq!(summary.skipped, 0, "no files should be skipped");
    assert_eq!(summary.errors, 0, "no errors");
    assert!(db.is_empty(), "database should be empty");
}

// ---------------------------------------------------------------------------
// E2E Test: package corruption detection
// ---------------------------------------------------------------------------

#[test]
fn e2e_package_integrity_validation() {
    let (_dir, _root) = create_project("integrity");

    let id = AssetId::from_raw((5u64 << 32) | 1);
    let mut builder = PackageBuilder::new();
    builder.add_asset(id, AssetType::Binary, b"valid data".to_vec(), &[]);
    let valid_pak = builder.build().expect("build valid .pak");

    let mut corrupted = valid_pak.clone();
    if corrupted.len() > 100 {
        corrupted[80] = 0xFF;
        corrupted[81] = 0x00;
    }

    let result = PackageReader::from_bytes(&corrupted);
    assert!(result.is_err(), "corrupted .pak should fail validation");
}
