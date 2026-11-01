//! Game View 的用户项目模块：编译、重载、每帧 tick。
//!
//! 编辑器本身只链接引擎；用户项目的 gameplay 代码编译成**独立的 cdylib**
//! （`projects/<p>/Cargo.toml` 的 `preview-module` feature），由
//! `prism-runtime-host` 通过稳定 C ABI 加载。项目模块不链接引擎，
//! 所以不会出现第二份 `World` / `renderer`。
//!
//! 编译在后台线程，加载/卸载必须在 UI 线程（`ProjectSession` 的回调指针
//! 不能跨线程）。节奏按**模块编译**来做（没有脚本层，所以也没有 domain reload）：
//!
//! - 源码改动**落定** 350ms 后才编译，连续保存不会触发多次 cargo；
//! - Play 期间不自动换模块，避免打断试玩；`Ctrl+R` 显式刷新例外；
//! - 换模块是**整模块重载**：旧 session 释放、`create` 重跑，项目静态状态归零；
//! - 编译失败保留上一次成功的模块继续跑，诊断进 Console 与 Problems 面板；
//! - 构建产物先复制成唯一名字再加载（Windows 上已加载的 dll 不能覆盖）。

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use prism_editor_core::LogLevel;
use prism_engine::input::InputManager;

use crate::diagnostics::{diagnostics_from_cargo, Diagnostic};
use prism_runtime_host::{
    AssetIo, InputSnapshot, ProjectModule, ProjectSession, UiQuad as HostUiQuad, WorldCommands,
};
use prism_scene::ComponentRegistry;

/// 源码改动后等待多久再编译（引擎式 debounce）。
const COMPILE_DEBOUNCE: Duration = Duration::from_millis(350);
/// 可以直接指向预编译好的模块（测试 / 非 Cargo 项目）。
const MODULE_ENV: &str = "PRISMREV_PROJECT_MODULE";

/// 一条要写进 Console 的项目模块事件。
#[derive(Clone, Debug)]
pub struct ProjectEvent {
    pub level: LogLevel,
    pub text: String,
}

/// 模块编译状态。状态栏、Problems 面板与 MCP `editor_status` 共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleState {
    /// 项目没有 `preview-module`（Play 只跑引擎 system）。
    Unavailable,
    /// 已加载，源码没有未编译的改动。
    Ready,
    /// 源码有改动，还没编（debounce 中，或 Auto Refresh 关掉了）。
    Pending,
    /// cargo 正在跑。
    Compiling,
    /// 上一次编译 / 加载失败（源码没再改就不会自动重试）。
    Failed,
}

impl ModuleState {
    pub fn as_str(self) -> &'static str {
        match self {
            ModuleState::Unavailable => "unavailable",
            ModuleState::Ready => "ready",
            ModuleState::Pending => "pending",
            ModuleState::Compiling => "compiling",
            ModuleState::Failed => "failed",
        }
    }

    /// 状态栏文案（UE 的 Compile 按钮那种一眼能看懂的状态）。
    pub fn label(self) -> &'static str {
        match self {
            ModuleState::Unavailable => "Module: off",
            ModuleState::Ready => "Module: ready",
            ModuleState::Pending => "Module: changes not compiled",
            ModuleState::Compiling => "Module: compiling…",
            ModuleState::Failed => "Module: compile failed",
        }
    }
}

/// 可轮询的状态（状态栏 / Problems / MCP `editor_status` 用）。
#[derive(Clone, Debug)]
pub struct ProjectModuleStatus {
    pub available: bool,
    pub loaded: bool,
    pub building: bool,
    pub auto_refresh: bool,
    pub state: ModuleState,
    /// 源码时间戳晚于上一次编译尝试：有未编译的改动。
    pub dirty: bool,
    pub manifest: Option<PathBuf>,
    pub error: Option<String>,
    /// 最近一次 build 的诊断条数（明细走 `ProjectModuleDriver::diagnostics`）。
    pub problems: usize,
}

enum BuildOutcome {
    Built {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    Failed {
        message: String,
        diagnostics: Vec<Diagnostic>,
    },
}

/// 只在没有编译任务时扫源码时间戳；和 350ms 的 debounce 错开，别每帧 stat。
const DIRTY_SCAN_INTERVAL: Duration = Duration::from_millis(500);

/// 项目模块的宿主侧驱动器。
pub struct ProjectModuleDriver {
    manifest: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    components: ComponentRegistry,
    assets: ProjectAssets,
    session: Option<ProjectSession>,
    /// 当前加载的副本路径（构建产物会被下一次 cargo 覆盖）。
    staged: Option<PathBuf>,
    /// 源码最近一次编译尝试时的时间戳（成功/失败都记，失败不重试到改动为止）。
    compiled_stamp: Option<SystemTime>,
    /// `compiled_stamp` 之后源码又变过：状态栏显示「有改动未编译」。
    dirty: bool,
    /// `dirty` 的扫描节流（`DIRTY_SCAN_INTERVAL`）。
    last_scan: Option<Instant>,
    /// 最近一次 build 的编译器诊断（成功时只有 warning）。
    diagnostics: Vec<Diagnostic>,
    /// 首次观察到改动的时刻，用于 debounce。
    pending_since: Option<Instant>,
    build: Option<Receiver<BuildOutcome>>,
    building: bool,
    /// `Ctrl+R`：下一次 `maybe_compile` 强制重编，无视 Auto Refresh / Play。
    refresh_requested: bool,
    auto_refresh: bool,
    /// 项目是否提供 `preview-module`（或给了预构建模块）。
    available: bool,
    /// 上一次 tick 把模块判成故障（panic / 返回错误）：暂停调用，等重新加载。
    faulted: bool,
    /// `PRISMREV_PROJECT_MODULE` 指定的预构建模块（只尝试加载一次）。
    prebuilt: Option<PathBuf>,
    events: Vec<ProjectEvent>,
    last_error: Option<String>,
    last_tick_error: Option<String>,
}

impl ProjectModuleDriver {
    pub fn new(root: &Path) -> Self {
        let manifest = crate::build::game_manifest_path(root);
        let manifest = manifest.is_file().then_some(manifest);
        let available_feature = manifest
            .as_deref()
            .is_some_and(manifest_declares_preview_module);
        let target_dir = manifest.as_deref().and_then(|manifest| {
            manifest
                .parent()
                .map(|dir| dir.join("target").join("preview-module"))
        });

        let prebuilt = std::env::var_os(MODULE_ENV)
            .map(PathBuf::from)
            .filter(|path| path.is_file());

        let available = prebuilt.is_some() || available_feature;
        let mut driver = Self {
            manifest,
            target_dir,
            components: builtin_component_registry(),
            assets: ProjectAssets {
                root: root.to_path_buf(),
            },
            session: None,
            staged: None,
            compiled_stamp: None,
            dirty: available,
            last_scan: None,
            diagnostics: Vec::new(),
            pending_since: None,
            build: None,
            building: false,
            refresh_requested: false,
            auto_refresh: true,
            available,
            faulted: false,
            prebuilt,
            events: Vec::new(),
            last_error: None,
            last_tick_error: None,
        };

        if driver.prebuilt.is_some() {
            driver.push(
                LogLevel::Info,
                format!("project module: using {MODULE_ENV}"),
            );
        } else if driver.manifest.is_none() {
            driver.push(
                LogLevel::Info,
                "project module: no Cargo.toml in project root; Play runs the engine only"
                    .to_string(),
            );
        } else if !available_feature {
            driver.push(
                LogLevel::Warn,
                "project module: Cargo.toml has no `preview-module` feature; \
                 Play runs the engine only"
                    .to_string(),
            );
        }

        driver
    }

    pub fn status(&self) -> ProjectModuleStatus {
        ProjectModuleStatus {
            available: self.available(),
            loaded: self.session.is_some(),
            building: self.building,
            auto_refresh: self.auto_refresh,
            state: self.state(),
            dirty: self.dirty,
            manifest: self.manifest.clone(),
            error: self.last_error.clone(),
            problems: self.diagnostics.len(),
        }
    }

    /// Problems 面板读这里（不随 `status()` 每帧克隆）。
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    fn state(&self) -> ModuleState {
        if !self.available() {
            return ModuleState::Unavailable;
        }
        if self.building {
            return ModuleState::Compiling;
        }
        // 改过源码但还没编（Auto Refresh 关掉时靠这个提示用户按 Ctrl+R）；
        // 失败后源码没再改才是稳定的 Failed。
        if self.dirty {
            return ModuleState::Pending;
        }
        if self.last_error.is_some() {
            return ModuleState::Failed;
        }
        ModuleState::Ready
    }

    pub fn available(&self) -> bool {
        self.available
    }

    pub fn auto_refresh(&self) -> bool {
        self.auto_refresh
    }

    pub fn set_auto_refresh(&mut self, enabled: bool) {
        if self.auto_refresh == enabled {
            return;
        }
        self.auto_refresh = enabled;
        self.pending_since = None;
        self.push(
            LogLevel::Info,
            format!(
                "project module: auto refresh {}",
                if enabled { "on" } else { "off" }
            ),
        );
    }

    /// Ctrl+R：立即重编。请求会保留到下一次 `maybe_compile`（正在构建时也不丢）。
    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
        self.pending_since = None;
    }

    pub fn drain_events(&mut self) -> Vec<ProjectEvent> {
        std::mem::take(&mut self.events)
    }

    /// 每帧调用（UI 线程）：收编译结果、按需触发编译。
    ///
    /// `playing` = Game view 正在模拟；自动编译会等 Play 结束后再做。
    pub fn poll(&mut self, playing: bool) {
        self.poll_build();
        self.scan_sources();
        self.maybe_compile(playing);
    }

    /// 源码有没有未编译的改动（节流扫描，Auto Refresh 关掉时也要能显示）。
    fn scan_sources(&mut self) {
        if self.manifest.is_none() || self.prebuilt.is_some() {
            self.dirty = false;
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_scan {
            if now.duration_since(last) < DIRTY_SCAN_INTERVAL {
                return;
            }
        }
        self.last_scan = Some(now);
        let Some(stamp) = self.source_stamp() else {
            return;
        };
        self.dirty = self.compiled_stamp.is_none_or(|compiled| compiled < stamp);
    }

    /// Game View 的 Play 每帧调用：在引擎 system 之前推进项目代码。
    ///
    /// 项目提交的 UI 四边形会写进 `ExternalUiQuads`，同帧的 `ui::render`
    /// 会把它们并进 `UiDrawList`。
    pub fn tick(
        &mut self,
        world: &mut prism_engine::ecs::World,
        input: &InputManager,
        dt: f32,
        scene_generation: u64,
        screen: [f32; 2],
    ) {
        // 上一帧已经把模块判为故障：等它被重新加载，不再往同一个坑里跳。
        if self.faulted {
            return;
        }
        // 字段级借用：session 与 components/assets 互不重叠。
        let Some(session) = self.session.as_mut() else {
            return;
        };

        let mut commands =
            WorldCommands::new(world, &self.components, scene_generation).with_assets(&self.assets);
        // 项目代码可以调 `GameWorld::save_scene_state()`（#484）；引擎在这里，
        // runtime-host 只持有函数指针。
        commands = commands.with_scene_state(
            prism_engine::scene_state::save_scene_state,
            prism_engine::scene_state::load_scene_state,
        );
        commands.set_input(input_snapshot(input, screen));
        let host = commands.api();
        let result = session.tick(&host, dt.max(0.000_5));
        let quads = commands.take_quads();
        queue_project_quads(world, quads);

        let error = result.err().map(|error| error.to_string());
        // 崩过一次就停在这里：工程每帧 panic 会把 stderr 和 GPU 时间都烧掉，
        // 重新加载模块（保存源码 / Ctrl+R）才会再试。
        self.faulted = error.is_some();
        if error != self.last_tick_error {
            match &error {
                Some(error) => self.push(LogLevel::Error, format!("project tick failed: {error}")),
                None => self.push(LogLevel::Info, "project tick recovered".to_string()),
            }
            self.last_tick_error = error;
        }
    }

    // ------------------------------------------------------------------
    // 编译 + 加载
    // ------------------------------------------------------------------

    fn maybe_compile(&mut self, playing: bool) {
        if self.building || !self.available() {
            return;
        }

        // 预构建模块优先：只尝试装一次，失败就停在错误状态（Ctrl+R 也不重装）。
        if let Some(path) = self.prebuilt.take() {
            match self.install(&path, Vec::new()) {
                Ok(()) => self.push(
                    LogLevel::Info,
                    format!(
                        "project module loaded from {MODULE_ENV}: {}",
                        path.display()
                    ),
                ),
                Err(error) => self.fail(error, Vec::new()),
            }
            return;
        }

        let manual = std::mem::take(&mut self.refresh_requested);
        if !manual && !self.auto_refresh {
            return;
        }
        // Play 期间不自动换模块：整模块重载会丢掉项目状态（没有脚本层可迁移）。
        // 放在扫描源码之前，避免每帧遍历 `src/`。
        if !manual && playing {
            return;
        }

        let Some(stamp) = self.source_stamp() else {
            return;
        };
        if !manual
            && self
                .compiled_stamp
                .is_some_and(|compiled| compiled >= stamp)
        {
            self.pending_since = None;
            return;
        }

        let Some((manifest, target_dir)) = self.build_paths() else {
            return;
        };
        let now = Instant::now();
        let pending_since = *self.pending_since.get_or_insert(now);
        if now.duration_since(pending_since) < COMPILE_DEBOUNCE {
            return;
        }
        self.pending_since = None;

        // 无论成败都记下时间戳：语法错误不该让编辑器每隔 350ms 重跑一次 cargo。
        self.compiled_stamp = Some(stamp);
        self.building = true;
        self.last_error = None;
        self.push(
            LogLevel::Info,
            format!(
                "project module: compiling `{}` (preview-module)",
                manifest.display()
            ),
        );

        let (tx, rx) = channel();
        self.build = Some(rx);
        std::thread::spawn(move || {
            let outcome = build_module(&manifest, &target_dir);
            let _ = tx.send(outcome);
        });
    }

    fn poll_build(&mut self) {
        let Some(rx) = self.build.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.build = None;
                self.building = false;
                match outcome {
                    BuildOutcome::Built { path, diagnostics } => {
                        let warnings = diagnostics.len();
                        match self.install(&path, diagnostics) {
                            Ok(()) => {
                                self.push(
                                    LogLevel::Info,
                                    if warnings == 0 {
                                        format!("project module loaded: {}", path.display())
                                    } else {
                                        format!(
                                            "project module loaded: {} ({warnings} warnings, see Problems)",
                                            path.display()
                                        )
                                    },
                                );
                            }
                            Err(error) => self.fail(error, Vec::new()),
                        }
                    }
                    BuildOutcome::Failed {
                        message,
                        diagnostics,
                    } => self.fail(message, diagnostics),
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.build = None;
                self.building = false;
                self.fail(
                    "project module build thread exited without a result".to_string(),
                    Vec::new(),
                );
            }
        }
    }

    fn install(&mut self, built: &Path, diagnostics: Vec<Diagnostic>) -> Result<(), String> {
        let staged = stage_module(built)?;
        let session = match ProjectModule::load(&staged)
            .map_err(|error| error.to_string())
            .and_then(|module| module.start().map_err(|error| error.to_string()))
        {
            Ok(session) => session,
            Err(error) => {
                remove_staged_file(&staged);
                return Err(error);
            }
        };

        // 先换 session 再删旧副本：旧代码指针必须在新模块生效前彻底失效。
        let previous = self.session.replace(session);
        drop(previous);
        if let Some(old) = self.staged.replace(staged) {
            remove_staged_file(&old);
        }
        self.last_error = None;
        self.last_tick_error = None;
        self.diagnostics = diagnostics;
        // 让下一帧立刻重扫：编译期间改过的源码不能因为“刚好编完”被当成已编译。
        self.last_scan = None;
        self.faulted = false;
        Ok(())
    }

    fn fail(&mut self, error: String, diagnostics: Vec<Diagnostic>) {
        let count = diagnostics.len();
        self.diagnostics = diagnostics;
        self.last_scan = None;
        self.last_error = Some(error.clone());
        self.push(LogLevel::Error, error);
        if count > 0 {
            self.push(
                LogLevel::Warn,
                format!("project module: {count} problem(s) — see the Problems panel"),
            );
        }
    }

    fn build_paths(&self) -> Option<(PathBuf, PathBuf)> {
        Some((self.manifest.clone()?, self.target_dir.clone()?))
    }

    fn source_stamp(&self) -> Option<SystemTime> {
        let manifest = self.manifest.as_deref()?;
        let project = manifest.parent()?;
        let mut newest = file_modified(manifest);
        collect_newest_source(&project.join("src"), &mut newest);
        newest
    }

    fn push(&mut self, level: LogLevel, text: String) {
        self.events.push(ProjectEvent { level, text });
    }
}

// ---------------------------------------------------------------------------
// 输入 / UI 四边形
// ---------------------------------------------------------------------------

/// 把宿主输入打包成 ABI 快照。
///
/// 映射本体在 [`prism_engine::input::input_snapshot`]：静态 Player 的
/// `GameWorld::input()` 读的是同一份实现（#589），两边不会漂移。
fn input_snapshot(input: &InputManager, screen: [f32; 2]) -> InputSnapshot {
    prism_engine::input::input_snapshot(input, screen)
}

/// 项目模块提交的四边形 → 引擎的 `ExternalUiQuads`。
///
/// 必须在 `engine.update()` 之前写入：同帧的 `ui::render` 会合并并清空它。
fn queue_project_quads(world: &mut prism_engine::ecs::World, quads: Vec<HostUiQuad>) {
    if quads.is_empty() {
        return;
    }
    let mut external = world
        .remove_resource::<prism_engine::ui::ExternalUiQuads>()
        .unwrap_or_default();
    external
        .0
        .extend(quads.into_iter().map(|quad| prism_engine::ui::UiQuad {
            rect: quad.rect,
            color: quad.color,
            border_radius: quad.border_radius,
            layer: quad.layer,
            order: prism_engine::ui::EXTERNAL_UI_ORDER,
        }));
    world.insert_resource(external);
}

fn builtin_component_registry() -> ComponentRegistry {
    let mut registry = ComponentRegistry::new();
    prism_scene::register_builtin_components(&mut registry);
    registry
}

// ---------------------------------------------------------------------------
// 松散资产
// ---------------------------------------------------------------------------

/// 项目自己的 `assets/`（不是仓库根的引擎资产）。
struct ProjectAssets {
    root: PathBuf,
}

impl ProjectAssets {
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return None;
        }
        Some(self.root.join("assets").join(relative))
    }
}

impl AssetIo for ProjectAssets {
    fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_some_and(|path| path.is_file())
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let path = self.resolve(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("asset path escapes the project root: {path}"),
            )
        })?;
        std::fs::read(&path)
    }
}

// ---------------------------------------------------------------------------
// 编译
// ---------------------------------------------------------------------------

fn build_module(manifest: &Path, target_dir: &Path) -> BuildOutcome {
    let mut command = std::process::Command::new("cargo");
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--no-default-features", "--features", "preview-module"])
        .arg("--target-dir")
        .arg(target_dir)
        // 结构化诊断：Problems 面板要 file:line:col，不是一段彩色 stderr。
        .args(["--message-format", "json"]);
    if let Some(dir) = manifest.parent() {
        command.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return BuildOutcome::Failed {
                message: format!("project module: cannot spawn cargo: {error}"),
                diagnostics: Vec::new(),
            }
        }
    };
    let root = manifest.parent().unwrap_or_else(|| Path::new("."));
    let diagnostics = diagnostics_from_cargo(&String::from_utf8_lossy(&output.stdout), root);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 第一条 error 就是用户要修的东西；没有编译诊断（链接错误等）才退回 stderr。
        let summary = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.level == LogLevel::Error)
            .map(|diagnostic| match diagnostic.location() {
                Some(location) => format!("{location}: {}", diagnostic.message),
                None => diagnostic.message.clone(),
            })
            .unwrap_or_else(|| tail_lines(stderr.trim(), 12));
        return BuildOutcome::Failed {
            message: format!("project module compile failed: {summary}"),
            diagnostics,
        };
    }

    let package = package_name(manifest).unwrap_or_else(|| "game".to_string());
    let library = target_dir.join("debug").join(library_file_name(&package));
    if library.is_file() {
        BuildOutcome::Built {
            path: library,
            diagnostics,
        }
    } else {
        BuildOutcome::Failed {
            message: format!(
                "project module compile succeeded but `{}` is missing",
                library.display()
            ),
            diagnostics,
        }
    }
}

/// 构建产物先复制成唯一名字：下一次 cargo 会覆盖原文件，而 Windows 上
/// 已加载的 dll 不能被覆盖。旧副本在换 session 之后删除。
fn stage_module(built: &Path) -> Result<PathBuf, String> {
    let parent = built
        .parent()
        .ok_or_else(|| "project module path has no parent directory".to_string())?;
    let staging = parent.join("staged");
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("create module staging dir failed: {error}"))?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let stem = built
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("project");
    // 唯一性放在**目录**上，文件名必须保持原样：dbghelp 是按 PE 里记录的符号
    // 文件名（`fishing.pdb`）去模块目录找的，不会从 `fishing-<nanos>.dll` 反推。
    // 改名的 dll 会让工程帧的符号退化成导出名（`prism_project_get_api`），
    // panic 调用栈就白抓了。
    let unique = staging.join(format!("{stem}-{nanos}"));
    std::fs::create_dir_all(&unique)
        .map_err(|error| format!("create module staging dir failed: {error}"))?;
    let Some(file_name) = built.file_name() else {
        return Err(format!(
            "project module path has no file name: {}",
            built.display()
        ));
    };
    let staged = unique.join(file_name);
    std::fs::copy(built, &staged)
        .map_err(|error| format!("copy project module failed: {error}"))?;

    // 符号文件与 dll 同目录：缺了它工程帧只有地址。
    let pdb = built.with_extension("pdb");
    if pdb.is_file() {
        if let Some(pdb_name) = pdb.file_name() {
            if let Err(error) = std::fs::copy(&pdb, unique.join(pdb_name)) {
                log::warn!("copy project module symbols failed (stack will lack frames): {error}");
            }
        }
    } else {
        log::debug!(
            "project module has no .pdb ({}), stack frames will be addresses",
            pdb.display()
        );
    }
    Ok(staged)
}

/// 删掉一次加载的暂存目录（dll + pdb）。模块 dll 必须已经 unload。
fn remove_staged_file(staged: &Path) {
    let Some(dir) = staged.parent() else {
        return;
    };
    if let Err(error) = std::fs::remove_dir_all(dir) {
        log::debug!(
            "remove project module staging dir failed {}: {error}",
            dir.display()
        );
    }
}

fn library_file_name(package: &str) -> String {
    let crate_name = package.replace('-', "_");
    if cfg!(target_os = "windows") {
        format!("{crate_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{crate_name}.dylib")
    } else {
        format!("lib{crate_name}.so")
    }
}

fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// 清单 / 源码时间戳
// ---------------------------------------------------------------------------

fn manifest_declares_preview_module(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest)
        .map(|text| text.contains("preview-module"))
        .unwrap_or(false)
}

/// 从 `Cargo.toml` 的 `[package]` 段读包名（构建产物名由它推导）。
fn package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix('=')?.trim();
        let name = rest.trim_matches('"').trim_matches('\'');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

fn file_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn collect_newest_source(dir: &Path, newest: &mut Option<SystemTime>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_newest_source(&path, newest);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            if let Some(modified) = file_modified(&path) {
                if newest.is_none_or(|current| modified > current) {
                    *newest = Some(modified);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_engine::input::MouseButton;
    use prism_engine::input::{ElementState, KeyCode};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prism-editor-project-module-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn package_name_comes_from_the_package_section() {
        let dir = temp_dir("package");
        let manifest = dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[workspace]\nname = \"not-this\"\n\n[package]\nname = \"fishing-game\"\n",
        )
        .unwrap();
        assert_eq!(package_name(&manifest), Some("fishing-game".to_string()));
    }

    #[test]
    fn preview_feature_is_detected_from_the_manifest() {
        let dir = temp_dir("feature");
        let manifest = dir.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"game\"\n").unwrap();
        assert!(!manifest_declares_preview_module(&manifest));
        std::fs::write(
            &manifest,
            "[package]\nname = \"game\"\n\n[features]\npreview-module = []\n",
        )
        .unwrap();
        assert!(manifest_declares_preview_module(&manifest));
    }

    #[test]
    fn library_name_uses_the_package_name() {
        let name = library_file_name("my-game");
        assert!(name.contains("my_game"));
        assert!(name.ends_with(".so") || name.ends_with(".dll") || name.ends_with(".dylib"));
    }

    #[test]
    fn held_keys_are_reported_even_after_begin_frame() {
        let mut input = InputManager::new();
        input.handle_keyboard(KeyCode::Space, ElementState::Pressed);
        // `begin_frame` 清掉 just_pressed，但按住状态必须保留：
        // fishing 用 `input.key(keys::SPACE)` 检测持续收线。
        input.begin_frame();
        let snapshot = input_snapshot(&input, [1280.0, 720.0]);
        assert!(snapshot.key(KeyCode::Space.bit_index()));
        assert_eq!(snapshot.screen, [1280.0, 720.0]);
    }

    #[test]
    fn click_is_reported_as_just_pressed() {
        let mut input = InputManager::new();
        input.handle_mouse_button(MouseButton::Left, ElementState::Pressed);
        let snapshot = input_snapshot(&input, [100.0, 50.0]);
        assert!(snapshot.left_clicked);
        assert!(snapshot.left_held);
    }

    #[test]
    fn right_and_middle_buttons_reach_the_snapshot() {
        let mut input = InputManager::new();
        input.handle_mouse_button(MouseButton::Right, ElementState::Pressed);
        let snapshot = input_snapshot(&input, [800.0, 600.0]);
        assert!(snapshot.right_clicked, "右键边沿必须能看见");
        assert!(snapshot.right_held);
        assert!(!snapshot.middle_held);

        let mut input = InputManager::new();
        input.handle_mouse_button(MouseButton::Middle, ElementState::Pressed);
        let snapshot = input_snapshot(&input, [800.0, 600.0]);
        assert!(snapshot.middle_clicked);
        assert!(snapshot.middle_held);
        assert!(!snapshot.right_held);
    }

    #[test]
    fn key_edges_are_reported_separately_from_held_keys() {
        let mut input = InputManager::new();
        input.handle_keyboard(KeyCode::Space, ElementState::Pressed);
        let snapshot = input_snapshot(&input, [1.0, 1.0]);
        assert!(snapshot.key(KeyCode::Space.bit_index()), "按住位图");
        assert!(snapshot.key_pressed(KeyCode::Space.bit_index()), "按下边沿");
        assert!(!snapshot.key_released(KeyCode::Space.bit_index()));

        // 松开的那一帧：按住位图清掉，释放边沿置位。
        // 宿主每帧开头会 `begin_frame`，这里照做，否则上一帧的按下边沿还留着。
        input.begin_frame();
        input.handle_keyboard(KeyCode::Space, ElementState::Released);
        let snapshot = input_snapshot(&input, [1.0, 1.0]);
        assert!(!snapshot.key(KeyCode::Space.bit_index()));
        assert!(!snapshot.key_pressed(KeyCode::Space.bit_index()));
        assert!(snapshot.key_released(KeyCode::Space.bit_index()));
    }

    #[test]
    fn scroll_and_pointer_lock_are_forwarded() {
        let mut input = InputManager::new();
        input.handle_scroll(2.0);
        input.pointer_locked = true;
        let snapshot = input_snapshot(&input, [1.0, 1.0]);
        assert!((snapshot.scroll - 2.0).abs() < 1e-6);
        assert!(snapshot.pointer_locked);
    }

    #[test]
    fn touches_keep_their_id_and_phase() {
        use prism_engine::input::TouchPhase;
        let mut input = InputManager::new();
        input.handle_touch(7, TouchPhase::Started, [10.0, 20.0]);
        input.handle_touch(9, TouchPhase::Moved, [30.0, 40.0]);
        let snapshot = input_snapshot(&input, [100.0, 100.0]);

        assert_eq!(snapshot.touch_count, 2);
        let touches = snapshot.touch_slice();
        assert_eq!(touches.len(), 2);
        assert_eq!(touches[0].id, 7);
        assert_eq!(touches[0].position, [10.0, 20.0]);
        assert_eq!(touches[0].phase, prism_runtime_host::TOUCH_PHASE_STARTED);
        assert_eq!(touches[1].id, 9);
        assert_eq!(touches[1].phase, prism_runtime_host::TOUCH_PHASE_MOVED);
        assert!(snapshot.any_touch());

        // 全是「已结束」时不再算有触点按下。
        input.begin_frame();
        input.handle_touch(7, TouchPhase::Ended, [10.0, 20.0]);
        let snapshot = input_snapshot(&input, [100.0, 100.0]);
        assert!(!snapshot.any_touch());
    }

    #[test]
    fn project_assets_are_read_below_the_project_assets_dir() {
        let dir = temp_dir("assets");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/lake_layers.json"), br#"{"layers":[]}"#).unwrap();
        let assets = ProjectAssets { root: dir.clone() };
        assert!(assets.exists("lake_layers.json"));
        assert_eq!(
            assets.read("lake_layers.json").unwrap(),
            br#"{"layers":[]}"#
        );
        assert!(!assets.exists("../Cargo.toml"));
        assert!(assets.read("../Cargo.toml").is_err());
    }

    #[test]
    fn source_stamp_tracks_rust_sources() {
        let dir = temp_dir("stamp");
        std::fs::create_dir_all(dir.join("src/game")).unwrap();
        let manifest = dir.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"game\"\n").unwrap();
        std::fs::write(dir.join("src/game/mod.rs"), "fn main() {}\n").unwrap();

        let driver = ProjectModuleDriver::new(&dir);
        assert!(driver.source_stamp().is_some());
    }

    /// 端到端：用真实 `.scene.json` 驱动 `projects/prisma` 的 preview 模块。
    ///
    /// 需要先构建模块并设置 `PRISMREV_TEST_PROJECT_MODULE`；没有设置时跳过，
    /// 和 `prism-runtime-host` 的同名环境测试一致。
    #[test]
    fn preview_module_drives_the_intro_scene_when_configured() {
        let Some(module_path) = std::env::var_os("PRISMREV_TEST_PROJECT_MODULE") else {
            return;
        };
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let scene = repo.join("assets/scenes/intro.scene.json");
        let mut engine = prism_engine::Engine::empty();
        engine
            .load_scene_json_file(&scene)
            .expect("intro scene should load");

        let module = ProjectModule::load(&module_path).expect("preview module should load");
        let mut session = module.start().expect("preview session should start");
        let components = builtin_component_registry();
        let assets = ProjectAssets { root: repo };
        let mut last_quads = 0;
        for _ in 0..90 {
            let mut commands =
                WorldCommands::new(engine.world_mut(), &components, 1).with_assets(&assets);
            let host = commands.api();
            session
                .tick(&host, 1.0 / 60.0)
                .expect("project tick should succeed");
            last_quads = commands.take_quads().len();
        }
        // intro 的雨滴通过 `GameWorld::draw_quad` 提交；能跨 ABI 回来说明
        // 组件读写和 UI 四边形通道都通了。
        assert!(last_quads > 0, "intro should submit UI quads");
    }

    #[test]
    fn poll_loads_a_prebuilt_module_when_configured() {
        let Some(module_path) = std::env::var_os("PRISMREV_TEST_PROJECT_MODULE") else {
            return;
        };
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut driver = ProjectModuleDriver::new(&repo.join("projects/prisma"));
        driver.prebuilt = Some(PathBuf::from(module_path));
        assert!(!driver.status().loaded);
        driver.poll(false);
        assert!(
            driver.status().loaded,
            "poll should install the prebuilt module"
        );
    }

    #[test]
    fn driver_stages_and_reloads_a_prebuilt_module_when_configured() {
        let Some(module_path) = std::env::var_os("PRISMREV_TEST_PROJECT_MODULE") else {
            return;
        };
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut driver = ProjectModuleDriver::new(&repo.join("projects/prisma"));
        assert!(driver.available());
        driver
            .install(Path::new(&module_path), Vec::new())
            .expect("prebuilt module should install");
        assert!(driver.status().loaded);
        let first = driver.staged.clone().expect("first staged copy");

        driver
            .install(Path::new(&module_path), Vec::new())
            .expect("prebuilt module should reload");
        let second = driver.staged.clone().expect("second staged copy");
        assert_ne!(first, second, "each load gets a unique file name");
        assert!(!first.exists(), "old copy is removed after the swap");
        assert!(second.exists());
    }

    #[test]
    fn driver_without_preview_feature_is_unavailable() {
        let dir = temp_dir("unavailable");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"game\"\n").unwrap();
        let driver = ProjectModuleDriver::new(&dir);
        assert!(!driver.available());
        assert!(!driver.status().loaded);
        assert_eq!(driver.status().state, ModuleState::Unavailable);
    }

    /// 状态机：有改动未编 = Pending，出错 = Failed，成功 = Ready。
    #[test]
    fn module_state_reports_pending_and_failed() {
        let dir = temp_dir("state");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"game\"\n\n[features]\npreview-module = []\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn create() {}\n").unwrap();
        let mut driver = ProjectModuleDriver::new(&dir);
        driver.scan_sources();
        assert_eq!(driver.status().state, ModuleState::Pending);
        assert!(driver.status().dirty);

        // 失败也会记下编译时间戳：源码没再改就不该每 350 ms 重跑 cargo。
        driver.compiled_stamp = driver.source_stamp();
        driver.last_scan = None;
        driver.scan_sources();
        driver.fail("boom".to_string(), Vec::new());
        assert_eq!(driver.status().state, ModuleState::Failed);
        assert!(!driver.status().dirty);

        // 改回源码（或用户手动重编）后，下一个编译周期重新开始。
        driver.last_error = None;
        assert_eq!(driver.status().state, ModuleState::Ready);
    }
}
