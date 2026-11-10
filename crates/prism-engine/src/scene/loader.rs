//! 场景加载器——解析 RSCN 二进制烘焙场景并生成 ECS 实体。
//!
//! ## 架构
//!
//! [`SceneLoader`] 通过 [`SceneSource`] 接受三种输入：
//! - **`RawCooked(Vec<u8>)`** — 已在内存中的 RSCN 字节（来自 `.pak` 资源的
//!   `ResourceManager`，或来自程序化固定数据）。
//! - **`CookedFile(PathBuf)`** — 磁盘上的松散 RSCN 文件（开发者便利）。
//! - **`JsonFile(PathBuf)`** — 松散的 `.scene.json`（开发期直接加载作者格式）。
//!
//! 前两者汇聚到 `spawn_from_bytes`：v4 走 `prism_scene::rscn::decode_scene`，
//! 旧 v1/v2 走 `parse_rscn`。随后统一插入 ECS 组件并解析层级。
//!
//! ## RSCN 二进制格式
//!
//! v4 是唯一的 shipped 场景格式，由 `prism-cooker` 的 `SceneCooker` 从
//! `.scene.json` 编出，codec 在 `prism-scene::rscn`：
//!
//! ```text
//! [magic:4]        b"RSCN"
//! [version:1]      4
//! [entities:4]     u32 LE
//! [types:4]        u32 LE — (type_id, schema_hash) 表
//!   [type_id:8][schema_hash:8] × types
//!
//! Per entity (parent-first):
//!   [name:str u16] [parent:i32] [position:f32×3]
//!   [rotation:f32×4] [scale:f32×3] [component_count:u16]
//!     [type_id:u64] [payload_len:u32] [payload]
//! ```
//!
//! 组件 payload 按 `TypeDesc::fields` 顺序位置编码，不含字段名；解码时用
//! 编译进二进制的 `TypeDesc` 校验 `schema_hash`。场景组件通过
//! [`prism_scene::ComponentRegistry`] 还原，GPU 句柄（`MeshRef` /
//! `MaterialRef`）由本加载器在之后注入。
//!
//! 为兼容旧资产，[`parse_rscn`] 仍能读取 v1/v2 的扁平组件记录；新 cook
//! 一律走 v4，不再生成旧格式。

use std::path::PathBuf;

use prism_asset::runtime::{MeshAsset, ResourceManager};
use prism_asset::GuidIndex;
use prism_ecs::{Entity, World};
use prism_render::managers::GpuMaterial;
use prism_scene::json::SceneJson;
use prism_scene::references::{sync_component, SyncMode};
use prism_scene::rscn;

use super::component_registry::ComponentRegistry;
use super::components::*;
use super::helpers::HierarchyHelper;
use super::runtime_components::SceneEntityId;

// ---------------------------------------------------------------------------
// Component flags (must match the cooker's constants)
// ---------------------------------------------------------------------------

const FLAG_HAS_MESH: u8 = 0b00001;
const FLAG_HAS_MATERIAL: u8 = 0b00010;
const FLAG_HAS_LIGHT: u8 = 0b00100;
const FLAG_HAS_CAMERA: u8 = 0b01000;
const FLAG_HAS_SKYBOX: u8 = 0b10000;

// ---------------------------------------------------------------------------
// Light type bytes in the RSCN light record
// ---------------------------------------------------------------------------

const LIGHT_DIRECTIONAL: u8 = 0;
#[allow(dead_code)]
const LIGHT_POINT: u8 = 1;
#[allow(dead_code)]
const LIGHT_SPOT: u8 = 2;

// ---------------------------------------------------------------------------
// SceneSource — what to load
// ---------------------------------------------------------------------------

/// Describes where to find scene data.
pub enum SceneSource {
    /// In-memory RSCN binary (e.g. from a `.pak` asset via `ResourceManager`).
    RawCooked(Vec<u8>),
    /// Loose RSCN binary file on disk (dev convenience).
    CookedFile(PathBuf),
    /// Loose `.scene.json` file on disk (dev convenience).
    JsonFile(PathBuf),
}

// ---------------------------------------------------------------------------
// SceneInstance — result of a load
// ---------------------------------------------------------------------------

/// The result of loading and spawning a scene.
pub struct SceneInstance {
    /// Generated or provided scene ID.
    pub scene_id: SceneAssetId,
    /// Entities that have no parent (roots of the scene hierarchy).
    pub root_entities: Vec<Entity>,
    /// Every entity that was spawned, in spawn order.
    pub all_entities: Vec<Entity>,
}

// ---------------------------------------------------------------------------
// ParsedRscnEntity — intermediate deserialised from the binary stream
// ---------------------------------------------------------------------------

/// One entity decoded from a legacy RSCN v1/v2 byte stream, before ECS insertion.
///
/// The fields correspond one-to-one with the RSCN on-disk format (see module
/// docs).  `name` is currently stored for debug/inspector use only.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ParsedEntity {
    pub name: String,
    pub parent: Option<u32>,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub has_mesh: bool,
    pub mesh_path: String,
    pub has_material: bool,
    pub material_path: String,
    pub(crate) has_light: bool,
    pub(crate) light_type: u8,
    pub(crate) light_color: [f32; 3],
    pub(crate) light_intensity: f32,
    pub(crate) light_range: f32,
    pub(crate) light_inner_cone: f32,
    pub(crate) light_outer_cone: f32,
    pub(crate) has_camera: bool,
    pub(crate) camera_fov: f32,
    pub(crate) camera_near: f32,
    pub(crate) camera_far: f32,
    pub(crate) has_skybox: bool,
    pub(crate) skybox_hdr_path: String,
    pub(crate) skybox_enabled: bool,
}

impl ParsedEntity {
    fn empty() -> Self {
        Self {
            name: String::new(),
            parent: None,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            has_mesh: false,
            mesh_path: String::new(),
            has_material: false,
            material_path: String::new(),
            has_light: false,
            light_type: 0,
            light_color: [1.0; 3],
            light_intensity: 1.0,
            light_range: 0.0,
            light_inner_cone: 0.0,
            light_outer_cone: 0.0,
            has_camera: false,
            camera_fov: 60.0,
            camera_near: 0.1,
            camera_far: 1000.0,
            has_skybox: false,
            skybox_hdr_path: String::new(),
            skybox_enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// RSCN parser
// ---------------------------------------------------------------------------

/// Result of parsing an RSCN binary blob.
#[derive(Debug)]
pub struct ParsedRscn {
    pub entities: Vec<ParsedEntity>,
}

/// Parse an RSCN binary blob into entities + header info.
///
/// Returns `Err` with a human-readable message on format errors.
pub fn parse_rscn(data: &[u8]) -> Result<ParsedRscn, String> {
    if rscn::is_v4(data) {
        return Err(
            "RSCN v4 requires the component registry (use SceneLoader::load_and_spawn)".into(),
        );
    }
    if data.len() < 9 {
        return Err("RSCN data too short".into());
    }
    if &data[..4] != b"RSCN" {
        return Err("bad RSCN magic".into());
    }
    let version = data[4];
    if version < 1 || version > 2 {
        return Err(format!("unsupported RSCN version {}", version));
    }

    let count = u32::from_le_bytes(data[5..9].try_into().unwrap()) as usize;
    let mut offset = 9usize;

    // v2: skip skybox HDR path in header (used by read_env_path_from_rscn
    // for the app layer; the per-entity skybox data is parsed below).
    if version >= 2 {
        if offset + 2 > data.len() {
            return Err("unexpected end of RSCN data (env_len)".into());
        }
        let env_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        if offset + env_len > data.len() {
            return Err("unexpected end of RSCN data (env_path)".into());
        }
        offset += env_len;
    }

    let mut entities = Vec::with_capacity(count);

    for _ in 0..count {
        if offset + 2 > data.len() {
            return Err("unexpected end of RSCN data (name_len)".into());
        }
        let name_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        let name = if name_len > 0 {
            if offset + name_len > data.len() {
                return Err("unexpected end of RSCN data (name)".into());
            }
            String::from_utf8_lossy(&data[offset..offset + name_len]).into_owned()
        } else {
            String::new()
        };
        offset += name_len;

        if offset + 4 > data.len() {
            return Err("unexpected end (parent)".into());
        }
        let parent_raw = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        let parent = if parent_raw < 0 {
            None
        } else {
            Some(parent_raw as u32)
        };
        offset += 4;

        // Transform: tx(12) + rot(16) + scale(12) = 40 bytes.
        if offset + 40 > data.len() {
            return Err("unexpected end (transform)".into());
        }
        let position = [
            f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()),
        ];
        offset += 12;
        let rotation = [
            f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 12..offset + 16].try_into().unwrap()),
        ];
        offset += 16;
        let scale = [
            f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()),
            f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()),
        ];
        offset += 12;

        // Flags.
        if offset + 1 > data.len() {
            return Err("unexpected end (flags)".into());
        }
        let flags = data[offset];
        offset += 1;

        let mut ent = ParsedEntity {
            name,
            parent,
            position,
            rotation,
            scale,
            ..ParsedEntity::empty()
        };

        // Optional components.
        if flags & FLAG_HAS_MESH != 0 {
            if offset + 2 > data.len() {
                return Err("unexpected end (mesh path_len)".into());
            }
            let plen = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            if offset + plen > data.len() {
                return Err("unexpected end (mesh path)".into());
            }
            ent.has_mesh = true;
            ent.mesh_path = String::from_utf8_lossy(&data[offset..offset + plen]).into_owned();
            offset += plen;
        }

        if flags & FLAG_HAS_MATERIAL != 0 {
            if offset + 2 > data.len() {
                return Err("unexpected end (mat path_len)".into());
            }
            let plen = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            if offset + plen > data.len() {
                return Err("unexpected end (mat path)".into());
            }
            ent.has_material = true;
            ent.material_path = String::from_utf8_lossy(&data[offset..offset + plen]).into_owned();
            offset += plen;
        }

        if flags & FLAG_HAS_LIGHT != 0 {
            // type(1) + color(12) + intensity(4) + range(4) + inner_cone(4) + outer_cone(4) = 29
            if offset + 29 > data.len() {
                return Err("unexpected end (light)".into());
            }
            ent.has_light = true;
            ent.light_type = data[offset];
            offset += 1;
            ent.light_color = [
                f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()),
                f32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()),
                f32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()),
            ];
            offset += 12;
            ent.light_intensity = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            ent.light_range = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            ent.light_inner_cone = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            ent.light_outer_cone = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
        }

        if flags & FLAG_HAS_CAMERA != 0 {
            if offset + 12 > data.len() {
                return Err("unexpected end (camera)".into());
            }
            ent.has_camera = true;
            ent.camera_fov = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            ent.camera_near = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            ent.camera_far = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
        }

        if flags & FLAG_HAS_SKYBOX != 0 {
            if offset + 2 > data.len() {
                return Err("unexpected end (skybox path_len)".into());
            }
            let plen = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
            offset += 2;
            if offset + plen + 1 > data.len() {
                return Err("unexpected end (skybox path)".into());
            }
            ent.has_skybox = true;
            ent.skybox_hdr_path =
                String::from_utf8_lossy(&data[offset..offset + plen]).into_owned();
            offset += plen;
            ent.skybox_enabled = data[offset] != 0;
            offset += 1;
        }

        entities.push(ent);
    }

    Ok(ParsedRscn { entities })
}

// ---------------------------------------------------------------------------
// Quaternion -> yaw/pitch conversion (for scene-loaded cameras)
// ---------------------------------------------------------------------------

/// Convert a scene entity's rotation quaternion `(x, y, z, w)` into the
/// `(yaw, pitch)` pair used by [`FlyCameraController`].
///
/// The free-fly forward (see `forward` in `scene::systems::camera`) is
/// `[-sin(yaw)·cos(pitch), sin(pitch), -cos(yaw)·cos(pitch)]`. The camera's
/// world-space forward is the entity's quaternion applied to the −Z basis,
/// `R·(0,0,-1)`. Inverting the forward formula gives:
///   - `pitch = asin(forward.y)`
///   - `yaw   = atan2(-forward.x, -forward.z)`
///
/// 音符 this means an identity 四元数 (向前 = (0,0,−1)) maps to
/// `yaw = 0, 音高 = 0` — the same orientation the entity's transform already
/// expresses. Roll is discarded (the free-fly 相机 has no roll). Matches the
/// right-handed conventions in `README.md` (+X right, +Y up, +Z toward
/// viewer, camera looks down −Z).
/// Inserts an unresolved [`SpriteRef`] for entities that carry a
/// [`SpriteRenderer`] with a texture path.
///
/// The GPU handle is filled in later by `GpuAssetResolver::apply_results`
/// (`generation == 0` means "not resolved yet"), mirroring `MeshRef`.
/// Resolve a scene's `assets/` root and its path relative to that root.
///
/// Used by the development JSON loader to build a GUID index without asking
/// the caller for a project root. Returns `None` for synthetic origins (tests)
/// and paths that do not live under an `assets/` directory.
fn assets_root_for_origin(origin: &str) -> Option<(PathBuf, String)> {
    let path = std::path::Path::new(origin);
    let mut dir = path.parent()?;
    loop {
        if dir.file_name().and_then(|name| name.to_str()) == Some("assets") {
            let relative = path
                .strip_prefix(dir)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            return Some((dir.to_path_buf(), relative));
        }
        dir = dir.parent()?;
    }
}

fn ensure_sprite_ref(world: &mut World, entity: Entity) {
    if world.get::<SpriteRef>(entity).is_some() {
        return;
    }
    let has_sprite = world
        .get::<SpriteRenderer>(entity)
        .map(|sprite| sprite.has_sprite())
        .unwrap_or(false);
    if has_sprite {
        world.insert(
            entity,
            SpriteRef::new(SceneAssetId::generate(), 0, [0, 0], 0),
        );
    }
}

fn quat_to_yaw_pitch(quat: [f32; 4]) -> (f32, f32) {
    let rotation = glam::Quat::from_array(quat);
    // Normalize to avoid degenerate input skewing the result.
    if rotation.length() < 1e-6 {
        log::warn!("scene camera quaternion is degenerate (|q|≈0); using identity");
        return (0.0, 0.0);
    }
    let forward = rotation.normalize() * glam::Vec3::NEG_Z;
    let pitch = forward.y.clamp(-1.0, 1.0).asin();
    let yaw = (-forward.x).atan2(-forward.z);
    (yaw, pitch)
}

// ---------------------------------------------------------------------------
// Public helpers for env path extraction
// ---------------------------------------------------------------------------

/// Read the skybox HDR path from cooked RSCN bytes (v4 component or v2 header).
///
/// Returns `None` if the data is not valid RSCN v2+ or has no skybox configured.
/// Used by the app layer to pre-load the environment map from either the
/// ResourceManager or a loose file.
pub fn read_env_path_from_rscn_bytes(data: &[u8]) -> Option<String> {
    prism_scene::rscn::read_env_path(data)
}

/// Read the skybox HDR path from a cooked RSCN **file**.
///
/// Convenience wrapper around [`read_env_path_from_rscn_bytes`] that reads the
/// file from disk first.
#[cfg(feature = "legacy-disk-scenes")]
pub fn read_env_path_from_rscn(path: &std::path::Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    read_env_path_from_rscn_bytes(&data)
}

// ---------------------------------------------------------------------------
// SceneLoader
// ---------------------------------------------------------------------------

/// Loads cooked scenes into the ECS [`World`].
///
/// Usage:
/// ```ignore
/// let mut loader = SceneLoader::new();
/// let instance = loader.load_and_spawn(&mut world, source)?;
/// ```
pub struct SceneLoader;

impl SceneLoader {
    pub fn new() -> Self {
        Self
    }

    /// High-level entry: accept any [`SceneSource`] and spawn into the world.
    ///
    /// `registry` 用于反序列化 `.scene.json` 中的组件。
    pub fn load_and_spawn(
        &mut self,
        world: &mut World,
        source: SceneSource,
        registry: &ComponentRegistry,
    ) -> Result<SceneInstance, String> {
        let scene_id = SceneAssetId::generate();
        match source {
            SceneSource::RawCooked(bytes) => {
                self.spawn_from_bytes(world, &bytes, scene_id, registry)
            }
            SceneSource::CookedFile(path) => {
                let bytes = std::fs::read(&path)
                    .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
                self.spawn_from_bytes(world, &bytes, scene_id, registry)
            }
            SceneSource::JsonFile(path) => {
                self.load_and_spawn_json(world, &path, scene_id, registry)
            }
        }
    }

    /// Decode either RSCN v4 (schema-typed) or the legacy v1/v2 format.
    fn spawn_from_bytes(
        &self,
        world: &mut World,
        bytes: &[u8],
        scene_id: SceneAssetId,
        registry: &ComponentRegistry,
    ) -> Result<SceneInstance, String> {
        if rscn::is_v4(bytes) {
            let decoded = rscn::decode_scene(bytes, registry)
                .map_err(|e| format!("RSCN v4 decode error: {e}"))?;
            self.spawn_from_decoded(world, &decoded, scene_id, registry)
        } else {
            let parsed = parse_rscn(bytes)?;
            self.spawn_from_parsed(world, &parsed, scene_id)
        }
    }

    /// Spawn entities from decoded RSCN v4 components.
    ///
    /// Authoring components (transforms, lights, camera, skybox, UI, …) are
    /// applied through the registry. Render handles are attached afterwards by
    /// the engine, keeping `prism-scene` free of GPU types.
    fn spawn_from_decoded(
        &self,
        world: &mut World,
        decoded: &rscn::DecodedScene,
        scene_id: SceneAssetId,
        registry: &ComponentRegistry,
    ) -> Result<SceneInstance, String> {
        let mut entities = Vec::with_capacity(decoded.entities.len());
        for decoded_entity in &decoded.entities {
            let entity = world.spawn();
            entities.push(entity);
            world.insert(entity, SceneMember(scene_id));
            world.insert(entity, Active(true));

            if !decoded_entity.name.is_empty() {
                world.insert(entity, Name(decoded_entity.name.clone()));
            }

            let local = LocalTransform {
                position: decoded_entity.position.into(),
                rotation: glam::Quat::from_xyzw(
                    decoded_entity.rotation[0],
                    decoded_entity.rotation[1],
                    decoded_entity.rotation[2],
                    decoded_entity.rotation[3],
                ),
                scale: decoded_entity.scale.into(),
            };
            world.insert(entity, local.clone());
            world.insert(entity, LocalToWorld(local.to_model_matrix()));

            for object in &decoded_entity.components {
                registry.apply_object(world, entity, object);
            }

            // Runtime GPU handles are derived from the authoring bundle.
            if let Some(renderer) = world.get::<MeshRenderer>(entity).cloned() {
                if renderer.has_mesh() {
                    world.insert(
                        entity,
                        MeshRef {
                            asset_id: SceneAssetId::generate(),
                            render_handle: prism_render::managers::MeshHandle::default(),
                            generation: 0,
                        },
                    );
                }
                if renderer.has_material() {
                    world.insert(
                        entity,
                        MaterialRef {
                            asset_id: SceneAssetId::generate(),
                            material_slot: 0,
                            generation: 0,
                        },
                    );
                }
            }

            ensure_sprite_ref(world, entity);

            // A cooked camera without an explicit controller gets the default
            // free-fly controller, matching the legacy loader behaviour.
            // Orthographic cameras are 2D cameras: a fly controller would fight
            // the game's own camera follow, so they never get one implicitly.
            let camera_is_orthographic = world
                .get::<Camera>(entity)
                .map(|camera| camera.orthographic)
                .unwrap_or(false);
            if world.get::<Camera>(entity).is_some()
                && !camera_is_orthographic
                && world.get::<FlyCameraController>(entity).is_none()
            {
                let (yaw, pitch) = quat_to_yaw_pitch(decoded_entity.rotation);
                world.insert(
                    entity,
                    FlyCameraController {
                        yaw,
                        pitch,
                        ..FlyCameraController::default()
                    },
                );
            }
        }

        for (index, decoded_entity) in decoded.entities.iter().enumerate() {
            if let Some(parent_index) = decoded_entity.parent {
                let parent_index = parent_index as usize;
                if parent_index < entities.len() {
                    HierarchyHelper::reparent(world, entities[index], Some(entities[parent_index]));
                }
            }
        }

        let root_entities: Vec<Entity> = entities
            .iter()
            .copied()
            .filter(|&entity| world.get::<Parent>(entity).is_none())
            .collect();

        Ok(SceneInstance {
            scene_id,
            root_entities,
            all_entities: entities,
        })
    }

    /// 从 `.scene.json` 文件加载场景。
    fn load_and_spawn_json(
        &self,
        world: &mut World,
        path: &std::path::Path,
        scene_id: SceneAssetId,
        registry: &ComponentRegistry,
    ) -> Result<SceneInstance, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let origin = path.display().to_string();
        self.spawn_from_json_text(world, &text, &origin, scene_id, registry)
    }

    /// 从 `.scene.json` 文本加载场景。
    ///
    /// 与 [`Self::load_and_spawn_json`] 共用同一套逻辑，只是把「读文件」与
    /// 「解析 + spawn」分开，便于不落盘的测试。
    fn spawn_from_json_text(
        &self,
        world: &mut World,
        text: &str,
        origin: &str,
        scene_id: SceneAssetId,
        registry: &ComponentRegistry,
    ) -> Result<SceneInstance, String> {
        // `SceneJson::parse` migrates older authoring documents (v1
        // `transform.translation`) up to the current schema.
        let mut scene = SceneJson::parse(text)
            .map_err(|e| format!("Scene JSON parse error in {origin}: {e}"))?;

        // #466: dev scenes load the authoring JSON directly. Resolve stable
        // GUID references here too so a renamed asset keeps working without
        // waiting for a cook. Packaged scenes already contain resolved paths.
        if let Some((assets_root, doc_rel)) = assets_root_for_origin(origin) {
            let index = GuidIndex::scan(&assets_root);
            let path_for_guid = |guid: &str| index.path_for_guid(guid).map(str::to_string);
            let guid_for_path = |path: &str| index.guid_for_path(path).map(str::to_string);
            for entity in &mut scene.entities {
                for component in entity.components.values_mut() {
                    sync_component(
                        component,
                        &doc_rel,
                        SyncMode::Load,
                        &path_for_guid,
                        &guid_for_path,
                    );
                }
            }
        }

        let entity_count = scene.entities.len();
        let mut entities: Vec<Entity> = Vec::with_capacity(entity_count);

        // Phase 1: spawn all entities, insert transform + scene components.
        for (_i, ej) in scene.entities.iter().enumerate() {
            let entity = world.spawn();
            entities.push(entity);

            world.insert(entity, SceneMember(scene_id));
            world.insert(entity, Active(true));

            // Name
            if let Some(name) = &ej.name {
                world.insert(entity, Name(name.clone()));
            }

            // 稳定 id：`.scene.json` v3+ 的 `id`，跨会话身份（scene_state / 编辑器同步）。
            world.insert(entity, SceneEntityId(ej.id));

            // Transform
            let local = LocalTransform {
                position: glam::Vec3::from(ej.transform.position),
                rotation: glam::Quat::from_xyzw(
                    ej.transform.rotation[0],
                    ej.transform.rotation[1],
                    ej.transform.rotation[2],
                    ej.transform.rotation[3],
                ),
                scale: glam::Vec3::from(ej.transform.scale),
            };
            world.insert(entity, local.clone());
            world.insert(entity, LocalToWorld(local.to_model_matrix()));

            // Components via registry
            for (comp_name, comp_data) in &ej.components {
                registry.apply(world, entity, comp_name, comp_data);
            }

            // Runtime GPU handles are derived from the authoring bundle for
            // JSON scenes too. Without this, editor-authored MeshRenderer
            // entities never enter the draw query because MeshRef is absent.
            if let Some(renderer) = world.get::<MeshRenderer>(entity).cloned() {
                if renderer.has_mesh() {
                    world.insert(
                        entity,
                        MeshRef {
                            asset_id: SceneAssetId::generate(),
                            render_handle: prism_render::managers::MeshHandle::default(),
                            generation: 0,
                        },
                    );
                }
                if renderer.has_material() {
                    world.insert(
                        entity,
                        MaterialRef {
                            asset_id: SceneAssetId::generate(),
                            material_slot: 0,
                            generation: 0,
                        },
                    );
                }
            }
        }

        // Phase 2: hierarchy.
        //
        // `.scene.json` v3+ 的 `parent` 是**稳定实体 id**，不是数组下标
        // （见 `prism_scene::json::EntityJson::parent`）；id 与下标只在
        // `migrate_scene_doc` 给旧文档按数组顺序补 id 时才恰好相等。
        // 直接拿 id 当下标会在实体重排/删除后接错父级，甚至越界变成根节点。
        let index_of_id = scene.index_of_id();
        for (i, ej) in scene.entities.iter().enumerate() {
            let Some(parent_id) = ej.parent else {
                continue;
            };
            match index_of_id.get(&parent_id).copied() {
                Some(parent_index) => {
                    HierarchyHelper::reparent(world, entities[i], Some(entities[parent_index]));
                }
                // `SceneJson::parse` 只校验作者写法，不跑 `validate()`；
                // dev 场景里出现悬空 id 时保留实体、按根节点处理，不要整场失败。
                None => log::warn!(
                    "scene entity {i} references parent id {parent_id} which does not exist; \
                     keeping it as a root"
                ),
            }
        }

        let root_entities: Vec<Entity> = entities
            .iter()
            .copied()
            .filter(|&e| world.get::<Parent>(e).is_none())
            .collect();

        Ok(SceneInstance {
            scene_id,
            root_entities,
            all_entities: entities,
        })
    }

    /// Core spawn: convert parsed RSCN entities into ECS components.
    ///
    /// This is the single path that creates entities — all `SceneSource`
    /// variants converge here.
    pub(crate) fn spawn_from_parsed(
        &self,
        world: &mut World,
        parsed: &ParsedRscn,
        scene_id: SceneAssetId,
    ) -> Result<SceneInstance, String> {
        let parsed_entities = &parsed.entities;
        // Phase 1: spawn all entities, insert scene & transform components.
        let mut entities: Vec<Entity> = Vec::with_capacity(parsed_entities.len());
        for pe in parsed_entities {
            let entity = world.spawn();
            entities.push(entity);

            // Always-added components.
            world.insert(entity, SceneMember(scene_id));
            world.insert(entity, Active(true));
            // Optional human-readable name (empty string -> no Name component,
            // the inspector falls back to the raw entity id).
            if !pe.name.is_empty() {
                world.insert(entity, Name(pe.name.clone()));
            }

            // Local transform.
            let local = LocalTransform {
                position: pe.position.into(),
                rotation: glam::Quat::from_xyzw(
                    pe.rotation[0],
                    pe.rotation[1],
                    pe.rotation[2],
                    pe.rotation[3],
                ),
                scale: pe.scale.into(),
            };
            world.insert(entity, local.clone());

            // Initial world transform = local (will be recomputed by hierarchy system).
            world.insert(entity, LocalToWorld(local.to_model_matrix()));

            // Mesh reference (generation 0 = unresolved; resolved by
            // resolve_assets_system once the .pak runtime is wired).
            if pe.has_mesh {
                world.insert(
                    entity,
                    MeshRef {
                        asset_id: SceneAssetId::generate(),
                        render_handle: prism_render::managers::MeshHandle::default(),
                        generation: 0,
                    },
                );
            }

            // Material reference (unresolved).
            if pe.has_material {
                world.insert(
                    entity,
                    MaterialRef {
                        asset_id: SceneAssetId::generate(),
                        material_slot: 0,
                        generation: 0,
                    },
                );
            }

            // MeshRenderer bundle — inserted when both mesh and material are
            // present. Carries the path strings for later resolution.
            if pe.has_mesh && pe.has_material {
                world.insert(
                    entity,
                    MeshRenderer {
                        mesh_path: pe.mesh_path.clone(),
                        material_path: pe.material_path.clone(),
                        ..Default::default()
                    },
                );
            }

            ensure_sprite_ref(world, entity);

            // Light component.
            if pe.has_light {
                match pe.light_type {
                    LIGHT_DIRECTIONAL => {
                        world.insert(
                            entity,
                            DirectionalLight {
                                euler_xyz: glam::Vec3::ZERO,
                                color: pe.light_color.into(),
                                intensity: pe.light_intensity,
                                ambient: 1.0,
                            },
                        );
                    }
                    LIGHT_POINT => {
                        world.insert(
                            entity,
                            PointLight {
                                color: pe.light_color.into(),
                                intensity: pe.light_intensity,
                                range: pe.light_range,
                            },
                        );
                    }
                    LIGHT_SPOT => {
                        world.insert(
                            entity,
                            SpotLight {
                                color: pe.light_color.into(),
                                intensity: pe.light_intensity,
                                range: pe.light_range,
                                inner_cone_angle: pe.light_inner_cone,
                                outer_cone_angle: pe.light_outer_cone,
                            },
                        );
                    }
                    _ => {}
                }
            }

            // Camera component.
            if pe.has_camera {
                // Data component: projection + exposure + runtime aspect cache.
                // `aspect` is a placeholder; `app.rs` writes the real value on
                // the first resize / orientation change.
                world.insert(
                    entity,
                    Camera {
                        fov_y_degrees: pe.camera_fov,
                        near: pe.camera_near,
                        far: pe.camera_far,
                        ..Camera::default()
                    },
                );
                // Free-fly input controller. yaw/pitch are derived from the
                // entity's quaternion so the scene file fully determines the
                // initial viewpoint; the camera position lives on the sibling
                // LocalTransform (translation). `move_speed`/`look_sensitivity`
                // keep their defaults.
                let (yaw, pitch) = quat_to_yaw_pitch(pe.rotation);
                world.insert(
                    entity,
                    FlyCameraController {
                        yaw,
                        pitch,
                        ..FlyCameraController::default()
                    },
                );
            }

            // Skybox component (the skybox entity is a regular entity in the
            // entities array; its Skybox component is inserted here).
            if pe.has_skybox {
                world.insert(
                    entity,
                    Skybox {
                        hdr_path: pe.skybox_hdr_path.clone(),
                        enabled: pe.skybox_enabled,
                        ..Skybox::default()
                    },
                );
                log::trace!(
                    "skybox component inserted: hdr_path={}, enabled={}",
                    pe.skybox_hdr_path,
                    pe.skybox_enabled
                );
            }
        }

        // Phase 2: build hierarchy via HierarchyHelper (must happen after all
        // entities exist so that parent indices resolve).
        for (i, pe) in parsed_entities.iter().enumerate() {
            if let Some(parent_idx) = pe.parent {
                let parent_idx = parent_idx as usize;
                if parent_idx < entities.len() {
                    HierarchyHelper::reparent(world, entities[i], Some(entities[parent_idx]));
                }
            }
        }

        // Collect root entities (no parent).
        let root_entities: Vec<Entity> = entities
            .iter()
            .copied()
            .filter(|&e| world.get::<Parent>(e).is_none())
            .collect();

        Ok(SceneInstance {
            scene_id,
            root_entities,
            all_entities: entities,
        })
    }
}

impl Default for SceneLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// ECS-driven scene geometry collection for offline baking
// ---------------------------------------------------------------------------

/// Collect geometry + material data from the ECS world for offline baking.
///
/// Queries all entities with [`MeshRenderer`] + [`LocalToWorld`] components,
/// loads their CPU vertex data via [`ResourceManager`], and returns:
///
/// - `instances` — one [`PtGeometryInstance`] per entity that carries
/// a 网格 + 材质 (world-space 顶点 from `LocalToWorld`).
/// - `materials_bytes` — a flat `GpuMaterial[96]` array (one entry per
///   unique material path, indexed by `PtGeometryInstance::material_slot`).
pub fn collect_bake_instances(
    world: &World,
    rm: &mut ResourceManager,
) -> anyhow::Result<(Vec<prism_render::bake_common::PtGeometryInstance>, Vec<u8>)> {
    use std::collections::HashMap;

    use prism_render::bake_common::PtGeometryInstance;

    let mut instances: Vec<PtGeometryInstance> = Vec::new();
    let mut mat_cache: HashMap<String, u32> = HashMap::new();
    let mut gpu_materials: Vec<GpuMaterial> = Vec::new();

    for (_entity, mr, wt) in world.query2::<MeshRenderer, LocalToWorld>() {
        // Resolve material slot.
        let material_slot = if !mr.material_path.is_empty() {
            *mat_cache
                .entry(mr.material_path.clone())
                .or_insert_with(|| {
                    let slot = gpu_materials.len() as u32;
                    let mat = load_material_for_bake(rm, &mr.material_path);
                    gpu_materials.push(
                        mat.as_ref()
                            .map(|r_info| rmat_to_gpu(r_info))
                            .unwrap_or_else(default_gpu_material),
                    );
                    slot
                })
        } else {
            *mat_cache.entry(String::new()).or_insert_with(|| {
                let slot = gpu_materials.len() as u32;
                gpu_materials.push(default_gpu_material());
                slot
            })
        };

        // Get the CPU vertex data for this mesh.
        let Some(mesh_info) = load_mesh_for_bake(rm, &mr.mesh_path) else {
            log::warn!(
                "collect_bake_instances: skipping entity with missing mesh '{}'",
                mr.mesh_path
            );
            continue;
        };

        let stride = mesh_info.stride_bytes as usize;
        let vert_count = mesh_info.vert_count as usize;
        let idx_count = mesh_info.idx_count as usize;
        if vert_count == 0 || idx_count == 0 {
            continue;
        }

        let world_mat = &wt.0;

        let mut vertices = Vec::with_capacity(vert_count);
        for vi in 0..vert_count {
            let off = vi * stride;
            let px = f32::from_le_bytes(mesh_info.vertex_data[off..off + 4].try_into().unwrap());
            let py =
                f32::from_le_bytes(mesh_info.vertex_data[off + 4..off + 8].try_into().unwrap());
            let pz =
                f32::from_le_bytes(mesh_info.vertex_data[off + 8..off + 12].try_into().unwrap());
            let wp = world_mat.transform_point3(glam::Vec3::new(px, py, pz));

            let normal = if stride >= 24 {
                let nx = f32::from_le_bytes(
                    mesh_info.vertex_data[off + 12..off + 16]
                        .try_into()
                        .unwrap(),
                );
                let ny = f32::from_le_bytes(
                    mesh_info.vertex_data[off + 16..off + 20]
                        .try_into()
                        .unwrap(),
                );
                let nz = f32::from_le_bytes(
                    mesh_info.vertex_data[off + 20..off + 24]
                        .try_into()
                        .unwrap(),
                );
                let wn = world_mat.transform_vector3(glam::Vec3::new(nx, ny, nz));
                [wn[0], wn[1], wn[2]]
            } else {
                [0.0, 0.0, 0.0]
            };

            let uv = if stride >= 24 + 8 && mesh_info.uv_channels > 0 {
                let u = f32::from_le_bytes(
                    mesh_info.vertex_data[off + 24..off + 28]
                        .try_into()
                        .unwrap(),
                );
                let v = f32::from_le_bytes(
                    mesh_info.vertex_data[off + 28..off + 32]
                        .try_into()
                        .unwrap(),
                );
                [u, v]
            } else {
                [0.0, 0.0]
            };

            vertices.push(prism_render::mesh::Vertex {
                position: [wp[0], wp[1], wp[2]],
                normal,
                color: [1.0, 1.0, 1.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
            });
        }

        let indices: Vec<u32> = mesh_info
            .index_data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();

        instances.push(PtGeometryInstance {
            vertices,
            indices,
            material_slot,
        });
    }

    let mat_bytes: Vec<u8> = bytemuck::cast_slice(&gpu_materials).to_vec();

    Ok((instances, mat_bytes))
}

// -------------------------------------------------------------------
// Internal helpers for collect_bake_instances
// -------------------------------------------------------------------

// (transform_point / transform_normal replaced by glam Mat4 methods)

/// Load a `MeshAsset` from the resource manager given its path.
fn load_mesh_for_bake(
    rm: &mut ResourceManager,
    path: &str,
) -> Option<prism_asset::runtime::RmesInfo> {
    let id = rm.id_by_path(path)?;
    let handle = rm.load_with_deps::<MeshAsset>(id).ok()?;
    let asset = rm.get::<MeshAsset>(handle).ok()?;
    Some(asset.info)
}

/// Load a `MaterialAsset` from the resource manager given its path.
fn load_material_for_bake(
    rm: &mut ResourceManager,
    path: &str,
) -> Option<prism_asset::runtime::RmatInfo> {
    use prism_asset::runtime::MaterialAsset;
    let id = rm.id_by_path(path)?;
    let handle = rm.load_with_deps::<MaterialAsset>(id).ok()?;
    let asset = rm.get::<MaterialAsset>(handle).ok()?;
    Some(asset.info)
}

/// Convert `RmatInfo` scalars into a `GpuMaterial` (texture slots = u32::MAX).
fn rmat_to_gpu(info: &prism_asset::runtime::RmatInfo) -> GpuMaterial {
    let s = &info.scalars;
    GpuMaterial {
        base_color: [s[0], s[1], s[2], s[3]],
        metallic_roughness_emissive: [s[4], s[5], s[6], s[9]],
        albedo_idx: u32::MAX,
        normal_idx: u32::MAX,
        metallic_roughness_idx: u32::MAX,
        emissive_idx: u32::MAX,
        transmission_factor: [s[12], s[13], s[14], s[15]],
        clearcoat: [s[16], s[17], 0.0, 0.0],
        transmission_tex_idx: u32::MAX,
        occlusion_idx: u32::MAX,
        normal_scale: s[10],
        occlusion_strength: s[11],
    }
}

/// Default material (pink error colour so missing materials are obvious).
fn default_gpu_material() -> GpuMaterial {
    GpuMaterial {
        base_color: [1.0, 0.0, 1.0, 1.0],
        metallic_roughness_emissive: [0.0, 0.5, 0.0, 0.0],
        albedo_idx: u32::MAX,
        normal_idx: u32::MAX,
        metallic_roughness_idx: u32::MAX,
        emissive_idx: u32::MAX,
        transmission_factor: [0.0, 1.5, 0.0, 0.0],
        clearcoat: [0.0, 0.0, 0.0, 0.0],
        transmission_tex_idx: u32::MAX,
        occlusion_idx: u32::MAX,
        normal_scale: 1.0,
        occlusion_strength: 1.0,
    }
}
