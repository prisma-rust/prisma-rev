//! # prism-game-api —— 与引擎实现无关的 gameplay API 层
//!
//! 对齐引擎的脚本绑定层：**同一份 gameplay 代码**既能被静态链接进
//! Player，也能被编译成动态库由编辑器宿主加载。区别只在 [`GameWorld`]
//! 背后接的是谁：
//!
//! - [`GameWorld::host`]：直接借用宿主自己的 `prism_ecs::World`，零序列化。
//! - [`GameWorld::plugin`]：通过宿主下发的 [`HostApi`] 函数表读写，组件数据
//!   以 RTTIBin 字节流跨边界。
//!
//! 两条路径暴露**完全相同的函数签名**，所以项目代码不需要写两套。
//!
//! ## 边界规则（不要破坏）
//!
//! - 本 crate **不依赖** `prism-engine` / `prism-render` / `prism-app`。
//!   一旦这里出现它们的依赖，插件就必须链接第二份引擎，`World` 类型身份
//!   分裂，ABI 立刻变成 UB。
//! - 组件类型不在这里镜像：`Style` / `Text` / `Name` / `Transform` 等直接来自
//!   `prism-scene`（纯数据 crate），项目自己依赖即可。
//! - 跨边界只传 [`EntityHandle`] 这类 `#[repr(C)]` POD 与字节流，不传引用、
//!   不传 `&mut World`、不传 trait object。

#[cfg(feature = "host")]
use prism_ecs::World;
use prism_ecs::{Component, Entity};
pub use prism_project_api::keys;
pub use prism_project_api::{EntityHandle, InputSnapshot};
use prism_reflect::Reflect;

#[cfg(feature = "plugin")]
use prism_project_api::{HostApi, HOST_CAP_SCENE, SUCCESS};
#[cfg(feature = "plugin")]
use prism_reflect::{rtti_bin, TypeRegistry};

/// 一条 2D UI 四边形（定义在 ABI crate，这里重新导出）。
pub use prism_project_api::UiQuad;

/// 把 ECS 句柄转成跨边界句柄。
pub fn handle_from_entity(entity: Entity) -> EntityHandle {
    EntityHandle {
        id: entity.id(),
        generation: entity.generation(),
    }
}

/// 把跨边界句柄还原成 ECS 句柄（宿主侧使用；插件侧无从构造真实 `Entity`）。
pub fn entity_from_handle(handle: EntityHandle) -> Entity {
    Entity::from_raw(handle.id, handle.generation)
}

/// gameplay 代码看到的世界。
///
/// 所有方法都在**宿主的**世界上操作；插件后端会把同样的调用翻译成宿主
/// 回调，所以调用方无需区分自己是怎么被编译的。
pub struct GameWorld<'a> {
    backend: Backend<'a>,
    /// 仅 host 后端使用：插件后端的输入来自 `HostApi::input`。
    ///
    /// 正常情况下 `input()` 优先读世界里的 `InputSnapshot` resource（引擎每帧
    /// 写入）；这个字段是资源还没写时的兜底与 `set_input` 的镜像。
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    input: InputSnapshot,
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    quads: Vec<UiQuad>,
}

enum Backend<'a> {
    #[cfg(feature = "host")]
    Host(&'a mut World),
    #[cfg(feature = "plugin")]
    Plugin(PluginBackend<'a>),
}

impl<'a> GameWorld<'a> {
    /// 直接借用宿主世界（静态 Player / 编辑器宿主内）。
    #[cfg(feature = "host")]
    pub fn host(world: &'a mut World) -> Self {
        Self {
            backend: Backend::Host(world),
            input: InputSnapshot::default(),
            quads: Vec::new(),
        }
    }

    /// 通过宿主下发的函数表操作宿主世界（动态库内）。
    ///
    /// `registry` 只用于把组件编码成 RTTIBin 的类型表；解码端自描述，
    /// 所以读取路径不需要它。
    #[cfg(feature = "plugin")]
    pub fn plugin(api: &'a HostApi, registry: &'a TypeRegistry) -> Self {
        Self {
            backend: Backend::Plugin(PluginBackend { api, registry }),
            input: api.input,
            quads: Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // 输入
    // ------------------------------------------------------------------

    pub fn input(&self) -> InputSnapshot {
        match &self.backend {
            // 静态 Player：引擎每帧把这一份写进 World resource（#589）。
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .get_resource::<InputSnapshot>()
                .copied()
                .unwrap_or(self.input),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.api.input,
        }
    }

    /// 覆盖本帧输入（host 后端）。
    ///
    /// 正常运行时不需要调用：`Engine::update` 每帧把
    /// [`prism_engine::input::input_snapshot`] 写进世界。这个入口留给测试与
    /// 自己驱动世界的宿主；写入后同一帧的 `input()` 立即读到新值。
    /// 插件后端的输入由宿主每 tick 通过 `HostApi::input` 下发。
    #[cfg(feature = "host")]
    pub fn set_input(&mut self, input: InputSnapshot) {
        self.input = input;
        if let Backend::Host(world) = &mut self.backend {
            world.insert_resource(input);
        }
    }

    pub fn screen_size(&self) -> [f32; 2] {
        self.input().screen
    }

    // ------------------------------------------------------------------
    // 资产（松散文件；`.pak` 内容仍由运行时资源包负责）
    // ------------------------------------------------------------------

    /// 松散资产是否存在（关卡 JSON、按时段回落的贴图等）。
    ///
    /// 插件后端还没有资产能力位，因此模块里恒为 `false`：需要资产的逻辑
    /// 目前只在静态 Player 里跑。
    pub fn asset_exists(&self, relative: &str) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => prism_engine::asset_paths::asset_exists(relative),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.asset_exists(relative),
        }
    }

    /// 保存引擎自带的场景存档（相机 / 灯光 / 变换，`scene_state.json`）。
    ///
    /// 存档路径由宿主决定（宿主按工程 `[app] company/name` 设置）；返回是否成功。
    /// 没有宿主回调时返回 `false` 并 warn 一次（#484）。
    pub fn save_scene_state(&self) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                prism_engine::scene_state::save_scene_state(world);
                true
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.save_scene_state(),
        }
    }

    /// 读取场景存档：`true` = 已恢复，`false` = 没有存档或宿主不支持。
    pub fn load_scene_state(&mut self) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => prism_engine::scene_state::load_scene_state(world),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.load_scene_state(),
        }
    }

    /// 请求切换场景（#482）：`additive = true` 保留旧场景（`LoadSceneMode::Additive`）。
    ///
    /// 返回 `true` 只表示**请求已交给宿主**；真正换场发生在本 tick 结束后，
    /// 下一帧的 [`Self::scene_generation`] 会递增。旧实体句柄全部失效。
    pub fn load_scene(&mut self, path: &str, additive: bool) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => {
                log::warn!(
                    "load_scene is only available through the runtime host (HOST_CAP_SCENE)"
                );
                false
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.load_scene(path, additive),
        }
    }

    /// 请求卸载当前场景（`Persistent` 子树保留）；同样是延迟执行。
    pub fn unload_scene(&mut self) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => {
                log::warn!(
                    "unload_scene is only available through the runtime host (HOST_CAP_SCENE)"
                );
                false
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.unload_scene(),
        }
    }

    /// 场景代数：宿主每次换场 / 卸载 +1。项目代码用它判断缓存的实体句柄是否失效。
    ///
    /// 静态 Player（host 后端）没有跨场景切换，恒为 `0`。
    pub fn scene_generation(&self) -> u64 {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => 0,
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.scene_generation(),
        }
    }

    /// 读取松散文本资产。
    pub fn read_asset_text(&self, relative: &str) -> std::io::Result<String> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => prism_engine::asset_paths::read_asset_text(relative),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.read_asset_text(relative),
        }
    }

    // ------------------------------------------------------------------
    // 实体
    // ------------------------------------------------------------------

    pub fn spawn(&mut self) -> EntityHandle {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => handle_from_entity(world.spawn()),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.spawn(),
        }
    }

    pub fn despawn(&mut self, entity: EntityHandle) {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.despawn(entity_from_handle(entity)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.despawn(entity),
        }
    }

    pub fn is_alive(&self, entity: EntityHandle) -> bool {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.is_alive(entity_from_handle(entity)),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.is_alive(entity),
        }
    }

    /// 拥有组件 `T` 的全部实体（与宿主 `World::query` 的 `Active` 语义一致）。
    pub fn query<T: Component + Reflect>(&self) -> Vec<EntityHandle> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world
                .query::<T>()
                .map(|(entity, _)| handle_from_entity(entity))
                .collect(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.query(T::type_desc().name),
        }
    }

    // ------------------------------------------------------------------
    // 组件
    // ------------------------------------------------------------------

    pub fn get<T: Component + Reflect + Clone>(&self, entity: EntityHandle) -> Option<T> {
        match &self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.get::<T>(entity_from_handle(entity)).cloned(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                let bytes = backend.get(entity, T::type_desc().name)?;
                let decoded = rtti_bin::decode(&bytes).ok()?;
                T::from_object(decoded.objects.first()?).ok()
            }
        }
    }

    /// 写入（无则新增）组件。句柄失效时返回 `false`。
    pub fn set<T: Component + Reflect>(&mut self, entity: EntityHandle, value: T) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => {
                let entity = entity_from_handle(entity);
                if !world.is_alive(entity) {
                    return false;
                }
                world.insert(entity, value);
                true
            }
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => {
                let object = value.to_object();
                match rtti_bin::encode_one(&object, backend.registry) {
                    Ok(bytes) => backend.set(entity, &bytes),
                    Err(error) => {
                        log::warn!("game-api: encode `{}` failed: {error}", T::type_desc().name);
                        false
                    }
                }
            }
        }
    }

    /// 移除组件，返回是否确实存在过。
    pub fn remove<T: Component + Reflect>(&mut self, entity: EntityHandle) -> bool {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(world) => world.remove::<T>(entity_from_handle(entity)).is_some(),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.remove(entity, T::type_desc().name),
        }
    }

    // ------------------------------------------------------------------
    // 2D 绘制
    // ------------------------------------------------------------------

    /// 提交一条 UI 四边形；宿主在本帧 UI 渲染前取走并合并进绘制列表。
    pub fn draw_quad(&mut self, quad: UiQuad) {
        match &mut self.backend {
            #[cfg(feature = "host")]
            Backend::Host(_) => self.quads.push(quad),
            #[cfg(feature = "plugin")]
            Backend::Plugin(backend) => backend.draw(quad),
        }
    }

    /// 取走本帧累积的四边形（宿主侧调用；插件后端由宿主自行取走）。
    #[cfg(feature = "host")]
    pub fn take_quads(&mut self) -> Vec<UiQuad> {
        std::mem::take(&mut self.quads)
    }
}

#[cfg(feature = "plugin")]
struct PluginBackend<'a> {
    api: &'a HostApi,
    registry: &'a TypeRegistry,
}

// ---------------------------------------------------------------------------
// 模块日志 → 宿主日志
// ---------------------------------------------------------------------------

/// 模块里 `log!` 记录的转发目标（宿主下发的 `HostApi::log`）。
#[cfg(feature = "plugin")]
#[derive(Clone, Copy)]
struct LogSink {
    user_data: usize,
    log: prism_project_api::LogMessage,
}

#[cfg(feature = "plugin")]
static LOG_SINK: std::sync::OnceLock<LogSink> = std::sync::OnceLock::new();

#[cfg(feature = "plugin")]
struct HostLogger;

/// `log::set_logger` 要 `&'static`：workspace 的 `log` 关了 `alloc` feature，
/// 用静态实例而不是 `set_boxed_logger`。
#[cfg(feature = "plugin")]
static HOST_LOGGER: HostLogger = HostLogger;

#[cfg(feature = "plugin")]
impl log::Log for HostLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        LOG_SINK.get().is_some()
    }

    fn log(&self, record: &log::Record<'_>) {
        if LOG_SINK.get().is_none() {
            return;
        }
        // 与 `prism-runtime-host::host_log` 的约定一致：0 error / 1 warn / 2 debug / 3 info。
        let level = match record.level() {
            log::Level::Error => 0,
            log::Level::Warn => 1,
            log::Level::Debug => 2,
            log::Level::Info | log::Level::Trace => 3,
        };
        let text = format!("{}: {}", record.target(), record.args());
        forward_log(level, &text);
    }

    fn flush(&self) {}
}

/// 把工程模块里的 **panic**（连同调用栈）送进宿主的日志。
///
/// 模块是独立动态库，`std` 也是它自己的一份：宿主 `set_hook` 装的钩子收不到模块里的
/// panic，默认行为只是往 stderr 打一行（编辑器 Console 一片空白，而且每帧 panic 会把
/// stderr 刷爆）。这里在模块自己的 `std` 上装钩子，把 `panicked at …` + `Backtrace`
/// 经 [`install_host_logger`] 的同一条通路发出去。
///
/// 调用栈里工程帧的符号要靠 `.pdb`：编辑器把模块复制到 `staged/` 时会连 `.pdb`
/// 一起复制（见 `prism-editor-egui::project_module::stage_module`）。
///
/// 与 [`install_host_logger`] 一样，重复调用是 no-op。必须在 `create` 里调用，
/// 且要在 [`install_host_logger`] 之后（否则日志还没有出口）。
#[cfg(feature = "plugin")]
pub fn install_host_panic_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // `Backtrace::force_capture` 不依赖 `RUST_BACKTRACE`，Play 时默认就要有栈。
            let text = format!(
                "{info}
{}",
                std::backtrace::Backtrace::force_capture()
            );
            forward_log(0, &text);
            // 保留原行为（stderr），终端排查仍然可用。
            previous(info);
        }));
    });
}

/// 按 ABI 的级别约定把一行文本发给宿主（0 error / 1 warn / 2 debug / 3 info）。
#[cfg(feature = "plugin")]
fn forward_log(level: u32, text: &str) {
    let Some(sink) = LOG_SINK.get() else {
        return;
    };
    // SAFETY: 同 `HostLogger::log`；宿主日志回调不使用 `user_data`，字符串只在本调用内有效。
    unsafe {
        (sink.log)(
            sink.user_data as *mut core::ffi::c_void,
            level,
            text.as_ptr(),
            text.len(),
        );
    }
}

/// 把工程模块自己的 `log!` 接到宿主的 `HostApi::log` 上。
///
/// 模块是独立动态库，链接的是**它自己那份** `log` crate —— 宿主在编辑器进程里
/// 装的 logger 收不到模块里的记录。工程只要在 `create` 里调一次这个函数，
/// `log::info!` / `log::error!` 就会经 ABI 送进编辑器的 Console（与宿主日志同一条
/// 通路，来源标 `Game`）。
///
/// 重复调用是 no-op（只取第一次的 `log` 回调）。`user_data` 每次调用都会更新，
/// 因为宿主每个 tick 都会换一个新的命令上下文；但宿主实现（`prism-runtime-host`）
/// 的日志回调并不依赖它。
#[cfg(feature = "plugin")]
pub fn install_host_logger(host: &HostApi) {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    let Some(log) = host.log else {
        return;
    };
    let sink = LogSink {
        user_data: host.user_data as usize,
        log,
    };
    // 第一次安装 logger；之后只刷新 sink（`OnceLock::set` 失败说明已经有了）。
    let fresh = LOG_SINK.set(sink).is_ok();
    if fresh {
        INSTALL.call_once(|| {
            if log::set_logger(&HOST_LOGGER).is_ok() {
                log::set_max_level(log::LevelFilter::Debug);
            }
        });
    }
}

#[cfg(feature = "plugin")]
impl PluginBackend<'_> {
    fn spawn(&self) -> EntityHandle {
        let Some(spawn) = self.api.spawn_entity else {
            return EntityHandle::PLACEHOLDER;
        };
        let mut out = EntityHandle::PLACEHOLDER;
        let status = unsafe { spawn(self.api.user_data, &mut out) };
        if status == SUCCESS {
            out
        } else {
            EntityHandle::PLACEHOLDER
        }
    }

    fn despawn(&self, entity: EntityHandle) {
        let Some(despawn) = self.api.despawn_entity else {
            log::debug!("game-api: host does not expose despawn_entity");
            return;
        };
        let status = unsafe { despawn(self.api.user_data, entity) };
        if status != SUCCESS {
            log::debug!("game-api: despawn rejected by host (status {status})");
        }
    }

    fn is_alive(&self, entity: EntityHandle) -> bool {
        match self.api.is_alive {
            Some(is_alive) => unsafe { is_alive(self.api.user_data, entity) == 1 },
            // 宿主没有提供存活查询时退回「句柄非空」；读写仍会各自校验。
            None => !entity.is_placeholder(),
        }
    }

    fn draw(&self, quad: UiQuad) {
        let Some(draw) = self.api.draw_quad else {
            log::debug!("game-api: host does not expose draw_quad");
            return;
        };
        unsafe { draw(self.api.user_data, quad) };
    }

    fn query(&self, type_name: &str) -> Vec<EntityHandle> {
        let Some(query) = self.api.query_entities else {
            return Vec::new();
        };
        let name = type_name.as_bytes();
        let mut count = 0usize;
        let status = unsafe {
            query(
                self.api.user_data,
                name.as_ptr(),
                name.len(),
                std::ptr::null_mut(),
                0,
                &mut count,
            )
        };
        if status != SUCCESS || count == 0 {
            return Vec::new();
        }

        let mut out = vec![EntityHandle::default(); count];
        let mut written = 0usize;
        let status = unsafe {
            query(
                self.api.user_data,
                name.as_ptr(),
                name.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        if status != SUCCESS {
            return Vec::new();
        }
        out.truncate(written);
        out
    }

    fn get(&self, entity: EntityHandle, type_name: &str) -> Option<Vec<u8>> {
        let get = self.api.get_component?;
        let name = type_name.as_bytes();
        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        let status = unsafe {
            get(
                self.api.user_data,
                entity,
                name.as_ptr(),
                name.len(),
                &mut ptr,
                &mut len,
            )
        };
        if status != SUCCESS || ptr.is_null() {
            return None;
        }
        // 指针只在下一次宿主调用前有效：立刻拷走。
        Some(unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec())
    }

    fn set(&self, entity: EntityHandle, bytes: &[u8]) -> bool {
        let Some(set) = self.api.set_component else {
            return false;
        };
        let status = unsafe { set(self.api.user_data, entity, bytes.as_ptr(), bytes.len()) };
        status == SUCCESS
    }

    fn remove(&self, entity: EntityHandle, type_name: &str) -> bool {
        let Some(remove) = self.api.remove_component else {
            return false;
        };
        let name = type_name.as_bytes();
        let status = unsafe { remove(self.api.user_data, entity, name.as_ptr(), name.len()) };
        status == SUCCESS
    }

    /// 走宿主的 `HOST_CAP_ASSET`：项目不自己拼 `assets/` 路径。
    fn asset_exists(&self, relative: &str) -> bool {
        let Some(callback) = self.api.asset_exists else {
            return false;
        };
        unsafe { callback(self.api.user_data, relative.as_ptr(), relative.len()) == 1 }
    }

    fn save_scene_state(&self) -> bool {
        let Some(callback) = self.api.save_scene_state else {
            log::warn!("host does not expose scene-state saving (HOST_CAP_STATE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == SUCCESS }
    }

    fn load_scene_state(&self) -> bool {
        let Some(callback) = self.api.load_scene_state else {
            log::warn!("host does not expose scene-state loading (HOST_CAP_STATE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == 1 }
    }

    fn load_scene(&self, path: &str, additive: bool) -> bool {
        if self.api.capabilities & HOST_CAP_SCENE == 0 {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        }
        let Some(callback) = self.api.load_scene else {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        };
        unsafe {
            callback(
                self.api.user_data,
                path.as_ptr(),
                path.len(),
                u8::from(additive),
            ) == SUCCESS
        }
    }

    fn unload_scene(&self) -> bool {
        if self.api.capabilities & HOST_CAP_SCENE == 0 {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        }
        let Some(callback) = self.api.unload_scene else {
            log::warn!("host does not expose scene switching (HOST_CAP_SCENE)");
            return false;
        };
        unsafe { callback(self.api.user_data) == SUCCESS }
    }

    fn scene_generation(&self) -> u64 {
        self.api.scene_generation
    }

    fn read_asset_text(&self, relative: &str) -> std::io::Result<String> {
        let Some(callback) = self.api.read_asset_file else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "host does not expose loose asset access",
            ));
        };
        let mut ptr: *const u8 = std::ptr::null();
        let mut len = 0usize;
        let status = unsafe {
            callback(
                self.api.user_data,
                relative.as_ptr(),
                relative.len(),
                &mut ptr,
                &mut len,
            )
        };
        if status != SUCCESS || (ptr.is_null() && len != 0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("asset `{relative}` not found"),
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        String::from_utf8(bytes.to_vec())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #589：静态 Player 的输入来自引擎每帧写进世界的 `InputSnapshot`，
    /// 不是 `GameWorld::host()` 构造时的默认值。
    #[cfg(feature = "host")]
    #[test]
    fn host_input_reads_the_world_resource() {
        let mut world = World::new();
        world.insert_resource(InputSnapshot {
            cursor: [12.0, 34.0],
            left_clicked: true,
            ..InputSnapshot::default()
        });

        let api = GameWorld::host(&mut world);
        assert_eq!(api.input().cursor, [12.0, 34.0]);
        assert!(api.input().left_clicked);
        assert_eq!(api.screen_size(), [0.0, 0.0]);
    }

    /// `set_input` 必须写穿到世界：同一个世界上的下一个 `GameWorld` 也能读到。
    #[cfg(feature = "host")]
    #[test]
    fn set_input_writes_through_to_the_world() {
        let mut world = World::new();
        {
            let mut api = GameWorld::host(&mut world);
            api.set_input(InputSnapshot {
                scroll: 2.5,
                ..InputSnapshot::default()
            });
            assert_eq!(api.input().scroll, 2.5);
        }
        assert_eq!(
            world
                .get_resource::<InputSnapshot>()
                .expect("set_input writes the resource")
                .scroll,
            2.5
        );
    }

    #[derive(Reflect, Clone, Debug, Default, PartialEq)]
    struct Health {
        value: f32,
    }

    #[cfg(feature = "plugin")]
    struct TestAssets;

    #[cfg(feature = "plugin")]
    impl prism_runtime_host::AssetIo for TestAssets {
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

    /// #484：宿主没注入存档实现时，`save/load_scene_state` 明确返回 `false`。
    #[cfg(feature = "plugin")]
    #[test]
    fn scene_state_is_unavailable_without_a_host_callback() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let host = commands.api();
        let mut api = GameWorld::plugin(&host, components.type_registry());

        assert!(!api.save_scene_state());
        assert!(!api.load_scene_state());
    }

    /// #482：换场请求走宿主回调，tick 结束后由宿主取走执行。
    #[cfg(feature = "plugin")]
    #[test]
    fn scene_requests_are_forwarded_to_the_host() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands =
            prism_runtime_host::WorldCommands::new(&mut world, &components, 7).with_scene_switch();
        let host = commands.api();
        assert_eq!(
            host.capabilities & prism_project_api::HOST_CAP_SCENE,
            prism_project_api::HOST_CAP_SCENE
        );
        let mut api = GameWorld::plugin(&host, components.type_registry());
        assert_eq!(api.scene_generation(), 7);

        assert!(api.load_scene("assets/scenes/second.scene.json", false));
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Load {
                path: "assets/scenes/second.scene.json".to_string(),
                additive: false,
            })
        );
        assert!(commands.take_scene_request().is_none());

        assert!(api.load_scene("assets/scenes/extra.scene.json", true));
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Load {
                path: "assets/scenes/extra.scene.json".to_string(),
                additive: true,
            })
        );

        assert!(api.unload_scene());
        assert_eq!(
            commands.take_scene_request(),
            Some(prism_runtime_host::SceneRequest::Unload)
        );
    }

    /// 静态 Player 没有换场：明确返回 false，而不是假装成功。
    #[cfg(feature = "host")]
    #[test]
    fn host_backend_reports_scene_switching_unavailable() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);
        assert!(!api.load_scene("assets/scenes/second.scene.json", false));
        assert!(!api.unload_scene());
        assert_eq!(api.scene_generation(), 0);
    }

    /// 插件后端通过 `HOST_CAP_ASSET` 读松散资产，签名与 host 后端一致。
    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_assets_through_the_host() {
        let mut world = World::new();
        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1)
            .with_assets(&TestAssets);
        let host = commands.api();
        let api = GameWorld::plugin(&host, components.type_registry());

        assert!(api.asset_exists("lake_layers.json"));
        assert!(!api.asset_exists("missing.json"));
        assert_eq!(
            api.read_asset_text("lake_layers.json").unwrap(),
            r#"{"layers":[]}"#
        );
        assert!(api.read_asset_text("missing.json").is_err());
    }

    #[test]
    fn components_round_trip_through_handles() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let entity = api.spawn();
        assert!(api.is_alive(entity));
        assert!(api.set(entity, Health { value: 3.0 }));
        assert_eq!(api.get::<Health>(entity), Some(Health { value: 3.0 }));
        assert_eq!(api.query::<Health>(), vec![entity]);

        assert!(api.remove::<Health>(entity));
        assert_eq!(api.get::<Health>(entity), None);
        assert!(api.query::<Health>().is_empty());
    }

    #[test]
    fn writes_to_dead_handles_are_rejected() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let entity = api.spawn();
        api.despawn(entity);
        assert!(!api.is_alive(entity));
        assert!(!api.set(entity, Health { value: 1.0 }));
        assert_eq!(api.get::<Health>(entity), None);
    }

    #[test]
    fn recycled_slot_does_not_revive_stale_handle() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        let stale = api.spawn();
        api.despawn(stale);
        let fresh = api.spawn();

        // 同一个槽位被复用，但代数不同：旧句柄必须仍然失效。
        assert_eq!(stale.id, fresh.id);
        assert_ne!(stale.generation, fresh.generation);
        assert!(!api.is_alive(stale));
        assert!(api.is_alive(fresh));
    }

    #[test]
    fn quads_are_drained_once() {
        let mut world = World::new();
        let mut api = GameWorld::host(&mut world);

        api.draw_quad(UiQuad::default());
        assert_eq!(api.take_quads().len(), 1);
        assert!(api.take_quads().is_empty());
    }

    #[cfg(feature = "plugin")]
    #[test]
    fn plugin_backend_reads_and_writes_through_host_callbacks() {
        use prism_scene::components::Name;

        let mut components = prism_scene::ComponentRegistry::new();
        prism_scene::register_builtin_components(&mut components);

        let mut world = World::new();
        let mut commands = prism_runtime_host::WorldCommands::new(&mut world, &components, 1);
        let api = commands.api();
        let registry = components.type_registry();

        {
            let mut view = GameWorld::plugin(&api, registry);
            let entity = view.spawn();
            assert!(view.set(entity, Name("from_plugin".to_owned())));
            assert_eq!(view.query::<Name>(), vec![entity]);
            assert_eq!(
                view.get::<Name>(entity).map(|name| name.0),
                Some("from_plugin".to_owned())
            );
            assert!(view.remove::<Name>(entity));
            assert!(view.query::<Name>().is_empty());
            assert!(view.is_alive(entity));
            view.despawn(entity);
            assert!(!view.is_alive(entity));
        }

        drop(commands);
        assert!(world.query::<Name>().next().is_none());
    }
}
