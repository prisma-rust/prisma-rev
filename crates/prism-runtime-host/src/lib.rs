//! Dynamic project module loading for the same-build desktop preview.
//!
//! The loaded library is kept alive for the whole host lifetime. This is
//! required because the engine schedule stores callbacks owned by the module.
//! Unloading before the engine is dropped would leave dangling function/code
//! pointers.
//!
//! ## 边界
//!
//! 宿主从不把 `World` / `Engine` 指针交给项目模块。模块只能通过
//! [`WorldCommands`] 构造的 [`HostApi`] 函数表操作世界；组件数据以 RTTIBin
//! 字节流往返，类型解析留在宿主自己的 `ComponentRegistry` 里。

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

/// ABI 载荷类型，供宿主侧直接使用（无需再依赖 `prism-project-api`）。
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

/// 宿主侧一次 tick 的命令上下文。
///
/// 回调只拿到一份 `user_data`，所以世界、注册表和临时缓冲必须打包在同一个
/// 结构里；它的生命周期由 [`WorldCommands`] 保证，不会活过本次 tick。
/// 宿主侧的松散资产访问。实现放在宿主（编辑器 / Player），资产根解析仍然只有
/// `prism-engine::asset_paths` 一份实现。
pub trait AssetIo {
    fn exists(&self, path: &str) -> bool;
    fn read(&self, path: &str) -> std::io::Result<Vec<u8>>;
}

/// 默认实现：不支持松散资产（例如只跑 `.pak` 内容的宿主）。
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

/// 项目模块请求的换场动作（#482）。
///
/// 回调执行时引擎的世界正被借用，所以只能**记录请求**；宿主在本 tick 结束后
/// 调 [`WorldCommands::take_scene_request`] 并真正切换场景。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneRequest {
    /// 加载 `.scene.json`；`additive` = 叠加（不卸载旧场景）。
    Load { path: String, additive: bool },
    /// 卸载当前场景（`Persistent` 子树保留）。
    Unload,
}

/// 宿主注入的射线查询结果（#522）。
///
/// 本 crate 不依赖 `prism-engine`（会带进第二份引擎），所以几何实现由宿主以
/// 函数指针注入；编辑器传 `prism_engine::physics::raycast` 的适配器。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RaycastResult {
    pub entity: prism_ecs::Entity,
    pub distance: f32,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub is_trigger: bool,
}

/// 射线查询实现的签名（`prism_engine::physics::raycast` 的等价物）。
pub type RaycastFn = fn(&prism_ecs::World, [f32; 3], [f32; 3], f32, u32) -> Option<RaycastResult>;

struct CommandContext<'a> {
    world: &'a mut prism_ecs::World,
    registry: &'a prism_scene::ComponentRegistry,
    /// `get_component` 的返回缓冲：指针只在下次宿主调用前有效。
    scratch: Vec<u8>,
    /// 项目提交的 UI 四边形；宿主在 UI 渲染前取走。
    quads: Vec<UiQuad>,
    scene_generation: u64,
    input: InputSnapshot,
    asset: &'a dyn AssetIo,
    /// `read_asset_file` 的返回缓冲：指针只在下次宿主调用前有效。
    asset_bytes: Vec<u8>,
    /// 引擎自带的场景存档（#484）。宿主注入，**本 crate 不依赖 prism-engine**，
    /// 所以走函数指针而不是直接调用。
    save_state: Option<fn(&prism_ecs::World)>,
    load_state: Option<fn(&mut prism_ecs::World) -> bool>,
    /// 本 tick 项目代码请求的换场（#482），由宿主 tick 结束后取走。
    scene_request: Option<SceneRequest>,
    /// 宿主是否会执行 `scene_request`；只有会执行的宿主才对外声明能力位。
    scene_switch: bool,
    /// 宿主的射线查询实现（#522）；`None` 时不声明 `HOST_CAP_PHYSICS`。
    raycast: Option<RaycastFn>,
}

/// 把宿主世界包装成 [`HostApi`] 函数表。
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

    /// 注入松散资产访问（宿主实现 `AssetIo`）。
    pub fn with_assets(mut self, asset: &'a dyn AssetIo) -> Self {
        self.context.asset = asset;
        self
    }

    /// 注入引擎自带的场景存档实现（宿主传 `prism_engine::scene_state::*`）。
    ///
    /// 本 crate 不能依赖 `prism-engine`（会带进第二份引擎），所以只能收函数指针。
    pub fn with_scene_state(
        mut self,
        save: fn(&prism_ecs::World),
        load: fn(&mut prism_ecs::World) -> bool,
    ) -> Self {
        self.context.save_state = Some(save);
        self.context.load_state = Some(load);
        self
    }

    /// 声明宿主会执行项目代码的换场请求（#482）。
    ///
    /// 只有真的会在 tick 结束后调用 [`Self::take_scene_request`] 并换场的宿主
    /// 才能打开这个能力位；否则项目代码会以为请求生效了。
    pub fn with_scene_switch(mut self) -> Self {
        self.context.scene_switch = true;
        self
    }

    /// 注入射线查询实现（#522）。
    ///
    /// 只有真正提供查询的宿主才打开 `HOST_CAP_PHYSICS`；编辑器传
    /// `prism_engine::physics::raycast` 的适配器（见 `engine_raycast`）。
    pub fn with_raycast(mut self, raycast: RaycastFn) -> Self {
        self.context.raycast = Some(raycast);
        self
    }

    /// 本帧输入快照；`ui`/项目代码通过 `HostApi::input` 读取。
    pub fn set_input(&mut self, input: InputSnapshot) {
        self.context.input = input;
    }

    /// 取走项目本帧提交的 UI 四边形（宿主在 UI 渲染前合并）。
    pub fn take_quads(&mut self) -> Vec<UiQuad> {
        std::mem::take(&mut self.context.quads)
    }

    /// 取走本 tick 项目代码请求的换场（#482）；宿主在 tick 结束后执行。
    pub fn take_scene_request(&mut self) -> Option<SceneRequest> {
        self.context.scene_request.take()
    }

    /// 构造本 tick 的能力表。指针只在 `api` 返回后到本 tick 结束之间有效。
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
/// `user_data` 必须来自本 tick 的 [`WorldCommands::api`]。
unsafe fn context<'a>(user_data: *mut std::ffi::c_void) -> Option<&'a mut CommandContext<'a>> {
    if user_data.is_null() {
        return None;
    }
    unsafe { user_data.cast::<CommandContext>().as_mut() }
}

/// # Safety
/// `ptr`/`len` 必须描述一块本次调用期间有效的 UTF-8 字节。
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
    // 运行时组件先走（注册表对它们只会打一条「unknown」警告再拒绝）。
    if apply_runtime_component(context.world, entity, object) {
        return SUCCESS;
    }
    if context.registry.apply_object(context.world, entity, object) {
        return SUCCESS;
    }
    UNKNOWN_TYPE
}

/// 写「运行时组件」：注册表刻意不让作者文档挂载它们，但工程代码必须能写。
///
/// `LocalTransform` 在注册表里只登记为**类型**（见 `prism-scene` 的
/// `register_builtin_components`：作者只写 `transform`，由 loader / cooker bake 成
/// `LocalTransform` + `LocalToWorld`，否则会在世界矩阵算完之后被覆盖）。
/// 玩家形态里工程代码是 `GameWorld::host(...)` → `world.set(entity, LocalTransform)`，
/// 直接进 ECS；插件形态必须有一条等价通路，否则模块 spawn 出来的实体没有变换，
/// 只能停在原点。
///
/// 只在注册表之前尝试：作者 JSON 走的是 `registry.apply`，语义一点没变。
fn apply_runtime_component(
    world: &mut prism_ecs::World,
    entity: prism_ecs::Entity,
    object: &prism_reflect::Object,
) -> bool {
    // 类型名与 `#[reflect(name = ...)]` 固定值对齐。
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

/// 射线查询（#522）：几何实现由宿主注入，本 crate 只做 ABI 适配。
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

/// 记录项目代码的换场请求（#482）。真正执行由宿主在 tick 结束后完成。
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

/// 记录卸载请求（#482）。
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

/// 把引擎自带的场景存档写盘（#484）。项目代码通过 `GameWorld::save_scene_state` 调用。
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

/// 读取场景存档：1 = 已恢复，0 = 没有存档。
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
    /// Load and validate a project module. The library remains loaded until
    /// this value is dropped.
    /// Validate that a project API satisfies the host ABI and schema requirements.
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
    // target 固定为 `project`：编辑器 Console 按 target 把模块日志归到 `Game`
    // 来源（`logging::source_for`），和引擎 / 编辑器日志分开。
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

    /// #484：存档能力是可选注入的，没注入时不宣称支持。
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
        // 回调本身总是在的（宿主统一实现），但没有注入时明确报 UNSUPPORTED。
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

    /// #482：换场请求只在宿主声明会执行时才可见，并且是延迟执行的记录。
    #[test]
    fn scene_switch_requests_are_recorded_only_when_enabled() {
        let mut world = prism_ecs::World::new();
        let registry = registry();

        // 没声明能力：不暴露回调，避免项目代码以为换场生效。
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

        // additive 标志透传。
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

        // 空路径 / 非 UTF-8 路径明确报错，不记录半个请求。
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

    /// #522：射线查询只有宿主注入实现后才声明能力位；结果按 POD 写回。
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

        // 未命中返回 0；空指针是参数错误。
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

    /// 项目模块拿到的松散资产访问与宿主同一份实现（走 `HOST_CAP_ASSET`）。
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

    /// 工程代码通过 ABI 写的 `LocalTransform` 必须真的进世界。
    ///
    /// 注册表刻意不让作者文档挂载它（作者只写 `transform`，由 loader bake），
    /// 但运行时模块 spawn 出来的实体没有变换就只会堆在原点 —— 插件形态必须和
    /// `GameWorld::host(...)` 的 `world.set(entity, LocalTransform)` 等价。
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

        // 查询：先探长度再填充。
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

        // 读取：指针在下次宿主调用前有效，立刻拷贝。
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

        // 移除。
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
        // 二次销毁必须被拒绝，而不是静默成功。
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
