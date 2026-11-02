//! Source → import → cook → `.pak` → runtime round-trip harness.
//!
//! The harness owns an isolated temporary project. Tests write source files,
//! import them with the real pipelines, cook them into a `.pak`, then load the
//! cooked bytes through `prism-asset` and compare decoded data / metadata.

#![allow(dead_code)] // Reusable helper surface; individual scenarios use a subset.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use prism_asset::core::{AssetId, AssetType};
use prism_asset::package::{PackageBuilder, PackageReader};
use prism_asset::runtime::{MeshAsset, ResourceManager, SceneAsset, TextureAsset};
use prism_cooker::cooker::profile::CookSettings;
use prism_cooker::cooker::scene::SceneCooker;
use prism_cooker::{
    default_cooker_registry, default_importer_registry, AssetDatabase, AssetRecord, CookPipeline,
    ImportCache, ImportPipeline,
};
use prism_scene::rscn::DecodedScene;

/// Summary of a cook + package run.
#[derive(Debug, Clone)]
pub struct CookReport {
    pub cooked: u32,
    pub skipped: u32,
    pub streamed: u32,
    pub pak_entries: usize,
    pub pak_bytes: usize,
}

/// An isolated project plus the import/cook state for one scenario.
pub struct AssetHarness {
    dir: tempfile::TempDir,
    database: AssetDatabase,
    cache: ImportCache,
    importer: ImportPipeline,
    intermediate: HashMap<AssetId, Vec<u8>>,
    ids: HashMap<String, AssetId>,
}

impl AssetHarness {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create asset harness temp dir");
        fs::create_dir_all(dir.path().join("assets")).expect("create assets dir");
        Self {
            dir,
            database: AssetDatabase::new(),
            cache: ImportCache::new(),
            importer: ImportPipeline::new(Arc::new(default_importer_registry())),
            intermediate: HashMap::new(),
            ids: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.dir.path().join("assets")
    }

    pub fn source_path(&self, relative: &str) -> PathBuf {
        self.assets_dir().join(relative)
    }

    /// Write a source file under `assets/` and return its absolute path.
    pub fn write_source(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.source_path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create source directory");
        }
        fs::write(&path, bytes).expect("write source file");
        path
    }

    /// Write a PNG fixture under `assets/`.
    pub fn write_png(&self, relative: &str, width: u32, height: u32, rgba: &[u8]) {
        let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .expect("RGBA buffer matches dimensions");
        let path = self.source_path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create texture directory");
        }
        image.save(&path).expect("write PNG fixture");
    }

    /// Import one source file and remember its intermediate bytes.
    ///
    /// `force = true` keeps repeated harness runs independent of the import
    /// cache, which is what a clean build would do.
    pub fn import(&mut self, relative: &str) -> Result<AssetId, String> {
        let path = self.source_path(relative);
        let result = self
            .importer
            .import_file_with_key(
                &path,
                relative,
                &mut self.database,
                &mut self.cache,
                None,
                true,
            )
            .map_err(|error| format!("import {relative}: {error}"))?;
        let id = self
            .database
            .id_by_path(relative)
            .ok_or_else(|| format!("import {relative}: no database record"))?;
        if let Some(data) = result.intermediate_data {
            self.intermediate.insert(id, data);
        }
        self.ids.insert(relative.to_string(), id);
        Ok(id)
    }

    /// Stage an author-format scene document for the `SceneCooker`.
    ///
    /// Calling this again with the same path reuses the `AssetId`, so tests can
    /// recook after a component/schema change and keep stable package entries.
    pub fn stage_scene(&mut self, relative: &str, json: &str) -> Result<AssetId, String> {
        let id = self
            .ids
            .get(relative)
            .copied()
            .or_else(|| self.database.id_by_path(relative))
            .unwrap_or_else(|| self.database.generate_id());
        let record = AssetRecord::new(id, relative.to_string(), AssetType::Scene, "scene-json");
        self.database
            .insert(record)
            .map_err(|error| format!("stage {relative}: {error}"))?;
        self.intermediate.insert(id, json.as_bytes().to_vec());
        self.ids.insert(relative.to_string(), id);
        Ok(id)
    }

    /// Replace the staged scene bytes and keep the same asset id.
    pub fn recook_scene(&mut self, relative: &str, json: &str) -> Result<AssetId, String> {
        self.stage_scene(relative, json)
    }

    /// Cook every staged asset and write a fresh `.pak`.
    pub fn cook(&mut self) -> Result<CookReport, String> {
        let mut builder = PackageBuilder::new();
        let pipeline = CookPipeline::new(default_cooker_registry());
        let summary = pipeline
            .cook_all(
                &self.database,
                &self.intermediate,
                &mut builder,
                &CookSettings::default(),
            )
            .map_err(|error| format!("cook: {error}"))?;
        let bytes = builder
            .build()
            .map_err(|error| format!("package: {error}"))?;
        fs::write(self.pak_path(), &bytes).map_err(|error| format!("write pak: {error}"))?;
        let reader =
            PackageReader::from_bytes(&bytes).map_err(|error| format!("read pak: {error}"))?;
        Ok(CookReport {
            cooked: summary.cooked,
            skipped: summary.skipped,
            streamed: summary.streamed,
            pak_entries: reader.asset_count(),
            pak_bytes: bytes.len(),
        })
    }

    pub fn pak_path(&self) -> PathBuf {
        self.dir.path().join("cooked.pak")
    }

    pub fn pak_bytes(&self) -> Vec<u8> {
        fs::read(self.pak_path()).expect("read cooked pak")
    }

    pub fn id(&self, relative: &str) -> AssetId {
        *self
            .ids
            .get(relative)
            .unwrap_or_else(|| panic!("asset {relative:?} was never staged/imported"))
    }

    /// Open the cooked package through the runtime loader.
    pub fn open_pak(&self) -> Result<ResourceManager, String> {
        let mut manager = ResourceManager::new();
        manager
            .load_package(self.pak_path())
            .map_err(|error| format!("load pak: {error}"))?;
        Ok(manager)
    }

    pub fn load_texture(
        &self,
        manager: &mut ResourceManager,
        relative: &str,
    ) -> Result<TextureAsset, String> {
        let handle = manager
            .load::<TextureAsset>(self.id(relative))
            .map_err(|error| format!("load texture {relative}: {error}"))?;
        manager
            .get(handle)
            .map_err(|error| format!("decode texture {relative}: {error}"))
    }

    pub fn load_mesh(
        &self,
        manager: &mut ResourceManager,
        relative: &str,
    ) -> Result<MeshAsset, String> {
        let handle = manager
            .load::<MeshAsset>(self.id(relative))
            .map_err(|error| format!("load mesh {relative}: {error}"))?;
        manager
            .get(handle)
            .map_err(|error| format!("decode mesh {relative}: {error}"))
    }

    pub fn load_scene(
        &self,
        manager: &mut ResourceManager,
        relative: &str,
    ) -> Result<DecodedScene, String> {
        let handle = manager
            .load::<SceneAsset>(self.id(relative))
            .map_err(|error| format!("load scene {relative}: {error}"))?;
        let asset = manager
            .get(handle)
            .map_err(|error| format!("decode scene {relative}: {error}"))?;
        let cooker = SceneCooker::default();
        prism_scene::rscn::decode_scene(&asset.bytes, cooker.registry())
            .map_err(|error| format!("decode RSCN {relative}: {error}"))
    }

    pub fn load_raw(
        &self,
        manager: &mut ResourceManager,
        relative: &str,
    ) -> Result<Vec<u8>, String> {
        manager
            .load_with_deps_raw(self.id(relative))
            .map_err(|error| format!("load raw {relative}: {error}"))
    }

    /// Package record for an asset path.
    pub fn package_record(
        &self,
        relative: &str,
    ) -> Result<prism_asset::package::RuntimeAssetRecord, String> {
        let reader = PackageReader::from_bytes(&self.pak_bytes())
            .map_err(|error| format!("read pak: {error}"))?;
        reader
            .find_record(self.id(relative))
            .cloned()
            .ok_or_else(|| format!("{} missing from pak", self.id(relative)))
    }
}

impl Default for AssetHarness {
    fn default() -> Self {
        Self::new()
    }
}

/// World-space AABB of cooked RMES position data.
///
/// RMES stores planar buffers (`[positions][normals][uv0]`) with
/// `stride_bytes` describing the renderer's interleaved upload layout, so the
/// first `vert_count * 3` floats are the positions.
pub fn mesh_bounds(info: &prism_asset::formats::RmesInfo) -> Option<([f32; 3], [f32; 3])> {
    if info.vert_count == 0 || info.stride_bytes < 12 {
        return None;
    }
    let floats = info.vert_count as usize * 3;
    let bytes = floats.checked_mul(4)?;
    if info.vertex_data.len() < bytes {
        return None;
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for vertex in info.vertex_data[..bytes].as_chunks::<12>().0 {
        for axis in 0..3 {
            let value = f32::from_le_bytes(
                vertex[axis * 4..axis * 4 + 4]
                    .try_into()
                    .expect("position float"),
            );
            min[axis] = min[axis].min(value);
            max[axis] = max[axis].max(value);
        }
    }
    Some((min, max))
}

/// RGBA pixel from mip 0 of a cooked RGBA8 texture.
pub fn texture_pixel(texture: &TextureAsset, x: u32, y: u32) -> [u8; 4] {
    assert_eq!(
        texture.info.format,
        prism_asset::formats::RTEX_FORMAT_RGBA8,
        "test helper only supports RGBA8"
    );
    let offset = ((y * texture.info.width + x) * 4) as usize;
    texture.info.mip_data[0][offset..offset + 4]
        .try_into()
        .expect("RGBA pixel")
}
