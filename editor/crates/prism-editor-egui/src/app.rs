//! 编辑器外壳：菜单 / 工具栏 / 状态栏 + split-tree docking + Scene/Game 视口。
//!
//! 布局与功能对齐 Web 编辑器（`editor/`）与引擎的经典布局；数据模型、
//! 命令与撤销都来自 `prism-editor-core`（与 GPUI 编辑器共用同一份）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use egui::{Align, Color32, CursorIcon, Layout, Rect, RichText, Sense, Stroke, Vec2};
use egui_ash::{App, CreationContext, ExitSignal, HandleRedraw};

use prism_editor_core::dock::split_side_length;
use prism_editor_core::{Command, DockDirection, DockNode, Locale, Preferences, Workspace};
use serde_json::json;

use crate::engine_host::ViewKind;
use crate::gpu::SharedVulkan;
use crate::logging;
use crate::panels::{self, UiState};
use crate::project_module::ModuleState;
use crate::theme;
use crate::viewport::{GameResolution, ImageBridge, ViewportState};
use egui_phosphor::regular as icons;

/// 面板 tab 的高度（与 Web `.dock-tabs` 的 28px 对齐，这里按 egui 行高收到 26）。
const TAB_BAR_HEIGHT: f32 = 26.0;

/// tab 条右侧给溢出下拉留的宽度（#455）。
const TAB_OVERFLOW_WIDTH: f32 = 22.0;

/// tab 的文字宽度 + 内边距 —— 用来判断 tab 条是否会溢出。
fn dock_tab_width(ui: &egui::Ui, title: &str) -> f32 {
    let text = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                title.to_owned(),
                egui::FontId::proportional(12.0),
                Color32::PLACEHOLDER,
            )
            .size()
            .x
    });
    text + 20.0
}

/// 在给定宽度里应该画 `widths[from..to]` 哪一段？
///
/// 能画完就全画；画不完时保证 **当前 tab 一定在可见段里**（Unity 的做法：
/// 被裁掉的 active tab 是最让人迷惑的），不够就把前面的 tab 挤出去。
fn visible_tab_range(widths: &[f32], available: f32, active: usize) -> (usize, usize) {
    if widths.is_empty() {
        return (0, 0);
    }
    let active = active.min(widths.len() - 1);
    let mut end = 0;
    let mut used = 0.0;
    while end < widths.len() && used + widths[end] <= available {
        used += widths[end];
        end += 1;
    }
    // 一个都放不下时至少画一个，否则这个 leaf 会没有可点的标签。
    if end == 0 {
        end = 1;
    }
    if active < end {
        return (0, end);
    }
    // active 在可见段之后：从它往前收，直到放下为止。
    let mut start = active;
    let mut used = widths[active];
    while start > 0 && used + widths[start - 1] <= available {
        used += widths[start - 1];
        start -= 1;
    }
    (start, active + 1)
}

/// tab 条需要滚动 / 溢出按钮吗？
///
/// 留 1px 余量：正好装满时不该为了取整误差显示按钮。
fn tab_bar_needs_overflow(needed: f32, available: f32) -> bool {
    needed > available - 1.0
}

/// 自绘 tab（对齐 Web `.dock-tab`）：`bg-sunken` 底、右侧 1px 分隔线、
/// 选中 = `panel-alt` 底 + 2px 强调下划线。egui 的 `selectable_label` 用整块蓝底，
/// 会让整个界面看起来发蓝，所以不使用它。
fn dock_tab(
    ui: &mut egui::Ui,
    title: &str,
    active: bool,
    colors: &prism_editor_core::ThemeColors,
) -> egui::Response {
    use prism_editor_core::ThemeToken;
    let text_color = theme::color32(
        colors,
        if active {
            ThemeToken::Text
        } else {
            ThemeToken::Muted
        },
    );
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        egui::FontId::proportional(12.0),
        text_color,
    );
    let pad = Vec2::new(10.0, 5.0);
    let size = Vec2::new(galley.size().x + pad.x * 2.0, TAB_BAR_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if ui.is_rect_visible(rect) {
        let paintable = ui.painter();
        let bg = if active {
            theme::color32(colors, ThemeToken::PanelAlt)
        } else if response.hovered() || response.dragged() {
            theme::color32(colors, ThemeToken::RowHover)
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            paintable.rect_filled(rect, 0.0, bg);
        }
        // 右侧分隔线（Web: border-right 1px）
        paintable.rect_filled(
            Rect::from_min_max(
                egui::pos2(rect.right() - 1.0, rect.top()),
                egui::pos2(rect.right(), rect.bottom()),
            ),
            0.0,
            theme::border(colors),
        );
        if active {
            paintable.rect_filled(
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - 2.0),
                    egui::pos2(rect.right() - 1.0, rect.bottom()),
                ),
                0.0,
                theme::color32(colors, ThemeToken::Accent),
            );
        }
        paintable.galley(
            egui::pos2(rect.left() + pad.x, rect.center().y - galley.size().y * 0.5),
            galley,
            text_color,
        );
    }
    response
}

/// 面板 tab 的英文标题（i18n 字典里也有，这里直接用英文，避免依赖字体）。
/// 鼠标落在 leaf 的哪个区域（引擎的分屏 / 合并判定）。
fn dock_zone(rect: Rect, pos: egui::Pos2) -> prism_editor_core::dock::DockZone {
    let fx = (pos.x - rect.left()) / rect.width().max(1.0);
    let fy = (pos.y - rect.top()) / rect.height().max(1.0);
    if fx < 0.25 {
        prism_editor_core::dock::DockZone::Left
    } else if fx > 0.75 {
        prism_editor_core::dock::DockZone::Right
    } else if fy < 0.25 {
        prism_editor_core::dock::DockZone::Top
    } else if fy > 0.75 {
        prism_editor_core::dock::DockZone::Bottom
    } else {
        prism_editor_core::dock::DockZone::Center
    }
}

/// 全局快捷键表：`(键位, 命令)`，菜单提示、Shortcuts 面板与分发共用这一份（#516 还在补别的差异）。
///
/// 裸键（不带修饰键）只留给 `Delete` / `F`：`Select All` 必须是 `Ctrl+A`，否则在输入框里
/// 打一个 `a` 就会全选实体（#499）。
/// 把 `prism_editor_core` 的快捷键表翻译成 egui 的分发表（#516）。
///
/// **单一事实源**：Shortcuts 面板、菜单提示、实际分发读的都是
/// [`prism_editor_core::SHORTCUTS`] / [`prism_editor_core::SHORTCUT_ALIASES`]，
/// 所以不会出现「面板写 Ctrl+Shift+S，按下却触发 Save」这种漂移。
///
/// 顺序按修饰键数量从多到少：带 Shift/Alt 的 chord 必须先于它的子集被消费。
fn editor_shortcuts(
    overrides: &std::collections::BTreeMap<String, String>,
) -> Vec<(egui::KeyboardShortcut, Command)> {
    use egui::{KeyboardShortcut, Modifiers};
    let mut shortcuts: Vec<(KeyboardShortcut, Command)> =
        prism_editor_core::effective_bindings(overrides)
            .into_iter()
            .filter_map(|(command, chord)| {
                let key = key_for(&chord.key)?;
                let mut modifiers = Modifiers::NONE;
                if chord.ctrl {
                    modifiers |= Modifiers::CTRL;
                }
                if chord.shift {
                    modifiers |= Modifiers::SHIFT;
                }
                if chord.alt {
                    modifiers |= Modifiers::ALT;
                }
                Some((KeyboardShortcut::new(modifiers, key), command))
            })
            .collect();
    shortcuts.sort_by_key(|(shortcut, _)| {
        std::cmp::Reverse(
            u8::from(shortcut.modifiers.ctrl)
                + u8::from(shortcut.modifiers.shift)
                + u8::from(shortcut.modifiers.alt),
        )
    });
    shortcuts
}

/// 表里的键名（`s` / `f5` / `home` / `del`）→ egui 的 `Key`。
///
/// `KeyChord::parse` 会把键名小写化，而 egui 的 `Key::from_name` 只认自己的拼写
/// （`Delete` / `Escape` …），所以这里补一张缩写对照。
fn key_for(name: &str) -> Option<egui::Key> {
    let canonical = match name {
        "del" => "delete",
        "ins" => "insert",
        "esc" => "escape",
        "return" => "enter",
        other => other,
    };
    egui::Key::ALL
        .iter()
        .copied()
        .find(|key| key.name().eq_ignore_ascii_case(canonical))
}

/// 只在**修饰键完全一致**时消费按键（#516）。
///
/// `InputState::consume_shortcut` 用的是 `matches_logically`：pattern 里没写的
/// Shift/Alt 也算匹配，于是 `Ctrl+Shift+S` 会先被 `Ctrl+S` 吃掉。这里改成
/// `matches_exact`，并跳过操作系统重复事件。
fn consume_shortcut_exact(input: &mut egui::InputState, shortcut: &egui::KeyboardShortcut) -> bool {
    let index = input.events.iter().position(|event| match event {
        egui::Event::Key {
            key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } => {
            *key == shortcut.logical_key && !*repeat && modifiers.matches_exact(shortcut.modifiers)
        }
        _ => false,
    });
    match index {
        Some(index) => {
            input.events.remove(index);
            true
        }
        None => false,
    }
}

/// 文本控件聚焦时**必须让路**的命令（#499）。
///
/// 这几条绑的是裸键（`F` / `Delete` / `Ctrl+A` 之外的全选），而输入框此刻正等这些字符；
/// 判断要发生在 `consume_shortcut` 之前，否则按键已经被从事件队列里摘掉了。
fn plain_shortcut(command: Command) -> bool {
    matches!(
        command,
        Command::SelectAll | Command::FrameSelection | Command::DeleteSelection
    )
}

fn tab_title(tab: &str) -> &str {
    match tab {
        "sceneView" => "Scene",
        "gameView" => "Game",
        "hierarchy" => "Hierarchy",
        "inspector" => "Inspector",
        "assets" => "Assets",
        "docs" => "Documents",
        "log" => "Console",
        "history" => "History",
        "renderGraph" => "Render Graph",
        "projectSettings" => "Project Settings",
        "preferences" => "Preferences",
        "theme" => "Theme",
        "build" => "Build",
        "problems" => "Problems",
        "prefabOverrides" => "Prefab Overrides",
        "material" => "Material",
        "tools" => "Tools",
        "shortcuts" => "Shortcuts",
        "about" => "About",
        other => other,
    }
}

/// 关闭确认框的三个回答。
enum CloseDecision {
    Save,
    Discard,
    Cancel,
}

/// 磁盘冲突对话框的选择（#452 / #618）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveConflictChoice {
    Overwrite,
    Reload,
    Cancel,
}

pub struct EditorApp {
    pub workspace: Workspace,
    pub state: UiState,
    pub preferences: Preferences,
    project_root: Option<PathBuf>,
    preview_scene: PathBuf,

    gpu: SharedVulkan,
    bridge: ImageBridge,
    scene_view: Option<ViewportState>,
    game_view: Option<ViewportState>,

    exit_signal: ExitSignal,
    /// 当前文档 JSON 的哈希（检测未保存的改动）。
    doc_hash: u64,
    /// 选中集合的哈希（Scene view 的 gizmo / 高亮跟着选择变）。
    selection_hash: u64,
    pending_doc: Option<String>,
    pending_since: Option<Instant>,
    last_frame: Instant,
    /// 上一帧窗口是否有焦点；`false → true` 时比对一次磁盘（#452）。
    was_focused: bool,
    playing: bool,
    paused: bool,
    /// 控制协议 / 自动化请求退出：跳过未保存确认。
    force_quit: bool,
    render_report: Option<String>,
    /// `render_report` 对应文件的 mtime（面板可见时按需重读）。
    render_report_mtime: Option<std::time::SystemTime>,
    /// 命名布局（Window → Layouts）：name → DockLayout JSON。
    named_layouts: std::collections::BTreeMap<String, serde_json::Value>,
    layouts_path: PathBuf,
    /// 正在跑的构建 / cook 子进程。
    build: Option<crate::build::BuildJob>,
    /// 松散资产轮询（#495）：`assets/` 下未打包的文件变化 → 自动重打场景包。
    loose_assets: Option<crate::loose_assets::LooseAssetWatcher>,
    /// 下一次允许扫描松散资产的时间（1s 节流）。
    loose_scan_at: Instant,
    /// 最近一次玩家构建的编译器诊断（Problems 面板与模块诊断并列显示）。
    build_diagnostics: Vec<crate::diagnostics::Diagnostic>,
    /// 两个视口共享的渲染设置（Console 里的「渲染图」面板可改）。
    render_settings: prism_engine::render_settings::RenderSettings,
    /// 已应用的 (preset, overrides) 签名，避免每帧重建 style。
    theme_signature: String,
    state_path: PathBuf,
    autosave_at: Instant,
    /// 根窗口的 HWND（Windows 截图用；其它平台为 `None`）。
    hwnd: Option<isize>,
    /// 本地控制通道（MCP 驱动截图 / 状态 / 退出）。
    control: Option<crate::control::ControlServer>,
    /// 已渲染帧数（状态汇报用）。
    frame_count: u64,
    /// 还没到「画面稳定」帧的截图请求：等 `ready_at` 帧再抓，回执延迟发送。
    pending_captures: Vec<PendingCapture>,
    /// 上一帧各面板的屏幕矩形（`ui.snapshot` 用）。
    panel_rects: std::collections::BTreeMap<String, egui::Rect>,
    /// 上一帧的 dock leaf：(leaf id, 激活 tab, 所有 tab)。
    dock_leaves: Vec<(String, String, Vec<String>)>,
    /// 全局搜索弹窗（`Ctrl+K`，见 `palette.rs`）。
    palette: crate::palette::Palette,
    /// 控制通道排进来的输入事件（自动化 / MCP 驱动 Play）。
    ///
    /// **每帧只消费一个**：`just_pressed` 在这一帧的 tick 里可见，下一帧就被
    /// `InputManager::begin_frame` 清掉；一次塞两个会让按下/松开落在同一帧里，
    /// 工程侧永远看不到点击。
    pending_input: std::collections::VecDeque<InjectedInput>,
}

impl EditorApp {
    pub fn new(cc: &CreationContext<'_>) -> anyhow::Result<Self> {
        let gpu = SharedVulkan::create(cc).context("create shared Vulkan device")?;
        let sampler = unsafe {
            gpu.context.device.create_sampler(
                &ash::vk::SamplerCreateInfo::default()
                    .mag_filter(ash::vk::Filter::LINEAR)
                    .min_filter(ash::vk::Filter::LINEAR)
                    .mipmap_mode(ash::vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(ash::vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(ash::vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(ash::vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .unnormalized_coordinates(false),
                None,
            )
        }
        .context("create viewport sampler")?;

        let bridge = ImageBridge {
            registry: cc.image_registry.clone(),
            sampler,
        };

        theme::install_fonts(&cc.context);

        // egui-ash 建根窗口时用的是 winit 默认属性（800×600 + 标题 "egui-ash"），
        // `RunOption::viewport_builder` 的 inner_size / title 不会生效。这里用
        // 视口命令补一次，否则一进来就是 800×600 的挤压布局。
        cc.context
            .send_viewport_cmd(egui::ViewportCommand::Title("PrismaRev Editor".to_owned()));
        cc.context
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1440.0, 900.0)));

        let mut workspace = load_workspace();
        workspace.set_locale(Locale::EnUs);
        theme::apply(&cc.context, &workspace.colors());

        let project_root = workspace.project_root.clone();
        let loose_assets = project_root
            .as_deref()
            .map(crate::loose_assets::LooseAssetWatcher::new);
        let loose_scan_at = Instant::now();
        let preview_dir = project_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("target/editor-egui")
            .join("preview");
        std::fs::create_dir_all(&preview_dir).ok();
        let preview_scene = preview_dir.join("scene.json");
        let state_path = project_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("target/editor-egui/state.json");

        // 恢复上次的 dock 布局 / 浮动窗口 / 语言（与 Web 的 `prisma-editor.dock.v4` 同形）。
        let layouts_path = state_path
            .parent()
            .map(|dir| dir.join("layouts.json"))
            .unwrap_or_else(|| PathBuf::from("layouts.json"));
        let named_layouts = read_named_layouts(&layouts_path);

        let saved_state = read_editor_state(&state_path);
        if let Some(state) = &saved_state {
            let layout = prism_editor_core::DockLayout::from_json(
                &state.dock,
                &workspace.known_panels(),
                1440.0,
                900.0,
            );
            workspace.dock = layout.dock;
            workspace.floating = layout.floating;
            // 上次最大化的面板（存档里的 tab 可能已经不存在，按 dock 过滤）。
            workspace.maximized_tab = state.maximized.clone().filter(|tab| {
                prism_editor_core::dock::collect_tabs(&workspace.dock)
                    .iter()
                    .any(|existing| existing == tab)
            });
            workspace.set_locale(Locale::parse(&state.locale));
            if !state.theme_preset.is_empty() {
                workspace.set_theme_preset(&state.theme_preset);
            }
        }

        let restored_scene = saved_state
            .as_ref()
            .map(|state| state.scene.clone())
            .unwrap_or_default();
        let restored_tool = match restored_scene.gizmo_tool.as_str() {
            "Rotate" => crate::viewport::GizmoTool::Rotate,
            "Scale" => crate::viewport::GizmoTool::Scale,
            _ => crate::viewport::GizmoTool::Translate,
        };
        let render_settings = restored_scene
            .render
            .as_ref()
            .map(|value| render_settings_from_json(value, workspace.show_grid))
            .unwrap_or_else(|| prism_engine::render_settings::RenderSettings {
                show_grid: workspace.show_grid,
                ..Default::default()
            });
        workspace.show_grid = render_settings.show_grid;

        let mut app = Self {
            doc_hash: 0,
            selection_hash: 0,
            pending_doc: None,
            pending_since: None,
            last_frame: Instant::now(),
            was_focused: false,
            playing: false,
            paused: false,
            force_quit: false,
            render_report: None,
            render_report_mtime: None,
            palette: crate::palette::Palette::new(),
            named_layouts,
            layouts_path,
            build: None,
            loose_assets,
            loose_scan_at,
            build_diagnostics: Vec::new(),
            // 引擎默认 `show_grid = false`（游戏里不该画网格），编辑器要从上次保存的
            // Scene 设置出发（旧存档没有这段时退回工作区默认值）。
            render_settings,
            theme_signature: String::new(),
            autosave_at: Instant::now(),
            workspace,
            state: UiState::new(),
            preferences: saved_state
                .as_ref()
                .map(|state| state.preferences.clone())
                .unwrap_or_default(),
            project_root,
            preview_scene,
            gpu,
            bridge,
            scene_view: None,
            game_view: None,
            exit_signal: cc.exit_signal.clone(),
            state_path: state_path.clone(),
            hwnd: main_window_hwnd(cc.main_window),
            control: crate::control::ControlServer::start(
                state_path.parent().unwrap_or_else(|| Path::new(".")),
            )
            .map_err(|error| {
                log::warn!("control channel disabled: {error:#}");
                error
            })
            .ok(),
            frame_count: 0,
            pending_captures: Vec::new(),
            panel_rects: std::collections::BTreeMap::new(),
            dock_leaves: Vec::new(),
            pending_input: std::collections::VecDeque::new(),
        };

        // 上次退出时留下的未保存快照（#619）：有就弹 Recover / Discard。
        app.state.pending_recovery = match app.workspace.recovery_pending() {
            Ok(count) => count,
            Err(error) => {
                log::warn!("failed to read editor recovery snapshot: {error:#}");
                0
            }
        };

        // Preferences 的日志上限在启动时生效，而不是等用户再拖一次滑条。
        app.workspace
            .log
            .set_capacity(app.preferences.max_log_lines as usize);

        // 首帧：把当前文档交给引擎，并创建 Scene view。
        if !app.workspace.document().doc.entities.is_empty() {
            // 打开场景时自动框住全部内容。
            app.workspace.frame_all();
        }
        app.write_preview_scene();
        app.ensure_view(ViewKind::Scene);
        // 恢复上次的 gizmo 工具（W / E / R），否则每次启动都回到 Move。
        if let Some(view) = app.scene_view.as_mut() {
            view.tool = restored_tool;
        }
        // Game view 的引擎在这里一起建好。留到第一次切 tab 再建的话，会卡住整个
        // UI 线程 ~2s（IBL 预过滤 1.5s + 管线创建 + 场景加载），观感就是「切 tab 卡半天」。
        let started = Instant::now();
        app.ensure_view(ViewKind::Game);
        log::info!(
            "game view engine ready in {:.0} ms",
            started.elapsed().as_secs_f32() * 1000.0
        );
        app.doc_hash = document_hash(&app.workspace);
        app.render_report = read_report(app.project_root.as_deref());
        app.workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Info,
            "PrismaRev editor ready — Scene/Game viewports share the egui VkDevice (zero-copy)",
        );
        Ok(app)
    }

    // -----------------------------------------------------------------------
    // 引擎 / 视口
    // -----------------------------------------------------------------------

    fn ensure_view(&mut self, kind: ViewKind) -> bool {
        let exists = match kind {
            ViewKind::Scene => self.scene_view.is_some(),
            ViewKind::Game => self.game_view.is_some(),
        };
        if exists {
            return true;
        }
        let size = (1280u32, 720u32);
        let created = ViewportState::new(kind, self.gpu.context.clone(), size.0, size.1);
        match created {
            Ok(mut view) => {
                // 新视口启动设置：Scene 从编辑器当前设置出发（网格 / 天空盒），
                // Game **不继承**编辑器调试开关（见 `initial_view_settings`）。
                *view.host.settings_mut() = initial_view_settings(kind, &self.render_settings);
                // Game view 代表「独立构建跑起来的样子」：渲染默认值必须来自工程
                // 自己的 `assets/settings.ini [render]`，否则曝光 / 色调映射和产物不一致。
                if kind == ViewKind::Game {
                    apply_project_render_defaults(&mut view.host, self.project_root.as_deref());
                    view.project_resolution =
                        project_window_resolution(self.project_root.as_deref());
                    // 默认按工程声明的窗口分辨率渲染，而不是面板的任意比例（#585）。
                    if let Some((width, height)) = view.project_resolution {
                        view.target = GameResolution::Fixed { width, height };
                    }
                }
                view.host.set_show_skybox(self.workspace.show_skybox);
                // 分析（渲染图）面板读这个文件；Scene view 出帧时自己写（#451）。
                if kind == ViewKind::Scene {
                    view.report_path = self
                        .project_root
                        .as_ref()
                        .map(|root| root.join("target/prism-render-graph.json"));
                }
                // Game view 的 Play 执行用户项目代码（Scene view 只跑编辑器预览）。
                if kind == ViewKind::Game {
                    if let Some(root) = self.project_root.as_deref() {
                        view.host.attach_project_module(root);
                    }
                }
                if let Err(error) = view.host.load_scene(&self.preview_scene) {
                    let message = format!("load scene failed: {error}");
                    self.workspace.log(
                        prism_editor_core::LogSource::Editor,
                        prism_editor_core::LogLevel::Error,
                        message.clone(),
                    );
                    view.error = Some(message);
                }
                match kind {
                    ViewKind::Scene => self.scene_view = Some(view),
                    ViewKind::Game => self.game_view = Some(view),
                }
                true
            }
            Err(error) => {
                self.workspace.log(
                    prism_editor_core::LogSource::Editor,
                    prism_editor_core::LogLevel::Error,
                    format!("create {kind:?} view failed: {error:#}"),
                );
                false
            }
        }
    }

    fn write_preview_scene(&mut self) {
        let text = self.workspace.document().doc.to_json_string();
        if let Err(error) = std::fs::write(&self.preview_scene, text) {
            log::warn!("write preview scene failed: {error}");
        }
    }

    /// 文档变化 → 防抖后重新载入两个引擎实例。
    fn sync_document(&mut self) {
        let hash = document_hash(&self.workspace);
        if hash != self.doc_hash {
            self.doc_hash = hash;
            self.pending_doc = Some(self.workspace.document().doc.to_json_string());
            self.pending_since = Some(Instant::now());
        }
        if self.playing {
            // Play 期间不重载（与 Tauri 编辑器一致）：编辑文档不影响运行中的模拟。
            return;
        }
        let Some(_) = self.pending_doc else {
            return;
        };
        let wait = self
            .pending_since
            .map(|since| since.elapsed().as_millis() >= 120)
            .unwrap_or(true);
        if !wait {
            return;
        }
        self.pending_doc = None;
        self.pending_since = None;
        self.write_preview_scene();
        // 只重载正在编辑的那一侧：Scene view 要跟手，Game view 在 Play 时
        // 本来就会从磁盘快照重新加载（见 `apply_command(Command::Play)`），
        // 编辑期间每 120ms 让它也重建一遍世界纯属浪费（#435）。
        if let Some(view) = self.scene_view.as_mut() {
            if let Err(error) = view.host.load_scene(&self.preview_scene) {
                view.error = Some(format!("{error}"));
            }
            view.mark_dirty();
        }
    }

    fn apply_command(&mut self, command: Command) {
        if command == Command::DeleteSelection
            && self.state.confirm_delete
            && !self.workspace.selection.is_empty()
        {
            self.state.pending_entity_delete = Some(self.workspace.selection.clone());
            return;
        }
        if command == Command::SaveAs {
            // 默认值必须是带后缀的完整文件名 —— 否则照着默认值点 OK 就会存出
            // 一个 `assets/scenes/level2`，文档面板和 Build Settings 都不认（#459）。
            let kind = self.workspace.document().kind;
            let title = prism_editor_core::project::ensure_document_suffix(
                kind,
                &self.workspace.document().title(),
            );
            self.open_prompt(
                panels::PromptKind::SaveAs,
                "Save As",
                "assets/ 下的相对路径（.scene.json / .prefab.json）",
                title,
            );
            return;
        }
        let was_playing = self.workspace.playing;
        let was_paused = self.workspace.paused;
        // Console 的 Clear on Play（#421）：在 Play 真正开始前清空，这样 Play 自己
        // 打的第一行日志不会被吞掉。
        if command == Command::Play && !was_playing && self.state.log_clear_on_play {
            self.workspace.log.clear();
        }
        self.workspace.run(command);

        if command == Command::Play && !was_playing {
            self.playing = true;
            self.paused = false;
            // Play 从「当前文档的磁盘快照」开始：先落盘，再让 Game view 重载。
            self.pending_doc = None;
            self.write_preview_scene();
            self.ensure_view(ViewKind::Game);
            let mut play_note = None;
            if let Some(view) = self.game_view.as_mut() {
                if let Err(error) = view.host.load_scene(&self.preview_scene) {
                    view.error = Some(format!("{error}"));
                }
                // 还没编译过项目模块时，按下 Play 立即插队编译一次；编译
                // 本身仍然在后台线程，不会卡住 UI。
                let status = view.host.project_module_status();
                match &status {
                    // 已加载但源码有未编译的改动（Auto Refresh 关 / 上一个 build 被
                    // Play 挡住）：Play 前补编一次，别让试玩跑旧代码。
                    Some(status) if status.loaded && status.dirty => {
                        if !status.building {
                            view.host.refresh_project_module();
                        }
                        play_note = Some(
                            "Play: compiling pending module changes — project state resets on reload"
                                .to_string(),
                        );
                    }
                    Some(status) if status.loaded => {}
                    Some(status) if status.available => {
                        if !status.building {
                            view.host.refresh_project_module();
                        }
                        play_note = Some(if status.building {
                            "Play: project module is compiling — engine systems only for now"
                                .to_string()
                        } else {
                            "Play: compiling project module — engine systems only until it loads"
                                .to_string()
                        });
                    }
                    _ => {
                        play_note = Some(
                            "Play: project has no preview-module — running engine systems only"
                                .to_string(),
                        );
                    }
                }
                view.host.set_simulate(true);
                view.host.set_paused(false);
                view.mark_dirty();
            }
            if let Some(note) = &play_note {
                self.state.status = note.clone();
                self.workspace.log(
                    prism_editor_core::LogSource::System,
                    prism_editor_core::LogLevel::Warn,
                    note.clone(),
                );
            }
            if let Some(view) = self.scene_view.as_mut() {
                view.mark_dirty();
            }
        }
        if command == Command::Stop && was_playing {
            self.playing = false;
            self.paused = false;
            // 播放期间的编辑已经被 `workspace.stop()` 丢弃（#500）：先把预览文件写回编辑态，
            // 否则 Game view 会从「被丢弃的那份文档」重新加载。
            let discarded = self.pending_doc.is_some();
            self.write_preview_scene();
            self.pending_since = None;
            if discarded {
                self.state.status =
                    "Stopped Play — edits made while playing were discarded".to_string();
            }
            // Stop must clear the live runtime immediately; do not wait for the
            // document debounce path to reload the Game view.
            if let Some(view) = self.game_view.as_mut() {
                if let Err(error) = view.host.load_scene(&self.preview_scene) {
                    view.error = Some(format!("{error}"));
                }
                view.host.set_simulate(false);
                view.host.set_paused(false);
                view.reset_forwarded_input();
                view.mark_dirty();
            }
            self.doc_hash = document_hash(&self.workspace);
            self.pending_doc = None;
            if let Some(view) = self.scene_view.as_mut() {
                view.mark_dirty();
            }
        }
        if command == Command::Pause || command == Command::Step {
            self.paused = self.workspace.paused;
        }
        if command == Command::Step {
            if let Some(view) = self.game_view.as_mut() {
                view.host.step();
                view.mark_dirty();
            }
        }
        // Play / Pause 同步到 Game view 的引擎。
        if self.playing != was_playing || self.paused != was_paused {
            if let Some(view) = self.game_view.as_mut() {
                view.host.set_simulate(self.playing);
                view.host.set_paused(self.paused);
            }
        }
    }

    // -----------------------------------------------------------------------
    // 菜单 / 工具栏 / 状态栏
    // -----------------------------------------------------------------------

    /// 菜单项：`label 	 shortcut`，禁用状态来自 `Workspace::is_enabled`。
    fn menu_item(&mut self, ui: &mut egui::Ui, label: &str, shortcut: &str, command: Command) {
        let enabled = self.workspace.is_enabled(command);
        let text = if shortcut.is_empty() {
            label.to_owned()
        } else {
            format!("{label}\t{shortcut}")
        };
        if ui.add_enabled(enabled, egui::Button::new(text)).clicked() {
            self.apply_command(command);
            ui.close();
        }
    }

    /// 直接执行一段操作（菜单里的非 `Command` 项，例如新建目录 / 实例化预制体）。
    /// 处理本地控制通道的请求（MCP 驱动）。每帧取一次，同一帧内完成。
    fn poll_control(&mut self, ctx: &egui::Context) {
        if let Some(control) = self.control.as_ref() {
            for request in control.drain() {
                if let Some(response) =
                    self.handle_command(ctx, &request.command, &request.args, request.reply.clone())
                {
                    let _ = request.reply.send(response);
                }
            }
        }
        // 延迟的截图请求：等到「已经呈现过一帧」再抓（刚启动时窗口还没合成）。
        let due: Vec<usize> = self
            .pending_captures
            .iter()
            .enumerate()
            .filter(|(_, pending)| self.frame_count >= pending.ready_at)
            .map(|(index, _)| index)
            .collect();
        for index in due.into_iter().rev() {
            let pending = self.pending_captures.remove(index);
            match pending.target {
                // 整窗截图必须离开 UI 线程：`PrintWindow` 会给自己的窗口发消息。
                crate::screenshot::CaptureTarget::Window => {
                    let Some(hwnd) = self.hwnd else {
                        let _ = pending.reply.send(serde_json::json!({
                            "ok": false,
                            "error": "window capture unavailable on this platform",
                        }));
                        continue;
                    };
                    let path = pending.path.clone();
                    let size = pending.size;
                    let reply = pending.reply;
                    std::thread::Builder::new()
                        .name("editor-window-capture".to_owned())
                        .spawn(move || {
                            let response = match crate::screenshot::save_window_png(hwnd, &path, size)
                            {
                                Ok(path) => serde_json::json!({
                                    "ok": true,
                                    "path": path.display().to_string(),
                                    "target": "window",
                                }),
                                Err(error) => {
                                    serde_json::json!({ "ok": false, "error": format!("{error:#}") })
                                }
                            };
                            let _ = reply.send(response);
                        })
                        .ok();
                    self.state.status = format!("screenshot: {}", pending.path.display());
                }
                target => {
                    let response = match self.capture(target, &pending.path, pending.size) {
                        Ok(path) => serde_json::json!({
                            "ok": true,
                            "path": path.display().to_string(),
                            "target": target.name(),
                        }),
                        Err(error) => {
                            serde_json::json!({ "ok": false, "error": format!("{error:#}") })
                        }
                    };
                    let _ = pending.reply.send(response);
                }
            }
        }
        if !self.pending_captures.is_empty() {
            ctx.request_repaint();
        }
    }

    /// 当前打开的模态对话框，供 `ui.snapshot` / `ui.click` 使用（#618）。
    fn dialogs(&self) -> Vec<crate::ui_tree::Dialog> {
        use crate::ui_tree::Dialog;
        let mut out = Vec::new();

        if let Some(indices) = &self.state.pending_entity_delete {
            let message = if indices.len() == 1 {
                "Delete the selected entity and its children?".to_string()
            } else {
                format!(
                    "Delete {} selected entities and their children?",
                    indices.len()
                )
            };
            out.push(
                Dialog::new("delete-entities", "Delete", message)
                    .button("delete", "Delete")
                    .button("cancel", "Cancel"),
            );
        }

        if let Some(path) = &self.state.pending_doc_delete {
            out.push(
                Dialog::new(
                    "delete-document",
                    "Delete Document",
                    format!(
                        "Delete {path}? The file, its open tab and its Build Settings entry are removed."
                    ),
                )
                .button("delete", "Delete")
                .button("cancel", "Cancel"),
            );
        }

        if let Some(paths) = &self.state.pending_asset_delete {
            let message = match paths.as_slice() {
                [one] => format!("删除 {one}？此操作不可撤销。"),
                many => format!("删除 {} 项资产？此操作不可撤销。", many.len()),
            };
            out.push(
                Dialog::new("delete-assets", "Delete Asset", message)
                    .button("delete", "Delete")
                    .button("cancel", "Cancel"),
            );
        }

        if let Some(index) = self.state.pending_save_conflict {
            let (title, path) = self
                .workspace
                .docs
                .get(index)
                .map(|document| (document.title(), document.path.clone().unwrap_or_default()))
                .unwrap_or_else(|| ("Document".to_string(), String::new()));
            out.push(
                Dialog::new(
                    "save-conflict",
                    "File Changed on Disk",
                    format!("{title} was modified outside the editor. ({path})"),
                )
                .button("overwrite", "Overwrite")
                .button("reload", "Reload from Disk")
                .button("cancel", "Cancel"),
            );
        }

        if let Some(target) = self.state.pending_close {
            let (message, detail) = match target {
                panels::CloseTarget::Document(index) => {
                    let title = self
                        .workspace
                        .docs
                        .get(index)
                        .map(|document| document.title())
                        .unwrap_or_else(|| "document".to_string());
                    (
                        format!("{title} has unsaved changes."),
                        "Close it anyway?".to_string(),
                    )
                }
                panels::CloseTarget::App => {
                    let count = self
                        .workspace
                        .docs
                        .iter()
                        .filter(|document| document.dirty)
                        .count();
                    (
                        format!("{count} document(s) have unsaved changes."),
                        "Quit anyway?".to_string(),
                    )
                }
            };
            out.push(
                Dialog::new("close", "Unsaved Changes", format!("{message} {detail}"))
                    .button("save", "Save")
                    .button("discard", "Don't Save")
                    .button("cancel", "Cancel"),
            );
        }

        if let Some(confirm) = &self.state.confirm {
            out.push(
                Dialog::new("confirm", confirm.title.clone(), confirm.message.clone())
                    .button("ok", &confirm.ok)
                    .button("cancel", "Cancel"),
            );
        }

        if let Some(prompt) = &self.state.prompt {
            out.push(
                Dialog::new(
                    "prompt",
                    prompt.title.clone(),
                    format!("{}（当前值：{}）", prompt.hint, prompt.value),
                )
                .button("ok", "OK")
                .button("cancel", "Cancel")
                .value(prompt.value.clone()),
            );
        }

        out
    }

    /// Scene / Game 视口里的物体：世界坐标 + 投影后的视口局部像素。
    ///
    /// 这是 `ui.snapshot` 的 `objects` 与 `ui.click {"node":"view:<tab>:entity:<id>"}`
    /// 的数据来源：投影在编辑器侧算好，脚本不用再猜坐标（更没有 DPI 换算问题）。
    fn viewport_objects(&self) -> Vec<crate::ui_tree::ViewObjects> {
        use crate::ui_tree::{ViewObject, ViewObjects};
        use prism_editor_core::viewport_math as vm;

        let mut out = Vec::new();

        if let Some(rect) = self.panel_rects.get("sceneView").copied() {
            let frame = vm::camera_frame(&self.workspace.camera);
            let doc = &self.workspace.document().doc;
            let objects = (0..doc.entities.len())
                .map(|index| {
                    let entity = &doc.entities[index];
                    let world = vm::world_position(doc, index);
                    let projected = vm::project(world, &frame, rect.width(), rect.height());
                    ViewObject {
                        node: crate::ui_tree::view_object_node(
                            "sceneView",
                            entity.id.unwrap_or(index as u32),
                        ),
                        entity: Some(entity.id.unwrap_or(index as u32)),
                        name: entity.name.clone().unwrap_or_default(),
                        world,
                        screen: projected.map(|(x, y, _)| [x, y]),
                        depth: projected.map(|(_, _, depth)| depth),
                        visible: scene_entity_visible(entity),
                    }
                })
                .collect();
            out.push(ViewObjects {
                tab: "sceneView",
                objects,
            });
        }

        if let (Some(view), Some(rect)) = (
            self.game_view.as_ref(),
            self.panel_rects.get("gameView").copied(),
        ) {
            out.push(ViewObjects {
                tab: "gameView",
                objects: game_view_objects(view, rect),
            });
        }

        out
    }

    /// 视口物体节点（`view:<tab>:entity:<id>`）点击：点它的投影点，
    /// 可选 `offset` 相对该点偏移（视口局部像素）。
    fn ui_click_view_object(
        &mut self,
        tab: &str,
        node: &str,
        offset: Option<[f32; 2]>,
    ) -> Result<serde_json::Value, String> {
        let objects = self.viewport_objects();
        let view = objects
            .iter()
            .find(|view| view.tab == tab)
            .ok_or_else(|| format!("viewport '{tab}' is not open"))?;
        let object = view
            .objects
            .iter()
            .find(|object| object.node == node)
            .ok_or_else(|| format!("'{node}' is not in view '{tab}'"))?;
        let screen = object
            .screen
            .ok_or_else(|| format!("'{node}' is behind the camera"))?;
        let [dx, dy] = offset.unwrap_or([0.0, 0.0]);
        self.viewport_click(tab, (screen[0] + dx, screen[1] + dy))
    }

    /// `panel:sceneView` / `panel:gameView` + `offset`：offset 是**视口局部**像素
    /// （面板左上角为原点），直接在该点做点击。
    fn ui_click_panel_offset(
        &mut self,
        node: &str,
        offset: [f32; 2],
    ) -> Result<serde_json::Value, String> {
        let tab = node
            .strip_prefix("panel:")
            .ok_or_else(|| format!("offset clicks need a viewport panel, got '{node}'"))?;
        if tab != "sceneView" && tab != "gameView" {
            return Err(format!(
                "panel '{tab}' has no viewport to click in (only sceneView / gameView)"
            ));
        }
        self.viewport_click(tab, (offset[0], offset[1]))
    }

    /// 在视口局部像素处「点一下」：
    ///
    /// - Scene view：走编辑器自己的拾取（`viewport::pick`），于是脚本测到的是
    ///   用户真实点击的同一条路径（点空处清空选择、点物体选中它）；
    /// - Game view：把坐标换算到引擎绘图区像素后进 Play 的输入队列。
    fn viewport_click(
        &mut self,
        tab: &str,
        local: (f32, f32),
    ) -> Result<serde_json::Value, String> {
        let rect = self
            .panel_rects
            .get(tab)
            .copied()
            .ok_or_else(|| format!("viewport '{tab}' was not laid out yet"))?;
        if !local.0.is_finite() || !local.1.is_finite() {
            return Err("offset must be finite".into());
        }
        match tab {
            "sceneView" => {
                if local.0 < 0.0
                    || local.1 < 0.0
                    || local.0 > rect.width()
                    || local.1 > rect.height()
                {
                    return Err(format!(
                        "({:.1},{:.1}) is outside the scene view ({}x{})",
                        local.0,
                        local.1,
                        rect.width(),
                        rect.height()
                    ));
                }
                let frame = prism_editor_core::viewport_math::camera_frame(&self.workspace.camera);
                let hit = {
                    let host = self.scene_view.as_ref().map(|view| &view.host);
                    crate::viewport::pick(&self.workspace, host, local, rect.size(), &frame)
                };
                let hit_node = hit.map(|index| {
                    crate::ui_tree::view_object_node(
                        tab,
                        self.workspace.document().doc.entities[index]
                            .id
                            .unwrap_or(index as u32),
                    )
                });
                match hit {
                    Some(index) => self.workspace.select(index, false),
                    None => self.workspace.clear_selection(),
                }
                Ok(json!({
                    "anchor": "top-left",
                    "at": [local.0, local.1],
                    "hit": hit_node,
                    "action": match hit {
                        Some(_) => "select",
                        None => "clear-selection",
                    },
                }))
            }
            "gameView" => {
                // 面板是逻辑像素，引擎绘图区是离屏像素：按 letterbox 后的图像区域折算。
                let cursor = self.game_graphic_cursor(local)?;
                self.pending_input
                    .push_back(InjectedInput::Cursor(cursor.0, cursor.1));
                self.pending_input.push_back(InjectedInput::Mouse(
                    prism_engine::input::MouseButton::Left,
                    true,
                ));
                self.pending_input.push_back(InjectedInput::Mouse(
                    prism_engine::input::MouseButton::Left,
                    false,
                ));
                Ok(json!({
                    "anchor": "top-left",
                    "at": [local.0, local.1],
                    "cursor": [cursor.0, cursor.1],
                    "queued": 3,
                }))
            }
            other => Err(format!("unknown viewport '{other}'")),
        }
    }

    /// `ui.click` 的节点分派：走和界面完全相同的代码路径。
    /// Scene 视口的面板矩形（控制通道的绝对坐标用）。
    fn scene_rect(&self) -> Result<egui::Rect, String> {
        self.panel_rects
            .get("sceneView")
            .copied()
            .ok_or_else(|| "scene view was not laid out yet".to_string())
    }

    /// `node`/`offset` 或 `tab`+`x`/`y` → `(面板, 视口局部坐标, 锚点)`（#643）。
    ///
    /// 物体节点的锚点是它的 `screen_rect` 中心，面板节点是左上角。
    fn viewport_point(
        &self,
        args: &serde_json::Value,
    ) -> Result<(String, (f32, f32), &'static str), String> {
        if let Some(node) = args.get("node").and_then(serde_json::Value::as_str) {
            let offset = args
                .get("offset")
                .and_then(parse_offset)
                .unwrap_or([0.0, 0.0]);
            if let Some(tab) = node.strip_prefix("panel:") {
                if !self.panel_rects.contains_key(tab) {
                    return Err(format!("panel '{tab}' was not laid out yet"));
                }
                return Ok((tab.to_string(), (offset[0], offset[1]), "top-left"));
            }
            let tab = crate::ui_tree::parse_view_object_node(node)
                .ok_or_else(|| format!("'{node}' is not a viewport object node"))?;
            let objects = self.viewport_objects();
            let view = objects
                .iter()
                .find(|view| view.tab == tab)
                .ok_or_else(|| format!("viewport '{tab}' is not open"))?;
            let object = view
                .objects
                .iter()
                .find(|object| object.node == node)
                .ok_or_else(|| format!("'{node}' is not in view '{tab}'"))?;
            let screen = object
                .screen
                .ok_or_else(|| format!("'{node}' is behind the camera"))?;
            return Ok((
                tab.to_string(),
                (screen[0] + offset[0], screen[1] + offset[1]),
                "center",
            ));
        }
        let tab = args
            .get("tab")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("sceneView")
            .to_string();
        let x = args.get("x").and_then(serde_json::Value::as_f64);
        let y = args.get("y").and_then(serde_json::Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) => Ok((tab, (x as f32, y as f32), "top-left")),
            _ => Err("need node+offset or x/y".to_string()),
        }
    }

    /// 视口局部像素 → Game 绘图区像素（含 letterbox 与离屏尺寸，#463/#585）。
    fn game_graphic_cursor(&self, local: (f32, f32)) -> Result<(f32, f32), String> {
        let rect = self
            .panel_rects
            .get("gameView")
            .copied()
            .ok_or_else(|| "game view was not laid out yet".to_string())?;
        let view = self
            .game_view
            .as_ref()
            .ok_or_else(|| "game view is not open".to_string())?;
        let extent = view.host.size();
        let image = crate::viewport::letterbox(rect, extent);
        let position = egui::pos2(rect.min.x + local.0, rect.min.y + local.1);
        let cursor = crate::viewport::game_cursor(image, position, extent);
        Ok((cursor[0], cursor[1]))
    }

    /// `ui.move`：把指针移到视口里某点，报告该点下的目标（#643）。
    fn ui_view_move(&mut self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        let (tab, local, anchor) = self.viewport_point(args)?;
        match tab.as_str() {
            "sceneView" => {
                let rect = self.scene_rect()?;
                if local.0 < 0.0
                    || local.1 < 0.0
                    || local.0 > rect.width()
                    || local.1 > rect.height()
                {
                    return Err(format!(
                        "({:.1},{:.1}) is outside the scene view ({}x{})",
                        local.0,
                        local.1,
                        rect.width(),
                        rect.height()
                    ));
                }
                let frame = prism_editor_core::viewport_math::camera_frame(&self.workspace.camera);
                let hit = {
                    let host = self.scene_view.as_ref().map(|view| &view.host);
                    crate::viewport::pick(&self.workspace, host, local, rect.size(), &frame)
                };
                let hit_node = hit.map(|index| {
                    crate::ui_tree::view_object_node(
                        "sceneView",
                        self.workspace.document().doc.entities[index]
                            .id
                            .unwrap_or(index as u32),
                    )
                });
                let gizmo_axis = self
                    .scene_view
                    .as_ref()
                    .and_then(|view| view.gizmo_hit_at(&self.workspace, &frame, rect, local))
                    .map(|(axis, _, _)| axis);
                Ok(json!({
                    "at": [local.0, local.1],
                    "anchor": anchor,
                    "hit": hit_node,
                    "gizmo_axis": gizmo_axis,
                }))
            }
            "gameView" => {
                let cursor = self.game_graphic_cursor(local)?;
                self.pending_input
                    .push_back(InjectedInput::Cursor(cursor.0, cursor.1));
                Ok(json!({
                    "at": [local.0, local.1],
                    "anchor": anchor,
                    "cursor": [cursor.0, cursor.1],
                    "queued": 1,
                }))
            }
            other => Err(format!("'{other}' is not a viewport panel")),
        }
    }

    /// `ui.wheel`：Scene 视图缩放到光标（#511），Game 视图把滚轮转给玩法（#643）。
    fn ui_view_wheel(&mut self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        let (tab, local, anchor) = self.viewport_point(args)?;
        let delta = args
            .get("delta")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(120.0) as f32;
        if !delta.is_finite() {
            return Err("delta must be finite".to_string());
        }
        match tab.as_str() {
            "sceneView" => {
                let rect = self.scene_rect()?;
                let frame = prism_editor_core::viewport_math::camera_frame(&self.workspace.camera);
                let (origin, direction) = prism_editor_core::viewport_math::ray_from_screen(
                    local.0,
                    local.1,
                    &frame,
                    rect.width(),
                    rect.height(),
                );
                let distance = self.workspace.camera.distance;
                let anchor_point = [
                    origin[0] + direction[0] * distance,
                    origin[1] + direction[1] * distance,
                    origin[2] + direction[2] * distance,
                ];
                self.workspace
                    .camera
                    .zoom_towards(delta, Some(anchor_point));
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
                Ok(json!({
                    "at": [local.0, local.1],
                    "anchor": anchor,
                    "delta": delta,
                    "distance": self.workspace.camera.distance,
                    "action": "zoom",
                }))
            }
            "gameView" => {
                let cursor = self.game_graphic_cursor(local)?;
                self.pending_input
                    .push_back(InjectedInput::Cursor(cursor.0, cursor.1));
                self.pending_input
                    .push_back(InjectedInput::Scroll(delta as f64));
                Ok(json!({
                    "at": [local.0, local.1],
                    "anchor": anchor,
                    "delta": delta,
                    "cursor": [cursor.0, cursor.1],
                    "queued": 2,
                    "action": "scroll",
                }))
            }
            other => Err(format!("'{other}' is not a viewport panel")),
        }
    }

    /// 取 `from` / `to` 的对象形式；两者必须落在同一个视口（#643）。
    fn drag_point(
        &self,
        args: &serde_json::Value,
        key: &str,
    ) -> Result<(String, (f32, f32), &'static str), String> {
        let value = args
            .get(key)
            .filter(|value| value.is_object())
            .ok_or_else(|| format!("drag needs '{key}' as {{node,offset}} or {{x,y}}"))?;
        self.viewport_point(value)
    }

    /// `ui.drag`：在视口里拖一次（gizmo / 框选 / 轨道 / 玩法指针，见 #643）。
    fn ui_view_drag(&mut self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        let (from_tab, from, _) = self.drag_point(args, "from")?;
        let (to_tab, to, _) = self.drag_point(args, "to")?;
        if from_tab != to_tab {
            return Err(format!(
                "from/to must be in the same view ('{from_tab}' vs '{to_tab}')"
            ));
        }
        let steps = args
            .get("steps")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 120) as u32;
        match from_tab.as_str() {
            "sceneView" => self.drag_scene(from, to, steps, args),
            "gameView" => self.drag_game(from, to, steps),
            other => Err(format!("'{other}' is not a viewport panel")),
        }
    }

    fn drag_scene(
        &mut self,
        from: (f32, f32),
        to: (f32, f32),
        steps: u32,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let rect = self.scene_rect()?;
        for (name, point) in [("from", from), ("to", to)] {
            if point.0 < 0.0 || point.1 < 0.0 || point.0 > rect.width() || point.1 > rect.height() {
                return Err(format!(
                    "{name} ({:.1},{:.1}) is outside the scene view ({}x{})",
                    point.0,
                    point.1,
                    rect.width(),
                    rect.height()
                ));
            }
        }
        let frame = prism_editor_core::viewport_math::camera_frame(&self.workspace.camera);
        let orbit = args.get("mode").and_then(serde_json::Value::as_str) == Some("orbit")
            || args.get("orbit").and_then(serde_json::Value::as_bool) == Some(true);
        let ctrl = args.get("ctrl").and_then(serde_json::Value::as_bool) == Some(true);
        let Some(view) = self.scene_view.as_mut() else {
            return Err("scene view is not open".to_string());
        };
        if !orbit && view.begin_gizmo_drag(&mut self.workspace, &frame, rect, from) {
            let mut changed = false;
            let mut previous = from;
            for current in crate::viewport::drag_samples(from, to, steps) {
                changed |= view.gizmo_drag_step(
                    &mut self.workspace,
                    &frame,
                    rect,
                    current,
                    (current.0 - previous.0, current.1 - previous.1),
                    ctrl,
                );
                previous = current;
            }
            view.finish_gizmo_drag(&mut self.workspace);
            return Ok(json!({
                "from": [from.0, from.1],
                "to": [to.0, to.1],
                "anchor": "top-left",
                "steps": steps,
                "changed": changed,
                "action": "gizmo",
            }));
        }
        if orbit {
            self.workspace.camera.orbit(to.0 - from.0, to.1 - from.1);
            view.mark_dirty();
            return Ok(json!({
                "from": [from.0, from.1],
                "to": [to.0, to.1],
                "anchor": "top-left",
                "steps": steps,
                "action": "orbit",
            }));
        }
        let hits = prism_editor_core::viewport_math::box_select(
            prism_editor_core::viewport_math::local_box(from, to, (0.0, 0.0)),
            &frame,
            rect.width(),
            rect.height(),
            &crate::viewport::proxy_transforms(&self.workspace),
        );
        let nodes: Vec<String> = hits
            .iter()
            .map(|index| {
                crate::ui_tree::view_object_node(
                    "sceneView",
                    self.workspace.document().doc.entities[*index]
                        .id
                        .unwrap_or(*index as u32),
                )
            })
            .collect();
        self.workspace.select_many(&hits, false);
        Ok(json!({
            "from": [from.0, from.1],
            "to": [to.0, to.1],
            "anchor": "top-left",
            "steps": steps,
            "selected": nodes,
            "count": hits.len(),
            "action": "box-select",
        }))
    }

    fn drag_game(
        &mut self,
        from: (f32, f32),
        to: (f32, f32),
        steps: u32,
    ) -> Result<serde_json::Value, String> {
        let start = self.game_graphic_cursor(from)?;
        let end = self.game_graphic_cursor(to)?;
        self.pending_input
            .push_back(InjectedInput::Cursor(start.0, start.1));
        self.pending_input.push_back(InjectedInput::Mouse(
            prism_engine::input::MouseButton::Left,
            true,
        ));
        for cursor in crate::viewport::drag_samples(start, end, steps) {
            self.pending_input
                .push_back(InjectedInput::Cursor(cursor.0, cursor.1));
        }
        self.pending_input.push_back(InjectedInput::Mouse(
            prism_engine::input::MouseButton::Left,
            false,
        ));
        Ok(json!({
            "from": [from.0, from.1],
            "to": [to.0, to.1],
            "anchor": "top-left",
            "cursor": [[start.0, start.1], [end.0, end.1]],
            "steps": steps,
            "queued": steps + 3,
            "action": "game-pointer",
        }))
    }

    fn ui_click(&mut self, node: &str) -> Result<String, String> {
        if let Some(name) = node.strip_prefix("command:") {
            let command = crate::ui_tree::command_by_name(name)
                .ok_or_else(|| format!("unknown command '{name}'"))?;
            self.apply_command(command);
            return Ok(format!("command {name}"));
        }
        if let Some(rest) = node.strip_prefix("doc:") {
            let index = rest
                .parse::<usize>()
                .map_err(|_| format!("bad document index '{rest}'"))?;
            if index >= self.workspace.docs.len() {
                return Err(format!("document {index} does not exist"));
            }
            self.workspace.set_active(index);
            return Ok(format!("activate document {index}"));
        }
        if let Some(rest) = node.strip_prefix("entity:") {
            let id = rest
                .parse::<u32>()
                .map_err(|_| format!("bad entity id '{rest}'"))?;
            let index = self
                .workspace
                .document()
                .doc
                .index_of_id(id)
                .ok_or_else(|| format!("entity {id} not found"))?;
            self.workspace.select(index, false);
            return Ok(format!("select entity {id}"));
        }
        if let Some(path) = node.strip_prefix("asset:") {
            self.state.select_asset_only(path.to_owned());
            return Ok(format!("select asset {path}"));
        }
        if let Some(tab) = node.strip_prefix("panel:") {
            // 面板可能在某个 leaf 里（切到那个 tab），也可能关着（打开它）。
            let leaf = self
                .dock_leaves
                .iter()
                .find(|(_, _, tabs)| tabs.iter().any(|name| name == tab))
                .map(|(leaf, _, _)| leaf.clone());
            match leaf {
                Some(leaf) => self.workspace.activate_panel(&leaf, tab),
                None => self.workspace.toggle_panel(tab),
            }
            return Ok(format!("open panel {tab}"));
        }
        if node == "search:open" {
            self.palette.open(&self.workspace);
            return Ok("open search".to_string());
        }
        if let Some(rest) = node.strip_prefix("resolution:") {
            // Game 视图的预览分辨率（#463）：`match` / `fit:16x9` / `1920x1080`。
            let target = if rest == "match" {
                GameResolution::MatchWindow
            } else if let Some(ratio) = rest.strip_prefix("fit:") {
                let (width, height) = ratio
                    .split_once('x')
                    .ok_or_else(|| format!("bad aspect '{ratio}' (want fit:16x9)"))?;
                GameResolution::Aspect {
                    width: width
                        .parse()
                        .map_err(|_| format!("bad aspect width '{width}'"))?,
                    height: height
                        .parse()
                        .map_err(|_| format!("bad aspect height '{height}'"))?,
                }
            } else {
                let (width, height) = rest
                    .split_once('x')
                    .ok_or_else(|| format!("bad resolution '{rest}' (want 1920x1080)"))?;
                GameResolution::Fixed {
                    width: width
                        .parse()
                        .map_err(|_| format!("bad resolution width '{width}'"))?,
                    height: height
                        .parse()
                        .map_err(|_| format!("bad resolution height '{height}'"))?,
                }
            };
            self.ensure_view(ViewKind::Game);
            if let Some(view) = self.game_view.as_mut() {
                view.target = target;
                view.mark_dirty();
            }
            return Ok(format!("game resolution {}", target.label()));
        }
        Err(format!(
            "unsupported node '{node}' (command:… / panel:… / doc:… / entity:… / asset:… / dialog:… / search:open / resolution:<WxH>|fit:<WxH>|match)"
        ))
    }

    /// `dialog:<id>:<button>`：走和真实按钮完全相同的处理路径（#618）。
    fn ui_click_dialog(
        &mut self,
        ctx: &egui::Context,
        node: &str,
    ) -> Result<serde_json::Value, String> {
        let (id, action) = crate::ui_tree::parse_dialog_node(node)
            .ok_or_else(|| format!("bad dialog node '{node}'"))?;
        let action = action.ok_or_else(|| format!("'{node}' 是对话框，不是按钮"))?;
        let label = match (id, action) {
            ("delete-entities", "delete") => {
                panels::apply_pending_entity_delete(&mut self.workspace, &mut self.state);
                "delete entities"
            }
            ("delete-entities", "cancel") => {
                self.state.pending_entity_delete = None;
                "cancel delete entities"
            }
            ("delete-document", "delete") => {
                panels::apply_pending_doc_delete(&mut self.workspace, &mut self.state);
                "delete document"
            }
            ("delete-document", "cancel") => {
                self.state.pending_doc_delete = None;
                "cancel delete document"
            }
            ("delete-assets", "delete") => {
                panels::apply_pending_asset_delete(&mut self.workspace, &mut self.state);
                "delete assets"
            }
            ("delete-assets", "cancel") => {
                self.state.pending_asset_delete = None;
                "cancel delete assets"
            }
            ("save-conflict", "overwrite") => {
                let index = self
                    .state
                    .pending_save_conflict
                    .ok_or_else(|| "no save conflict".to_string())?;
                self.apply_save_conflict_choice(index, SaveConflictChoice::Overwrite);
                "overwrite"
            }
            ("save-conflict", "reload") => {
                let index = self
                    .state
                    .pending_save_conflict
                    .ok_or_else(|| "no save conflict".to_string())?;
                self.apply_save_conflict_choice(index, SaveConflictChoice::Reload);
                "reload"
            }
            ("save-conflict", "cancel") => {
                self.state.pending_save_conflict = None;
                "cancel save conflict"
            }
            ("close", "save") => {
                let target = self
                    .state
                    .pending_close
                    .ok_or_else(|| "no pending close".to_string())?;
                self.apply_close_decision(ctx, target, CloseDecision::Save);
                "save and close"
            }
            ("close", "discard") => {
                let target = self
                    .state
                    .pending_close
                    .ok_or_else(|| "no pending close".to_string())?;
                self.apply_close_decision(ctx, target, CloseDecision::Discard);
                "discard and close"
            }
            ("close", "cancel") => {
                self.state.pending_close = None;
                "cancel close"
            }
            ("confirm", "ok") => {
                self.accept_confirm();
                "confirm"
            }
            ("confirm", "cancel") => {
                self.state.confirm = None;
                "cancel confirm"
            }
            ("prompt", "ok") => {
                let Some(prompt) = self.state.prompt.clone() else {
                    return Err("no prompt".into());
                };
                self.commit_prompt(prompt.kind, prompt.value.trim().to_string())?;
                "prompt ok"
            }
            ("prompt", "cancel") => {
                self.state.prompt = None;
                "cancel prompt"
            }
            _ => return Err(format!("unknown dialog button '{node}'")),
        };
        Ok(json!({
            "dialog": id,
            "button": action,
            "action": label,
            "pending_dialog": self.dialogs().first().map(|dialog| dialog.node()),
        }))
    }

    /// 执行全局搜索里确认的那一条（选择实体 / 打开文档或资产 / 跑命令）。
    fn run_palette_action(&mut self, action: prism_editor_core::palette::PaletteAction) {
        use prism_editor_core::palette::PaletteAction;
        match action {
            PaletteAction::RunCommand(command) => self.apply_command(command),
            PaletteAction::OpenDocument(index) => {
                self.workspace.set_active(index);
                self.state.status = format!("opened {}", self.workspace.document().title());
            }
            PaletteAction::SelectEntity(index) => {
                let name = self.workspace.document().doc.entities[index]
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("entity {index}"));
                self.workspace.select(index, false);
                self.workspace.frame_selection();
                self.mark_scene_dirty();
                self.state.status = format!("selected {name}");
            }
            PaletteAction::OpenAsset(path) => {
                if path.ends_with(".scene.json") || path.ends_with(".prefab.json") {
                    if let Err(error) = self.workspace.open_project_file(&path) {
                        self.state.status = format!("open {path} failed: {error}");
                    } else {
                        self.state.status = format!("opened {path}");
                    }
                    return;
                }
                // 其它类型：在资产面板里选中，并把它的父目录都展开。
                if let Some((folder, _)) = path.rsplit_once('/') {
                    let mut prefix = String::new();
                    for segment in folder.split('/') {
                        if !prefix.is_empty() {
                            prefix.push('/');
                        }
                        prefix.push_str(segment);
                        self.workspace.expanded_assets.insert(prefix.clone());
                    }
                }
                self.state.select_asset_only(path.clone());
                self.state.status = format!("selected asset {path}");
            }
        }
    }

    /// `ui.set`：目前支持检查器字段 `field:<entity>:<component>:<path>`。
    fn ui_set(&mut self, node: &str, value: serde_json::Value) -> Result<String, String> {
        if let Some(("prompt", None)) = crate::ui_tree::parse_dialog_node(node) {
            let text = value
                .as_str()
                .ok_or_else(|| "dialog:prompt 需要字符串 value".to_string())?;
            let Some(prompt) = self.state.prompt.as_mut() else {
                return Err("no prompt is open".into());
            };
            prompt.value = text.to_string();
            return Ok("set prompt value".to_string());
        }
        let rest = node.strip_prefix("field:").ok_or_else(|| {
            format!("unsupported node '{node}' (field:<entity>:<component>:<path>)")
        })?;
        // 组件名本身可以带 `::`（`prism_engine::scene::DirectionalLight`），
        // 所以 entity 从左边切、path 从右边切（path 里没有冒号）。
        let (entity, rest) = rest
            .split_once(':')
            .ok_or_else(|| "field node needs an entity id".to_string())?;
        let entity = entity
            .parse::<u32>()
            .map_err(|_| "field node needs an entity id".to_string())?;
        let (component, path) = rest
            .rsplit_once(':')
            .ok_or_else(|| "field node needs <component>:<path>".to_string())?;
        let component = component.to_owned();
        let path = path.to_owned();
        // 先确认路径真的存在：`set_field` 找不到路径时是静默返回的，
        // agent 会误以为写成功。
        let current = self
            .workspace
            .document()
            .doc
            .index_of_id(entity)
            .and_then(|index| {
                let doc = &self.workspace.document().doc;
                let entity = doc.entities.get(index)?;
                if component == "Transform" {
                    Some(serde_json::to_value(&entity.transform).unwrap_or_default())
                } else {
                    entity.components.get(&component).cloned()
                }
            })
            .ok_or_else(|| format!("entity {entity} 没有组件 '{component}'"))?;
        if prism_editor_core::schema::get_at(&current, &path).is_none() {
            let mut available = Vec::new();
            crate::ui_tree::flatten_paths(&current, "", &mut |path, _| {
                available.push(path.to_string())
            });
            available.truncate(24);
            return Err(format!(
                "'{component}' 里没有字段 '{path}'；可用：{}",
                available.join(", ")
            ));
        }
        if component == "Transform" {
            // Transform 不在 `components` 里（是实体的结构字段），要走专用 setter
            // 才能进撤销栈；`Workspace::set_field` 对它是静默 no-op。
            let (field, axis) = split_axis(&path).ok_or_else(|| {
                format!("Transform 只支持 position/rotation/scale 的 [轴] 写法，收到 '{path}'")
            })?;
            let axes: Vec<(usize, f64)> = match value.as_array() {
                Some(items) => items
                    .iter()
                    .enumerate()
                    .filter_map(|(axis, item)| item.as_f64().map(|value| (axis, value)))
                    .collect(),
                None => {
                    let scalar = value
                        .as_f64()
                        .ok_or_else(|| format!("Transform.{field}[{axis}] 需要数字"))?;
                    vec![(axis, scalar)]
                }
            };
            for (axis, value) in axes {
                self.workspace
                    .set_transform_axis(entity, field, axis, value as f32);
            }
            return Ok(format!("set Transform.{path}"));
        }
        self.workspace.set_field(entity, &component, &path, value);
        Ok(format!("set {component}.{path}"))
    }

    /// 请求退出：有未保存改动时先弹确认，否则直接关窗口。
    ///
    /// `egui-ash` 的 `ExitSignal` 并没有接进事件循环（`run.rs` 里轮询它的那段是注释掉的），
    /// 唯一生效的路径是给根视口发 `Close`：winit 的 `CloseRequested` →
    /// `event_loop.exit()`。
    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.workspace.docs.iter().any(|document| document.dirty) {
            self.state.pending_close = Some(panels::CloseTarget::App);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 一条控制命令 → 回执 JSON（`None` = 稍后由 `pending_captures` 回复）。
    fn handle_command(
        &mut self,
        ctx: &egui::Context,
        command: &str,
        args: &serde_json::Value,
        reply: std::sync::mpsc::Sender<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        use serde_json::json;
        let direct = match command {
            "ping" => json!({
                "ok": true,
                "pid": std::process::id(),
                "frames": self.frame_count,
                "fps": self.game_view.as_ref().map(|view| view.fps).unwrap_or(0.0),
                "scene": self.workspace.document().path.clone(),
            }),
            "status" => json!({
                "ok": true,
                "pid": std::process::id(),
                "frames": self.frame_count,
                "fps": self.game_view.as_ref().map(|view| view.fps).unwrap_or(0.0),
                "playing": self.playing,
                "paused": self.paused,
                "project_module": self
                    .game_view
                    .as_ref()
                    .and_then(|view| view.host.project_module_status())
                    .map(|status| {
                        json!({
                            "available": status.available,
                            "loaded": status.loaded,
                            "building": status.building,
                        })
                    }),
                "scene": self.workspace.document().path.clone(),
                "dirty": self.workspace.document().dirty,
                "project_root": self
                    .project_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                "selection": self.workspace.selection.clone(),
                "status": self.state.status.clone(),
                "project_module": self.project_module_json(),
            }),
            "logs" | "log.dump" => {
                // Console 面板的同一份缓冲：MCP / 自动化没窗口可看，也能读到
                // 引擎日志与工程模块（target `project`）的日志和 panic 调用栈。
                let since = args
                    .get("since")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let limit = args
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(200) as usize;
                let lines: Vec<serde_json::Value> = self
                    .workspace
                    .log
                    .all()
                    .iter()
                    .filter(|line| line.seq >= since)
                    .take(limit)
                    .map(|line| {
                        json!({
                            "seq": line.seq,
                            "source": format!("{:?}", line.source),
                            "level": format!("{:?}", line.level),
                            "text": line.text,
                        })
                    })
                    .collect();
                let next = self
                    .workspace
                    .log
                    .all()
                    .back()
                    .map(|line| line.seq + 1)
                    .unwrap_or(since);
                json!({ "ok": true, "lines": lines, "next": next })
            }
            "input" | "input.send" => {
                // 驱动 Play 用的输入注入：key=space/w/a/s/d/...、mouse=left/right，
                // state=press/release（默认 press）。`click` 等价于 left 的按下+松开。
                let state = args
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("press");
                let pressed = match state {
                    "press" | "down" | "true" => true,
                    "release" | "up" | "false" => false,
                    other => {
                        return Some(json!({
                            "ok": false,
                            "error": format!("state 只能是 press / release，收到 '{other}'"),
                        }))
                    }
                };
                if args.get("click").and_then(serde_json::Value::as_bool) == Some(true) {
                    self.pending_input.push_back(InjectedInput::Mouse(
                        prism_engine::input::MouseButton::Left,
                        true,
                    ));
                    self.pending_input.push_back(InjectedInput::Mouse(
                        prism_engine::input::MouseButton::Left,
                        false,
                    ));
                    return Some(json!({
                        "ok": true,
                        "queued": 2,
                        "pending": self.pending_input.len(),
                    }));
                }
                if let Some(button) = args.get("mouse").and_then(serde_json::Value::as_str) {
                    let button = parse_mouse(button);
                    let Some(button) = button else {
                        return Some(
                            json!({ "ok": false, "error": "mouse 只能是 left / right / middle" }),
                        );
                    };
                    self.pending_input
                        .push_back(InjectedInput::Mouse(button, pressed));
                } else if let Some(key) = args.get("key").and_then(serde_json::Value::as_str) {
                    let Some(key) = parse_key(key) else {
                        return Some(json!({
                            "ok": false,
                            "error": format!("不认识的按键 '{key}'（可用：w/a/s/d/q/e/space/shift/ctrl/alt/escape/tab/enter/方向键/0-9）"),
                        }));
                    };
                    self.pending_input
                        .push_back(InjectedInput::Key(key, pressed));
                } else {
                    return Some(json!({ "ok": false, "error": "需要 key / mouse / click 之一" }));
                }
                json!({ "ok": true, "queued": 1, "pending": self.pending_input.len() })
            }
            "play" | "play.start" => {
                self.apply_command(Command::Play);
                json!({ "ok": true, "playing": self.playing })
            }
            "stop" | "play.stop" => {
                self.apply_command(Command::Stop);
                json!({ "ok": true, "playing": self.playing })
            }
            "pause" => {
                self.apply_command(Command::Pause);
                json!({ "ok": true, "paused": self.paused, "playing": self.playing })
            }
            "capture" => {
                let target = args
                    .get("target")
                    .and_then(serde_json::Value::as_str)
                    .and_then(crate::screenshot::CaptureTarget::parse)
                    .unwrap_or(crate::screenshot::CaptureTarget::Window);
                let size = args
                    .get("size")
                    .and_then(serde_json::Value::as_str)
                    .and_then(crate::screenshot::parse_size);
                let path = match args.get("path").and_then(serde_json::Value::as_str) {
                    Some(value) => std::path::PathBuf::from(value),
                    None => crate::screenshot::default_path(&self.capture_dir(), target),
                };
                // **永远延后一帧**：整窗截图走 `PrintWindow`，它对目标窗口
                // `SendMessage`，在 UI 线程里同步调用会自己等自己（死锁）。
                // 前几帧窗口还没合成，PS 抓下来是全白，所以至少等到 STABLE_FRAME。
                const STABLE_FRAME: u64 = 3;
                let ready_at = (self.frame_count + 1).max(STABLE_FRAME);
                self.pending_captures.push(PendingCapture {
                    target,
                    path,
                    size,
                    ready_at,
                    reply,
                });
                ctx.request_repaint();
                return None;
            }
            "ui.snapshot" | "ui_snapshot" => {
                let logs: Vec<String> = self
                    .workspace
                    .log
                    .visible_lines()
                    .iter()
                    .map(|line| format!("[{:?}] {}", line.level, line.text))
                    .collect();
                let viewports = self.viewport_objects();
                let dialogs = self.dialogs();
                let input = crate::ui_tree::SnapshotInput {
                    workspace: &self.workspace,
                    playing: self.playing,
                    paused: self.paused,
                    status: &self.state.status,
                    frames: self.frame_count,
                    fps: self.game_view.as_ref().map(|view| view.fps).unwrap_or(0.0),
                    gizmo_tool: self.scene_view.as_ref().map(|view| view.tool.label()),
                    // Keep the control protocol locale-independent. The toolbar may use
                    // localized labels, but MCP clients need stable enum-like values.
                    gizmo_space: match self.workspace.gizmo_space {
                        prism_editor_core::viewport_math::GizmoSpace::World => "World",
                        prism_editor_core::viewport_math::GizmoSpace::Local => "Local",
                    },
                    rects: &self.panel_rects,
                    leaves: &self.dock_leaves,
                    logs: &logs,
                    viewports: &viewports,
                    dialogs: &dialogs,
                };
                crate::ui_tree::snapshot(&input)
            }
            "ui.click" | "ui_click" => {
                let node = args
                    .get("node")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let offset = args.get("offset").and_then(parse_offset);
                let clicked = match crate::ui_tree::parse_view_object_node(node) {
                    Some(tab) => self.ui_click_view_object(tab, node, offset),
                    None if crate::ui_tree::parse_dialog_node(node).is_some() => {
                        self.ui_click_dialog(ctx, node)
                    }
                    None => match offset {
                        Some(offset) => self.ui_click_panel_offset(node, offset),
                        None => self
                            .ui_click(node)
                            .map(|message| json!({ "node": node, "action": message })),
                    },
                };
                match clicked {
                    Ok(mut value) => {
                        value["ok"] = json!(true);
                        // #618：命令可能只是挂起了确认框；回执里直接给出 pending，
                        // 脚本不用再猜「为什么 ok 了却没生效」。
                        value["pending_dialog"] = self
                            .dialogs()
                            .first()
                            .map(|dialog| json!(dialog.node()))
                            .unwrap_or(serde_json::Value::Null);
                        value
                    }
                    Err(error) => json!({ "ok": false, "node": node, "error": error }),
                }
            }
            "ui.set" | "ui_set" => {
                let node = args
                    .get("node")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let value = args
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                match self.ui_set(node, value) {
                    Ok(message) => json!({ "ok": true, "node": node, "action": message }),
                    Err(error) => json!({ "ok": false, "node": node, "error": error }),
                }
            }
            "ui.move" | "ui_move" | "ui.hover" => match self.ui_view_move(args) {
                Ok(mut value) => {
                    value["ok"] = json!(true);
                    value
                }
                Err(error) => json!({ "ok": false, "error": error }),
            },
            "ui.wheel" | "ui_wheel" => match self.ui_view_wheel(args) {
                Ok(mut value) => {
                    value["ok"] = json!(true);
                    value
                }
                Err(error) => json!({ "ok": false, "error": error }),
            },
            "ui.drag" | "ui_drag" => match self.ui_view_drag(args) {
                Ok(mut value) => {
                    value["ok"] = json!(true);
                    value
                }
                Err(error) => json!({ "ok": false, "error": error }),
            },
            "quit" => {
                // 自动化路径（MCP / 脚本）要求确定性地退出，跳过未保存确认。
                self.force_quit = true;
                self.request_quit(ctx);
                json!({ "ok": true })
            }
            other => json!({ "ok": false, "error": format!("unknown cmd: {other}") }),
        };
        Some(direct)
    }

    /// `target/editor-egui/captures/`（state 文件所在目录下）。
    fn capture_dir(&self) -> PathBuf {
        self.state_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 统一的截图入口：`window` / `scene` / `game`。
    fn capture(
        &mut self,
        target: crate::screenshot::CaptureTarget,
        path: &Path,
        size: Option<[u32; 2]>,
    ) -> anyhow::Result<PathBuf> {
        match target {
            crate::screenshot::CaptureTarget::Window => {
                let hwnd = self.hwnd.ok_or_else(|| {
                    anyhow::anyhow!("window capture unavailable on this platform")
                })?;
                let path = crate::screenshot::save_window_png(hwnd, path, size)?;
                self.state.status = format!("screenshot: {}", path.display());
                self.workspace.log(
                    prism_editor_core::LogSource::Editor,
                    prism_editor_core::LogLevel::Info,
                    format!("screenshot saved: {}", path.display()),
                );
                Ok(path)
            }
            crate::screenshot::CaptureTarget::Scene | crate::screenshot::CaptureTarget::Game => {
                let bridge = ImageBridge {
                    registry: self.bridge.registry.clone(),
                    sampler: self.bridge.sampler,
                };
                let Some(view) = self.view_for(target) else {
                    anyhow::bail!("view not available (GPU init failed?)");
                };
                view.host.request_capture(path.to_path_buf());
                view.mark_dirty();
                let render_error = view.render_frame(&bridge);
                if view.error.is_some() {
                    anyhow::bail!(
                        "view render failed: {}",
                        view.error.clone().unwrap_or_default()
                    );
                }
                let _ = render_error;
                if !path.exists() {
                    anyhow::bail!("viewport capture wrote nothing (no frame?)");
                }
                self.state.status = format!("screenshot: {}", path.display());
                self.workspace.log(
                    prism_editor_core::LogSource::Editor,
                    prism_editor_core::LogLevel::Info,
                    format!("viewport screenshot: {}", path.display()),
                );
                Ok(path.to_path_buf())
            }
        }
    }

    fn view_for(&mut self, target: crate::screenshot::CaptureTarget) -> Option<&mut ViewportState> {
        let kind = match target {
            crate::screenshot::CaptureTarget::Scene => ViewKind::Scene,
            _ => ViewKind::Game,
        };
        if !self.ensure_view(kind) {
            return None;
        }
        match kind {
            ViewKind::Scene => self.scene_view.as_mut(),
            ViewKind::Game => self.game_view.as_mut(),
        }
    }

    /// 菜单命令：抓整窗到 `<state 目录>/captures/`。
    ///
    /// 走和 MCP 一样的延后 + 工作线程路径（`PrintWindow` 不能在 UI 线程同步调用）。
    fn capture_window_now(&mut self) {
        let path = crate::screenshot::default_path(
            &self.capture_dir(),
            crate::screenshot::CaptureTarget::Window,
        );
        let (reply, _unused) = std::sync::mpsc::channel();
        self.pending_captures.push(PendingCapture {
            target: crate::screenshot::CaptureTarget::Window,
            path: path.clone(),
            size: None,
            ready_at: self.frame_count + 1,
            reply,
        });
        self.state.status = format!("screenshot: {}", path.display());
    }

    fn menu_do(&mut self, ui: &mut egui::Ui, label: &str, action: impl FnOnce(&mut Self)) {
        if ui.button(label).clicked() {
            action(self);
            ui.close();
        }
    }

    fn open_prompt(
        &mut self,
        kind: panels::PromptKind,
        title: &str,
        hint: &str,
        value: impl Into<String>,
    ) {
        self.state.prompt = Some(panels::Prompt::new(kind, title, hint, value));
    }

    /// 保存所有脏文档。返回保存数量；任何一份失败就返回错误（调用方决定是否继续，
    /// 构建路径必须因此中止，否则会打包旧内容）。
    fn save_all_documents(&mut self) -> Result<usize, String> {
        let count = self.workspace.docs.len();
        let mut failures = Vec::new();
        let mut saved = 0usize;
        for index in 0..count {
            if self.workspace.docs[index].dirty {
                match self.workspace.save_document(index) {
                    Ok(_) => saved += 1,
                    Err(error) if self.workspace.save_conflict(index).is_some() => {
                        // 先弹冲突框，别把「别人的改动」直接覆盖掉（#452）。
                        self.state.pending_save_conflict.get_or_insert(index);
                        failures.push(error.to_string());
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }
        if failures.is_empty() {
            self.state.status = if saved == 0 {
                "nothing to save".to_string()
            } else {
                format!("saved {saved} document(s)")
            };
            Ok(saved)
        } else {
            let message = failures.join("; ");
            self.state.status = format!("save failed: {message}");
            Err(message)
        }
    }

    /// 资产面板的 Delete：作用于整个选中集合，按首选项决定是否先确认。
    fn request_asset_delete(&mut self) {
        if self.state.asset_selection.is_empty() {
            self.state.status = "no asset selected".to_string();
            return;
        }
        let paths = self.state.asset_selection.clone();
        if self.preferences.confirm_delete {
            panels::request_delete_assets(&mut self.workspace, &mut self.state, paths);
        } else {
            let mut deleted = 0usize;
            for path in &paths {
                panels::delete_asset_now(&mut self.workspace, &mut self.state, path);
                deleted += 1;
            }
            self.state.status = format!("deleted {deleted} asset(s)");
        }
    }

    /// 资产面板的 Ctrl+D：批量复制整个选中集合。
    fn duplicate_selected_asset(&mut self) {
        if self.state.asset_selection.is_empty() {
            self.state.status = "no asset selected".to_string();
            return;
        }
        let mut created = Vec::new();
        let mut failures = Vec::new();
        for path in self.state.asset_selection.clone() {
            match self.workspace.duplicate_asset(&path) {
                Ok(copy) => created.push(copy),
                Err(error) => failures.push(format!("{path}: {error}")),
            }
        }
        self.state.status = if failures.is_empty() {
            format!("duplicated {} asset(s)", created.len())
        } else {
            format!("duplicate failed: {}", failures.join("; "))
        };
        if let Some(last) = created.last() {
            self.state.select_asset_only(last.clone());
        }
    }

    /// 资产面板的 F2。
    /// F2 / 右键 Rename：在资产行上就地改名（不再弹模态框）。
    fn rename_selected_asset_prompt(&mut self) {
        let Some(path) = self.state.asset_primary().cloned() else {
            self.state.status = "no asset selected".to_string();
            return;
        };
        if self.state.asset_selection.len() > 1 {
            self.state.status = "renaming one asset at a time".to_string();
        }
        if path.ends_with(".scene.json") || path.ends_with(".prefab.json") {
            self.state.status =
                "scenes and prefabs rename through Save As / the document panel".to_string();
            return;
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.state.asset_rename = Some((path, name));
        self.state.asset_rename_focus = true;
    }

    fn create_empty_parent(&mut self) {
        let parents: Vec<usize> = self.workspace.selection.clone();
        if parents.is_empty() {
            self.create_empty_child();
            return;
        }
        let before = self.workspace.document().doc.entities.len();
        self.workspace.add_entity("");
        let created = self
            .workspace
            .document()
            .doc
            .entities
            .len()
            .saturating_sub(1);
        if created >= before {
            for index in parents {
                self.workspace.reparent(index, Some(created), None);
            }
            self.workspace.select(created, false);
        }
    }

    fn invert_selection(&mut self) {
        let total = self.workspace.document().doc.entities.len();
        let inverted: Vec<usize> = (0..total)
            .filter(|index| !self.workspace.selection.contains(index))
            .collect();
        self.workspace.select_many(&inverted, false);
    }

    fn select_children_of_selection(&mut self) {
        let indices = self.workspace.selection.clone();
        let mut children = Vec::new();
        for index in indices {
            children.extend(self.workspace.document().doc.descendants(index));
        }
        if !children.is_empty() {
            self.workspace.select_many(&children, false);
        }
    }

    fn select_prefab_root(&mut self) {
        let Some(mut index) = self.workspace.primary_selection() else {
            return;
        };
        let doc = &self.workspace.document().doc;
        let Some(link) = doc.entities[index]
            .prefab_link()
            .map(|link| link.to_string())
        else {
            self.state.status = "selection is not a prefab instance".to_string();
            return;
        };
        while let Some(parent) = doc.entities[index].parent {
            let Some(parent_index) = doc.index_of_id(parent) else {
                break;
            };
            if doc.entities[parent_index].prefab_link() != Some(link.as_str()) {
                break;
            }
            index = parent_index;
        }
        self.workspace.select(index, false);
    }

    fn cycle_selection(&mut self, delta: i32) {
        let total = self.workspace.document().doc.entities.len();
        if total == 0 {
            return;
        }
        let current = self.workspace.primary_selection().unwrap_or(0) as i64;
        let next = ((current + delta as i64).rem_euclid(total as i64)) as usize;
        self.workspace.select(next, false);
    }

    fn copy_transform(&mut self, field: &str) {
        let Some(index) = self.workspace.primary_selection() else {
            return;
        };
        let transform = self.workspace.document().doc.entities[index].transform;
        let values = match field {
            "position" => transform.position.to_vec(),
            "rotation" => transform.rotation.to_vec(),
            "scale" => transform.scale.to_vec(),
            _ => return,
        };
        self.state.transform_clip = Some((field.to_string(), values));
        self.state.status = format!("copied {field}");
    }

    fn paste_transform(&mut self, field: &str) {
        let Some((clipboard_field, values)) = self.state.transform_clip.clone() else {
            self.state.status = "transform clipboard is empty".to_string();
            return;
        };
        if clipboard_field != field {
            self.state.status =
                format!("clipboard holds {clipboard_field}, not {field} — copy it first");
            return;
        }
        let ids: Vec<u32> = self
            .workspace
            .selection
            .iter()
            .filter_map(|index| self.workspace.document().doc.id_of_index(*index))
            .collect();
        if ids.is_empty() {
            return;
        }
        for entity in &ids {
            for (axis, value) in values.iter().enumerate() {
                self.workspace
                    .set_transform_axis(*entity, field, axis, *value);
            }
        }
        self.state.status = format!("pasted {field} to {} entities", ids.len());
    }

    fn find_references(&mut self, path: &str, project_wide: bool) {
        let mut hits = Vec::new();
        let docs = if project_wide {
            self.workspace.docs.len()
        } else {
            self.workspace.active_index() + 1
        };
        let start = if project_wide {
            0
        } else {
            self.workspace.active_index()
        };
        for index in start..docs {
            let Some(doc) = self.workspace.docs.get(index) else {
                continue;
            };
            for (entity, component, field) in doc.doc.field_references(path) {
                let name = doc
                    .doc
                    .entities
                    .get(entity)
                    .map(|entity| entity.display_name())
                    .unwrap_or_else(|| format!("#{entity}"));
                hits.push(format!("{}: {name} · {component}.{field}", doc.title()));
            }
        }
        if hits.is_empty() {
            self.state.status = format!("no references to {path}");
        } else {
            self.state.status = format!("{} reference(s) to {path}", hits.len());
        }
        for hit in hits {
            self.workspace.log(
                prism_editor_core::LogSource::Editor,
                prism_editor_core::LogLevel::Info,
                hit,
            );
        }
    }

    fn create_empty_child(&mut self) {
        let parent = self.workspace.primary_selection();
        let before = self.workspace.document().doc.entities.len();
        self.workspace.add_entity("");
        let created = self
            .workspace
            .document()
            .doc
            .entities
            .len()
            .saturating_sub(1);
        if created >= before {
            self.workspace.reparent(created, parent, None);
        }
    }

    // -----------------------------------------------------------------------
    // 用户项目模块（Game View 的 Play）
    // -----------------------------------------------------------------------

    fn project_module_status(&self) -> Option<crate::project_module::ProjectModuleStatus> {
        self.game_view
            .as_ref()
            .and_then(|view| view.host.project_module_status())
    }

    fn project_module_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self.project_module_status() {
            Some(status) => json!({
                "available": status.available,
                "loaded": status.loaded,
                "building": status.building,
                "auto_refresh": status.auto_refresh,
                "state": status.state.as_str(),
                "dirty": status.dirty,
                "problems": status.problems,
                "manifest": status.manifest.as_ref().map(|path| path.display().to_string()),
                "error": status.error,
            }),
            None => serde_json::Value::Null,
        }
    }

    /// 每帧把后台编译结果换进 Game view，并推进模块重载的 debounce。
    fn poll_project_module(&mut self) {
        let playing = self.playing;
        let events = if let Some(view) = self.game_view.as_mut() {
            view.host.poll_project_module(playing);
            view.host.drain_project_events()
        } else {
            Vec::new()
        };
        for event in events {
            self.workspace.log(
                prism_editor_core::LogSource::System,
                event.level,
                event.text,
            );
        }
    }

    /// Ctrl+R / File → Compile Project Module。
    fn refresh_project_module(&mut self) {
        let available = if let Some(view) = self.game_view.as_mut() {
            let available = view
                .host
                .project_module_status()
                .is_some_and(|status| status.available);
            if available {
                view.host.refresh_project_module();
            }
            available
        } else {
            false
        };
        if available {
            self.workspace.log(
                prism_editor_core::LogSource::System,
                prism_editor_core::LogLevel::Info,
                "project module: compile requested",
            );
            self.state.status = "compiling project module…".to_string();
        } else {
            self.state.status = "project has no preview-module (see Console)".to_string();
        }
    }

    fn project_auto_refresh(&self) -> bool {
        self.game_view
            .as_ref()
            .map(|view| view.host.project_auto_refresh())
            .unwrap_or(true)
    }

    fn set_project_auto_refresh(&mut self, enabled: bool) {
        if let Some(view) = self.game_view.as_mut() {
            view.host.set_project_auto_refresh(enabled);
        }
        self.workspace.log(
            prism_editor_core::LogSource::System,
            prism_editor_core::LogLevel::Info,
            format!(
                "project module: auto refresh {}",
                if enabled { "on" } else { "off" }
            ),
        );
    }

    fn build_job_running(&self) -> bool {
        self.build
            .as_ref()
            .map(|job| job.running())
            .unwrap_or(false)
    }

    /// 中断正在跑的构建（`File → Cancel Build` / 状态栏的 Cancel）。
    fn cancel_build(&mut self) {
        let Some(job) = self.build.as_ref() else {
            return;
        };
        job.cancel();
        let label = job.label().to_string();
        self.state.status = format!("cancelling build: {label}");
        self.workspace.log(
            prism_editor_core::LogSource::System,
            prism_editor_core::LogLevel::Warn,
            format!("build cancel requested: {label}"),
        );
    }

    fn start_build(&mut self, kind: crate::build::BuildKind) {
        let Some(root) = self.project_root.clone() else {
            self.state.status = "no project root — cannot build".to_string();
            return;
        };
        // 构建读的是磁盘上的作者文档：未保存的改动必须先落盘，否则打出来的是旧内容。
        if let Err(error) = self.save_all_documents() {
            self.state.status = format!("build aborted: {error}");
            self.workspace.log(
                prism_editor_core::LogSource::System,
                prism_editor_core::LogLevel::Error,
                format!("build aborted: save failed ({error})"),
            );
            return;
        }
        let label = kind.label();
        self.workspace.log(
            prism_editor_core::LogSource::System,
            prism_editor_core::LogLevel::Info,
            format!("build started: {label}"),
        );
        if kind.is_player_build() {
            // profile / output_dir 都来自 `assets/settings.ini` 的 `[build]`；
            // 先把生效值写进 Console，改错路径时一眼就能看出来。
            let settings = prism_editor_core::settings::ProjectSettings::read(&root)
                .unwrap_or_else(|error| {
                    self.workspace.log(
                        prism_editor_core::LogSource::System,
                        prism_editor_core::LogLevel::Warn,
                        format!("cannot read build settings ({error}); using defaults"),
                    );
                    Default::default()
                });
            let plan = prism_editor_core::build::plan_build(&root, &settings);
            self.workspace.log(
                prism_editor_core::LogSource::System,
                prism_editor_core::LogLevel::Info,
                format!(
                    "build plan: profile={} output_dir={}",
                    settings.build.profile,
                    plan.output_dir.display()
                ),
            );
        }
        self.build_diagnostics.clear();
        self.build = Some(crate::build::BuildJob::spawn(kind, &root));
    }

    fn cook_active_scene(&mut self) {
        let Some(root) = self.project_root.clone() else {
            self.state.status = "no project root — cannot cook".to_string();
            return;
        };
        // 先把未保存的文档落盘，再让 cooker 读磁盘上的作者文档。
        let _ = self.workspace.save_active_if_dirty();
        let Some(path) = self.workspace.document().path.clone() else {
            self.state.status = "current document has no path".to_string();
            return;
        };
        self.start_build(crate::build::BuildKind::Cook(root.join(&path)));
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        // Web 的 `.menu-trigger`：无底色、无边框，hover 是 `menu-hover`，
        // 展开时 `menu-active`。egui 默认给 MenuButton 铺一块立体按钮底，
        // 那是 imGui 味的主要来源之一。
        let colors = self.workspace.colors();
        let text_soft = theme::color32(&colors, prism_editor_core::ThemeToken::TextSoft);
        let text_strong = theme::color32(&colors, prism_editor_core::ThemeToken::TextStrong);
        let menu_hover = theme::color32(&colors, prism_editor_core::ThemeToken::MenuHover);
        let menu_active = theme::color32(&colors, prism_editor_core::ThemeToken::MenuActive);
        let style = ui.style_mut();
        style.spacing.button_padding = egui::vec2(7.0, 1.0);
        let widgets = &mut style.visuals.widgets;
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text_soft);
        widgets.hovered.weak_bg_fill = menu_hover;
        widgets.hovered.bg_fill = menu_hover;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text_strong);
        widgets.open.weak_bg_fill = menu_active;
        widgets.open.bg_fill = menu_active;
        widgets.open.bg_stroke = Stroke::NONE;
        widgets.open.fg_stroke = Stroke::new(1.0_f32, text_strong);

        egui::MenuBar::new().ui(ui, |ui| {
            // ---- File ----
            ui.menu_button("File", |ui| {
                self.menu_item(ui, "New Scene", "Ctrl+N", Command::NewScene);
                let scene_templates: Vec<(String, String)> = self
                    .workspace
                    .templates
                    .iter()
                    .filter(|template| template.kind == prism_editor_core::DocKind::Scene)
                    .map(|template| (template.id.clone(), template.label.clone()))
                    .collect();
                if !scene_templates.is_empty() {
                    ui.menu_button("New Scene From Template", |ui| {
                        for (id, label) in &scene_templates {
                            if ui.button(label).clicked() {
                                self.workspace
                                    .new_document(prism_editor_core::DocKind::Scene, Some(id));
                                self.doc_hash = 0;
                                ui.close();
                            }
                        }
                    });
                }
                self.menu_item(ui, "New Prefab", "", Command::NewPrefab);
                ui.separator();
                self.menu_item(ui, "Open Scene…", "Ctrl+O", Command::OpenDocument);
                let docs: Vec<(usize, String, bool)> = self
                    .workspace
                    .docs
                    .iter()
                    .enumerate()
                    .map(|(index, doc)| {
                        (index, doc.title(), index == self.workspace.active_index())
                    })
                    .collect();
                ui.menu_button("Recent Scenes", |ui| {
                    for (index, title, active) in &docs {
                        if ui.selectable_label(*active, title).clicked() {
                            self.workspace.set_active(*index);
                            self.doc_hash = 0;
                            ui.close();
                        }
                    }
                });
                self.menu_item(ui, "Save", "Ctrl+S", Command::Save);
                self.menu_item(ui, "Save As…", "Ctrl+Shift+S", Command::SaveAs);
                self.menu_do(ui, "Save Project", |app| {
                    let _ = app.save_all_documents();
                });
                self.menu_item(ui, "Reload Scene", "", Command::ReloadDocument);
                ui.separator();
                self.menu_item(ui, "Build Settings…", "", Command::ToggleBuild);
                if ui
                    .add_enabled(
                        !self.build_job_running(),
                        egui::Button::new("Build Scene Pack"),
                    )
                    .clicked()
                {
                    self.start_build(crate::build::BuildKind::ScenePack);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        !self.build_job_running(),
                        egui::Button::new("Cook Active Scene"),
                    )
                    .clicked()
                {
                    self.cook_active_scene();
                    ui.close();
                }
                if ui
                    .add_enabled(!self.build_job_running(), egui::Button::new("Build Player"))
                    .clicked()
                {
                    self.start_build(crate::build::BuildKind::Player);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        !self.build_job_running(),
                        egui::Button::new("Build And Run  (Ctrl+B)"),
                    )
                    .clicked()
                {
                    self.start_build(crate::build::BuildKind::PlayerRun);
                    ui.close();
                }
                let module_available = self
                    .project_module_status()
                    .is_some_and(|status| status.available);
                if ui
                    .add_enabled(
                        module_available,
                        egui::Button::new("Compile Project Module  (Ctrl+R)"),
                    )
                    .clicked()
                {
                    self.refresh_project_module();
                    ui.close();
                }
                let mut auto_refresh = self.project_auto_refresh();
                if ui
                    .add_enabled(
                        module_available,
                        egui::Checkbox::new(&mut auto_refresh, "Auto Refresh Project Module"),
                    )
                    .changed()
                {
                    self.set_project_auto_refresh(auto_refresh);
                }
                if ui
                    .add_enabled(!self.build_job_running(), egui::Button::new("Clean Build"))
                    .clicked()
                {
                    // 引擎的 Clean Build 只清构建缓存 / 输出，不动引擎编译产物。
                    self.state.confirm = Some(panels::Confirm::new(
                        panels::ConfirmKind::CleanPlayer,
                        "Clean Build",
                        "清理玩家工程（projects/prisma）的编译产物，下次 Build 会全量重编。\n\
                         不会动仓库根的引擎 target。",
                        "Clean Player",
                    ));
                    ui.close();
                }
                if ui
                    .add_enabled(
                        !self.build_job_running(),
                        egui::Button::new("Clean Engine Cache…"),
                    )
                    .clicked()
                {
                    self.state.confirm = Some(panels::Confirm::new(
                        panels::ConfirmKind::CleanEngine,
                        "Clean Engine Cache",
                        "清理仓库根 workspace 的 target/（引擎 + 全部 crate 的编译产物，\n\
                         通常几个 GB，重编需要十几分钟）。",
                        "Clean Engine",
                    ));
                    ui.close();
                }
                if self.build_job_running() && ui.button("Cancel Build").clicked() {
                    self.cancel_build();
                    ui.close();
                }
                ui.separator();
                let quit_ctx = ui.ctx().clone();
                self.menu_do(ui, "Exit", |app| app.request_quit(&quit_ctx));
            });

            // ---- Edit ----
            ui.menu_button("Edit", |ui| {
                self.menu_do(ui, "Search… (Ctrl+K)", |app| {
                    app.palette.open(&app.workspace)
                });
                ui.separator();
                self.menu_item(ui, "Undo", "Ctrl+Z", Command::Undo);
                self.menu_item(ui, "Redo", "Ctrl+Y", Command::Redo);
                ui.separator();
                self.menu_item(ui, "Cut", "Ctrl+X", Command::CutEntity);
                self.menu_item(ui, "Copy", "Ctrl+C", Command::CopyEntity);
                self.menu_item(ui, "Paste", "Ctrl+V", Command::PasteEntity);
                self.menu_item(ui, "Duplicate", "Ctrl+D", Command::DuplicateEntity);
                self.menu_item(ui, "Delete", "Del", Command::DeleteSelection);
                ui.separator();
                self.menu_item(ui, "Select All", "Ctrl+A", Command::SelectAll);
                self.menu_do(ui, "Deselect All", |app| app.workspace.clear_selection());
                self.menu_do(ui, "Invert Selection", |app| app.invert_selection());
                self.menu_do(ui, "Select Children", |app| {
                    app.select_children_of_selection()
                });
                self.menu_do(ui, "Select Prefab Root", |app| app.select_prefab_root());
                ui.separator();
                self.menu_do(ui, "Next Selection", |app| app.cycle_selection(1));
                self.menu_do(ui, "Previous Selection", |app| app.cycle_selection(-1));
                ui.separator();
                ui.menu_button("Copy Transform", |ui| {
                    for field in ["position", "rotation", "scale"] {
                        if ui.button(field).clicked() {
                            self.copy_transform(field);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Paste Transform", |ui| {
                    for field in ["position", "rotation", "scale"] {
                        if ui.button(field).clicked() {
                            self.paste_transform(field);
                            ui.close();
                        }
                    }
                });
                ui.separator();
                self.menu_item(
                    ui,
                    "Paste Component",
                    "Ctrl+Shift+V",
                    Command::PasteComponent,
                );
                self.menu_item(
                    ui,
                    "Clear Component Clipboard",
                    "",
                    Command::ClearComponentClipboard,
                );
                ui.separator();
                self.menu_item(ui, "Play", "Ctrl+P", Command::Play);
                self.menu_item(ui, "Pause", "Ctrl+Shift+P", Command::Pause);
                self.menu_item(ui, "Step", "Ctrl+Alt+P", Command::Step);
                ui.separator();
                self.menu_item(ui, "Project Settings…", "", Command::ProjectSettings);
                self.menu_item(ui, "Preferences…", "", Command::Preferences);
                self.menu_item(ui, "Theme…", "", Command::ThemePanel);
            });

            // ---- Assets ----
            ui.menu_button("Assets", |ui| {
                ui.menu_button("Create", |ui| {
                    self.menu_item(ui, "Scene", "", Command::NewScene);
                    self.menu_item(ui, "Prefab", "", Command::NewPrefab);
                    let can_variant = self.has_prefab_instance();
                    if ui
                        .add_enabled(can_variant, egui::Button::new("Prefab Variant…"))
                        .clicked()
                    {
                        self.open_prompt(
                            panels::PromptKind::PrefabVariant,
                            "Create Prefab Variant",
                            "变体文件名（不含目录，写到 assets/prefabs/）",
                            "Variant",
                        );
                        ui.close();
                    }
                    self.menu_do(ui, "Folder…", |app| {
                        app.open_prompt(
                            panels::PromptKind::NewFolder,
                            "New Folder",
                            "assets/ 下的相对目录，例如 prefabs/enemies",
                            "assets/",
                        )
                    });
                });
                let selected = self.state.asset_primary().cloned();
                if let Some(path) = selected.as_deref() {
                    self.menu_do(ui, "Open", |app| {
                        if let Err(error) = app.workspace.open_project_file(path) {
                            app.state.status = format!("open {path} failed: {error}");
                        }
                    });
                    let selected_path = path.to_string();
                    self.menu_do(ui, "Show in Explorer", |app| {
                        app.state.status =
                            match panels::reveal_asset(&app.workspace, &selected_path) {
                                Ok(()) => format!("revealed {selected_path}"),
                                Err(error) => error,
                            };
                    });
                    let ctx_copy = ui.ctx().clone();
                    let copy_path = selected_path.clone();
                    self.menu_do(ui, "Copy Path", move |app| {
                        ctx_copy.copy_text(copy_path.clone());
                        app.state.status = format!("copied {copy_path}");
                    });
                    self.menu_do(ui, "Duplicate", |app| {
                        match app.workspace.duplicate_asset(path) {
                            Ok(copy) => app.state.status = format!("duplicated → {copy}"),
                            Err(error) => app.state.status = format!("duplicate failed: {error}"),
                        }
                    });
                    self.menu_do(ui, "Delete", |app| match app.workspace.delete_asset(path) {
                        Ok(()) => {
                            app.state.forget_asset(path);
                            app.state.status = format!("deleted {path}");
                        }
                        Err(error) => app.state.status = format!("delete failed: {error}"),
                    });
                    if path.ends_with(".prefab.json") {
                        self.menu_do(ui, "Instantiate In Scene", |app| {
                            match app.workspace.instantiate_prefab_asset(path, None) {
                                Ok(count) => {
                                    app.state.status = format!("instantiated {count} entities");
                                    app.doc_hash = 0;
                                }
                                Err(error) => {
                                    app.state.status = format!("instantiate failed: {error}")
                                }
                            }
                        });
                    }
                } else {
                    ui.add_enabled(false, egui::Button::new("Open"));
                    ui.add_enabled(false, egui::Button::new("Show in Explorer"));
                    ui.add_enabled(false, egui::Button::new("Duplicate"));
                    ui.add_enabled(false, egui::Button::new("Delete"));
                }
                ui.separator();
                self.menu_do(ui, "Find References In Scene", |app| {
                    if let Some(path) = app.state.asset_primary().cloned() {
                        app.find_references(&path, false);
                    }
                });
                self.menu_do(ui, "Find References In Project", |app| {
                    if let Some(path) = app.state.asset_primary().cloned() {
                        app.find_references(&path, true);
                    }
                });
                ui.separator();
                self.menu_do(ui, "Refresh", |app| {
                    app.workspace.refresh_project_files();
                    app.state.status = "assets refreshed".to_string();
                });
            });

            // ---- GameObject ----
            ui.menu_button("GameObject", |ui| {
                self.menu_do(ui, "Create Empty", |app| app.workspace.add_entity(""));
                self.menu_do(ui, "Create Empty Child", |app| app.create_empty_child());
                self.menu_do(ui, "Create Empty Parent", |app| app.create_empty_parent());
                ui.separator();
                ui.menu_button("Create Other", |ui| {
                    ui.menu_button("3D Object", |ui| {
                        for (id, label) in [
                            ("cube", "Cube"),
                            ("sphere", "Sphere"),
                            ("cylinder", "Cylinder"),
                            ("capsule", "Capsule"),
                            ("quad", "Quad"),
                            ("plane", "Plane"),
                        ] {
                            let known = self.workspace.presets.iter().any(|p| p.id == id);
                            if ui.add_enabled(known, egui::Button::new(label)).clicked() {
                                self.workspace.add_entity(id);
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Rendering", |ui| {
                        for (id, label) in [("mesh", "Mesh Renderer"), ("skybox", "Skybox")] {
                            let known = self.workspace.presets.iter().any(|p| p.id == id);
                            if ui.add_enabled(known, egui::Button::new(label)).clicked() {
                                self.workspace.add_entity(id);
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Light", |ui| {
                        for (id, label) in [
                            ("sun", "Directional Light"),
                            ("point_light", "Point Light"),
                            ("spot_light", "Spot Light"),
                        ] {
                            let known = self.workspace.presets.iter().any(|p| p.id == id);
                            if ui.add_enabled(known, egui::Button::new(label)).clicked() {
                                self.workspace.add_entity(id);
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Camera", |ui| {
                        let known = self.workspace.presets.iter().any(|p| p.id == "camera");
                        if ui.add_enabled(known, egui::Button::new("Camera")).clicked() {
                            self.workspace.add_entity("camera");
                            ui.close();
                        }
                    });
                    ui.menu_button("UI", |ui| {
                        let known = self.workspace.presets.iter().any(|p| p.id == "ui");
                        if ui
                            .add_enabled(known, egui::Button::new("UI Element"))
                            .clicked()
                        {
                            self.workspace.add_entity("ui");
                            ui.close();
                        }
                    });
                });
                ui.separator();
                self.menu_do(ui, "Move To View", |app| {
                    if app.workspace.align_selection_to_view() {
                        app.state.status = "moved selection to view".to_string();
                        app.mark_scene_dirty();
                    } else {
                        app.state.status = "Move To View needs a selection".to_string();
                    }
                });
                self.menu_do(ui, "Align View to Selected", |app| {
                    if app.workspace.align_view_to_selection() {
                        app.mark_scene_dirty();
                    }
                });
                self.menu_do(ui, "Snap to Ground", |app| {
                    let count = app.workspace.snap_selection_to_ground();
                    app.state.status = format!("snapped {count} entities");
                    app.mark_scene_dirty();
                });
            });

            // ---- Component ----
            ui.menu_button("Component", |ui| {
                // 与检查器的 Add Component 一致：已挂的组件不列出。
                let attachable: std::collections::HashSet<String> = self
                    .workspace
                    .component_types()
                    .into_iter()
                    .map(|component| component.name.clone())
                    .collect();
                let categories: Vec<(String, Vec<(String, String)>)> = self
                    .workspace
                    .schema
                    .component_menu()
                    .into_iter()
                    .map(|(category, items)| {
                        (
                            category.to_string(),
                            items
                                .into_iter()
                                .filter(|item| attachable.contains(&item.name))
                                .map(|item| (item.name.clone(), item.display_name().to_string()))
                                .collect(),
                        )
                    })
                    .filter(|(_, items): &(String, Vec<(String, String)>)| !items.is_empty())
                    .collect();
                if categories.is_empty() {
                    ui.label("no more components");
                }
                for (category, items) in categories {
                    ui.menu_button(category, |ui| {
                        for (name, label) in &items {
                            if ui.button(label).clicked() {
                                self.workspace.add_component(name);
                                ui.close();
                            }
                        }
                    });
                }
            });

            // ---- Window ----
            ui.menu_button("Window", |ui| {
                ui.menu_button("General", |ui| {
                    for (label, command) in [
                        ("Scene", Command::ToggleSceneView),
                        ("Game", Command::ToggleGameView),
                        ("Hierarchy", Command::ToggleHierarchy),
                        ("Project", Command::ToggleAssets),
                        ("Inspector", Command::ToggleInspector),
                        ("Console", Command::ToggleLog),
                        ("Documents", Command::ToggleDocs),
                        ("History", Command::ToggleHistory),
                        ("Render Graph", Command::ToggleRenderGraph),
                        ("Build Settings", Command::ToggleBuild),
                        ("Offline Tools", Command::ToggleTools),
                        ("Project Settings", Command::ProjectSettings),
                        ("Preferences", Command::Preferences),
                        ("Theme", Command::ThemePanel),
                    ] {
                        self.menu_item(ui, label, "", command);
                    }
                    if ui.button("Problems").clicked() {
                        self.workspace.show_panel("problems");
                        ui.close();
                    }
                    if ui.button("Prefab Overrides").clicked() {
                        self.workspace.show_panel("prefabOverrides");
                        ui.close();
                    }
                    let mut maximize_on_play = self.workspace.maximize_on_play;
                    if ui
                        .checkbox(&mut maximize_on_play, "Maximize Game View on Play")
                        .on_hover_text(
                            "Enter Play with the Game view maximized (Unity maximizeOnPlay)",
                        )
                        .changed()
                    {
                        self.workspace.maximize_on_play = maximize_on_play;
                    }
                    if ui.button("Material").clicked() {
                        self.workspace.show_panel("material");
                        ui.close();
                    }
                });
                ui.separator();
                self.menu_do(ui, "Capture Window", |app| app.capture_window_now());
                ui.menu_button("Layouts", |ui| {
                    for (id, label) in prism_editor_core::workspace::layout_preset_ids() {
                        let active = self.workspace.layout_preset == id;
                        if ui.selectable_label(active, label).clicked() {
                            self.workspace.set_layout_preset(id);
                            ui.close();
                        }
                    }
                    ui.separator();
                    let names: Vec<String> = self.named_layouts.keys().cloned().collect();
                    for name in names {
                        ui.menu_button(&name, |ui| {
                            if ui.button("Load").clicked() {
                                let value = self.named_layouts.get(&name).cloned();
                                if let Some(value) = value {
                                    self.apply_layout_json(&value);
                                    self.state.status = format!("layout loaded: {name}");
                                }
                                ui.close();
                            }
                            if ui.button("Delete").clicked() {
                                self.named_layouts.remove(&name);
                                self.write_named_layouts();
                                ui.close();
                            }
                        });
                    }
                    if !self.named_layouts.is_empty() {
                        ui.separator();
                    }
                    self.menu_do(ui, "Save Layout…", |app| {
                        app.open_prompt(
                            panels::PromptKind::LayoutName,
                            "Save Layout",
                            "布局名（写进 target/editor-egui/layouts.json）",
                            "My Layout",
                        )
                    });
                    self.menu_do(ui, "Save Layout to File…", |app| {
                        app.open_prompt(
                            panels::PromptKind::LayoutFileSave,
                            "Save Layout to File",
                            "输出 JSON 路径",
                            "target/editor-egui/layout.json",
                        )
                    });
                    self.menu_do(ui, "Load Layout from File…", |app| {
                        app.open_prompt(
                            panels::PromptKind::LayoutFileLoad,
                            "Load Layout from File",
                            "读取 JSON 路径（v4 {version,dock,floating}）",
                            "target/editor-egui/layout.json",
                        )
                    });
                    self.menu_do(ui, "Reset All Layouts", |app| {
                        app.named_layouts.clear();
                        app.write_named_layouts();
                        app.workspace.set_layout_preset("default");
                        app.state.status = "layouts reset".to_string();
                    });
                });
                ui.separator();
                self.menu_item(ui, "Reset Layout", "", Command::ResetLayout);
            });

            // ---- Help ----
            ui.menu_button("Help", |ui| {
                self.menu_item(ui, "Shortcuts", "", Command::Shortcuts);
                self.menu_do(ui, "Editor Guide", |app| {
                    app.state.status = "docs/editor-egui.md".to_string();
                });
                self.menu_item(ui, "About PrismaRev", "", Command::About);
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let colors = self.workspace.colors();
                let accent = theme::color32(&colors, prism_editor_core::ThemeToken::Accent);
                ui.label(
                    RichText::new("zero-copy viewports")
                        .size(10.0)
                        .color(accent),
                );
                if let Some(view) = &self.scene_view {
                    ui.label(
                        RichText::new(format!("{:.1} ms", view.last_frame_ms))
                            .size(10.0)
                            .weak(),
                    );
                }
                let build_label = self
                    .build
                    .as_ref()
                    .filter(|job| job.running())
                    .map(|job| job.label().to_string());
                if let Some(label) = build_label {
                    // 构建是几十秒的阻塞操作：状态栏必须能一键中断。
                    if ui
                        .small_button(RichText::new("Cancel").size(10.0))
                        .on_hover_text("Stop the running cargo build")
                        .clicked()
                    {
                        self.cancel_build();
                    }
                    ui.label(
                        RichText::new(format!("building: {label}"))
                            .size(10.0)
                            .color(egui::Color32::from_rgb(224, 176, 80)),
                    );
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // 预制体面包屑（#420）：从场景实例打开的预制体，一键回到来源场景。
            if let Some(origin) = self.workspace.document().prefab_origin.clone() {
                let prefab_title = self.workspace.document().title();
                if ui
                    .small_button(format!("{} {}", icons::ARROW_LEFT, origin.scene_title))
                    .on_hover_text("Back to the scene this prefab instance came from")
                    .clicked()
                {
                    let prefab_index = self.workspace.active_index();
                    if self.workspace.return_from_prefab(prefab_index) {
                        self.state.status = format!(
                            "returned to {} · selected {}",
                            origin.scene_title, prefab_title
                        );
                    }
                }
                ui.label(
                    RichText::new(format!("{} / {}", origin.scene_title, prefab_title))
                        .weak()
                        .size(11.0),
                );
                ui.separator();
            }
            let playing = self.playing;
            let paused = self.paused;
            // 引擎的运输控件是纯图标（tooltip 补文字）。
            let transport = |ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool| {
                ui.add_enabled(enabled, egui::Button::new(RichText::new(glyph).size(14.0)))
                    .on_hover_text(tip)
                    .clicked()
            };
            if transport(ui, icons::PLAY, "Play", !playing) {
                self.apply_command(Command::Play);
            }
            if transport(
                ui,
                if paused { icons::PLAY } else { icons::PAUSE },
                if paused { "Resume" } else { "Pause" },
                playing,
            ) {
                self.apply_command(Command::Pause);
            }
            if transport(ui, icons::SKIP_FORWARD, "Step", playing) {
                self.apply_command(Command::Step);
            }
            if transport(ui, icons::STOP, "Stop", playing) {
                self.apply_command(Command::Stop);
            }

            ui.separator();

            // 当前文档
            let docs: Vec<String> = self
                .workspace
                .docs
                .iter()
                .map(|document| document.title_with_dirty())
                .collect();
            let active = self.workspace.active_index();
            let current = docs.get(active).cloned().unwrap_or_default();
            egui::ComboBox::from_id_salt("toolbar-docs")
                .width(200.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (index, title) in docs.iter().enumerate() {
                        if ui.selectable_label(index == active, title).clicked() {
                            self.workspace.set_active(index);
                            self.doc_hash = 0;
                            self.pending_doc = None;
                        }
                    }
                });

            ui.separator();
            // 引擎的工具条：Q/W/E/R + 世界/局部 + 网格。
            for (tool, label, key) in [
                (crate::viewport::GizmoTool::Translate, "Move", "W"),
                (crate::viewport::GizmoTool::Rotate, "Rotate", "E"),
                (crate::viewport::GizmoTool::Scale, "Scale", "R"),
            ] {
                let active = self
                    .scene_view
                    .as_ref()
                    .map(|view| view.tool == tool)
                    .unwrap_or(false);
                if ui
                    .selectable_label(active, format!("{label} ({key})"))
                    .clicked()
                {
                    self.set_gizmo_tool(tool);
                }
            }
            let space = self.workspace.gizmo_space;
            if ui
                .button(format!("{:?} (X)", space))
                .on_hover_text("Toggle world / local gizmo space")
                .clicked()
            {
                self.workspace.toggle_gizmo_space();
                self.mark_scene_dirty();
            }
            let pivot = self.workspace.pivot_mode
                == prism_editor_core::workspace::PivotMode::Pivot;
            if ui
                .selectable_label(pivot, "Pivot")
                .on_hover_text(
                    "Rotate / scale each object around its own pivot \
                     (off = selection centre, group transforms)",
                )
                .clicked()
            {
                self.workspace.pivot_mode = if pivot {
                    prism_editor_core::workspace::PivotMode::Center
                } else {
                    prism_editor_core::workspace::PivotMode::Pivot
                };
                self.mark_scene_dirty();
            }
            let snap = self.workspace.snap;
            if ui
                .selectable_label(snap.enabled, "Snap (Ctrl)")
                .on_hover_text(format!(
                    "Snap gizmo drags: {translate} units / {rotate}° / x{scale} — hold Ctrl to invert",
                    translate = snap.translate,
                    rotate = snap.rotate_degrees,
                    scale = snap.scale,
                ))
                .clicked()
            {
                self.workspace.snap.enabled = !snap.enabled;
            }
            let grid_on = self.render_settings.show_grid;
            if ui.selectable_label(grid_on, "Grid (G)").clicked() {
                let mut settings = self.render_settings.clone();
                settings.show_grid = !grid_on;
                self.apply_render_settings(settings);
            }
            let outline_on = self.workspace.show_selection_outline;
            if ui
                .selectable_label(outline_on, "Outline")
                .on_hover_text("Draw a wireframe around the selected objects")
                .clicked()
            {
                self.workspace.show_selection_outline = !outline_on;
                self.mark_scene_dirty();
            }
            let view_2d = self.workspace.camera.orthographic;
            if ui
                .selectable_label(view_2d, "2D")
                .on_hover_text(
                    "Orthographic XY view for sprites and UI (locks the orbit, pan and zoom only)",
                )
                .clicked()
            {
                self.workspace.camera.toggle_2d();
                self.mark_scene_dirty();
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
            }
            // 相机 / 组件图标设置（#416）：FOV、近远裁剪面、按类型开关图标。
            let mut fov = self.workspace.camera.fov_y_degrees;
            let mut near = self.workspace.camera.near;
            let mut far = self.workspace.camera.far;
            let mut camera_changed = false;
            ui.menu_button("View…", |ui| {
                ui.label(egui::RichText::new("Editor camera").strong().size(11.0));
                if ui
                    .add(egui::Slider::new(&mut fov, 20.0..=110.0).text("FOV"))
                    .changed()
                {
                    camera_changed = true;
                }
                if ui
                    .add(
                        egui::DragValue::new(&mut near)
                            .speed(0.01)
                            .range(0.001..=100.0)
                            .prefix("near "),
                    )
                    .changed()
                {
                    camera_changed = true;
                }
                if ui
                    .add(
                        egui::DragValue::new(&mut far)
                            .speed(1.0)
                            .range(1.0..=100_000.0)
                            .prefix("far "),
                    )
                    .changed()
                {
                    camera_changed = true;
                }
                ui.separator();
                ui.checkbox(&mut self.workspace.show_camera_icons, "Camera icons");
                ui.checkbox(&mut self.workspace.show_light_icons, "Light icons");
            });
            if camera_changed {
                self.workspace.camera.set_fov_degrees(fov);
                self.workspace.camera.set_clip_planes(near, far);
                self.mark_scene_dirty();
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
            }
            let mut settings = self.render_settings.clone();
            if panels::scene_render_mode_combo(ui, &mut settings) {
                self.apply_render_settings(settings);
            }
            let skybox_on = self.workspace.show_skybox;
            if ui
                .selectable_label(skybox_on, "Skybox (S)")
                .on_hover_text("Scene view background: scene skybox or a flat color")
                .clicked()
            {
                self.workspace.toggle_skybox();
                let on = self.workspace.show_skybox;
                if let Some(view) = self.scene_view.as_mut() {
                    view.host.set_show_skybox(on);
                    view.mark_dirty();
                }
            }

            ui.separator();
            if ui.button("Frame All (F)").clicked() {
                self.apply_command(Command::FrameAll);
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
            }
            if ui.button("Frame Selection").clicked() {
                self.apply_command(Command::FrameSelection);
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
            }
            if ui
                .button("Reset Scene Cam (Ctrl+0)")
                .on_hover_text("Reset Scene view camera to its default angle and distance")
                .clicked()
            {
                self.apply_command(Command::ResetSceneCamera);
                self.mark_scene_dirty();
            }

        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.state.status).size(11.0));
            if !self.workspace.status.is_empty() {
                ui.label(RichText::new(&self.workspace.status).size(11.0).weak());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // 模块编译状态 + 显式的 Compile：没有脚本层，代码就是一个可重载的 cdylib，
                // 所以这里对应 UE 的 Compile 按钮，而不是 Unity 的脚本编译。
                // 放在状态栏右侧：工具栏在 1366/1440 宽度下已经没有空间再塞这一组（#603）。
                if let Some(status) = self.project_module_status() {
                    if status.available {
                        if ui
                            .add_enabled(
                                !status.building,
                                egui::Button::new("Compile (Ctrl+R)").small(),
                            )
                            .on_hover_text(
                                "Recompile and reload the project module \
                                 (whole-module reload: project state resets)",
                            )
                            .clicked()
                        {
                            self.refresh_project_module();
                        }
                        let color = match status.state {
                            ModuleState::Ready => Color32::from_rgb(120, 200, 120),
                            ModuleState::Pending => Color32::from_rgb(220, 190, 100),
                            ModuleState::Compiling => Color32::from_rgb(120, 170, 230),
                            ModuleState::Failed => Color32::from_rgb(232, 96, 96),
                            ModuleState::Unavailable => Color32::from_gray(140),
                        };
                        let label = if status.problems > 0 {
                            format!("{} · {} problems", status.state.label(), status.problems)
                        } else {
                            status.state.label().to_string()
                        };
                        let hint = status.error.as_deref().unwrap_or(
                            "Project module build state (diagnostics live in the Problems panel)",
                        );
                        ui.label(RichText::new(label).size(10.0).color(color))
                            .on_hover_text(hint);
                    }
                }
                let text = if self.playing {
                    if self.paused {
                        "PAUSED"
                    } else {
                        "PLAYING"
                    }
                } else {
                    "EDIT"
                };
                let color = if self.playing {
                    panels::play_mode_tint(self.paused)
                } else {
                    Color32::from_gray(150)
                };
                let mut label = RichText::new(text).size(10.0).color(color).strong();
                if self.playing {
                    // 播放 / 暂停时给状态栏一个底色，余光也能看出当前不在编辑态（#500）。
                    label = label.background_color(color.gamma_multiply(0.22));
                }
                ui.label(label);
                let selected = self.workspace.selection.len();
                let entities = self.workspace.document().doc.entities.len();
                ui.label(
                    RichText::new(format!("{entities} entities · {selected} selected"))
                        .size(10.5)
                        .weak(),
                );
                if let Some(root) = &self.project_root {
                    ui.label(RichText::new(root.display().to_string()).size(10.0).weak());
                } else {
                    ui.label(RichText::new("no project (fixture only)").size(10.0).weak());
                }
            });
        });
    }

    // -----------------------------------------------------------------------
    // Docking
    // -----------------------------------------------------------------------

    fn dock_tree(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        // 最大化：只画那一个 leaf 的 tab bar + body，dock tree 原样留着。
        if let Some(tab) = self.workspace.maximized_tab.clone() {
            match prism_editor_core::dock::find_leaf(&self.workspace.dock, &tab) {
                Some(DockNode::Leaf { id, tabs, .. }) => {
                    let (id, tabs) = (id.clone(), tabs.clone());
                    self.dock_leaf(ui, rect, &id, &tabs, &tab);
                    return;
                }
                _ => self.workspace.restore_maximized(),
            }
        }
        let dock = self.workspace.dock.clone();
        self.dock_node(ui, rect, &dock);
    }

    fn dock_node(&mut self, ui: &mut egui::Ui, rect: Rect, node: &DockNode) {
        match node {
            DockNode::Leaf { id, tabs, active } => {
                self.dock_leaf(ui, rect, id, tabs, active);
            }
            DockNode::Split {
                id,
                dir,
                ratio,
                a,
                b,
            } => {
                let splitter = 5.0;
                match dir {
                    DockDirection::Row => {
                        let usable = (rect.width() - splitter).max(2.0);
                        // 嵌套 split 里 `usable` 可能小于两个最小尺寸（#498）：
                        // 直接 `clamp(60.0, usable - 60.0)` 会在 min > max 时 panic。
                        let a_w = split_side_length(usable, *ratio, 60.0);
                        let a_rect = Rect::from_min_size(rect.min, Vec2::new(a_w, rect.height()));
                        let split_rect = Rect::from_min_size(
                            egui::pos2(rect.min.x + a_w, rect.min.y),
                            Vec2::new(splitter, rect.height()),
                        );
                        let b_rect = Rect::from_min_size(
                            egui::pos2(split_rect.max.x, rect.min.y),
                            Vec2::new(usable - a_w, rect.height()),
                        );
                        self.splitter(
                            ui,
                            split_rect,
                            CursorIcon::ResizeHorizontal,
                            id,
                            usable,
                            true,
                        );
                        self.dock_node(ui, a_rect, a);
                        self.dock_node(ui, b_rect, b);
                    }
                    DockDirection::Column => {
                        let usable = (rect.height() - splitter).max(2.0);
                        let a_h = split_side_length(usable, *ratio, 50.0);
                        let a_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), a_h));
                        let split_rect = Rect::from_min_size(
                            egui::pos2(rect.min.x, rect.min.y + a_h),
                            Vec2::new(rect.width(), splitter),
                        );
                        let b_rect = Rect::from_min_size(
                            egui::pos2(rect.min.x, split_rect.max.y),
                            Vec2::new(rect.width(), usable - a_h),
                        );
                        self.splitter(
                            ui,
                            split_rect,
                            CursorIcon::ResizeVertical,
                            id,
                            usable,
                            false,
                        );
                        self.dock_node(ui, a_rect, a);
                        self.dock_node(ui, b_rect, b);
                    }
                }
            }
        }
    }

    fn splitter(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        cursor: CursorIcon,
        split_id: &str,
        usable: f32,
        horizontal: bool,
    ) {
        let response = ui.interact(
            rect,
            ui.id().with(("splitter", split_id)),
            Sense::click_and_drag(),
        );
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(cursor);
        }
        // 分隔条平时就是 1px 边框色（没有蓝灰），拖动时才亮成强调色。
        let colors = self.workspace.colors();
        let color = if response.dragged() {
            theme::color32(&colors, prism_editor_core::ThemeToken::Accent)
        } else if response.hovered() {
            theme::color32(&colors, prism_editor_core::ThemeToken::BorderStrong)
        } else {
            theme::border(&colors)
        };
        ui.painter().rect_filled(rect, 0.0, color);
        if response.dragged() {
            let delta = ui.input(|i| i.pointer.delta());
            let delta = if horizontal { delta.x } else { delta.y };
            let current = self.split_ratio(split_id).unwrap_or(0.5);
            let next = (current + delta / usable.max(1.0)).clamp(0.12, 0.88);
            self.set_split_ratio(split_id, next);
        }
    }

    fn split_ratio(&self, split_id: &str) -> Option<f32> {
        fn walk(node: &DockNode, id: &str) -> Option<f32> {
            match node {
                DockNode::Leaf { .. } => None,
                DockNode::Split {
                    id: node_id,
                    ratio,
                    a,
                    b,
                    ..
                } => {
                    if node_id == id {
                        Some(*ratio)
                    } else {
                        walk(a, id).or_else(|| walk(b, id))
                    }
                }
            }
        }
        walk(&self.workspace.dock, split_id)
    }

    fn set_split_ratio(&mut self, split_id: &str, ratio: f32) {
        self.workspace.dock =
            prism_editor_core::dock::set_ratio(&self.workspace.dock, split_id, ratio, 0.12, 0.88);
    }

    fn dock_leaf(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        leaf_id: &str,
        tabs: &[String],
        active: &str,
    ) {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::top_down(Align::Min)),
        );
        child.set_clip_rect(rect);
        self.dock_leaves.retain(|(id, _, _)| id != leaf_id);
        self.dock_leaves
            .push((leaf_id.to_owned(), active.to_owned(), tabs.to_vec()));

        // 面板底 + 1px 边框（Web `.dock-leaf` 是 panel + 1px border）。
        let colors = self.workspace.colors();
        let sunken = theme::color32(&colors, prism_editor_core::ThemeToken::BgSunken);
        let border = theme::border(&colors);
        child
            .painter()
            .rect_filled(rect, 0.0, theme::panel_fill(&colors));
        child.painter().rect_stroke(
            rect,
            egui::CornerRadius::ZERO,
            Stroke::new(1.0_f32, border),
            egui::StrokeKind::Inside,
        );

        // tab bar：`bg-sunken` 底 + 底部 1px 边框；选中页 = `panel-alt` + 2px 强调下划线。
        let mut activate: Option<String> = None;
        let mut float: Option<String> = None;
        let mut begin_drag: Option<String> = None;
        let mut maximize: Option<String> = None;
        let mut close: Option<String> = None;
        let mut close_others: Option<String> = None;
        let mut add_tab: Option<String> = None;
        let (bar, _) = child.allocate_exact_size(
            Vec2::new(child.available_width(), TAB_BAR_HEIGHT),
            egui::Sense::hover(),
        );
        child.painter().rect_filled(bar, 0.0, sunken);
        child.painter().rect_filled(
            Rect::from_min_max(
                egui::pos2(bar.left(), bar.bottom() - 1.0),
                egui::pos2(bar.right(), bar.bottom()),
            ),
            0.0,
            border,
        );
        // tab 条宽度有限：装不下时只画能放下的那一段，右侧留一个溢出下拉
        // 列出全部 tab（#455）。没有这一步，超出的 tab 会被直接裁掉，既点不到
        // 也拖不出来。
        let widths: Vec<f32> = tabs
            .iter()
            .map(|tab| dock_tab_width(&child, &tab_title(tab)))
            .collect();
        let needed: f32 = widths.iter().sum();
        let overflow = tab_bar_needs_overflow(needed, bar.width());
        let tabs_rect = if overflow {
            Rect::from_min_max(
                bar.min,
                egui::pos2(bar.right() - TAB_OVERFLOW_WIDTH, bar.bottom()),
            )
        } else {
            bar
        };
        let active_index = tabs.iter().position(|tab| tab == active).unwrap_or(0);
        let (visible_from, visible_to) =
            visible_tab_range(&widths, tabs_rect.width(), active_index);
        let mut bar_ui = child.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        bar_ui.set_clip_rect(tabs_rect);
        let mut overflow_activate: Option<String> = None;
        // `Add Tab` 菜单要列出所有面板（去掉本 leaf 已有的）。
        let addable: Vec<(String, String)> = self
            .workspace
            .known_panels()
            .into_iter()
            .filter(|id| !tabs.iter().any(|tab| tab == id))
            .map(|id| {
                let title = self.workspace.panel_title(&id);
                (id, title)
            })
            .collect();
        let maximized = self.workspace.maximized_tab.clone();
        let bar_ui = &mut bar_ui;
        for tab in &tabs[visible_from..visible_to] {
            let selected = tab == active;
            let response = dock_tab(bar_ui, &tab_title(tab), selected, &colors);
            if response.clicked() {
                activate = Some(tab.clone());
                self.state.focused_panel = Some(tab.clone());
            }
            // 双击标签 = 最大化 / 还原（Unity 的 DockArea 行为）。
            if response.double_clicked() {
                maximize = Some(tab.clone());
            }
            if response.drag_started() {
                begin_drag = Some(tab.clone());
            }
            response.context_menu(|ui| {
                let is_maximized = maximized.as_deref() == Some(tab.as_str());
                if ui
                    .button(if is_maximized { "Restore" } else { "Maximize" })
                    .clicked()
                {
                    maximize = Some(tab.clone());
                    ui.close();
                }
                if tabs.len() > 1 && ui.button("Float panel").clicked() {
                    float = Some(tab.clone());
                    ui.close();
                }
                ui.separator();
                if tabs.len() > 1 && ui.button("Close tab").clicked() {
                    close = Some(tab.clone());
                    ui.close();
                }
                if tabs.len() > 1 && ui.button("Close others").clicked() {
                    close_others = Some(tab.clone());
                    ui.close();
                }
                if !addable.is_empty() {
                    ui.menu_button("Add tab", |ui| {
                        for (id, title) in &addable {
                            if ui.button(title).clicked() {
                                add_tab = Some(id.clone());
                                ui.close();
                            }
                        }
                    });
                }
            });
        }
        if overflow {
            // Unity 的 tab scroller 等价物：一个下拉，列出本 leaf 的全部 tab。
            let button_rect = Rect::from_min_max(
                egui::pos2(bar.right() - TAB_OVERFLOW_WIDTH + 2.0, bar.top() + 2.0),
                egui::pos2(bar.right() - 2.0, bar.bottom() - 3.0),
            );
            let mut button_ui = child.new_child(
                egui::UiBuilder::new()
                    .max_rect(button_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            button_ui.set_clip_rect(button_rect);
            button_ui.menu_button(icons::CARET_DOWN, |ui| {
                for tab in tabs {
                    if ui.selectable_label(tab == active, tab_title(tab)).clicked() {
                        overflow_activate = Some(tab.clone());
                        ui.close();
                    }
                }
            });
        }
        if let Some(tab) = overflow_activate {
            activate = Some(tab);
        }

        let body = child.available_rect_before_wrap();
        let mut body_ui = child.new_child(
            egui::UiBuilder::new()
                .max_rect(body)
                .layout(Layout::top_down(Align::Min)),
        );
        body_ui.set_clip_rect(body);
        self.panel_body(&mut body_ui, active, body);

        if let Some(tab) = begin_drag {
            self.state.tab_drag = Some((tab, leaf_id.to_string()));
        }

        // 拖拽标签到本 leaf 的边缘 = 分屏；中间 = 合并成标签页。
        if let Some((drag_tab, source)) = self.state.tab_drag.clone() {
            let pointer = ui.input(|input| input.pointer.hover_pos());
            if let Some(pos) = pointer {
                if rect.contains(pos) {
                    let zone = dock_zone(rect, pos);
                    if zone != prism_editor_core::dock::DockZone::Center || source != leaf_id {
                        self.state.tab_drop = Some((leaf_id.to_string(), zone, 0.5));
                        let preview = match zone {
                            prism_editor_core::dock::DockZone::Center => rect.shrink(4.0),
                            prism_editor_core::dock::DockZone::Left => Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.center().x, rect.max.y),
                            ),
                            prism_editor_core::dock::DockZone::Right => Rect::from_min_max(
                                egui::pos2(rect.center().x, rect.min.y),
                                rect.max,
                            ),
                            prism_editor_core::dock::DockZone::Top => Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.max.x, rect.center().y),
                            ),
                            prism_editor_core::dock::DockZone::Bottom => Rect::from_min_max(
                                egui::pos2(rect.min.x, rect.center().y),
                                rect.max,
                            ),
                        };
                        ui.painter().rect_filled(
                            preview,
                            egui::CornerRadius::ZERO,
                            egui::Color32::from_rgba_unmultiplied(80, 140, 230, 48),
                        );
                        ui.painter().rect_stroke(
                            preview,
                            egui::CornerRadius::ZERO,
                            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                            egui::StrokeKind::Inside,
                        );
                        let _ = &drag_tab;
                    }
                }
            }
        }

        if let Some(tab) = activate {
            self.workspace.activate_panel(leaf_id, &tab);
        }
        if let Some(tab) = float {
            self.workspace.float_panel(&tab, 260.0, 160.0);
        }
        if let Some(tab) = maximize {
            self.workspace.toggle_maximized(&tab);
        }
        if let Some(tab) = close {
            self.workspace.close_panel(&tab);
        }
        if let Some(tab) = close_others {
            self.workspace.close_other_panels(&tab);
        }
        if let Some(tab) = add_tab {
            self.workspace.add_tab_to_leaf(leaf_id, &tab);
        }
    }

    /// 松手：把拖动的标签页停靠到目标 leaf（分屏 / 合并），拖到面板区外则浮动。
    fn finish_tab_drag(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.pointer.any_released()) {
            self.state.tab_drop = None;
            return;
        }
        let Some((tab, source)) = self.state.tab_drag.take() else {
            self.state.tab_drop = None;
            return;
        };
        match self.state.tab_drop.take() {
            Some((target, zone, ratio)) => {
                if target != source || zone != prism_editor_core::dock::DockZone::Center {
                    let dock = prism_editor_core::dock::insert_panel(
                        &self.workspace.dock,
                        &target,
                        &tab,
                        zone,
                        ratio,
                        None,
                    );
                    self.workspace.dock = dock;
                }
            }
            None => {
                if let Some(pos) = ctx.input(|input| input.pointer.interact_pos()) {
                    self.workspace.float_panel(&tab, pos.x, pos.y);
                }
            }
        }
    }

    fn panel_body(&mut self, ui: &mut egui::Ui, tab: &str, rect: Rect) {
        self.panel_rects.insert(tab.to_owned(), rect);
        // 引擎的 focusedWindow：点过哪个面板，快捷键就作用在哪个面板上。
        if ui.rect_contains_pointer(rect) && ui.input(|input| input.pointer.any_click()) {
            self.state.focused_panel = Some(tab.to_owned());
        }
        // 调试开关：完全跳过引擎 / 用户纹理注册，用来区分「UI 自身的渲染问题」与
        // 「引擎离屏图像注册进来的问题」（无窗口/软件 Vulkan 时排查用）。
        let no_viewport = std::env::var("PRISMREV_EDITOR_NO_VIEWPORT").is_ok();
        match tab {
            "sceneView" if no_viewport => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("viewport disabled (PRISMREV_EDITOR_NO_VIEWPORT)").weak(),
                    )
                });
            }
            "gameView" if no_viewport => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("viewport disabled (PRISMREV_EDITOR_NO_VIEWPORT)").weak(),
                    )
                });
            }
            "sceneView" => self.show_viewport(ui, ViewKind::Scene, rect),
            "gameView" => self.show_viewport(ui, ViewKind::Game, rect),
            "hierarchy" => panels::hierarchy(ui, &mut self.workspace, &mut self.state),
            "inspector" => panels::inspector(ui, &mut self.workspace, &mut self.state),
            "assets" => {
                panels::assets(ui, &mut self.workspace, &mut self.state);
                if let Some((path, project_wide)) = self.state.pending_find_references.take() {
                    self.find_references(&path, project_wide);
                }
            }
            "docs" => panels::docs(ui, &mut self.workspace, &mut self.state),
            "log" => panels::log(ui, &mut self.workspace, &mut self.state),
            "history" => panels::history(ui, &mut self.workspace, &mut self.state),
            "renderGraph" => {
                // 报告是 Scene view 出帧时写盘的：启动那一份可能还不存在。
                refresh_render_report(self);
                let was_paused = self.state.render_graph_paused;
                if was_paused && self.state.render_graph_frozen.is_none() {
                    self.state.render_graph_frozen = self.render_report.clone();
                }
                let report = if was_paused {
                    self.state.render_graph_frozen.as_deref()
                } else {
                    self.render_report.as_deref()
                };
                let mut paused = was_paused;
                let mut settings = self.render_settings.clone();
                if panels::render_graph(
                    ui,
                    report,
                    &mut settings,
                    &mut self.state.viewport_scale,
                    &mut paused,
                ) {
                    self.apply_render_settings(settings);
                }
                if paused != was_paused {
                    self.state.render_graph_paused = paused;
                    self.state.render_graph_frozen = if paused {
                        self.render_report.clone()
                    } else {
                        None
                    };
                }
            }
            "projectSettings" => {
                panels::project_settings(ui, self.project_root.as_deref(), &mut self.state)
            }
            "preferences" => panels::preferences(
                ui,
                &mut self.preferences,
                &mut self.workspace,
                &mut self.state,
            ),
            "theme" => panels::theme(ui, &mut self.workspace),
            "build" => {
                panels::build(ui, &mut self.workspace, &mut self.state);
                if let Some(kind) = self.state.pending_build.take() {
                    self.start_build(kind);
                }
            }
            "problems" => {
                // 只在面板可见时拷一份诊断；状态栏只拿条数。
                let module = self
                    .game_view
                    .as_ref()
                    .map(|view| view.host.project_diagnostics().to_vec())
                    .unwrap_or_default();
                panels::problems(ui, &module, &self.build_diagnostics, &mut self.state);
            }
            "prefabOverrides" => panels::prefab_overrides(ui, &mut self.workspace, &mut self.state),
            "material" => panels::material(ui, &mut self.workspace, &mut self.state),
            "tools" => {
                panels::tools(ui, &mut self.workspace, &mut self.state);
                if let Some(kind) = self.state.pending_build.take() {
                    self.start_build(kind);
                }
            }
            "shortcuts" => panels::shortcuts(ui, &mut self.preferences, &mut self.state),
            "about" => panels::about(ui),
            other => {
                ui.label(RichText::new(format!("panel '{other}' not implemented")).weak());
            }
        }
    }

    /// Scene 视图左上角的浮层工具条（#421）：把最常用的视口开关放在鼠标旁边。
    ///
    /// 只做这个实例，不引入 Unity `OverlayCanvas` 那套通用抽象；位置固定
    /// （左上角 8px），折叠状态存在 `UiState`。
    fn scene_overlay(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let collapsed = self.state.scene_overlay_collapsed;
        let bar = scene_overlay_rect(rect, collapsed);
        let mut settings = self.render_settings.clone();
        let mut changed = false;
        let mut skybox_changed = false;
        let mut camera_changed = false;
        let mut collapsed_now = collapsed;

        ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(18, 20, 24, 200))
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                ))
                .corner_radius(egui::CornerRadius::same(4))
                .inner_margin(egui::Margin::symmetric(5, 2))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button(if collapsed_now {
                                icons::CARET_RIGHT
                            } else {
                                icons::CARET_DOWN
                            })
                            .on_hover_text("Collapse / expand viewport tools")
                            .clicked()
                        {
                            collapsed_now = !collapsed_now;
                        }
                        if collapsed_now {
                            return;
                        }
                        if ui
                            .selectable_label(settings.show_grid, "Grid")
                            .on_hover_text("Ground grid (G)")
                            .clicked()
                        {
                            settings.show_grid = !settings.show_grid;
                            changed = true;
                        }
                        if ui
                            .selectable_label(self.workspace.show_skybox, "Sky")
                            .on_hover_text("Scene skybox background (S)")
                            .clicked()
                        {
                            self.workspace.show_skybox = !self.workspace.show_skybox;
                            skybox_changed = true;
                        }
                        if ui
                            .selectable_label(self.workspace.camera.orthographic, "2D")
                            .on_hover_text("Orthographic XY view")
                            .clicked()
                        {
                            self.workspace.camera.toggle_2d();
                            camera_changed = true;
                        }
                        if ui
                            .selectable_label(self.workspace.snap.enabled, "Snap")
                            .on_hover_text("Snap gizmo drags (hold Ctrl to invert)")
                            .clicked()
                        {
                            self.workspace.snap.enabled = !self.workspace.snap.enabled;
                        }
                        ui.add(
                            egui::DragValue::new(&mut self.workspace.snap.translate)
                                .speed(0.01)
                                .range(0.01..=100.0)
                                .prefix("Δ "),
                        )
                        .on_hover_text("Translate snap step (world units)");
                    });
                });
        });

        if collapsed_now != collapsed {
            self.state.scene_overlay_collapsed = collapsed_now;
        }
        if changed {
            self.apply_render_settings(settings);
        }
        if skybox_changed {
            let on = self.workspace.show_skybox;
            if let Some(view) = self.scene_view.as_mut() {
                view.host.set_show_skybox(on);
            }
            self.mark_scene_dirty();
        }
        if camera_changed {
            self.mark_scene_dirty();
        }
    }

    fn show_viewport(&mut self, ui: &mut egui::Ui, kind: ViewKind, rect: Rect) {
        let _ = rect;
        if !self.ensure_view(kind) {
            ui.label(
                RichText::new("Engine viewport unavailable (see Console)")
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }

        // Scene view：把编辑器的轨道相机 / 选中标记写进引擎世界。
        if kind == ViewKind::Scene {
            let selection_hash = selection_hash(&self.workspace);
            if selection_hash != self.selection_hash {
                self.selection_hash = selection_hash;
                if let Some(view) = self.scene_view.as_mut() {
                    view.mark_dirty();
                }
            }
            let camera = self.workspace.camera;
            if let Some(view) = self.scene_view.as_mut() {
                view.host.place_editor_camera(&camera);
            }
        }

        // Play 期间 Game view 由 `ui()` 每帧推进；这里只决定是否重渲染。
        let playing = self.playing;

        let mut document_changed = false;
        {
            let bridge = &self.bridge;
            let playing = playing;
            let workspace = &mut self.workspace;
            let view = match kind {
                ViewKind::Scene => self.scene_view.as_mut(),
                ViewKind::Game => self.game_view.as_mut(),
            };
            if let Some(view) = view {
                let rerender = playing && kind == ViewKind::Game;
                let response =
                    view.show(ui, workspace, bridge, rerender, self.state.viewport_scale);
                document_changed = response.document_changed;
            }
        }
        if document_changed {
            self.state.status = "moved selection".to_string();
        }
        if kind == ViewKind::Scene {
            self.scene_overlay(ui, rect);
        }
    }

    // -----------------------------------------------------------------------
    // 快捷键 & 浮动窗口
    // -----------------------------------------------------------------------

    /// 统一输入框：另存为 / 新建目录 / 存为预制体 / 重命名。
    /// 破坏性操作的确认框（引擎的 `EditorUtility.DisplayDialog`）。
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.state.confirm.clone() else {
            return;
        };
        let mut open = true;
        let mut accepted = false;
        let mut cancelled = false;
        egui::Window::new(&confirm.title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(&confirm.message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(confirm.ok.as_str()).clicked() {
                        accepted = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            });
        if accepted {
            self.accept_confirm();
        } else if cancelled || !open {
            self.state.confirm = None;
        }
    }

    /// 接受当前的通用确认框（UI 与 `dialog:confirm:ok` 共用，#618）。
    fn accept_confirm(&mut self) {
        let Some(confirm) = self.state.confirm.take() else {
            return;
        };
        match confirm.kind {
            panels::ConfirmKind::CleanPlayer => self.start_build(crate::build::BuildKind::Clean),
            panels::ConfirmKind::CleanEngine => {
                self.start_build(crate::build::BuildKind::CleanEngine)
            }
        }
    }

    fn prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.state.prompt.clone() else {
            return;
        };
        let mut open = true;
        let mut commit = false;
        egui::Window::new(&prompt.title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(&prompt.hint);
                let response =
                    ui.add(egui::TextEdit::singleline(&mut prompt.value).desired_width(380.0));
                if !response.has_focus() && !response.lost_focus() {
                    response.request_focus();
                }
                commit = ui.button("OK").clicked()
                    || ui.input(|input| input.key_pressed(egui::Key::Enter));
            });

        if commit {
            let value = prompt.value.trim().to_string();
            let _ = self.commit_prompt(prompt.kind, value);
        } else if !open {
            self.state.prompt = None;
        } else {
            self.state.prompt = Some(prompt);
        }
    }

    /// 提交当前输入框（UI 与 `dialog:prompt:ok` 共用，#618）。
    fn commit_prompt(&mut self, kind: panels::PromptKind, value: String) -> Result<(), String> {
        let result = match kind {
            panels::PromptKind::SaveAs => self
                .workspace
                .save_active_as(&value)
                .map(|path| format!("saved {path}")),
            panels::PromptKind::NewFolder => self
                .workspace
                .create_asset_folder(&value)
                .map(|path| format!("created {path}")),
            panels::PromptKind::SavePrefab => self
                .workspace
                .save_selection_as_prefab_at(Some("assets/prefabs"), &value)
                .map(|path| format!("saved prefab {path}")),
            panels::PromptKind::PrefabVariant => self.create_prefab_variant(&value),
            panels::PromptKind::LayoutName => {
                self.save_named_layout(&value);
                Ok(format!("layout saved: {value}"))
            }
            panels::PromptKind::LayoutFileSave => {
                let path = PathBuf::from(&value);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&self.current_layout_json()).unwrap_or_default(),
                )
                .map(|()| format!("layout written: {}", path.display()))
            }
            panels::PromptKind::LayoutFileLoad => {
                let text = std::fs::read_to_string(&value);
                match text {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(json) => {
                            self.apply_layout_json(&json);
                            Ok(format!("layout loaded: {value}"))
                        }
                        Err(error) => Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid layout json: {error}"),
                        )),
                    },
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(message) => {
                self.state.status = message;
                self.doc_hash = 0;
                self.state.prompt = None;
                Ok(())
            }
            Err(error) => {
                self.state.status = format!("{error}");
                Err(error.to_string())
            }
        }
    }

    /// 保存时发现文件被外部改过：覆盖 / 重新载入 / 取消（#452）。
    ///
    /// 典型场景是 `git checkout` 切分支后仍然按了 `Ctrl+S`：盲写会把刚拉下来的
    /// 版本覆盖掉，而且没有任何提示。
    fn save_conflict_dialog(&mut self, ctx: &egui::Context) {
        let Some(index) = self.state.pending_save_conflict else {
            return;
        };
        let Some(document) = self.workspace.docs.get(index) else {
            self.state.pending_save_conflict = None;
            return;
        };
        let title = document.title();
        let path = document.path.clone().unwrap_or_default();

        let mut choice: Option<SaveConflictChoice> = None;
        egui::Window::new("File Changed on Disk")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("{title} was modified outside the editor."));
                ui.label(egui::RichText::new(path).weak().small());
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Overwriting discards the other copy.").weak());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Overwrite").clicked() {
                        choice = Some(SaveConflictChoice::Overwrite);
                    }
                    if ui.button("Reload from Disk").clicked() {
                        choice = Some(SaveConflictChoice::Reload);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(SaveConflictChoice::Cancel);
                    }
                });
            });

        if let Some(choice) = choice {
            self.apply_save_conflict_choice(index, choice);
        }
    }

    /// 执行磁盘冲突的选择（UI 与 `dialog:save-conflict:*` 共用，#618）。
    fn apply_save_conflict_choice(&mut self, index: usize, choice: SaveConflictChoice) {
        self.state.pending_save_conflict = None;
        match choice {
            SaveConflictChoice::Overwrite => match self.workspace.save_document_forced(index) {
                Ok(path) => self.state.status = format!("saved {path} (overwrote)"),
                Err(error) => self.state.status = format!("save failed: {error}"),
            },
            SaveConflictChoice::Reload => match self.workspace.reload_document_from_disk(index) {
                Ok(()) => self.state.status = "reloaded from disk".to_string(),
                Err(error) => self.state.status = format!("reload failed: {error}"),
            },
            SaveConflictChoice::Cancel => {}
        }
    }

    /// 关闭未保存文档 / 窗口时的三选一确认（引擎的 Save / Don't Save / Cancel）。
    fn close_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(target) = self.state.pending_close else {
            return;
        };
        let (message, detail) = match target {
            panels::CloseTarget::Document(index) => {
                let title = self
                    .workspace
                    .docs
                    .get(index)
                    .map(|document| document.title())
                    .unwrap_or_else(|| "document".to_string());
                (
                    format!("{title} has unsaved changes."),
                    "Close it anyway?".to_string(),
                )
            }
            panels::CloseTarget::App => {
                let count = self
                    .workspace
                    .docs
                    .iter()
                    .filter(|document| document.dirty)
                    .count();
                (
                    format!("{count} document(s) have unsaved changes."),
                    "Quit anyway?".to_string(),
                )
            }
        };

        let mut open = true;
        let mut decision: Option<CloseDecision> = None;
        egui::Window::new("Unsaved Changes")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(message);
                ui.label(egui::RichText::new(detail).weak());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        decision = Some(CloseDecision::Save);
                    }
                    if ui.button("Don't Save").clicked() {
                        decision = Some(CloseDecision::Discard);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(CloseDecision::Cancel);
                    }
                });
            });

        match decision {
            Some(decision) => self.apply_close_decision(ctx, target, decision),
            None => {
                // 标题栏的 × 等同 Cancel。
                if !open {
                    self.state.pending_close = None;
                }
            }
        }
    }

    /// 上次退出后留下的未保存快照（#619）：Recover 读回为 dirty 文档，Discard 删除。
    fn recovery_dialog(&mut self, ctx: &egui::Context) {
        let count = self.state.pending_recovery;
        if count == 0 {
            return;
        }
        let mut recover = None;
        egui::Window::new("Unsaved Changes Recovered")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{count} document(s) from the previous session were not saved."
                ));
                ui.label(
                    egui::RichText::new(
                        "Recover them as unsaved documents, or discard the snapshot?",
                    )
                    .weak(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Recover").clicked() {
                        recover = Some(true);
                    }
                    if ui.button("Discard").clicked() {
                        recover = Some(false);
                    }
                });
            });

        match recover {
            Some(true) => match self.workspace.recover_unsaved() {
                Ok(count) => {
                    self.state.pending_recovery = 0;
                    self.state.status = format!("recovered {count} unsaved document(s)");
                    // 让引擎按恢复后的文档重新加载。
                    self.doc_hash = 0;
                    self.mark_scene_dirty();
                }
                Err(error) => {
                    self.state.status = format!("recover failed: {error}");
                }
            },
            Some(false) => match self.workspace.discard_recovery() {
                Ok(()) => {
                    self.state.pending_recovery = 0;
                    self.state.status = "discarded the recovery snapshot".to_string();
                }
                Err(error) => {
                    self.state.status = format!("discard failed: {error}");
                }
            },
            None => {}
        }
    }

    /// 执行未保存关闭对话框的选择（UI 与 `dialog:close:*` 共用，#618）。
    fn apply_close_decision(
        &mut self,
        ctx: &egui::Context,
        target: panels::CloseTarget,
        decision: CloseDecision,
    ) {
        match decision {
            CloseDecision::Save => match self.save_before_close(target) {
                Ok(()) => self.finish_close(ctx, target),
                Err(error) => self.state.status = format!("save failed: {error}"),
            },
            CloseDecision::Discard => self.finish_close(ctx, target),
            CloseDecision::Cancel => self.state.pending_close = None,
        }
    }

    /// 保存关闭目标涉及的所有未保存文档；任一失败就保留确认框。
    fn save_before_close(&mut self, target: panels::CloseTarget) -> Result<(), String> {
        match target {
            panels::CloseTarget::Document(index) => self
                .workspace
                .save_document(index)
                .map(|path| {
                    self.state.status = format!("saved {path}");
                })
                .map_err(|error| error.to_string()),
            panels::CloseTarget::App => {
                let mut failure: Option<String> = None;
                for index in 0..self.workspace.docs.len() {
                    if !self.workspace.docs[index].dirty {
                        continue;
                    }
                    if let Err(error) = self.workspace.save_document(index) {
                        failure = Some(error.to_string());
                    }
                }
                failure.map_or(Ok(()), Err)
            }
        }
    }

    /// 确认后的真正关闭动作。
    fn finish_close(&mut self, ctx: &egui::Context, target: panels::CloseTarget) {
        self.state.pending_close = None;
        match target {
            panels::CloseTarget::Document(index) => self.workspace.close_document(index),
            panels::CloseTarget::App => {
                self.autosave_state();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        // 改键捕获期间按键属于 Shortcuts 面板：不要触发任何命令（#421）。
        if self.state.shortcut_capture.is_some() {
            if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                self.state.shortcut_capture = None;
            }
            return;
        }
        // Ctrl+K：全局搜索（Unity 的习惯；Ctrl+P 已经是 Play）。
        if ctx.input_mut(|input| {
            input.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::K))
        }) {
            self.palette.toggle(&self.workspace);
        }
        // 弹窗开着时按键属于输入框：Esc / ↑↓ / Enter 由 `palette::show` 处理。
        if self.palette.is_open() {
            return;
        }
        if self
            .scene_view
            .as_ref()
            .is_some_and(|view| view.flying_camera())
        {
            return;
        }
        let shortcuts = editor_shortcuts(&self.preferences.shortcut_overrides);

        let wants_keyboard = ctx.memory(|memory| memory.focused()).is_some();
        // W / E / R 工具、X 世界/局部、G 网格（文本输入时不抢键）。
        if !wants_keyboard {
            // Shift+Space：最大化 / 还原焦点面板（Unity 的 Maximize 习惯）。
            if ctx.input(|input| input.key_pressed(egui::Key::Space) && input.modifiers.shift) {
                if self.workspace.maximized_tab.is_some() {
                    self.workspace.restore_maximized();
                } else if let Some(tab) = self.state.focused_panel.clone() {
                    self.workspace.toggle_maximized(&tab);
                }
            }
            use crate::viewport::GizmoTool;
            if ctx.input(|input| input.key_pressed(egui::Key::W)) {
                self.set_gizmo_tool(GizmoTool::Translate);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::E)) {
                self.set_gizmo_tool(GizmoTool::Rotate);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::R)) {
                self.set_gizmo_tool(GizmoTool::Scale);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::X)) {
                self.workspace.toggle_gizmo_space();
                self.mark_scene_dirty();
            }
            if ctx.input(|input| input.key_pressed(egui::Key::G)) {
                let mut settings = self.render_settings.clone();
                settings.show_grid = !settings.show_grid;
                self.apply_render_settings(settings);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::S)) {
                self.workspace.toggle_skybox();
                let on = self.workspace.show_skybox;
                if let Some(view) = self.scene_view.as_mut() {
                    view.host.set_show_skybox(on);
                    view.mark_dirty();
                }
            }
            // Ctrl+B = Build And Run，Ctrl+I = 反选（引擎的默认绑定）。
            if ctx.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::B,
                ))
            }) {
                if !self.build_job_running() {
                    self.start_build(crate::build::BuildKind::PlayerRun);
                }
            }
            // Ctrl+R = 重新编译并加载用户项目模块（引擎的 Assets → Refresh）。
            if ctx.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::R,
                ))
            }) {
                self.refresh_project_module();
            }
            if ctx.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::I,
                ))
            }) {
                self.invert_selection();
            }
        }

        // 引擎的快捷键跟着焦点面板走：在资产面板按 Delete 不能删场景实体。
        let focus = self.state.focused_panel.clone().unwrap_or_default();
        let assets_focused = focus == "assets";
        let inspector_focused = focus == "inspector";

        for (shortcut, command) in shortcuts {
            // 判断必须在 `consume_shortcut` **之前**：那个调用会把按键事件从
            // `input.events` 里 retain 掉，之后再跳过就晚了 —— 输入框已经收不到字符（#499）。
            if wants_keyboard && plain_shortcut(command) {
                continue;
            }
            if ctx.input_mut(|input| consume_shortcut_exact(input, &shortcut)) {
                if assets_focused {
                    match command {
                        Command::DeleteSelection => {
                            self.request_asset_delete();
                            continue;
                        }
                        Command::DuplicateEntity => {
                            self.duplicate_selected_asset();
                            continue;
                        }
                        // 资产面板里的复制 / 剪切 / 粘贴 / 全选暂时没有资产语义，
                        // 绝不能落到场景实体上。
                        Command::CopyEntity
                        | Command::CutEntity
                        | Command::PasteEntity
                        | Command::SelectAll => continue,
                        _ => {}
                    }
                }
                if inspector_focused
                    && matches!(
                        command,
                        Command::DeleteSelection
                            | Command::DuplicateEntity
                            | Command::CopyEntity
                            | Command::CutEntity
                            | Command::PasteEntity
                    )
                {
                    continue;
                }
                self.apply_command(command);
                if matches!(command, Command::FrameSelection | Command::ResetSceneCamera) {
                    if let Some(view) = self.scene_view.as_mut() {
                        view.mark_dirty();
                    }
                }
            }
        }

        // 引擎的 F2：重命名当前面板里的选中项。
        if !wants_keyboard && ctx.input(|input| input.key_pressed(egui::Key::F2)) {
            if assets_focused {
                self.rename_selected_asset_prompt();
            } else if let Some(index) = self.workspace.primary_selection() {
                let name = self.workspace.document().doc.entities[index].display_name();
                self.state.rename = Some((index, name));
                self.state.rename_focus = true;
            }
        }
    }

    fn floating_panels(&mut self, ctx: &egui::Context) {
        let mut to_close: Vec<String> = Vec::new();
        for index in 0..self.workspace.floating.len() {
            let Some(panel) = self.workspace.floating.get(index).cloned() else {
                continue;
            };
            let mut open = true;
            let mut titled = false;
            let shown = egui::Window::new(tab_title(&panel.tab))
                .id(egui::Id::new(("float", &panel.tab)))
                .open(&mut open)
                .default_pos(egui::pos2(panel.x, panel.y))
                .default_size(egui::vec2(panel.width.max(260.0), panel.height.max(180.0)))
                .resizable(true)
                .show(ctx, |ui| {
                    titled = true;
                    // 浮动窗口没有 dock leaf，直接画面板主体。
                    let rect = ui.available_rect_before_wrap();
                    self.panel_body(ui, &panel.tab, rect);
                });
            let _ = titled;
            // 把拖拽 / 缩放后的真实矩形写回模型：布局序列化（自动保存、
            // Save Layout、跨会话恢复）读的就是这份数据。
            if let Some(shown) = shown {
                let rect = shown.response.rect;
                let moved =
                    (rect.min.x - panel.x).abs() > 0.5 || (rect.min.y - panel.y).abs() > 0.5;
                let resized = (rect.width() - panel.width).abs() > 0.5
                    || (rect.height() - panel.height).abs() > 0.5;
                if moved || resized {
                    if let Some(slot) = self.workspace.floating.get_mut(index) {
                        slot.x = rect.min.x;
                        slot.y = rect.min.y;
                        slot.width = rect.width();
                        slot.height = rect.height();
                    }
                }
            }
            if !open {
                to_close.push(panel.tab.clone());
            }
        }
        for tab in to_close {
            self.workspace.close_floating(&tab);
        }
    }

    fn autosave_state(&mut self) {
        if self.preferences.autosave
            && self.autosave_at.elapsed() > std::time::Duration::from_secs(5)
        {
            self.autosave_at = Instant::now();
            let mut state =
                prism_editor_core::EditorState::capture(&self.workspace, &self.preferences);
            if let Some(view) = self.scene_view.as_ref() {
                state.scene.gizmo_tool = match view.tool {
                    crate::viewport::GizmoTool::Translate => "Translate".to_string(),
                    crate::viewport::GizmoTool::Rotate => "Rotate".to_string(),
                    crate::viewport::GizmoTool::Scale => "Scale".to_string(),
                };
            }
            state.scene.render = Some(render_settings_json(&self.render_settings));
            if let Ok(text) = serde_json::to_string_pretty(&state) {
                if let Some(parent) = self.state_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if let Err(error) = std::fs::write(&self.state_path, text) {
                    log::warn!("write editor state failed: {error}");
                }
            }
        }
    }
}

impl EditorApp {
    /// 相机 / 选中标记 / 相机视角变了 → 下一帧重绘 Scene view。
    /// 引擎的 Create → Prefab Variant：把选中的实例存成「继承 base + 覆盖」的预制体，
    /// 然后把场景里的这个实例改指向新变体。
    fn create_prefab_variant(&mut self, name: &str) -> std::io::Result<String> {
        let root_dir = self
            .project_root
            .clone()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no project root"))?;
        let Some(index) = self.workspace.selection.iter().copied().find(|index| {
            self.workspace
                .document()
                .doc
                .entities
                .get(*index)
                .is_some_and(|entity| entity.prefab_link().is_some())
        }) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "selection has no prefab instance",
            ));
        };
        let name = name.trim().trim_end_matches(".prefab.json");
        let stem = if name.is_empty() { "Variant" } else { name };
        let doc = self
            .workspace
            .document()
            .doc
            .extract_subtree_preserving_links(&[index]);
        let mut relative = format!("assets/prefabs/{stem}.prefab.json");
        let mut suffix = 2;
        while root_dir.join(&relative).exists() {
            relative = format!("assets/prefabs/{stem}-{suffix}.prefab.json");
            suffix += 1;
        }
        prism_editor_core::project::write_document(&root_dir, &relative, &doc)?;
        self.workspace.refresh_project_files();
        // 场景里的这个实例改为指向新变体（引擎的行为）。
        if let Some(entity) = self.workspace.document().doc.id_of_index(index) {
            self.workspace.set_field(
                entity,
                "prism_engine::scene::PrefabInstance",
                "prefab",
                serde_json::Value::String(relative.clone()),
            );
        }
        self.workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Info,
            format!("prefab variant created: {relative}"),
        );
        self.doc_hash = 0;
        Ok(relative)
    }

    fn has_prefab_instance(&self) -> bool {
        let doc = &self.workspace.document().doc;
        self.workspace.selection.iter().any(|index| {
            doc.entities
                .get(*index)
                .is_some_and(|entity| entity.prefab_link().is_some())
        })
    }

    fn current_layout_json(&self) -> serde_json::Value {
        let layout = prism_editor_core::DockLayout {
            version: prism_editor_core::DockLayout::VERSION,
            dock: self.workspace.dock.clone(),
            floating: self.workspace.floating.clone(),
        };
        layout.to_json()
    }

    fn write_named_layouts(&mut self) {
        if let Some(dir) = self.layouts_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let value = serde_json::json!(self.named_layouts);
        match std::fs::write(
            &self.layouts_path,
            serde_json::to_string_pretty(&value).unwrap_or_default(),
        ) {
            Ok(()) => self.state.status = format!("layouts saved: {}", self.layouts_path.display()),
            Err(error) => self.state.status = format!("save layouts failed: {error}"),
        }
    }

    fn apply_layout_json(&mut self, value: &serde_json::Value) {
        let known: Vec<String> = self
            .workspace
            .panels
            .iter()
            .map(|panel| panel.id.clone())
            .collect();
        let layout = prism_editor_core::DockLayout::from_json(value, &known, 1440.0, 900.0);
        self.workspace.dock = layout.dock;
        self.workspace.floating = layout.floating;
        // 布局整体换掉时，最大化指向的面板可能已经不在树里。
        self.workspace.maximized_tab = None;
    }

    fn save_named_layout(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let value = self.current_layout_json();
        self.named_layouts.insert(name.to_string(), value);
        self.write_named_layouts();
    }

    fn set_gizmo_tool(&mut self, tool: crate::viewport::GizmoTool) {
        self.ensure_view(ViewKind::Scene);
        if let Some(view) = self.scene_view.as_mut() {
            view.tool = tool;
            view.mark_dirty();
        }
    }

    fn mark_scene_dirty(&mut self) {
        if let Some(view) = self.scene_view.as_mut() {
            view.mark_dirty();
        }
    }

    /// 每帧取走构建子进程的输出（写进 Console，System 来源）。
    /// 松散资产热重载（#495 第 ② 条）。
    ///
    /// 每 1s（而不是每帧）扫描一次 `assets/`；检测到未打包的作者资产变化后等
    /// 800ms 安静期，再触发一次现有的 Build Scene Pack。构建期间不叠加新任务，
    /// 打完包后 `EngineHost::poll_asset_changes` 会重开 `.pak` 并重新解析实体。
    ///
    /// 关掉 Preferences 里的开关就完全不做扫描（发布版 / 低配机器）。
    fn poll_loose_assets(&mut self) {
        if !self.preferences.hot_reload_assets {
            return;
        }
        let Some(root) = self.project_root.clone() else {
            return;
        };
        let now = Instant::now();
        if now < self.loose_scan_at {
            return;
        }
        self.loose_scan_at = now + std::time::Duration::from_secs(1);
        if self.build_job_running() {
            // 正在打包：下一次扫描再来，避免排队一串构建。
            return;
        }
        let Some(watcher) = self.loose_assets.as_mut() else {
            return;
        };
        if !watcher.poll(&root, now) {
            return;
        }
        self.workspace.log(
            prism_editor_core::LogSource::System,
            prism_editor_core::LogLevel::Info,
            "loose assets changed — rebuilding the scene pack",
        );
        self.state.status = "assets changed — rebuilding scene pack".to_string();
        self.start_build(crate::build::BuildKind::ScenePack);
    }

    fn poll_build(&mut self) {
        let Some(job) = self.build.as_mut() else {
            return;
        };
        let (lines, running) = job.poll();
        let new_diagnostics = job.take_diagnostics();
        for line in lines {
            self.workspace.log(
                prism_editor_core::LogSource::System,
                prism_editor_core::LogLevel::Info,
                line,
            );
        }
        if !running {
            let cancelled = job.was_cancelled();
            // `failed` 来自 worker：非 0 退出、产物缺失、spawn 失败都算失败 ——
            // 以前这里只看「进程结束」，编译报错也显示 build finished。
            let failed = job.failed() && !cancelled;
            let label = job.label().to_string();
            if cancelled {
                self.state.status = format!("build cancelled: {label}");
                self.workspace.log(
                    prism_editor_core::LogSource::System,
                    prism_editor_core::LogLevel::Warn,
                    format!("build cancelled: {label}"),
                );
            } else if failed {
                self.state.status = format!("build failed: {label}");
                self.workspace.log(
                    prism_editor_core::LogSource::System,
                    prism_editor_core::LogLevel::Error,
                    format!("build failed: {label} (see Console)"),
                );
            } else {
                self.state.status = format!("build finished: {label}");
                self.workspace.log(
                    prism_editor_core::LogSource::System,
                    prism_editor_core::LogLevel::Info,
                    format!("build finished: {label}"),
                );
            }
            self.build = None;
            // 构建自己会写 `scenes.pak` / `.rscn`（扫描已排除），这里再对齐一次快照，
            // 避免构建期间的外部改动被误判成「刚完成的构建」引起的循环（#495）。
            if let (Some(root), Some(watcher)) =
                (self.project_root.as_deref(), self.loose_assets.as_mut())
            {
                watcher.resync(root);
            }
        }
        self.build_diagnostics.extend(new_diagnostics);
    }

    /// 把 `log` 记录搬进 Console 面板（每帧一次）。
    fn drain_engine_logs(&mut self) {
        let entries = logging::drain();
        let mut saw_error = false;
        for (source, level, text) in entries {
            if matches!(
                level,
                prism_editor_core::LogLevel::Error | prism_editor_core::LogLevel::Panic
            ) {
                saw_error = true;
            }
            self.workspace.log.push(source, level, text);
        }
        // Error Pause（#421）：Play 中第一次出现 Error / Panic 就暂停，并写一行
        // 说明原因 —— 否则画面只是「停住」，看起来像卡死。
        if saw_error
            && self.state.log_error_pause
            && self.workspace.playing
            && !self.workspace.paused
        {
            self.apply_command(Command::Pause);
            self.state.status = "paused: an error was logged (Error Pause)".to_string();
            self.workspace.log(
                prism_editor_core::LogSource::Editor,
                prism_editor_core::LogLevel::Warn,
                "Error Pause: paused because an error was logged while playing",
            );
        }
    }

    /// 渲染设置改动 → 应用到两个引擎实例 → 下一帧重绘。
    fn apply_render_settings(&mut self, settings: prism_engine::render_settings::RenderSettings) {
        self.render_settings = settings;
        // 只改 **Scene view**：Game view 是「独立构建跑起来的样子」，设置来自工程
        // `assets/settings.ini`（`apply_project_render_defaults`），被编辑器预览覆盖
        // 就谈不上对齐产物了。
        if let Some(view) = self.scene_view.as_mut() {
            *view.host.settings_mut() = self.render_settings.clone();
            view.mark_dirty();
        }
    }

    /// egui-ash 需要的渲染状态（同一设备 / 队列 / 分配器）。
    pub fn render_state(
        &self,
    ) -> egui_ash::AshRenderState<Arc<std::sync::Mutex<gpu_allocator::vulkan::Allocator>>> {
        self.gpu.render_state()
    }
}

impl App for EditorApp {
    /// egui-ash 在 winit `CloseRequested` 里已经先 `event_loop.exit()` 了，app 的
    /// `ui` 可能来不及看到 `close_requested`。`LoopExiting` 是事件循环结束前
    /// 唯一还能访问 app 的钩子：在这里把 dirty 文档落成恢复快照（#619）。
    fn handle_event(&mut self, event: egui_ash::event::Event<'_>) {
        let egui_ash::event::Event::AppEvent {
            event: egui_ash::event::AppEvent::LoopExiting,
        } = event
        else {
            return;
        };
        // 控制通道的 quit 是故意跳过确认的（自动化需要确定性退出），不制造快照。
        if self.force_quit {
            return;
        }
        match self.workspace.write_recovery() {
            Ok(0) => {}
            Ok(count) => {
                log::warn!("wrote recovery snapshot for {count} unsaved document(s) before exit")
            }
            Err(error) => log::error!("failed to write recovery snapshot: {error:#}"),
        }
    }

    fn ui(&mut self, ctx: &egui::Context) {
        self.frame_count += 1;
        self.state.build_running = self.build_job_running();
        // 光源预算（#468）：视口每帧采集，检查器只读这份镜像。
        if let Some(view) = self.scene_view.as_ref() {
            let (kept, total) = view.host.local_light_stats();
            self.state.lights_kept = kept;
            self.state.lights_total = total;
            let dropped = view.host.dropped_light_ids();
            if self.state.dropped_light_ids.as_slice() != dropped {
                self.state.dropped_light_ids = dropped.to_vec();
            }
        }
        // 鼠标松开 = 一次交互结束（拖动 gizmo / 拖拽检查器数字框）：
        // 本次交互压成一步，下一次修改必然开新撤销步。
        if ctx.input(|input| input.pointer.any_released()) {
            self.workspace.end_interaction();
        }
        if let Some(action) = self.palette.show(ctx) {
            self.run_palette_action(action);
        }
        self.poll_control(ctx);
        self.drain_engine_logs();
        // 核心层（`Workspace::run(Command::Save)`）挡住了一次覆盖，交给对话框（#452）。
        if let Some(index) = self.workspace.pending_save_conflict.take() {
            self.state.pending_save_conflict = Some(index);
        }
        // 窗口重新获得焦点 = 「回到前台」，像 Unity 的 `OnApplicationFocus`：
        // git checkout / 外部编辑器改过的文档与资产在这里被发现（#452）。
        let focused = ctx.input(|input| input.focused);
        if focused && !self.was_focused {
            let changed = self.workspace.check_external_changes();
            if changed > 0 {
                self.state.status = format!("{changed} document(s) changed on disk");
            }
            // 外部新建 / 删除的资产也在这里重新扫出来（原来只有手动 Refresh）。
            self.workspace.refresh_project_files();
        }
        self.was_focused = focused;
        self.poll_build();
        self.poll_loose_assets();
        self.poll_project_module();
        self.sync_document();
        self.handle_shortcuts(ctx);

        // 控制通道注入的输入：每帧只放一个进去（见 `pending_input` 的注释）。
        if let Some(event) = self.pending_input.pop_front() {
            if let Some(view) = self.game_view.as_mut() {
                event.apply(view.host.input_mut());
            }
        }

        // 资产热重载（#495）：Play 中重建 `.pak` 之后不用退出再进。
        // 轮询自己在 500ms 节流里，所以每帧调用是便宜的。
        let mut re_resolved = 0usize;
        for view in [self.scene_view.as_mut(), self.game_view.as_mut()]
            .into_iter()
            .flatten()
        {
            let count = view.host.poll_asset_changes();
            if count > 0 {
                view.mark_dirty();
                re_resolved += count;
            }
        }
        if re_resolved > 0 {
            self.state.status = format!("hot-reloaded assets: {re_resolved} reference(s)");
        }

        // Play 时无论 Game 面板是否可见都推进模拟（切走 tab 不能暂停游戏）。
        if self.playing {
            if let Some(view) = self.game_view.as_mut() {
                view.host.tick();
                view.mark_dirty();
            }
        }

        // 主题只在 token 变化时重建样式（每帧 set_style 会白扔一次布局缓存）。
        let signature = format!(
            "{}:{}",
            self.workspace.theme.preset,
            self.workspace.theme.overrides.len()
        );
        if signature != self.theme_signature {
            self.theme_signature = signature;
            theme::apply(ctx, &self.workspace.colors());
        }

        // 与 Web 编辑器同构的三段式：菜单栏 22px（chrome）、工具栏 32px（panel）、
        // 状态栏 22px（chrome），中间是 bg 色的面板区（面板之间的缝）。
        let colors = self.workspace.colors();
        let chrome_fill = theme::chrome_fill(&colors);
        let panel_fill = theme::panel_fill(&colors);
        let bg_fill = theme::color32(&colors, prism_editor_core::ThemeToken::Bg);

        egui::TopBottomPanel::top("menu-bar")
            .exact_height(22.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome_fill)
                    .inner_margin(egui::Margin::symmetric(4, 1)),
            )
            .show(ctx, |ui| self.menu_bar(ui, ctx));

        egui::TopBottomPanel::top("toolbar")
            .exact_height(32.0)
            .frame(
                egui::Frame::new()
                    .fill(panel_fill)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| self.toolbar(ui));

        egui::TopBottomPanel::bottom("status-bar")
            .exact_height(22.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome_fill)
                    .inner_margin(egui::Margin::symmetric(6, 1)),
            )
            .show(ctx, |ui| self.status_bar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(bg_fill)
                    .inner_margin(egui::Margin::same(4)),
            )
            .show(ctx, |ui| self.dock_tree(ui));

        // 面板（例如资产检查器的 Reimport）可能在任意 tab 里排队构建请求：
        // 这里兜底消费一次，不要求用户先切到 Build 面板。
        if let Some(kind) = self.state.pending_build.take() {
            self.start_build(kind);
        }

        // Play 态染色（#500）：整窗一圈描边，暂停时换琥珀色。只画边框，
        // 不覆盖内容 —— 悬浮面板和弹窗仍然清晰可读。
        if self.playing {
            let color = panels::play_mode_tint(self.paused);
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("play-mode-frame"),
            ));
            painter.rect_stroke(
                ctx.viewport_rect().shrink(1.5),
                0.0,
                egui::Stroke::new(3.0_f32, color),
                egui::StrokeKind::Inside,
            );
        }

        self.finish_tab_drag(ctx);
        self.floating_panels(ctx);

        self.prompt_dialog(ctx);
        // Preferences 是编辑器本机设置，界面直接读镜像；这里每帧同步一次。
        self.state.compact = self.preferences.compact;
        self.state.confirm_delete = self.preferences.confirm_delete;
        panels::entity_delete_confirm(ctx, &mut self.workspace, &mut self.state);
        self.confirm_dialog(ctx);
        self.close_confirm_dialog(ctx);
        self.save_conflict_dialog(ctx);
        self.recovery_dialog(ctx);
        panels::doc_delete_confirm(ctx, &mut self.workspace, &mut self.state);

        let mut about = self.state.about_open;
        panels::boolean_window(ctx, &mut about, "About", panels::about);
        self.state.about_open = about;

        let mut shortcuts = self.state.shortcuts_open;
        panels::boolean_window(ctx, &mut shortcuts, "Shortcuts", |ui| {
            panels::shortcuts(ui, &mut self.preferences, &mut self.state);
        });
        self.state.shortcuts_open = shortcuts;
        if !self.state.shortcuts_open {
            self.state.shortcut_capture = None;
        }

        // 关闭窗口 = 退出（与游戏窗口一致的行为）；有未保存改动时先拦截。
        if ctx.input(|input| input.viewport().close_requested()) {
            if !self.force_quit && self.workspace.docs.iter().any(|document| document.dirty) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.state.pending_close = Some(panels::CloseTarget::App);
            } else {
                self.exit_signal.send(std::process::ExitCode::SUCCESS);
            }
        }

        self.autosave_state();
        let _ = self.last_frame;
        ctx.request_repaint_after(std::time::Duration::from_millis(if self.playing {
            16
        } else {
            250
        }));
    }

    fn request_redraw(&mut self, _viewport_id: egui::ViewportId) -> HandleRedraw {
        HandleRedraw::Auto
    }
}

// ---------------------------------------------------------------------------
// 数据装载
// ---------------------------------------------------------------------------

fn load_workspace() -> Workspace {
    let candidates: Vec<PathBuf> = std::env::var("PRISMREV_EDITOR_DEMO")
        .ok()
        .map(PathBuf::from)
        .into_iter()
        .chain([
            PathBuf::from("editor/public/demo.json"),
            PathBuf::from("../editor/public/demo.json"),
            PathBuf::from("../../editor/public/demo.json"),
        ])
        .collect();

    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut workspace = Workspace::load_fixture(&value);
        workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Info,
            format!("fixture: {}", path.display()),
        );
        attach_project(&mut workspace);
        return workspace;
    }

    let mut workspace = Workspace::sample();
    attach_project(&mut workspace);
    workspace
}

/// 找到项目根（含 `assets/`）并把资产 / 场景清单读进来。
fn attach_project(workspace: &mut Workspace) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(root) = prism_editor_core::find_project_root(&cwd) else {
        workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Warn,
            "no project root found; running with the fixture only",
        );
        return;
    };
    match workspace.load_project(root.clone()) {
        Ok(count) => workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Info,
            format!("project {} ({count} documents)", root.display()),
        ),
        Err(error) => workspace.log(
            prism_editor_core::LogSource::Editor,
            prism_editor_core::LogLevel::Error,
            format!("load project failed: {error}"),
        ),
    }
}

fn read_named_layouts(path: &Path) -> std::collections::BTreeMap<String, serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn read_editor_state(path: &Path) -> Option<prism_editor_core::EditorState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 把 `RenderSettings` 映射成可持久化的 JSON。
///
/// 引擎结构体没有 serde 派生（也不该为编辑器加），编辑器自己列一遍字段；
/// 新增引擎字段时这里要同步，缺失字段回落到默认值以兼容旧存档。
fn render_settings_json(
    settings: &prism_engine::render_settings::RenderSettings,
) -> serde_json::Value {
    use prism_engine::render_settings::RenderMode;
    serde_json::json!({
        "debugFlags": settings.debug_flags,
        "tonemapMode": settings.tonemap_mode,
        "aaMode": settings.aa_mode,
        "contrast": settings.contrast,
        "saturation": settings.saturation,
        "vignette": settings.vignette,
        "shadowDistance": settings.shadow_distance,
        "debugRt": settings.debug_rt,
        "pathTrace": settings.render_mode == RenderMode::PathTrace,
        "ptMaxBounces": settings.pt_max_bounces,
        "ptRayMaxDistance": settings.pt_ray_max_distance,
        "ptMaxIterations": settings.pt_max_iterations,
        "showGrid": settings.show_grid,
    })
}

/// `render_settings_json` 的逆过程；`fallback_grid` 用于旧存档没有该字段时跟随工作区默认值。
fn render_settings_from_json(
    value: &serde_json::Value,
    fallback_grid: bool,
) -> prism_engine::render_settings::RenderSettings {
    use prism_engine::render_settings::RenderMode;
    let base = prism_engine::render_settings::RenderSettings::default();
    let u32_field = |key: &str, default: u32| {
        value
            .get(key)
            .and_then(|value| value.as_u64())
            .map(|value| value as u32)
            .unwrap_or(default)
    };
    let f32_field = |key: &str, default: f32| {
        value
            .get(key)
            .and_then(|value| value.as_f64())
            .map(|value| value as f32)
            .unwrap_or(default)
    };
    let path_trace = value
        .get("pathTrace")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    prism_engine::render_settings::RenderSettings {
        debug_flags: u32_field("debugFlags", base.debug_flags),
        tonemap_mode: u32_field("tonemapMode", base.tonemap_mode),
        aa_mode: u32_field("aaMode", base.aa_mode),
        contrast: f32_field("contrast", base.contrast),
        saturation: f32_field("saturation", base.saturation),
        vignette: f32_field("vignette", base.vignette),
        shadow_distance: f32_field("shadowDistance", base.shadow_distance),
        debug_rt: u32_field("debugRt", base.debug_rt),
        render_mode: if path_trace {
            RenderMode::PathTrace
        } else {
            RenderMode::Raster
        },
        pt_max_bounces: u32_field("ptMaxBounces", base.pt_max_bounces),
        pt_ray_max_distance: f32_field("ptRayMaxDistance", base.pt_ray_max_distance),
        pt_max_iterations: u32_field("ptMaxIterations", base.pt_max_iterations),
        show_grid: value
            .get("showGrid")
            .and_then(|value| value.as_bool())
            .unwrap_or(fallback_grid),
        // #613：内部分辨率属于工程配置（`assets/settings.ini [render]`），
        // 编辑器偏好不覆盖它——否则 Game 视图和打包产物的像素比例会不一致。
        internal_resolution: base.internal_resolution,
        internal_scale_mode: base.internal_scale_mode,
    }
}

fn selection_hash(workspace: &Workspace) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for index in &workspace.selection {
        hash ^= *index as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn document_hash(workspace: &Workspace) -> u64 {
    let text = workspace.document().doc.to_json_string();
    fnv1a(text.as_bytes())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 面板可见时按 mtime 重读渲染图报告（文件由 Scene view 出帧时写）。
fn refresh_render_report(app: &mut EditorApp) {
    let Some(root) = app.project_root.clone() else {
        return;
    };
    let path = root.join("target/prism-render-graph.json");
    let Ok(modified) = std::fs::metadata(&path).and_then(|meta| meta.modified()) else {
        return;
    };
    if app.render_report_mtime == Some(modified) {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(&path) {
        app.render_report = Some(text);
        app.render_report_mtime = Some(modified);
    }
}

fn read_report(root: Option<&Path>) -> Option<String> {
    let root = root?;
    let path = root.join("target/prism-render-graph.json");
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_editor_core::{KeyChord, SHORTCUTS, SHORTCUT_ALIASES};

    #[test]
    fn the_scene_overlay_stays_inside_the_viewport() {
        let viewport = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 600.0));
        let expanded = scene_overlay_rect(viewport, false);
        assert_eq!(expanded.min, egui::pos2(18.0, 28.0));
        assert!(expanded.width() > 200.0);
        assert!(viewport.contains(expanded.min) && viewport.contains(expanded.max));

        let collapsed = scene_overlay_rect(viewport, true);
        assert!(collapsed.width() < expanded.width());

        // 窄视口：宽度被夹到视口内。
        let tiny = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(40.0, 40.0));
        let rect = scene_overlay_rect(tiny, false);
        assert!(rect.width() <= tiny.width());
        assert!(tiny.contains(rect.min) && tiny.contains(rect.max));
    }

    fn rect() -> Rect {
        Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(400.0, 300.0))
    }

    #[test]
    fn shortcut_overrides_change_the_consumed_binding() {
        // #421：改键后分发用的是覆盖表，不是 `SHORTCUTS` 的默认值。
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert(Command::Play.id().to_string(), "Ctrl+Shift+P".to_string());
        let table = editor_shortcuts(&overrides);
        let (play, _) = table
            .iter()
            .find(|(_, command)| *command == Command::Play)
            .expect("play binding");
        assert_eq!(play.logical_key, egui::Key::P);
        assert!(play.modifiers.ctrl && play.modifiers.shift);
        // 默认的 Ctrl+P 不再绑 Play。
        assert!(!table.iter().any(|(shortcut, command)| {
            *command == Command::Play
                && shortcut.logical_key == egui::Key::P
                && shortcut.modifiers == egui::Modifiers::CTRL
        }));
        // 其它命令不受影响。
        assert!(table.iter().any(|(_, command)| *command == Command::Save));
    }

    /// #516：表里的每一条都必须能翻译成 egui 的键位，否则面板写着却按不出来。
    #[test]
    fn every_listed_shortcut_maps_to_a_key() {
        for entry in SHORTCUTS.iter().chain(SHORTCUT_ALIASES.iter()) {
            let chord = KeyChord::parse(entry.keys)
                .unwrap_or_else(|| panic!("{:?} 的键位 {:?} 解析不了", entry.command, entry.keys));
            assert!(
                key_for(&chord.key).is_some(),
                "{:?} 的键名 {:?} 在 egui 里没有对应键",
                entry.command,
                chord.key
            );
        }
        assert_eq!(
            editor_shortcuts(&Default::default()).len(),
            SHORTCUTS.len() + SHORTCUT_ALIASES.len(),
            "分发表应当覆盖整张表（含别名）"
        );
    }

    /// #516：带 Shift / Alt 的 chord 必须先于它的子集被消费，且基键不能被它抢走。
    #[test]
    fn shifted_chords_do_not_fall_back_to_their_base_key() {
        let table = editor_shortcuts(&Default::default());
        // 修饰键多的排在前面。
        for pair in table.windows(2) {
            let modifiers = |shortcut: &egui::KeyboardShortcut| {
                u8::from(shortcut.modifiers.ctrl)
                    + u8::from(shortcut.modifiers.shift)
                    + u8::from(shortcut.modifiers.alt)
            };
            assert!(
                modifiers(&pair[0].0) >= modifiers(&pair[1].0),
                "排序必须按修饰键数量从多到少，否则 Ctrl+Shift+S 会被 Ctrl+S 吃掉"
            );
        }

        let mut input = egui::InputState::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        });
        let save = egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::S);
        let save_as = egui::KeyboardShortcut::new(
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            egui::Key::S,
        );
        assert!(
            !consume_shortcut_exact(&mut input, &save),
            "Ctrl+Shift+S 不能触发 Save"
        );
        assert!(
            consume_shortcut_exact(&mut input, &save_as),
            "Ctrl+Shift+S 必须触发 Save As"
        );
        assert!(input.events.is_empty(), "命中后事件要被消费掉");

        // 反方向：只有 Ctrl 时不能匹配带 Shift 的 chord。
        input.events.push(egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        });
        assert!(!consume_shortcut_exact(&mut input, &save_as));
        assert!(consume_shortcut_exact(&mut input, &save));
    }

    /// #516：表里每条命令只出现一次（别名除外），免得两个键位互相抢。
    #[test]
    fn the_shortcut_table_has_no_duplicate_commands() {
        let mut seen: Vec<Command> = Vec::new();
        for entry in SHORTCUTS {
            assert!(
                !seen.contains(&entry.command),
                "{:?} 在 SHORTCUTS 里出现了两次",
                entry.command
            );
            seen.push(entry.command);
        }
    }

    /// #516：表里列出的命令必须真的能触发（Stop / Frame All / Save As 曾经只是「写了」）。
    #[test]
    fn every_table_command_is_dispatched() {
        let table = editor_shortcuts(&Default::default());
        for entry in SHORTCUTS {
            assert!(
                table.iter().any(|(_, command)| *command == entry.command),
                "{:?} 在表里但没有进分发表",
                entry.command
            );
        }
    }

    /// #499：输入框聚焦时，裸键（F / Delete / 全选）不能被执行。
    #[test]
    fn typing_in_a_text_field_keeps_the_bare_shortcuts() {
        assert!(plain_shortcut(Command::FrameSelection), "F 是裸键");
        assert!(plain_shortcut(Command::DeleteSelection), "Delete 是裸键");
        assert!(plain_shortcut(Command::SelectAll));
        // 带修饰键的命令在输入时也要能用：Ctrl+S 存盘不能因为光标在输入框里失效。
        assert!(!plain_shortcut(Command::Save));
        assert!(!plain_shortcut(Command::Play));
    }

    /// #499：`Select All` 必须带 Ctrl，裸 `A` 不再绑任何命令。
    #[test]
    fn select_all_is_ctrl_a() {
        let table = editor_shortcuts(&Default::default());
        let (shortcut, _) = table
            .iter()
            .find(|(_, command)| *command == Command::SelectAll)
            .expect("Select All must be bound");
        assert_eq!(shortcut.modifiers, egui::Modifiers::CTRL);
        assert_eq!(shortcut.logical_key, egui::Key::A);
        assert!(
            !table
                .iter()
                .any(|(entry, _)| entry.modifiers == egui::Modifiers::NONE
                    && entry.logical_key == egui::Key::A),
            "裸 A 会把输入框里打的字变成全选实体"
        );
    }

    /// #455：tab 条装不下时要有溢出入口，而不是把 tab 裁掉。
    #[test]
    fn a_crowded_tab_bar_falls_back_to_overflow() {
        // 装得下（含正好装满）时不要多余的按钮。
        assert!(!tab_bar_needs_overflow(120.0, 400.0));
        assert!(!tab_bar_needs_overflow(399.0, 400.0));
        // 差一点点就已经点不到最后一个 tab 了。
        assert!(tab_bar_needs_overflow(400.5, 400.0));
        assert!(tab_bar_needs_overflow(900.0, 300.0));
    }

    /// #455：放不下的 tab 要能被换进来，active 的那个永远不能被裁掉。
    #[test]
    fn the_active_tab_stays_visible_in_a_crowded_bar() {
        let widths = [100.0_f32; 8];
        // 全放得下（800 的条）。
        assert_eq!(visible_tab_range(&widths, 900.0, 5), (0, 8));
        // 只放得下 3 个时，active 在前 3 个里就是普通窗口。
        assert_eq!(visible_tab_range(&widths, 320.0, 1), (0, 3));
        // active 在第 6 个：窗口整体后移，让它露出来。
        let (from, to) = visible_tab_range(&widths, 320.0, 5);
        assert_eq!(to, 6, "active tab must be inside the window");
        assert!(from <= 5);
        assert_eq!(visible_tab_range(&widths, 250.0, 7), (6, 8));
        // 极端窄条：至少画一个，不会出现没有标签的 leaf。
        assert_eq!(visible_tab_range(&widths, 10.0, 4), (4, 5));
        // 空 leaf（理论上不该出现）不 panic。
        assert_eq!(visible_tab_range(&[], 100.0, 0), (0, 0));
    }

    /// #454：编辑器的调试开关不能漏进 Game view。
    #[test]
    fn game_view_never_inherits_the_editor_debug_toggles() {
        use prism_engine::render_settings::{RenderMode, RenderSettings};
        let editor = RenderSettings {
            show_grid: true,
            debug_flags: 0b1010,
            debug_rt: 2,
            render_mode: RenderMode::PathTrace,
            tonemap_mode: 2,
            ..Default::default()
        };

        let game = initial_view_settings(ViewKind::Game, &editor);
        assert!(!game.show_grid, "grid is a scene-view helper");
        assert_eq!(game.debug_flags, 0, "PBR isolate is a scene-view helper");
        assert_eq!(game.debug_rt, 0, "debug RT is a scene-view helper");

        // Scene view 反过来：它就该从工具栏当前状态出发。
        let scene = initial_view_settings(ViewKind::Scene, &editor);
        assert!(scene.show_grid);
        assert_eq!(scene.debug_flags, 0b1010);
        assert_eq!(scene.debug_rt, 2);
    }

    #[test]
    fn dock_zone_uses_quarter_bands() {
        use prism_editor_core::dock::DockZone;
        let rect = rect();
        assert_eq!(dock_zone(rect, egui::pos2(110.0, 250.0)), DockZone::Left);
        assert_eq!(dock_zone(rect, egui::pos2(490.0, 250.0)), DockZone::Right);
        assert_eq!(dock_zone(rect, egui::pos2(300.0, 110.0)), DockZone::Top);
        assert_eq!(dock_zone(rect, egui::pos2(300.0, 390.0)), DockZone::Bottom);
        assert_eq!(dock_zone(rect, egui::pos2(300.0, 250.0)), DockZone::Center);
    }

    #[test]
    fn transform_paths_split_only_supported_fields_and_axes() {
        assert_eq!(split_axis("position[2]"), Some(("position", 2)));
        assert_eq!(split_axis("rotation"), Some(("rotation", 0)));
        assert_eq!(split_axis("scale[bad]"), None);
        assert_eq!(split_axis("name"), None);
    }

    #[test]
    fn render_settings_round_trip_and_old_state_uses_grid_fallback() {
        let mut settings = prism_engine::render_settings::RenderSettings::default();
        settings.show_grid = true;
        settings.pt_max_bounces = 7;
        settings.pt_ray_max_distance = 123.5;
        // #502 调色参数也要能存进项目文件。
        settings.contrast = 1.4;
        settings.saturation = 0.6;
        settings.vignette = 0.3;
        settings.shadow_distance = 90.0;
        let value = render_settings_json(&settings);
        let loaded = render_settings_from_json(&value, false);
        assert_eq!(loaded.show_grid, true);
        assert_eq!(loaded.pt_max_bounces, 7);
        assert_eq!(loaded.pt_ray_max_distance, 123.5);
        assert_eq!(loaded.contrast, 1.4);
        assert_eq!(loaded.saturation, 0.6);
        assert_eq!(loaded.vignette, 0.3);
        assert_eq!(loaded.shadow_distance, 90.0);
        assert_eq!(
            render_settings_from_json(&serde_json::json!({}), true).show_grid,
            true
        );
    }

    #[test]
    fn workspace_hashes_change_with_document_and_selection() {
        let mut workspace = Workspace::sample();
        let document_before = document_hash(&workspace);
        let selection_before = selection_hash(&workspace);
        workspace.add_entity("Changed");
        assert_ne!(document_hash(&workspace), document_before);
        workspace.select(0, false);
        assert_ne!(selection_hash(&workspace), selection_before);
    }

    #[test]
    fn game_view_reads_the_project_window_resolution() {
        let root = std::env::temp_dir().join(format!(
            "prisma-editor-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(
            root.join("assets/settings.ini"),
            "[window]\nwidth = 1600\nheight = 900\n",
        )
        .unwrap();
        assert_eq!(project_window_resolution(Some(&root)), Some((1600, 900)));
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(project_window_resolution(None), None);
    }
}

/// `position[1]` → (`position`, 1)；没有下标就是 (`position`, 0)。
/// Scene 视图浮层工具条的位置（#421）：左上角 8px，宽度随折叠状态变化。
///
/// 窄视口下宽度也不会超过视口，避免浮层盖住整块画面。
fn scene_overlay_rect(viewport: Rect, collapsed: bool) -> Rect {
    let width: f32 = if collapsed { 30.0 } else { 360.0 };
    let available = (viewport.width() - 16.0).max(30.0);
    Rect::from_min_size(
        viewport.min + egui::vec2(8.0, 8.0),
        egui::vec2(width.min(available), 26.0),
    )
}

fn split_axis(path: &str) -> Option<(&str, usize)> {
    match path.split_once('[') {
        Some((field, rest)) => {
            let axis = rest.trim_end_matches(']').parse::<usize>().ok()?;
            Some((field, axis))
        }
        None => match path {
            "position" | "rotation" | "scale" => Some((path, 0)),
            _ => None,
        },
    }
}

/// 还没到「画面稳定」帧的截图请求。
struct PendingCapture {
    target: crate::screenshot::CaptureTarget,
    path: PathBuf,
    size: Option<[u32; 2]>,
    ready_at: u64,
    reply: std::sync::mpsc::Sender<serde_json::Value>,
}

/// 根窗口的 `HWND`（只有 Windows 有意义，其它平台返回 `None`）。
fn main_window_hwnd(window: &egui_ash::winit::window::Window) -> Option<isize> {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = window.window_handle().ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        None
    }
}

/// 把工程 `assets/settings.ini` 的 `[render]` 应用到 Game View。
///
/// 独立构建启动时走的正是这条链路：`AppConfig { render }` → `RenderSettings`
/// （`prism_app::App::build`）。Game View 用同一份默认值，Play 看到的画面才和
/// 打包产物一致。文件不存在（老工程）时保持引擎默认值，不报错。
/// 新视口启动时的渲染设置。
///
/// Game view 是「独立构建跑起来的样子」：**不**继承编辑器视口的调试开关——
/// 网格（`show_grid`）、PBR 分量 isolate（`debug_flags`）、debug RT（`debug_rt`）
/// 以及调试用的 `render_mode` 都只属于 Scene 视图。否则在编辑器里打开网格，
/// 按 Play 之后运行时画面里也有网格，和发布产物对不上（#454）。
fn initial_view_settings(
    kind: ViewKind,
    editor: &prism_engine::render_settings::RenderSettings,
) -> prism_engine::render_settings::RenderSettings {
    match kind {
        ViewKind::Scene => editor.clone(),
        // `pt_*` 是真实渲染参数，由工程 `settings.ini` 或引擎默认值决定；
        // 这里从默认值出发，`apply_project_render_defaults` 再套工程配置。
        ViewKind::Game => prism_engine::render_settings::RenderSettings {
            // 只有「两边都该有」的调试项才继承：目前一个都没有。
            ..Default::default()
        },
    }
}

/// 工程在 `assets/settings.ini [window]` 里声明的目标分辨率（#585）。
///
/// 文件不存在或尺寸为 0 时返回 `None`，Game 视图保持旧的 `Match Window` 行为。
fn project_window_resolution(project_root: Option<&Path>) -> Option<(u32, u32)> {
    let root = project_root?;
    let path = root.join("assets").join("settings.ini");
    let text = std::fs::read_to_string(path).ok()?;
    let config = prism_engine::config::AppConfig::from_ini(&text);
    (config.window.width > 0 && config.window.height > 0)
        .then_some((config.window.width, config.window.height))
}

fn apply_project_render_defaults(
    host: &mut crate::engine_host::EngineHost,
    project_root: Option<&Path>,
) {
    let Some(root) = project_root else {
        return;
    };
    let path = root.join("assets").join("settings.ini");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            log::debug!(
                "game view: no project render settings at {}: {error}",
                path.display()
            );
            return;
        }
    };
    let config = prism_engine::config::AppConfig::from_ini(&text);
    config.render.apply(host.settings_mut());
    log::info!("game view render defaults: {}", path.display());
}

/// 控制通道排进 `pending_input` 的一个输入事件。
enum InjectedInput {
    Key(prism_engine::input::KeyCode, bool),
    Mouse(prism_engine::input::MouseButton, bool),
    /// 把光标挪到引擎绘图区像素坐标（`ui.click` 的 Game view 分支用）。
    Cursor(f32, f32),
    /// 滚轮增量（`ui.wheel` 的 Game view 分支用）。
    Scroll(f64),
}

impl InjectedInput {
    fn apply(self, input: &mut prism_engine::input::InputManager) {
        use prism_engine::input::ElementState;
        match self {
            Self::Key(key, pressed) => input.handle_keyboard(
                key,
                if pressed {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                },
            ),
            Self::Mouse(button, pressed) => input.handle_mouse_button(
                button,
                if pressed {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                },
            ),
            Self::Cursor(x, y) => input.set_mouse_position([x as f64, y as f64]),
            Self::Scroll(delta) => input.handle_scroll(delta),
        }
    }
}

/// `ui.click` 的 `offset`：`[dx, dy]` 两个有限数。
fn parse_offset(value: &serde_json::Value) -> Option<[f32; 2]> {
    let array = value.as_array()?;
    if array.len() != 2 {
        return None;
    }
    let x = array[0].as_f64()? as f32;
    let y = array[1].as_f64()? as f32;
    Some([x, y])
}

/// 控制通道里的按键名 → `KeyCode`。大小写不敏感，覆盖自动化常用的那些。
fn parse_key(name: &str) -> Option<prism_engine::input::KeyCode> {
    use prism_engine::input::KeyCode;
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "w" => KeyCode::KeyW,
        "a" => KeyCode::KeyA,
        "s" => KeyCode::KeyS,
        "d" => KeyCode::KeyD,
        "q" => KeyCode::KeyQ,
        "e" => KeyCode::KeyE,
        "space" | " " => KeyCode::Space,
        "shift" | "shiftleft" => KeyCode::ShiftLeft,
        "shiftright" => KeyCode::ShiftRight,
        "ctrl" | "control" | "controlleft" => KeyCode::ControlLeft,
        "controlright" => KeyCode::ControlRight,
        "alt" | "altleft" => KeyCode::AltLeft,
        "altright" => KeyCode::AltRight,
        "escape" | "esc" => KeyCode::Escape,
        "tab" => KeyCode::Tab,
        "enter" | "return" => KeyCode::Enter,
        "up" | "arrowup" => KeyCode::ArrowUp,
        "down" | "arrowdown" => KeyCode::ArrowDown,
        "left" | "arrowleft" => KeyCode::ArrowLeft,
        "right" | "arrowright" => KeyCode::ArrowRight,
        "0" => KeyCode::Digit0,
        "1" => KeyCode::Digit1,
        "2" => KeyCode::Digit2,
        "3" => KeyCode::Digit3,
        "4" => KeyCode::Digit4,
        "5" => KeyCode::Digit5,
        "6" => KeyCode::Digit6,
        "7" => KeyCode::Digit7,
        "8" => KeyCode::Digit8,
        "9" => KeyCode::Digit9,
        _ => return None,
    })
}

fn parse_mouse(name: &str) -> Option<prism_engine::input::MouseButton> {
    use prism_engine::input::MouseButton;
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => return None,
    })
}

/// `Active` 组件（作者字段）在文档里是否为真；缺省 `true`。
fn scene_entity_visible(entity: &prism_editor_core::EntityJson) -> bool {
    match entity.components.get("prism_engine::scene::Active") {
        Some(serde_json::Value::Bool(value)) => *value,
        // `Active` 是 newtype，反射编码下可能落到 `{"0": true}`。
        Some(value) => value
            .get("0")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        None => true,
    }
}

/// Game 视图里那台工程相机的帧（正交/透视都支持）。
///
/// 透视沿用编辑器一贯的 60° 垂直 FOV（`CameraFrame` 没有 fov 字段，见 #601）。
fn game_camera_frame(
    world: &prism_engine::ecs::World,
) -> Option<prism_editor_core::viewport_math::CameraFrame> {
    use prism_engine::scene::components::{Camera, LocalToWorld};
    use prism_engine::scene::systems::camera;

    let entity = camera::active_camera(world)?;
    let camera = world.get::<Camera>(entity)?;
    let matrix = world.get::<LocalToWorld>(entity)?.0;
    let eye = matrix.w_axis.truncate();
    let right = matrix.x_axis.truncate().normalize_or_zero();
    let up = matrix.y_axis.truncate().normalize_or_zero();
    let forward = -matrix.z_axis.truncate().normalize_or_zero();
    Some(prism_editor_core::viewport_math::CameraFrame {
        fov_y_degrees: 60.0,
        eye: eye.to_array(),
        right: right.to_array(),
        up: up.to_array(),
        forward: forward.to_array(),
        ortho_half_height: camera
            .orthographic
            .then(|| camera.orthographic_size.max(1e-4)),
    })
}

/// Game 视图里的物体：引擎世界里带 `Name` 的实体（换场/生成的道具也算），
/// 投影到视口局部像素。
fn game_view_objects(view: &ViewportState, rect: egui::Rect) -> Vec<crate::ui_tree::ViewObject> {
    use prism_engine::scene::components::{LocalToWorld, Name};
    use prism_engine::scene::runtime_components::SceneEntityId;
    use prism_engine::scene::systems::hierarchy::is_active_in_hierarchy;

    let world = view.host.world();
    let Some(frame) = game_camera_frame(world) else {
        return Vec::new();
    };
    let mut objects: Vec<crate::ui_tree::ViewObject> = world
        .query::<LocalToWorld>()
        .filter_map(|(entity, matrix)| {
            let name = world.get::<Name>(entity)?.0.clone();
            let world_pos = matrix.0.w_axis.truncate().to_array();
            let projected = prism_editor_core::viewport_math::project(
                world_pos,
                &frame,
                rect.width(),
                rect.height(),
            );
            let scene_id = world.get::<SceneEntityId>(entity).map(|id| id.0);
            Some(crate::ui_tree::ViewObject {
                node: match scene_id {
                    Some(id) => crate::ui_tree::view_object_node("gameView", id),
                    None => crate::ui_tree::view_object_node_ecs("gameView", entity.id()),
                },
                entity: scene_id,
                name,
                world: world_pos,
                screen: projected.map(|(x, y, _)| [x, y]),
                depth: projected.map(|(_, _, depth)| depth),
                visible: is_active_in_hierarchy(world, entity),
            })
        })
        .collect();
    objects.sort_by(|a, b| a.node.cmp(&b.node));
    objects
}
