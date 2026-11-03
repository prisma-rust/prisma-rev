//! Dynamic project module loading for the same-build desktop preview.
//!
//! The loaded library is kept alive for the whole host lifetime. This is
//! required because the engine schedule stores callbacks owned by the module.
//! Unloading before the engine is dropped would leave dangling function/code
//! pointers.
//!
//! ## Boundaries
//!
//! The host never hands `World` / `Engine` pointers to the project module. The
//! module can only touch the world through the [`HostApi`] function table built
//! by [`WorldCommands`]; component data travels as RTTIBin byte streams and type
//! resolution stays in the host's own `ComponentRegistry`.

use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};
use prism_project_api::{
    GetProjectApi, HostApi, ProjectApi, RaycastHit, ABI_VERSION, BUFFER_TOO_SMALL, HOST_CAP_ASSET,
    HOST_CAP_COMPONENTS, HOST_CAP_DRAW, HOST_CAP_LOGGING, HOST_CAP_PHYSICS, HOST_CAP_SCENE,
    HOST_CAP_STATE, HOST_CAP_WORLD_COMMANDS, INVALID_ARGUMENT, SUCCESS, UNKNOWN_TYPE,
};
use prism_reflect::rtti_bin;
use thiserror::Error;

const GET_API_SYMBOL: &[u8] = b"prism_project_get_api\0";

/// ABI payload types re-exported for direct host use (no dependency on `prism-project-api` needed).
pub use prism_project_api::{
    EntityHandle, InputSnapshot, TouchSnapshot, UiQuad, TOUCH_PHASE_CANCELLED, TOUCH_PHASE_ENDED,
    TOUCH_PHASE_MOVED, TOUCH_PHASE_STARTED,
};

#[derive(Debug, Error)]
pub enum ProjectModuleError {
    #[error("load project module `{path}`: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("project module `{path}` does not export prism_project_get_api")]
    MissingEntry { path: PathBuf },
    #[error("project module `{path}` returned status {status} while reading its API")]
    GetApi { path: PathBuf, status: i32 },
    #[error("project module `{path}` ABI {actual} is incompatible with host ABI {expected}")]
    AbiMismatch {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
    #[error("project module `{path}` schema fingerprint {actual:#018x} does not match host {expected:#018x}")]
    SchemaMismatch {
        path: PathBuf,
        actual: u64,
        expected: u64,
    },
    #[error("project module `{path}` has an incomplete lifecycle API")]
    IncompleteApi { path: PathBuf },
    #[error("project module `{path}` create failed with status {status}")]
    Create { path: PathBuf, status: i32 },
    #[error("project module `{path}` tick failed with status {status}")]
    Tick { path: PathBuf, status: i32 },
}

/// Host-side loose asset access.
///
/// The implementation lives in the host (editor / Player), but asset root
/// resolution still has a single implementation in `prism-engine::asset_paths`.
pub trait AssetIo {
    fn exists(&self, path: &str) -> bool;
    fn read(&self, path: &str) -> std::io::Result<Vec<u8>>;
}

/// Default implementation that supports no loose assets (for hosts that only run `.pak` content).
pub struct NoAssets;

impl AssetIo for NoAssets {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("loose asset access disabled for `{path}`"),
        ))
    }
}

static NO_ASSETS: NoAssets = NoAssets;

/// Scene-switch action requested by a project module (#482).
///
/// The engine world is borrowed while callbacks run, so the request can only be
/// **recorded**; the host calls [`WorldCommands::take_scene_request`] after the
/// tick and performs the actual switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneRequest {
    /// Load a `.scene.json`; `additive` layers it on without unloading the old scene.
    Load { path: String, additive: bool },
    /// Unload the current scene (`Persistent` subtrees are kept).
    Unload,
}

/// Raycast hit injected by the host (#522).
///
/// This crate does not depend on `prism-engine` (that would pull in a second
/// engine), so the geometry implementation is injected by the host as a function
/// pointer; the editor passes an adapter over `prism_engine::physics::raycast`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RaycastResult {
    pub entity: prism_ecs::Entity,
    pub distance: f32,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub is_trigger: bool,
}

/// Signature of a raycast implementation (the equivalent of `prism_engine::physics::raycast`).
pub type RaycastFn = fn(&prism_ecs::World, [f32; 3], [f32; 3], f32, u32) -> Option<RaycastResult>;

/// Per-tick command context on the host side.
///
/// Callbacks only receive a single `user_data`, so the world, the registry and
/// the scratch buffers must be packed into one struct; [`WorldCommands`] bounds
/// its lifetime and it never outlives the tick.
struct CommandContext<'a> {
    world: &'a mut prism_ecs::World,
    registry: &'a prism_scene::ComponentRegistry,
    /// Return buffer for `get_component`; the pointer is valid only until the next host call.
    scratch: Vec<u8>,
    /// UI quads submitted by the project; the host takes them before UI rendering.
    quads: Vec<UiQuad>,
    scene_generation: u64,
    input: InputSnapshot,
    asset: &'a dyn AssetIo,
    /// Return buffer for `read_asset_file`; the pointer is valid only until the next host call.
    asset_bytes: Vec<u8>,
    /// Engine-provided scene state (#484). Injected by the host; **this crate does not depend on
    /// `prism-engine`**, so it is called through function pointers.
    save_state: Option<fn(&prism_ecs::World)>,
    load_state: Option<fn(&mut prism_ecs::World) -> bool>,
    /// Scene switch requested by the project during this tick (#482), taken by the host afterwards.
    scene_request: Option<SceneRequest>,
    /// Whether the host will execute `scene_request`; only such hosts advertise the capability bit.
    scene_switch: bool,
    /// The host's raycast implementation (#522); `HOST_CAP_PHYSICS` is not advertised when `None`.
    raycast: Option<RaycastFn>,
}

/// Wraps the host world into the [`HostApi`] function table.
pub struct WorldCommands<'a> {
    context: CommandContext<'a>,
}

impl<'a> WorldCommands<'a> {
    pub fn new(
        world: &'a mut prism_ecs::World,
        registry: &'a prism_scene::ComponentRegistry,
        scene_generation: u64,
    ) -> Self {
        Self {
            context: CommandContext {
                world,
                registry,
                scratch: Vec::new(),
                quads: Vec::new(),
                scene_generation,
                input: InputSnapshot::default(),
                asset: &NO_ASSETS,
                asset_bytes: Vec::new(),
                save_state: None,
                load_state: None,
                scene_request: None,
                scene_switch: false,
                raycast: None,
            },
        }
    }

    /// Injects loose asset access (the host implements `AssetIo`).
    pub fn with_assets(mut self, asset: &'a dyn AssetIo) -> Self {
        self.context.asset = asset;
        self
    }

    /// Injects the engine-provided scene-state implementation (the host passes
    /// `prism_engine::scene_state::*`).
    ///
    /// This crate cannot depend on `prism-engine` (that would pull in a second
    /// engine), so it only accepts function pointers.
    pub fn with_scene_state(
        mut self,
        save: fn(&prism_ecs::World),
        load: fn(&mut prism_ecs::World) -> bool,
    ) -> Self {
        self.context.save_state = Some(save);
        self.context.load_state = Some(load);
        self
    }

    /// Declares that the host will execute scene-switch requests from project code (#482).
    ///
    /// Only a host that really calls [`Self::take_scene_request`] after the tick
    /// and performs the switch may set this capability bit; otherwise project code
    /// would assume the request took effect.
    pub fn with_scene_switch(mut self) -> Self {
        self.context.scene_switch = true;
        self
    }

    /// Injects the raycast implementation (#522).
    ///
    /// Only a host that really provides queries sets `HOST_CAP_PHYSICS`; the editor
    /// passes an adapter over `prism_engine::physics::raycast` (see `engine_raycast`).
    pub fn with_raycast(mut self, raycast: RaycastFn) -> Self {
        self.context.raycast = Some(raycast);
        self
    }

    /// Input snapshot for this frame; UI / project code reads it through `HostApi::input`.
    pub fn set_input(&mut self, input: InputSnapshot) {
        self.context.input = input;
    }

    /// Takes the UI quads the project submitted this frame (the host merges them
    /// before UI rendering).
    pub fn take_quads(&mut self) -> Vec<UiQuad> {
        std::mem::take(&mut self.context.quads)
    }

    /// Takes the scene switch requested by project code this tick (#482); the host
    /// executes it after the tick.
    pub fn take_scene_request(&mut self) -> Option<SceneRequest> {
        self.context.scene_request.take()
    }

    /// Builds the capability table for this tick. The pointers are valid only from
    /// the return of `api` until the end of the tick.
    pub fn api(&mut self) -> HostApi {
        HostApi {
            abi_version: ABI_VERSION,
            schema_fingerprint: self.context.registry.schema_fingerprint(),
            capabilities: HOST_CAP_LOGGING
                | HOST_CAP_WORLD_COMMANDS
                | HOST_CAP_COMPONENTS
                | HOST_CAP_ASSET
                | HOST_CAP_DRAW
                | if self.context.save_state.is_some() && self.context.load_state.is_some() {
                    HOST_CAP_STATE
                } else {
                    0
                }
                | if self.context.scene_switch {
                    HOST_CAP_SCENE
                } else {
                    0
                }
                | if self.context.raycast.is_some() {
                    HOST_CAP_PHYSICS
                } else {
                    0
                },
            scene_generation: self.context.scene_generation,
            user_data: (&mut self.context as *mut CommandContext).cast(),
            input: self.context.input,
            log: Some(host_log),
            spawn_entity: Some(host_spawn_entity),
            set_entity_name: Some(host_set_entity_name),
            asset_exists: Some(host_asset_exists),
            read_asset_file: Some(host_read_asset_file),
            save_scene_state: Some(host_save_scene_state),
            load_scene_state: Some(host_load_scene_state),
            load_scene: if self.context.scene_switch {
                Some(host_load_scene)
            } else {
                None
            },
            unload_scene: if self.context.scene_switch {
                Some(host_unload_scene)
            } else {
                None
            },
            raycast: if self.context.raycast.is_some() {
                Some(host_raycast)
            } else {
                None
            },
            query_entities: Some(host_query_entities),
            get_component: Some(host_get_component),
            set_component: Some(host_set_component),
            remove_component: Some(host_remove_component),
            despawn_entity: Some(host_despawn_entity),
            is_alive: Some(host_is_alive),
            draw_quad: Some(host_draw_quad),
        }
    }
}

/// # Safety
/// `user_data` must come from [`WorldCommands::api`] for this tick.
unsafe fn context<'a>(user_data: *mut std::ffi::c_void) -> Option<&'a mut CommandContext<'a>> {
    if user_data.is_null() {
        return None;
    }
    unsafe { user_data.cast::<CommandContext>().as_mut() }
}

/// # Safety
/// `ptr`/`len` must describe a block of UTF-8 bytes valid for the duration of this call.
unsafe fn type_name<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if len == 0 {
        return None;
    }
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok()
}

fn handle_to_entity(handle: EntityHandle) -> prism_ecs::Entity {
    prism_ecs::Entity::from_raw(handle.id, handle.generation)
}

unsafe extern "C" fn host_spawn_entity(
    user_data: *mut std::ffi::c_void,
    out_entity: *mut EntityHandle,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    if out_entity.is_null() {
        return INVALID_ARGUMENT;
    }
    let entity = context.world.spawn();
    unsafe {
        *out_entity = EntityHandle {
            id: entity.id(),
            generation: entity.generation(),
        };
    }
    SUCCESS
}

unsafe extern "C" fn host_set_entity_name(
    user_data: *mut std::ffi::c_void,
    entity: EntityHandle,
    name: *const u8,
    name_len: usize,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(name) = (unsafe { type_name(name, name_len) }) else {
        return INVALID_ARGUMENT;
    };
    let entity = handle_to_entity(entity);
    if !context.world.is_alive(entity) {
        return INVALID_ARGUMENT;
    }
    context
        .world
        .insert(entity, prism_scene::components::Name(name.to_owned()));
    SUCCESS
}

unsafe extern "C" fn host_query_entities(
    user_data: *mut std::ffi::c_void,
    type_name_ptr: *const u8,
    type_name_len: usize,
    out_entities: *mut EntityHandle,
    capacity: usize,
    out_count: *mut usize,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    if out_count.is_null() {
        return INVALID_ARGUMENT;
    }
    let Some(name) = (unsafe { type_name(type_name_ptr, type_name_len) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(type_id) = context.registry.type_id(name) else {
        return UNKNOWN_TYPE;
    };
    let entities = context
        .registry
        .entities_with(context.world, type_id)
        .unwrap_or_default();

    unsafe { *out_count = entities.len() };
    if out_entities.is_null() {
        return SUCCESS;
    }
    if capacity < entities.len() {
        return BUFFER_TOO_SMALL;
    }
    for (index, entity) in entities.iter().enumerate() {
        let handle = EntityHandle {
            id: entity.id(),
            generation: entity.generation(),
        };
        unsafe { *out_entities.add(index) = handle };
    }
    SUCCESS
}

unsafe extern "C" fn host_get_component(
    user_data: *mut std::ffi::c_void,
    entity: EntityHandle,
    type_name_ptr: *const u8,
    type_name_len: usize,
    out_bytes: *mut *const u8,
    out_len: *mut usize,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    if out_bytes.is_null() || out_len.is_null() {
        return INVALID_ARGUMENT;
    }
    let Some(name) = (unsafe { type_name(type_name_ptr, type_name_len) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(type_id) = context.registry.type_id(name) else {
        return UNKNOWN_TYPE;
    };
    let entity = handle_to_entity(entity);
    if !context.world.is_alive(entity) {
        return INVALID_ARGUMENT;
    }
    let Some(object) = context
        .registry
        .extract_object(context.world, entity, type_id)
    else {
        return INVALID_ARGUMENT;
    };
    let bytes = match rtti_bin::encode_one(&object, context.registry.type_registry()) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!("project host: encode `{name}` failed: {error}");
            return INVALID_ARGUMENT;
        }
    };
    context.scratch = bytes;
    unsafe {
        *out_bytes = context.scratch.as_ptr();
        *out_len = context.scratch.len();
    }
    SUCCESS
}

unsafe extern "C" fn host_set_component(
    user_data: *mut std::ffi::c_void,
    entity: EntityHandle,
    bytes: *const u8,
    len: usize,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    if bytes.is_null() || len == 0 {
        return INVALID_ARGUMENT;
    }
    let payload = unsafe { std::slice::from_raw_parts(bytes, len) };
    let decoded = match rtti_bin::decode(payload) {
        Ok(decoded) => decoded,
        Err(error) => {
            log::warn!("project host: decode component failed: {error}");
            return INVALID_ARGUMENT;
        }
    };
    let Some(object) = decoded.objects.first() else {
        return INVALID_ARGUMENT;
    };
    let entity = handle_to_entity(entity);
    if !context.world.is_alive(entity) {
        return INVALID_ARGUMENT;
    }
    // Try runtime components first (the registry would only log an "unknown"
    // warning and reject them).
    if apply_runtime_component(context.world, entity, object) {
        return SUCCESS;
    }
    if context.registry.apply_object(context.world, entity, object) {
        return SUCCESS;
    }
    UNKNOWN_TYPE
}

/// Writes a "runtime component": the registry deliberately keeps them out of
/// authored documents, but project code must still be able to write them.
///
/// `LocalTransform` is registered as a **type** only (see
/// `register_builtin_components` in `prism-scene`: authors write only `transform`,
/// and the loader / cooker bakes it into `LocalTransform` + `LocalToWorld`;
/// otherwise it would be overwritten after the world matrix is computed). In
/// player form, project code calls `GameWorld::host(...)` →
/// `world.set(entity, LocalTransform)` straight into the ECS; the plugin form
/// needs an equivalent path, or entities spawned by a module would have no
/// transform and pile up at the origin.
///
/// Tried before the registry: authored JSON goes through `registry.apply`, so the
/// semantics are unchanged.
fn apply_runtime_component(
    world: &mut prism_ecs::World,
    entity: prism_ecs::Entity,
    object: &prism_reflect::Object,
) -> bool {
    // The type name is pinned to the `#[reflect(name = ...)]` value.
    const LOCAL_TRANSFORM: &str = "prism_engine::scene::LocalTransform";
    if object.type_name != LOCAL_TRANSFORM {
        return false;
    }
    let Ok(transform) =
        <prism_scene::components::LocalTransform as prism_reflect::Reflect>::from_object(object)
    else {
        return false;
    };
    world.insert(entity, transform);
    true
}

unsafe extern "C" fn host_remove_component(
    user_data: *mut std::ffi::c_void,
    entity: EntityHandle,
    type_name_ptr: *const u8,
    type_name_len: usize,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(name) = (unsafe { type_name(type_name_ptr, type_name_len) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(type_id) = context.registry.type_id(name) else {
        return UNKNOWN_TYPE;
    };
    let entity = handle_to_entity(entity);
    if !context.world.is_alive(entity) {
        return INVALID_ARGUMENT;
    }
    if context
        .registry
        .remove_component(context.world, entity, type_id)
    {
        SUCCESS
    } else {
        INVALID_ARGUMENT
    }
}

unsafe extern "C" fn host_despawn_entity(
    user_data: *mut std::ffi::c_void,
    entity: EntityHandle,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    let entity = handle_to_entity(entity);
    if !context.world.is_alive(entity) {
        return INVALID_ARGUMENT;
    }
    context.world.despawn(entity);
    SUCCESS
}

unsafe extern "C" fn host_is_alive(user_data: *mut std::ffi::c_void, entity: EntityHandle) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return -1;
    };
    i32::from(context.world.is_alive(handle_to_entity(entity)))
}

/// Raycast query (#522): the geometry implementation is injected by the host; this
/// crate only adapts the ABI.
unsafe extern "C" fn host_raycast(
    user_data: *mut std::ffi::c_void,
    origin: *const [f32; 3],
    direction: *const [f32; 3],
    max_distance: f32,
    layer_mask: u32,
    out_hit: *mut RaycastHit,
) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    if origin.is_null() || direction.is_null() || out_hit.is_null() {
        return INVALID_ARGUMENT;
    }
    let (origin, direction) = unsafe { (*origin, *direction) };
    let Some(raycast) = context.raycast else {
        return prism_project_api::UNSUPPORTED;
    };
    let Some(hit) = raycast(&*context.world, origin, direction, max_distance, layer_mask) else {
        return 0;
    };
    let handle = EntityHandle {
        id: hit.entity.id(),
        generation: hit.entity.generation(),
    };
    unsafe {
        out_hit.write(RaycastHit {
            entity: handle,
            distance: hit.distance,
            point: hit.point,
            normal: hit.normal,
            is_trigger: u8::from(hit.is_trigger),
            _reserved: [0; 3],
        });
    }
    1
}

/// Records the scene switch requested by project code (#482). The host performs it after the tick.
unsafe extern "C" fn host_load_scene(
    user_data: *mut std::ffi::c_void,
    path: *const u8,
    path_len: usize,
    additive: u8,
) -> i32 {
    let Some(context) = (user_data as *mut CommandContext).as_mut() else {
        return INVALID_ARGUMENT;
    };
    if path.is_null() || path_len == 0 {
        return INVALID_ARGUMENT;
    }
    let bytes = unsafe { core::slice::from_raw_parts(path, path_len) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return INVALID_ARGUMENT;
    };
    context.scene_request = Some(SceneRequest::Load {
        path: path.to_string(),
        additive: additive != 0,
    });
    SUCCESS
}

/// Records an unload request (#482).
unsafe extern "C" fn host_unload_scene(user_data: *mut std::ffi::c_void) -> i32 {
    let Some(context) = (user_data as *mut CommandContext).as_mut() else {
        return INVALID_ARGUMENT;
    };
    context.scene_request = Some(SceneRequest::Unload);
    SUCCESS
}

unsafe extern "C" fn host_draw_quad(user_data: *mut std::ffi::c_void, quad: UiQuad) -> i32 {
    let Some(context) = (unsafe { context(user_data) }) else {
        return INVALID_ARGUMENT;
    };
    context.quads.push(quad);
    SUCCESS
}

/// Writes the engine-provided scene state to disk (#484). Project code calls it through
/// `GameWorld::save_scene_state`.
unsafe extern "C" fn host_save_scene_state(user_data: *mut std::ffi::c_void) -> i32 {
    let Some(context) = (unsafe { (user_data as *mut CommandContext<'_>).as_mut() }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    match context.save_state {
        Some(save) => {
            save(context.world);
            prism_project_api::SUCCESS
        }
        None => prism_project_api::UNSUPPORTED,
    }
}

/// Reads the scene state: 1 = restored, 0 = no saved state.
unsafe extern "C" fn host_load_scene_state(user_data: *mut std::ffi::c_void) -> i32 {
    let Some(context) = (unsafe { (user_data as *mut CommandContext<'_>).as_mut() }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    match context.load_state {
        Some(load) => i32::from(load(context.world)),
        None => prism_project_api::UNSUPPORTED,
    }
}

unsafe extern "C" fn host_asset_exists(
    user_data: *mut std::ffi::c_void,
    path: *const u8,
    path_len: usize,
) -> i32 {
    let Some(path) = (unsafe { asset_path_str(path, path_len) }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    let Some(context) = (unsafe { (user_data as *mut CommandContext<'_>).as_ref() }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    i32::from(context.asset.exists(path))
}

unsafe extern "C" fn host_read_asset_file(
    user_data: *mut std::ffi::c_void,
    path: *const u8,
    path_len: usize,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> i32 {
    if out_ptr.is_null() || out_len.is_null() {
        return prism_project_api::INVALID_ARGUMENT;
    }
    let Some(path) = (unsafe { asset_path_str(path, path_len) }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    let Some(context) = (unsafe { (user_data as *mut CommandContext<'_>).as_mut() }) else {
        return prism_project_api::INVALID_ARGUMENT;
    };
    match context.asset.read(path) {
        Ok(bytes) => {
            context.asset_bytes = bytes;
            unsafe {
                *out_ptr = context.asset_bytes.as_ptr();
                *out_len = context.asset_bytes.len();
            }
            prism_project_api::SUCCESS
        }
        Err(_) => prism_project_api::INVALID_ARGUMENT,
    }
}

unsafe fn asset_path_str<'a>(path: *const u8, path_len: usize) -> Option<&'a str> {
    if path.is_null() && path_len != 0 {
        return None;
    }
    let bytes = if path_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(path, path_len) }
    };
    std::str::from_utf8(bytes).ok()
}

pub struct ProjectModule {
    path: PathBuf,
    // Kept solely to keep the module loaded while its callbacks are registered.
    _library: Library,
    api: ProjectApi,
}

impl ProjectModule {
    /// Validates that a project API satisfies the host ABI and schema requirements.
    pub fn validate_api(
        api: &ProjectApi,
        expected_fingerprint: u64,
        path: &Path,
    ) -> Result<(), ProjectModuleError> {
        if api.abi_version != ABI_VERSION {
            return Err(ProjectModuleError::AbiMismatch {
                path: path.to_path_buf(),
                actual: api.abi_version,
                expected: ABI_VERSION,
            });
        }
        if api.schema_fingerprint != expected_fingerprint {
            return Err(ProjectModuleError::SchemaMismatch {
                path: path.to_path_buf(),
                actual: api.schema_fingerprint,
                expected: expected_fingerprint,
            });
        }
        Ok(())
    }

    /// Load and validate a project module against the builtin schema fingerprint.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ProjectModuleError> {
        Self::load_with_fingerprint(path, prism_scene::builtin_schema_fingerprint())
    }

    /// Load and validate a project module against a specific component registry.
    pub fn load_with_registry(
        path: impl AsRef<Path>,
        registry: &prism_scene::ComponentRegistry,
    ) -> Result<Self, ProjectModuleError> {
        Self::load_with_fingerprint(path, registry.schema_fingerprint())
    }

    /// Load and validate a project module against an expected schema fingerprint.
    pub fn load_with_fingerprint(
        path: impl AsRef<Path>,
        expected_fingerprint: u64,
    ) -> Result<Self, ProjectModuleError> {
        let path = path.as_ref().to_path_buf();
        let library =
            unsafe { Library::new(&path) }.map_err(|source| ProjectModuleError::Load {
                path: path.clone(),
                source,
            })?;
        let get_api: Symbol<'_, GetProjectApi> = unsafe {
            library
                .get(GET_API_SYMBOL)
                .map_err(|_| ProjectModuleError::MissingEntry { path: path.clone() })?
        };
        let mut api = ProjectApi::empty();
        let status = unsafe { get_api(&mut api) };
        if status != SUCCESS {
            return Err(ProjectModuleError::GetApi { path, status });
        }
        Self::validate_api(&api, expected_fingerprint, &path)?;
        Ok(Self {
            path,
            _library: library,
            api,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn start(self) -> Result<ProjectSession, ProjectModuleError> {
        let (Some(create), Some(tick), Some(shutdown)) =
            (self.api.create, self.api.tick, self.api.shutdown)
        else {
            return Err(ProjectModuleError::IncompleteApi { path: self.path });
        };
        let mut host = Box::new(HostApi::logging_only());
        host.schema_fingerprint = self.api.schema_fingerprint;
        host.log = Some(host_log);
        let mut state = std::ptr::null_mut();
        let status = unsafe { create(host.as_ref(), &mut state) };
        if status != SUCCESS || state.is_null() {
            return Err(ProjectModuleError::Create {
                path: self.path.clone(),
                status,
            });
        }
        Ok(ProjectSession {
            module: self,
            host,
            state,
            tick,
            shutdown,
        })
    }
}

pub struct ProjectSession {
    module: ProjectModule,
    host: Box<HostApi>,
    state: *mut std::ffi::c_void,
    tick: prism_project_api::TickProject,
    shutdown: prism_project_api::ShutdownProject,
}

impl ProjectSession {
    pub fn module(&self) -> &ProjectModule {
        &self.module
    }

    pub fn host_capabilities(&self) -> u64 {
        self.host.capabilities
    }

    pub fn tick(&mut self, host: &HostApi, dt: f32) -> Result<(), ProjectModuleError> {
        let status = unsafe { (self.tick)(host, self.state, dt) };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(ProjectModuleError::Tick {
                path: self.module.path.clone(),
                status,
            })
        }
    }
}

impl Drop for ProjectSession {
    fn drop(&mut self) {
        unsafe { (self.shutdown)(self.state) };
        // `module` is deliberately dropped after shutdown so no callback can
        // point into an unloaded library.
    }
}

unsafe extern "C" fn host_log(
    _user_data: *mut std::ffi::c_void,
    level: u32,
    message: *const u8,
    message_len: usize,
) {
    if message.is_null() {
        return;
    }
    let bytes = if message_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(message, message_len) }
    };
    let text = String::from_utf8_lossy(bytes);
    // The target is always `project`: the editor Console groups module logs under
    // the `Game` source by target (`logging::source_for`), keeping them apart from
    // engine / editor logs.
    match level {
        0 => log::error!(target: "project", "{text}"),
        1 => log::warn!(target: "project", "{text}"),
        2 => log::debug!(target: "project", "{text}"),
        _ => log::info!(target: "project", "{text}"),
    }
}

impl std::fmt::Debug for ProjectModule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectModule")
            .field("path", &self.path)
            .field("abi_version", &self.api.abi_version)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_reflect::Reflect;

    fn registry() -> prism_scene::ComponentRegistry {
        let mut registry = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut registry);
        registry
    }

    struct FixedAssets;

    impl AssetIo for FixedAssets {
        fn exists(&self, path: &str) -> bool {
            path == "lake_layers.json"
        }

        fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
            if path == "lake_layers.json" {
                Ok(br#"{"layers":[]}"#.to_vec())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, path))
            }
        }
    }

    /// #484: the state capability is injected optionally; it is not advertised when missing.
    #[test]
    fn scene_state_capability_requires_an_injected_implementation() {
        fn save(_world: &prism_ecs::World) {}
        fn load(_world: &mut prism_ecs::World) -> bool {
            true
        }

        let mut world = prism_ecs::World::new();
        let registry = registry();

        let mut plain = WorldCommands::new(&mut world, &registry, 1);
        let api = plain.api();
        assert_eq!(api.capabilities & HOST_CAP_STATE, 0);
        // The callback itself is always present (the host implements it), but it reports
        // UNSUPPORTED when nothing was injected.
        let callback = api.save_scene_state.expect("callback");
        assert_eq!(
            unsafe { callback(api.user_data) },
            prism_project_api::UNSUPPORTED
        );

        let mut wired = WorldCommands::new(&mut world, &registry, 1).with_scene_state(save, load);
        let api = wired.api();
        assert_ne!(api.capabilities & HOST_CAP_STATE, 0);
        let callback = api.load_scene_state.expect("callback");
        assert_eq!(unsafe { callback(api.user_data) }, 1);
    }

    /// #482: scene-switch requests are visible only when the host declares it will
    /// execute them, and are recorded for later execution.
    #[test]
    fn scene_switch_requests_are_recorded_only_when_enabled() {
        let mut world = prism_ecs::World::new();
        let registry = registry();

        // Capability not declared: the callback is not exposed, so project code does not
        // assume the switch took effect.
        let mut plain = WorldCommands::new(&mut world, &registry, 3);
        let api = plain.api();
        assert_eq!(api.capabilities & HOST_CAP_SCENE, 0);
        assert!(api.load_scene.is_none() && api.unload_scene.is_none());
        assert!(plain.take_scene_request().is_none());

        let mut wired = WorldCommands::new(&mut world, &registry, 3).with_scene_switch();
        let api = wired.api();
        assert_ne!(api.capabilities & HOST_CAP_SCENE, 0);
        let load = api.load_scene.expect("load_scene callback");
        let unload = api.unload_scene.expect("unload_scene callback");

        let path = b"assets/scenes/second.scene.json";
        assert_eq!(
            unsafe { load(api.user_data, path.as_ptr(), path.len(), 0) },
            SUCCESS
        );
        assert_eq!(
            wired.take_scene_request(),
            Some(SceneRequest::Load {
                path: "assets/scenes/second.scene.json".to_string(),
                additive: false,
            })
        );
        assert!(wired.take_scene_request().is_none());

        // The additive flag is passed through.
        assert_eq!(
            unsafe { load(api.user_data, path.as_ptr(), path.len(), 1) },
            SUCCESS
        );
        assert_eq!(
            wired.take_scene_request(),
            Some(SceneRequest::Load {
                path: "assets/scenes/second.scene.json".to_string(),
                additive: true,
            })
        );

        assert_eq!(unsafe { unload(api.user_data) }, SUCCESS);
        assert_eq!(wired.take_scene_request(), Some(SceneRequest::Unload));

        // Empty and non-UTF-8 paths report an error instead of recording a half request.
        assert_eq!(
            unsafe { load(api.user_data, path.as_ptr(), 0, 0) },
            prism_project_api::INVALID_ARGUMENT
        );
        let invalid = [0xffu8, 0xfe];
        assert_eq!(
            unsafe { load(api.user_data, invalid.as_ptr(), invalid.len(), 0) },
            prism_project_api::INVALID_ARGUMENT
        );
        assert!(wired.take_scene_request().is_none());
    }

    /// #522: the raycast capability bit is set only after the host injects an
    /// implementation; the hit is written back as POD.
    #[test]
    fn raycast_capability_requires_an_injected_implementation() {
        fn fake_raycast(
            _world: &prism_ecs::World,
            origin: [f32; 3],
            direction: [f32; 3],
            _max_distance: f32,
            layer_mask: u32,
        ) -> Option<RaycastResult> {
            if layer_mask == 0 {
                return None;
            }
            Some(RaycastResult {
                entity: prism_ecs::Entity::from_raw(7, 2),
                distance: direction[0],
                point: origin,
                normal: [0.0, 1.0, 0.0],
                is_trigger: true,
            })
        }

        let mut world = prism_ecs::World::new();
        let registry = registry();

        let mut plain = WorldCommands::new(&mut world, &registry, 1);
        let api = plain.api();
        assert_eq!(api.capabilities & HOST_CAP_PHYSICS, 0);
        assert!(api.raycast.is_none());

        let mut wired = WorldCommands::new(&mut world, &registry, 1).with_raycast(fake_raycast);
        let api = wired.api();
        assert_ne!(api.capabilities & HOST_CAP_PHYSICS, 0);
        let raycast = api.raycast.expect("raycast callback");

        let mut hit = RaycastHit::default();
        assert_eq!(
            unsafe {
                raycast(
                    api.user_data,
                    &[1.0, 2.0, 3.0],
                    &[4.0, 0.0, 0.0],
                    10.0,
                    1,
                    &mut hit,
                )
            },
            1
        );
        assert_eq!(
            hit.entity,
            EntityHandle {
                id: 7,
                generation: 2
            }
        );
        assert_eq!(hit.distance, 4.0);
        assert_eq!(hit.point, [1.0, 2.0, 3.0]);
        assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
        assert_eq!(hit.is_trigger, 1);

        // A miss returns 0; a null pointer is an argument error.
        assert_eq!(
            unsafe { raycast(api.user_data, &[0.0; 3], &[0.0; 3], 10.0, 0, &mut hit) },
            0
        );
        assert_eq!(
            unsafe {
                raycast(
                    api.user_data,
                    &[0.0; 3],
                    &[0.0; 3],
                    10.0,
                    1,
                    std::ptr::null_mut(),
                )
            },
            prism_project_api::INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe {
                raycast(
                    api.user_data,
                    std::ptr::null(),
                    &[0.0; 3],
                    10.0,
                    1,
                    &mut hit,
                )
            },
            prism_project_api::INVALID_ARGUMENT
        );
    }

    /// The loose asset access the project module gets uses the same host implementation
    /// (through `HOST_CAP_ASSET`).
    #[test]
    fn asset_capability_reads_through_host_callbacks() {
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1).with_assets(&FixedAssets);
        let api = commands.api();
        assert_ne!(api.capabilities & HOST_CAP_ASSET, 0);

        let exists = api.asset_exists.expect("asset capability");
        let read = api.read_asset_file.expect("asset capability");
        let name = b"lake_layers.json";
        assert_eq!(
            unsafe { exists(api.user_data, name.as_ptr(), name.len()) },
            1
        );
        let missing = b"nope.json";
        assert_eq!(
            unsafe { exists(api.user_data, missing.as_ptr(), missing.len()) },
            0
        );

        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        let status = unsafe { read(api.user_data, name.as_ptr(), name.len(), &mut ptr, &mut len) };
        assert_eq!(status, SUCCESS);
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert_eq!(bytes, br#"{"layers":[]}"#);
    }

    /// A `LocalTransform` written by project code through the ABI must really reach the world.
    ///
    /// The registry deliberately keeps it out of authored documents (authors write only
    /// `transform`, which the loader bakes), but entities spawned by a runtime module
    /// would have no transform and pile up at the origin — the plugin form must be
    /// equivalent to `world.set(entity, LocalTransform)` in `GameWorld::host(...)`.
    #[test]
    fn plugin_set_can_write_local_transform() {
        use prism_scene::components::LocalTransform;

        let mut world = prism_ecs::World::new();
        let registry = registry();
        let entity = world.spawn();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let api = commands.api();
        let set = api.set_component.expect("component capability");

        let mut transform = LocalTransform::default();
        transform.position.x = 1.0;
        transform.position.y = 2.0;
        transform.position.z = 3.0;
        transform.scale.x = 2.0;
        transform.scale.y = 2.0;
        transform.scale.z = 2.0;
        let bytes = prism_reflect::rtti_bin::encode_one(
            &Reflect::to_object(&transform),
            registry.type_registry(),
        )
        .expect("encode LocalTransform");
        let status = unsafe {
            set(
                api.user_data,
                EntityHandle {
                    id: entity.id(),
                    generation: entity.generation(),
                },
                bytes.as_ptr(),
                bytes.len(),
            )
        };
        assert_eq!(status, SUCCESS);

        let stored = world
            .get::<LocalTransform>(entity)
            .expect("transform inserted");
        assert_eq!(stored.position.x, 1.0);
        assert_eq!(stored.position.z, 3.0);
        assert_eq!(stored.scale.y, 2.0);
    }

    #[test]
    fn validate_api_accepts_matching_abi_and_fingerprint() {
        let path = Path::new("test.so");
        let fp = prism_scene::builtin_schema_fingerprint();
        let api = ProjectApi {
            abi_version: ABI_VERSION,
            schema_fingerprint: fp,
            create: None,
            tick: None,
            shutdown: None,
        };
        assert!(ProjectModule::validate_api(&api, fp, path).is_ok());
    }

    #[test]
    fn validate_api_rejects_abi_mismatch() {
        let path = Path::new("test.so");
        let fp = prism_scene::builtin_schema_fingerprint();
        let api = ProjectApi {
            abi_version: ABI_VERSION + 1,
            schema_fingerprint: fp,
            create: None,
            tick: None,
            shutdown: None,
        };
        let err =
            ProjectModule::validate_api(&api, fp, path).expect_err("mismatched ABI must fail");
        assert!(
            matches!(err, ProjectModuleError::AbiMismatch { actual, expected, .. } if actual == ABI_VERSION + 1 && expected == ABI_VERSION)
        );
    }

    #[test]
    fn validate_api_rejects_schema_fingerprint_mismatch() {
        let path = Path::new("test.so");
        let fp = prism_scene::builtin_schema_fingerprint();
        let api = ProjectApi {
            abi_version: ABI_VERSION,
            schema_fingerprint: fp ^ 0xdeadbeef,
            create: None,
            tick: None,
            shutdown: None,
        };
        let err = ProjectModule::validate_api(&api, fp, path)
            .expect_err("mismatched fingerprint must fail");
        assert!(
            matches!(err, ProjectModuleError::SchemaMismatch { actual, expected, .. } if actual == fp ^ 0xdeadbeef && expected == fp)
        );
    }

    #[test]
    fn missing_module_reports_path() {
        let error = ProjectModule::load("target/does-not-exist/project.so")
            .expect_err("missing module must fail");
        assert!(error.to_string().contains("project.so"));
    }

    #[test]
    fn loads_and_runs_project_module_when_configured() {
        let Some(path) = std::env::var_os("PRISMREV_TEST_PROJECT_MODULE") else {
            return;
        };
        let module = ProjectModule::load(path).expect("project module should load");
        let mut session = module.start().expect("project session should start");
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let host = commands.api();
        session
            .tick(&host, 1.0 / 60.0)
            .expect("project tick should succeed");
    }

    #[test]
    fn world_commands_spawn_and_name_entities_through_opaque_handles() {
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let api = commands.api();
        assert_ne!(api.capabilities & HOST_CAP_COMPONENTS, 0);

        let mut raw = EntityHandle::default();
        let spawn = api.spawn_entity.expect("world command capability");
        let set_name = api.set_entity_name.expect("world command capability");
        assert_eq!(unsafe { spawn(api.user_data, &mut raw) }, SUCCESS);
        let name = b"from_project_module";
        assert_eq!(
            unsafe { set_name(api.user_data, raw, name.as_ptr(), name.len()) },
            SUCCESS
        );
        let entity = handle_to_entity(raw);
        assert!(world.is_alive(entity));
        assert_eq!(
            world
                .get::<prism_scene::components::Name>(entity)
                .unwrap()
                .0,
            "from_project_module"
        );

        world.despawn(entity);
        assert_eq!(
            unsafe { set_name(api.user_data, raw, name.as_ptr(), name.len()) },
            INVALID_ARGUMENT
        );
    }

    #[test]
    fn components_round_trip_as_rttibin_bytes() {
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let api = commands.api();

        let entity = world.spawn();
        let handle = EntityHandle {
            id: entity.id(),
            generation: entity.generation(),
        };
        let name = prism_scene::components::Name("round_trip".to_owned());
        let object = name.to_object();
        let bytes = rtti_bin::encode_one(&object, registry.type_registry()).expect("encode");

        let set = api.set_component.expect("component capability");
        assert_eq!(
            unsafe { set(api.user_data, handle, bytes.as_ptr(), bytes.len()) },
            SUCCESS
        );
        assert_eq!(
            world
                .get::<prism_scene::components::Name>(entity)
                .unwrap()
                .0,
            "round_trip"
        );

        // Query: probe the length first, then fill.
        let query = api.query_entities.expect("component capability");
        let type_name = prism_scene::components::Name::type_desc().name.as_bytes();
        let mut count = 0usize;
        assert_eq!(
            unsafe {
                query(
                    api.user_data,
                    type_name.as_ptr(),
                    type_name.len(),
                    std::ptr::null_mut(),
                    0,
                    &mut count,
                )
            },
            SUCCESS
        );
        assert_eq!(count, 1);
        let mut out = [EntityHandle::default(); 4];
        assert_eq!(
            unsafe {
                query(
                    api.user_data,
                    type_name.as_ptr(),
                    type_name.len(),
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            SUCCESS
        );
        assert_eq!(out[0], handle);

        // Read: the pointer is valid until the next host call, so copy immediately.
        let get = api.get_component.expect("component capability");
        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        assert_eq!(
            unsafe {
                get(
                    api.user_data,
                    handle,
                    type_name.as_ptr(),
                    type_name.len(),
                    &mut ptr,
                    &mut len,
                )
            },
            SUCCESS
        );
        let payload = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        let decoded = rtti_bin::decode(&payload).expect("decode");
        let restored =
            prism_scene::components::Name::from_object(&decoded.objects[0]).expect("restore");
        assert_eq!(restored.0, "round_trip");

        let remove = api.remove_component.expect("component capability");
        assert_eq!(
            unsafe { remove(api.user_data, handle, type_name.as_ptr(), type_name.len()) },
            SUCCESS
        );
        assert!(world.get::<prism_scene::components::Name>(entity).is_none());
    }

    #[test]
    fn despawn_is_alive_and_draw_quad_cross_the_abi() {
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let api = commands.api();

        let spawn = api.spawn_entity.expect("world command capability");
        let is_alive = api.is_alive.expect("liveness capability");
        let despawn = api.despawn_entity.expect("despawn capability");
        let draw = api.draw_quad.expect("draw capability");

        let mut handle = EntityHandle::default();
        assert_eq!(unsafe { spawn(api.user_data, &mut handle) }, SUCCESS);
        assert_eq!(unsafe { is_alive(api.user_data, handle) }, 1);

        let quad = UiQuad {
            rect: [1.0, 2.0, 3.0, 4.0],
            color: [1.0; 4],
            border_radius: 2.0,
            layer: 3,
        };
        assert_eq!(unsafe { draw(api.user_data, quad) }, SUCCESS);
        assert_eq!(commands.take_quads().len(), 1);

        assert_eq!(unsafe { despawn(api.user_data, handle) }, SUCCESS);
        assert_eq!(unsafe { is_alive(api.user_data, handle) }, 0);
        // A second despawn must be rejected instead of silently succeeding.
        assert_eq!(unsafe { despawn(api.user_data, handle) }, INVALID_ARGUMENT);
    }

    #[test]
    fn unknown_type_names_are_rejected() {
        let mut world = prism_ecs::World::new();
        let registry = registry();
        let mut commands = WorldCommands::new(&mut world, &registry, 1);
        let api = commands.api();
        let query = api.query_entities.expect("component capability");
        let name = b"prism_engine::does::NotExist";
        let mut count = 0usize;
        assert_eq!(
            unsafe {
                query(
                    api.user_data,
                    name.as_ptr(),
                    name.len(),
                    std::ptr::null_mut(),
                    0,
                    &mut count,
                )
            },
            UNKNOWN_TYPE
        );
    }
}
