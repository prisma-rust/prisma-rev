//! Scene / Game 视口：**零拷贝**。
//!
//! 引擎把帧渲染进自己的离屏 `VkImage`（`GraphRenderer` 的 `OffscreenTarget`），
//! 我们只把这个图像的 `VkImageView` + 一个线性采样器注册到 egui-ash 的
//! `ImageRegistry`，然后用 `painter.image()` 在同一块 `VkDevice`、同一条命令
//! 缓冲区里采样 —— 与引擎的 `Camera.targetTexture` + `GUI.DrawTexture`
//! 是同一个模型，没有 GPU→CPU 回读。
//!
//! 帧节奏：只在「文档变化 / 相机变化 / Play / resize / 首帧」时重渲染，
//! 静止的视口零 GPU 工作（保留上一帧纹理）。

use std::collections::HashSet;
use std::time::Instant;

use ash::vk;
use egui::{Align, Layout, Rect, Sense, Vec2};
use egui_ash::ImageRegistry;
use prism_editor_core::{viewport_math as vm, SelectMode, TransformJson, Workspace};
use prism_engine::input::{ElementState, KeyCode, MouseButton};

use crate::engine_host::{EngineHost, ViewKind};

fn pan_camera(camera: &mut prism_editor_core::workspace::OrbitCamera, dx: f32, dy: f32) {
    let (sin_yaw, cos_yaw) = camera.yaw.sin_cos();
    let (sin_pitch, cos_pitch) = camera.pitch.sin_cos();
    let right = [cos_yaw, 0.0, -sin_yaw];
    let up = [-sin_pitch * sin_yaw, cos_pitch, -sin_pitch * cos_yaw];
    let scale = camera.distance * 0.0015;
    for axis in 0..3 {
        camera.target[axis] += (-dx * right[axis] + dy * up[axis]) * scale;
    }
}

/// 这次拖动要不要吸附：工具栏开关 ⊕ Ctrl（按住 Ctrl 临时反转，Unity 同款）。
fn snap_active(enabled: bool, modifier: bool) -> bool {
    enabled != modifier
}

/// 视口角标里的光源预算文本（#468）。
///
/// 前向渲染的局部光 SSBO 只有 `LIGHT_MAX` 个槽位，超出的按重要性挑并丢弃。
/// 没有任何局部光时返回 `None`（不占角标位置）。
fn light_budget_label(kept: usize, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    if kept < total {
        Some(format!("Lights {kept}/{total} ({} dropped)", total - kept))
    } else {
        Some(format!("Lights {total}"))
    }
}

/// 平移拖动这一帧要应用的增量（沿 gizmo 轴，世界单位）。
///
/// 吸附按「起始坐标 + 累计量」取整再减回起点 —— 逐帧取整会把慢拖的每一帧
/// 都抹成 0，鼠标松开前物体一动不动。
fn drag_translate_step(
    snap: prism_editor_core::workspace::SnapSettings,
    snapping: bool,
    start_axis_value: f32,
    accum: f32,
    applied: f32,
) -> f32 {
    let desired = if snapping {
        snap.snapped_translate(start_axis_value + accum) - start_axis_value
    } else {
        accum
    };
    desired - applied
}

/// 组件图标是否显示（#416）：相机 / 光源可以各自关掉，互不影响。
fn icon_visible(workspace: &Workspace, is_camera: bool, is_light: bool) -> bool {
    (is_camera && workspace.show_camera_icons) || (is_light && workspace.show_light_icons)
}

/// 方向 gizmo 的中心（视口右上角，屏幕坐标）与半径（#416）。
fn orientation_gizmo_center(rect: Rect) -> (f32, f32) {
    (rect.max.x - 46.0, rect.min.y + 46.0)
}

const ORIENTATION_GIZMO_RADIUS: f32 = 30.0;

fn orientation_axis_color(axis: usize) -> (u8, u8, u8) {
    match axis {
        0 => (214, 104, 104),
        1 => (120, 196, 120),
        _ => (104, 150, 232),
    }
}

/// 右上角的世界轴方向 gizmo（#416）。
///
/// 复用 `viewport_math` 里早已写好、但一直没人调用的 `orientation_axis_end`：
/// 背向相机的轴先画且更淡，正对相机的轴后画，点一下就把相机对齐到那根轴。
fn draw_orientation_gizmo(painter: &egui::Painter, rect: Rect, frame: &vm::CameraFrame) {
    use egui::Color32;
    let center = orientation_gizmo_center(rect);
    painter.circle_filled(
        egui::pos2(center.0, center.1),
        24.0,
        Color32::from_rgba_unmultiplied(16, 18, 22, 130),
    );
    let mut axes: Vec<(usize, f32, f32)> = Vec::new();
    for axis in 0..3 {
        for sign in [1.0_f32, -1.0] {
            let end = vm::orientation_axis_end(frame, axis, sign, center, ORIENTATION_GIZMO_RADIUS);
            // 正对相机的轴投影长度为 0，画出来只是一个点。
            if (end.0 - center.0).abs() + (end.1 - center.1).abs() < 1e-3 {
                continue;
            }
            let world = match axis {
                0 => [sign, 0.0, 0.0],
                1 => [0.0, sign, 0.0],
                _ => [0.0, 0.0, sign],
            };
            axes.push((axis, sign, vm::dot(world, frame.forward)));
        }
    }
    axes.sort_by(|a, b| a.2.total_cmp(&b.2));
    for (axis, sign, depth) in axes {
        let end = vm::orientation_axis_end(frame, axis, sign, center, ORIENTATION_GIZMO_RADIUS);
        let (r, g, b) = orientation_axis_color(axis);
        let alpha = if depth > 0.15 {
            110
        } else if depth < -0.15 {
            235
        } else {
            170
        };
        let position = egui::pos2(end.0, end.1);
        painter.circle_filled(
            position,
            7.0,
            Color32::from_rgba_unmultiplied(r, g, b, alpha),
        );
        painter.text(
            position + egui::vec2(9.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "{}{}",
                if sign < 0.0 { "-" } else { "+" },
                ["X", "Y", "Z"][axis]
            ),
            egui::FontId::proportional(9.5),
            Color32::from_rgba_unmultiplied(r, g, b, alpha),
        );
    }
}

/// 同一位置连续点击的点选循环（#416）。
#[derive(Clone, Debug, PartialEq)]
struct PickCycle {
    point: (f32, f32),
    candidates: Vec<usize>,
    index: usize,
}

/// 下一次点击该选谁：同一位置 + 同一候选集 → 下一层；否则回到第一层。
fn next_pick(
    cycle: Option<&PickCycle>,
    point: (f32, f32),
    candidates: Vec<usize>,
) -> Option<(usize, PickCycle)> {
    if candidates.is_empty() {
        return None;
    }
    let repeated = cycle
        .map(|cycle| {
            (cycle.point.0 - point.0).abs() <= 4.0
                && (cycle.point.1 - point.1).abs() <= 4.0
                && cycle.candidates == candidates
        })
        .unwrap_or(false);
    let index = if repeated {
        (cycle.map(|cycle| cycle.index).unwrap_or(0) + 1) % candidates.len()
    } else {
        0
    };
    Some((
        candidates[index],
        PickCycle {
            point,
            candidates,
            index,
        },
    ))
}

/// 共享给视口的 GPU 句柄（设备 + egui 纹理表 + 采样器）。
pub struct ImageBridge {
    pub registry: ImageRegistry,
    pub sampler: vk::Sampler,
}

struct ViewportTexture {
    id: egui::TextureId,
}

/// 离屏目标尺寸：points × pixels_per_point × 倍率，夹在 `1..=4096`。
///
/// `scale` 是「渲染分辨率倍率」：高 DPI 屏上可以降到 0.5–1.0 省 GPU，
/// 只影响离屏目标大小，不影响 UI 布局的 points 尺寸。
pub fn viewport_extent(width: f32, height: f32, ppp: f32, scale: f32) -> (u32, u32) {
    let scale = scale.clamp(0.25, 2.0);
    (
        ((width * ppp * scale).round() as u32).clamp(1, 4096),
        ((height * ppp * scale).round() as u32).clamp(1, 4096),
    )
}

/// 面板内位置（egui points，左上原点）→ 离屏渲染目标的像素坐标。
///
/// 只能按 `extent / rect` 缩放，不能直接乘 `pixels_per_point × scale`：
/// [`viewport_extent`] 会把每个维度夹到 4096，而 2560 points × DPR 2 就已经
/// 5120 了。直接乘会让 `InputSnapshot::cursor` 跑出 `screen` 之外，
/// 按光标做瞄准 / UI 命中的 gameplay 代码会整体偏移（#405）。
pub(crate) fn game_cursor(rect: Rect, position: egui::Pos2, extent: (u32, u32)) -> [f32; 2] {
    let width = rect.width().max(1.0);
    let height = rect.height().max(1.0);
    [
        ((position.x - rect.min.x) / width) * extent.0 as f32,
        ((position.y - rect.min.y) / height) * extent.1 as f32,
    ]
}

/// Game 视图的预览分辨率。
///
/// `MatchWindow` 是旧行为：渲染目标跟着面板走，于是同一份 UI 在不同面板大小下
/// 看到的比例 / 字号都不一样。其余选项让渲染目标与面板**解耦**——面板只决定这张图
/// 在屏幕上占多大（等比 letterbox），构图和 UI 尺寸与目标设备一致（#463）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GameResolution {
    #[default]
    MatchWindow,
    /// 固定渲染分辨率（像素）。
    Fixed { width: u32, height: u32 },
    /// 固定宽高比，尺寸随面板缩放（#585）。
    ///
    /// 与 [`Fixed`](Self::Fixed) 的区别是分辨率不锁死：只保证
    /// `width / height` 不变，适合「按 16:9 构图，但面板多大就渲染多大」。
    Aspect { width: u32, height: u32 },
}

impl GameResolution {
    pub fn label(self) -> String {
        match self {
            GameResolution::MatchWindow => "Match Window".to_string(),
            GameResolution::Fixed { width, height } => format!("{width}x{height}"),
            GameResolution::Aspect { width, height } => {
                format!("{width}:{height} (fit)")
            }
        }
    }

    /// 渲染目标尺寸。`MatchWindow` 用面板算出来的尺寸。
    pub fn extent(self, panel_extent: (u32, u32)) -> (u32, u32) {
        match self {
            GameResolution::MatchWindow => panel_extent,
            // 与 `viewport_extent` 同一个上限：超过 4096 的离屏目标很多驱动直接失败。
            GameResolution::Fixed { width, height } => {
                (width.clamp(1, 4096), height.clamp(1, 4096))
            }
            GameResolution::Aspect { width, height } => {
                let ratio = width.max(1) as f32 / height.max(1) as f32;
                let (panel_w, panel_h) =
                    (panel_extent.0.max(1) as f32, panel_extent.1.max(1) as f32);
                let (target_w, target_h) = if panel_w / panel_h > ratio {
                    (panel_h * ratio, panel_h)
                } else {
                    (panel_w, panel_w / ratio)
                };
                (
                    (target_w.round() as u32).clamp(1, 4096),
                    (target_h.round() as u32).clamp(1, 4096),
                )
            }
        }
    }
}

/// Game 视图工具栏里列出的预设（横屏 / 竖屏 / 常见设备）。
pub const GAME_RESOLUTION_PRESETS: &[GameResolution] = &[
    GameResolution::MatchWindow,
    GameResolution::Fixed {
        width: 1920,
        height: 1080,
    },
    GameResolution::Fixed {
        width: 1280,
        height: 720,
    },
    GameResolution::Fixed {
        width: 2560,
        height: 1440,
    },
    GameResolution::Fixed {
        width: 1080,
        height: 1920,
    },
    GameResolution::Fixed {
        width: 2400,
        height: 1080,
    },
    GameResolution::Fixed {
        width: 750,
        height: 1334,
    },
];

/// 「只锁比例、不锁分辨率」的预设（#585）。
pub const GAME_ASPECT_PRESETS: &[GameResolution] = &[
    GameResolution::Aspect {
        width: 16,
        height: 9,
    },
    GameResolution::Aspect {
        width: 16,
        height: 10,
    },
    GameResolution::Aspect {
        width: 4,
        height: 3,
    },
    GameResolution::Aspect {
        width: 21,
        height: 9,
    },
];

/// 工具栏下拉的完整列表：工程分辨率排在最前，其次比例预设，最后固定分辨率。
///
/// `project` 来自 `assets/settings.ini [window]`；没有工程文件时保持旧列表。
pub fn game_resolution_presets(project: Option<(u32, u32)>) -> Vec<GameResolution> {
    let mut out = vec![GameResolution::MatchWindow];
    if let Some((width, height)) = project.filter(|(w, h)| *w > 0 && *h > 0) {
        out.push(GameResolution::Fixed { width, height });
    }
    out.extend_from_slice(GAME_ASPECT_PRESETS);
    for preset in GAME_RESOLUTION_PRESETS.iter().skip(1) {
        if !out.contains(preset) {
            out.push(*preset);
        }
    }
    out
}

/// 把一次拖动拆成 `steps` 个采样点（不含起点），供控制通道的 `ui.drag` 使用。
///
/// 与真实拖动一样按增量推进：每步的位移是相邻采样点之差，所以吸附、旋转角度
/// 这类依赖「一步一步累积」的逻辑在脚本里也能复现（#643）。
pub(crate) fn drag_samples(from: (f32, f32), to: (f32, f32), steps: u32) -> Vec<(f32, f32)> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|step| {
            let t = step as f32 / steps as f32;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// 把 `extent` 的图等比放进 `outer`，居中留黑边（Unity Game 视图的 letterbox）。
///
/// 面板比例与目标比例不一致时，**不拉伸**：拉伸会让 16:9 的构图在 4:3 面板上
/// 变形，等于又回到「编辑器里看到的不等于设备上看到的」（#463）。
pub fn letterbox(outer: Rect, extent: (u32, u32)) -> Rect {
    if extent.0 == 0 || extent.1 == 0 || outer.width() <= 0.0 || outer.height() <= 0.0 {
        return outer;
    }
    let target = extent.0 as f32 / extent.1 as f32;
    let panel = outer.width() / outer.height();
    if panel > target {
        // 面板更宽：以高度为准，左右留边。
        let width = outer.height() * target;
        Rect::from_center_size(outer.center(), Vec2::new(width, outer.height()))
    } else {
        // 面板更高：以宽度为准，上下留边。
        let height = outer.width() / target;
        Rect::from_center_size(outer.center(), Vec2::new(outer.width(), height))
    }
}

/// 引擎的 W / E / R 工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GizmoTool {
    #[default]
    Translate,
    Rotate,
    Scale,
}

impl GizmoTool {
    pub fn label(self) -> &'static str {
        match self {
            GizmoTool::Translate => "Move",
            GizmoTool::Rotate => "Rotate",
            GizmoTool::Scale => "Scale",
        }
    }
}

struct GizmoDrag {
    /// 本次拖动作用的整组实体（#416：多选时批量变换）。
    targets: Vec<u32>,
    axis: usize,
    /// 每个实体的 `(id, 本地缩放轴, 起始缩放)`，缩放在各自轴心上应用。
    scale_targets: Vec<(u32, usize, f32)>,
    screen_dir: [f32; 2],
    px_per_unit: f32,
    /// 旋转拖动：上一帧的光标角度（屏幕空间）。
    last_angle: f32,
    /// 缩放拖动：起始缩放分量。
    start_scale: f32,
    /// 平移拖动：开始时沿 gizmo 轴的世界坐标（吸附要按绝对值取整）。
    start_axis_value: f32,
    /// 本次拖动累计的原始量（平移 = 世界单位 / 旋转 = 弧度 / 缩放 = 倍数）。
    accum: f32,
    /// 已经应用到实体上的量（吸附后），用来算这一帧还差多少。
    applied: f32,
}

/// 一个视口面板的状态。
pub struct ViewportState {
    pub host: EngineHost,
    texture: Option<ViewportTexture>,
    registered_view: vk::ImageView,
    extent: (u32, u32),
    /// 需要重渲染（文档 / 相机 / 选中变化）。
    pub dirty: bool,
    flying_camera: bool,
    pub error: Option<String>,
    pub last_frame_ms: f32,
    pub fps: f32,
    pub frames: u64,
    fps_window_started: Instant,
    fps_window_frames: u32,
    /// 当前 gizmo 工具（W / E / R）。
    pub tool: GizmoTool,
    gizmo: Option<GizmoDrag>,
    orbit_last_pos: Option<egui::Pos2>,
    /// 框选起点 / 当前点（视口局部坐标）。
    box_start: Option<egui::Pos2>,
    box_current: Option<egui::Pos2>,
    /// 同一位置连续点击时的候选栈（#416 点选循环穿透）。
    pick_cycle: Option<PickCycle>,
    /// Game view 转发给引擎 `InputManager` 的按住键（用于合成 release）。
    forwarded_keys: HashSet<KeyCode>,
    /// Game view 是否已经把左键当成按住转发（离开视口也能收到 release）。
    forwarded_left_down: bool,
    /// 渲染图报告的输出路径（只有 Scene view 设；`None` = 不写）。
    pub report_path: Option<std::path::PathBuf>,
    /// Game 视图的预览分辨率（Scene 视图忽略它）。
    pub target: GameResolution,
    /// 工程 `assets/settings.ini [window]` 的分辨率，用于默认值与下拉预设（#585）。
    pub project_resolution: Option<(u32, u32)>,
}

pub struct ViewportResponse {
    /// 文档被视口交互改动了（gizmo 拖动）。
    pub document_changed: bool,
}

impl ViewportState {
    pub fn new(
        kind: ViewKind,
        context: std::sync::Arc<prism_render::VulkanContext>,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let host = EngineHost::new(kind, context, width, height)?;
        Ok(Self {
            host,
            texture: None,
            registered_view: vk::ImageView::null(),
            extent: (0, 0),
            dirty: true,
            flying_camera: false,
            error: None,
            last_frame_ms: 0.0,
            fps: 0.0,
            frames: 0,
            fps_window_started: Instant::now(),
            fps_window_frames: 0,
            tool: GizmoTool::Translate,
            gizmo: None,
            orbit_last_pos: None,
            box_start: None,
            box_current: None,
            pick_cycle: None,
            forwarded_keys: HashSet::new(),
            forwarded_left_down: false,
            report_path: None,
            target: GameResolution::default(),
            project_resolution: None,
        })
    }

    pub fn kind(&self) -> ViewKind {
        self.host.kind()
    }

    pub fn flying_camera(&self) -> bool {
        self.flying_camera
    }

    /// 把 egui 的键盘 / 鼠标状态转成引擎 `InputManager` 事件。
    ///
    /// Game view 的项目模块通过 `InputSnapshot` 读输入；Scene view 的
    /// 轨道相机不走这里。只在 Play 中转发，停止 Play 时清空遗留状态。
    fn forward_game_input(&mut self, ui: &egui::Ui, rect: Rect, response: &egui::Response) {
        if self.kind() != ViewKind::Game || !self.host.is_simulating() {
            self.forwarded_keys.clear();
            self.forwarded_left_down = false;
            return;
        }

        let (mut keys_down, modifiers, hover_pos, primary_down) = ui.input(|input| {
            let keys = input
                .keys_down
                .iter()
                .filter_map(|key| engine_key(*key))
                .collect::<HashSet<_>>();
            (
                keys,
                input.modifiers,
                input.pointer.hover_pos(),
                input.pointer.primary_down(),
            )
        });
        // egui 的 `Key` 不区分左右修饰键；按修饰状态补成 ABI 的 Left 位。
        if modifiers.shift {
            keys_down.insert(KeyCode::ShiftLeft);
        }
        if modifiers.ctrl {
            keys_down.insert(KeyCode::ControlLeft);
        }
        if modifiers.alt {
            keys_down.insert(KeyCode::AltLeft);
        }

        if let Some(position) = hover_pos {
            if rect.contains(position) {
                let local = game_cursor(rect, position, self.extent);
                self.host
                    .input_mut()
                    .set_mouse_position([local[0] as f64, local[1] as f64]);
            }
        }

        // 左键：第一次按下必须发生在视口里；一旦开始转发，松开可以在视口外。
        let held = if self.forwarded_left_down {
            primary_down
        } else {
            primary_down && response.hovered()
        };
        if held != self.forwarded_left_down {
            let state = if held {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            self.host
                .input_mut()
                .handle_mouse_button(MouseButton::Left, state);
            self.forwarded_left_down = held;
        }

        let pressed: Vec<KeyCode> = keys_down
            .difference(&self.forwarded_keys)
            .copied()
            .collect();
        let released: Vec<KeyCode> = self
            .forwarded_keys
            .difference(&keys_down)
            .copied()
            .collect();
        for key in pressed {
            self.host
                .input_mut()
                .handle_keyboard(key, ElementState::Pressed);
        }
        for key in released {
            self.host
                .input_mut()
                .handle_keyboard(key, ElementState::Released);
        }
        self.forwarded_keys = keys_down;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 停止 Play 时清空转发记录：下一次 Play 的按键会重新合成 Pressed。
    pub fn reset_forwarded_input(&mut self) {
        self.forwarded_keys.clear();
        self.forwarded_left_down = false;
    }

    /// 重新渲染并（必要时）重新注册纹理。返回是否成功出帧。
    pub fn render_frame(&mut self, bridge: &ImageBridge) -> bool {
        if let Err(error) = self.relayout(bridge) {
            self.error = Some(format!("{error:#}"));
            return false;
        }
        let started = Instant::now();
        match self.host.render() {
            Ok(Some((view, extent))) => {
                self.extent = (extent.width, extent.height);
                if view != self.registered_view {
                    if let Some(old) = self.texture.take() {
                        bridge.registry.unregister_user_texture(old.id);
                    }
                    let id = bridge.registry.register_user_texture(view, bridge.sampler);
                    self.texture = Some(ViewportTexture { id });
                    self.registered_view = view;
                }
                self.last_frame_ms = started.elapsed().as_secs_f32() * 1000.0;
                self.frames += 1;
                self.fps_window_frames += 1;
                let elapsed = self.fps_window_started.elapsed().as_secs_f32();
                if elapsed >= 0.25 {
                    self.fps = self.fps_window_frames as f32 / elapsed;
                    self.fps_window_frames = 0;
                    self.fps_window_started = Instant::now();
                }
                self.dirty = false;
                self.error = None;
                self.maybe_write_render_graph_report();
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                false
            }
        }
    }

    /// 首帧 + 每 `REFRESH_INTERVAL_FRAMES` 帧把渲染图写盘，分析面板据此画图。
    ///
    /// 节流与 `prism-app` 的报告写入器一致（Play 时视口会连续出帧）。
    fn maybe_write_render_graph_report(&mut self) {
        let Some(path) = self.report_path.clone() else {
            return;
        };
        let interval = prism_render::render_graph_report::REFRESH_INTERVAL_FRAMES;
        if self.frames != 1 && self.frames % interval != 0 {
            return;
        }
        if let Err(error) = self.host.write_render_graph_report(&path, self.frames) {
            self.error = Some(format!("渲染图报告写入失败: {error:#}"));
        }
    }

    /// 尺寸变化时重建离屏目标（视图随之变化 → 下次出帧会重新注册纹理）。
    fn relayout(&mut self, bridge: &ImageBridge) -> anyhow::Result<()> {
        let target = self.extent;
        if target.0 == 0 || target.1 == 0 {
            return Ok(());
        }
        let current = self
            .host
            .renderer()
            .offscreen_extent()
            .map(|extent| (extent.width, extent.height))
            .unwrap_or_default();
        if current == target {
            return Ok(());
        }
        self.host.resize(target.0, target.1)?;
        // 旧图像已经销毁；把注册的 id 换掉前先注销，避免采样悬空视图。
        if let Some(old) = self.texture.take() {
            bridge.registry.unregister_user_texture(old.id);
        }
        self.registered_view = vk::ImageView::null();
        Ok(())
    }

    /// 光标下的 gizmo 轴（`(轴, 屏幕方向, 每世界单位像素)`）；没有命中返回 `None`。
    ///
    /// 旋转环返回固定的 `(1,0) / 60` —— 角度由光标绕环心的角度算出，不用这组数。
    pub(crate) fn gizmo_hit_at(
        &self,
        workspace: &Workspace,
        frame: &vm::CameraFrame,
        rect: Rect,
        local: (f32, f32),
    ) -> Option<(usize, [f32; 2], f32)> {
        let center = workspace.gizmo_center_world()?;
        let basis = workspace.gizmo_basis();
        let world_length = vm::gizmo_world_length(&workspace.camera, rect.height());
        match self.tool {
            GizmoTool::Rotate => vm::pick_rotation_axis_oriented(
                local,
                frame,
                center,
                basis,
                world_length,
                rect.width(),
                rect.height(),
            )
            .map(|axis| (axis, [1.0f32, 0.0], 60.0)),
            GizmoTool::Translate | GizmoTool::Scale => vm::pick_gizmo_axis_oriented(
                local,
                frame,
                center,
                basis,
                world_length,
                rect.width(),
                rect.height(),
            ),
        }
    }

    /// 结束当前 gizmo 拖动（与 `show()` 里的松开路径一致，#643）。
    pub(crate) fn finish_gizmo_drag(&mut self, workspace: &mut Workspace) {
        if self.gizmo.take().is_some() {
            workspace.end_interaction();
        }
    }

    /// 起点是否命中 gizmo 轴；命中就开始一次 gizmo 拖动（#643）。
    ///
    /// 与 `show()` 里的真实拖动共用同一段逻辑，控制通道的 `ui.drag` 因此走的是
    /// 用户手势本身，而不是另写一套变换。
    pub(crate) fn begin_gizmo_drag(
        &mut self,
        workspace: &mut Workspace,
        frame: &vm::CameraFrame,
        rect: Rect,
        local: (f32, f32),
    ) -> bool {
        let Some(center) = workspace.gizmo_center_world() else {
            return false;
        };
        let Some(entity) = workspace
            .primary_selection()
            .and_then(|index| workspace.document().doc.id_of_index(index))
        else {
            return false;
        };
        let world_length = vm::gizmo_world_length(&workspace.camera, rect.height());
        let basis = workspace.gizmo_basis();
        let Some((axis, dir, px_per_unit)) = self.gizmo_hit_at(workspace, frame, rect, local)
        else {
            return false;
        };
        // 整组实体 + 每个实体的本地缩放轴 / 起始缩放（#416：多选批量变换）。
        let targets: Vec<u32> = workspace.selection_entity_ids();
        let scale_targets: Vec<(u32, usize, f32)> = targets
            .iter()
            .filter_map(|entity_id| {
                let index = workspace.document().doc.index_of_id(*entity_id)?;
                let scale_axis = workspace.gizmo_scale_axis(*entity_id, axis);
                let start = workspace.document().doc.entities[index].transform.scale[scale_axis];
                Some((*entity_id, scale_axis, start))
            })
            .collect();
        let start_scale = workspace.document().doc.entities
            [workspace.document().doc.index_of_id(entity).unwrap_or(0)]
        .transform
        .scale[workspace.gizmo_scale_axis(entity, axis)];
        let angle_center = vm::ring_screen_radius_oriented(
            frame,
            center,
            basis,
            axis,
            world_length,
            rect.width(),
            rect.height(),
        )
        .map(|(cx, cy, _)| (cx, cy))
        .unwrap_or((rect.width() * 0.5, rect.height() * 0.5));
        // 吸附要按绝对值取整，所以记下起点沿轴的投影；
        // 缩放拖动的 `applied` 从起始倍数开始，其它两种从 0 开始。
        let start_axis_value = {
            let index = workspace.document().doc.index_of_id(entity).unwrap_or(0);
            let position = vm::world_position(&workspace.document().doc, index);
            position[0] * basis[axis][0]
                + position[1] * basis[axis][1]
                + position[2] * basis[axis][2]
        };
        let applied = match self.tool {
            GizmoTool::Scale => start_scale,
            GizmoTool::Translate | GizmoTool::Rotate => 0.0,
        };
        self.gizmo = Some(GizmoDrag {
            targets,
            axis,
            scale_targets,
            screen_dir: dir,
            px_per_unit,
            last_angle: vm::screen_angle(local, angle_center),
            start_scale,
            start_axis_value,
            accum: 0.0,
            applied,
        });
        // 一次拖动 = 一个撤销步：交互期间忽略 800ms 合并窗口，
        // 中途停顿不会把同一次拖动拆成两步。
        workspace.begin_interaction();
        self.box_start = None;
        true
    }

    /// 应用一次 gizmo 拖动增量；返回是否真的改动了文档（#643）。
    pub(crate) fn gizmo_drag_step(
        &mut self,
        workspace: &mut Workspace,
        frame: &vm::CameraFrame,
        rect: Rect,
        local: (f32, f32),
        delta: (f32, f32),
        ctrl: bool,
    ) -> bool {
        let tool = self.tool;
        let Some(drag) = self.gizmo.as_mut() else {
            return false;
        };
        let axis = drag.axis;
        let snap = workspace.snap;
        let snapping = snap_active(snap.enabled, ctrl);
        let mut changed = false;
        match tool {
            GizmoTool::Translate => {
                let step = (delta.0 * drag.screen_dir[0] + delta.1 * drag.screen_dir[1])
                    / drag.px_per_unit.max(4.0);
                if step.abs() > f32::EPSILON {
                    drag.accum += step;
                    let apply = drag_translate_step(
                        snap,
                        snapping,
                        drag.start_axis_value,
                        drag.accum,
                        drag.applied,
                    );
                    if apply.abs() > 1e-6 {
                        let basis = workspace.gizmo_basis();
                        let world = basis[axis].map(|component| component * apply);
                        workspace.translate_entities_world(&drag.targets, world);
                        drag.applied += apply;
                        changed = true;
                    }
                }
            }
            GizmoTool::Rotate => {
                let center = workspace.gizmo_center_world().unwrap_or([0.0; 3]);
                let world_length = vm::gizmo_world_length(&workspace.camera, rect.height());
                let angle_center = vm::ring_screen_radius_oriented(
                    frame,
                    center,
                    workspace.gizmo_basis(),
                    axis,
                    world_length,
                    rect.width(),
                    rect.height(),
                )
                .map(|(cx, cy, _)| (cx, cy))
                .unwrap_or((rect.width() * 0.5, rect.height() * 0.5));
                let angle = vm::screen_angle(local, angle_center);
                let delta_angle = vm::wrap_angle(angle - drag.last_angle);
                drag.last_angle = angle;
                if delta_angle.abs() > 1e-4 {
                    drag.accum += delta_angle;
                    let desired = if snapping {
                        snap.snapped_radians(drag.accum)
                    } else {
                        drag.accum
                    };
                    let apply = desired - drag.applied;
                    if apply.abs() > 1e-4 {
                        workspace.rotate_entities(&drag.targets, axis, -apply);
                        drag.applied = desired;
                        changed = true;
                    }
                }
            }
            GizmoTool::Scale => {
                let step = (delta.0 * drag.screen_dir[0] + delta.1 * drag.screen_dir[1])
                    / drag.px_per_unit.max(4.0)
                    * 1.5;
                drag.accum += step;
                let raw = (drag.start_scale + drag.accum).max(0.01);
                let next = if snapping {
                    snap.snapped_scale(raw)
                } else {
                    raw
                };
                if (next - drag.applied).abs() > 1e-4 {
                    let ratio = next / drag.start_scale.max(1e-4);
                    workspace.scale_entities_gizmo(&drag.scale_targets, axis, ratio);
                    drag.applied = next;
                    changed = true;
                }
            }
        }
        self.dirty = true;
        changed
    }

    /// 面板主体：分配矩形、按需出帧、采样纹理、处理轨道 / 拾取 / gizmo。
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &mut Workspace,
        bridge: &ImageBridge,
        playing: bool,
        scale: f32,
    ) -> ViewportResponse {
        let rect = ui.available_rect_before_wrap();
        let ppp = ui.ctx().pixels_per_point();
        let panel_extent = viewport_extent(rect.width(), rect.height(), ppp, scale);
        // Game 视图可以钉一个目标分辨率：面板大小只影响显示尺寸，不影响出图。
        self.extent = if self.kind() == ViewKind::Game {
            self.target.extent(panel_extent)
        } else {
            panel_extent
        };
        // 面板比例与目标比例不一致时居中留黑边，绝不拉伸（#463）。
        let image_rect = if self.kind() == ViewKind::Game {
            letterbox(rect, self.extent)
        } else {
            rect
        };
        // 面板尺寸变化（splitter / 窗口 resize）必须重建离屏目标，否则图像会被拉伸。
        let current = self
            .host
            .renderer()
            .offscreen_extent()
            .map(|extent| (extent.width, extent.height))
            .unwrap_or_default();
        if current != self.extent {
            self.dirty = true;
        }

        // 1) 按需出帧（Play 时每帧）。
        if self.dirty || playing {
            self.render_frame(bridge);
        }

        // 2) 采样纹理（同一设备、同一命令缓冲区 → 零拷贝）。
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        // Game view 的 Play 把键鼠转发给项目模块（下一帧 tick 读取）。
        self.forward_game_input(ui, image_rect, &response);
        let painter = ui.painter_at(rect);

        // Project → 场景视口：预制体落到地面（y = 0）生成实例，场景文件则打开文档。
        //
        // 拖动中就把落点画出来（#646）：只描一圈视口边框看不出「会落在哪」，
        // 也看不出这个资产到底能不能放。
        if let Some(payload) = response.dnd_hover_payload::<crate::widgets::AssetDrag>() {
            let accepts =
                payload.path.ends_with(".prefab.json") || payload.path.ends_with(".scene.json");
            let (edge, tint, hint) = if accepts {
                (
                    egui::Color32::from_rgb(90, 170, 120),
                    egui::Color32::from_rgba_unmultiplied(90, 170, 120, 36),
                    drop_hint(&payload.path),
                )
            } else {
                (
                    egui::Color32::from_rgb(214, 96, 96),
                    egui::Color32::from_rgba_unmultiplied(214, 96, 96, 28),
                    "cannot be dropped here".to_string(),
                )
            };
            painter.rect_stroke(
                rect.shrink(2.0),
                egui::CornerRadius::ZERO,
                egui::Stroke::new(2.0_f32, edge),
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(rect.shrink(2.0), egui::CornerRadius::ZERO, tint);
            if let Some(pos) = response
                .hover_pos()
                .or_else(|| response.interact_pointer_pos())
            {
                let local = (pos.x - rect.min.x, pos.y - rect.min.y);
                let frame = vm::camera_frame(&workspace.camera);
                let ray =
                    vm::ray_from_screen(local.0, local.1, &frame, rect.width(), rect.height());
                if accepts {
                    if let Some(point) = vm::drop_point(&workspace.camera, ray) {
                        // 落点：投影到屏幕的十字 + 单位立方体轮廓。
                        if let Some((x, y, _)) =
                            vm::project(point, &frame, rect.width(), rect.height())
                        {
                            let center = rect.min + egui::vec2(x, y);
                            let stroke = egui::Stroke::new(1.5_f32, edge);
                            painter.line_segment(
                                [
                                    center + egui::vec2(-7.0, 0.0),
                                    center + egui::vec2(7.0, 0.0),
                                ],
                                stroke,
                            );
                            painter.line_segment(
                                [
                                    center + egui::vec2(0.0, -7.0),
                                    center + egui::vec2(0.0, 7.0),
                                ],
                                stroke,
                            );
                        }
                        let ghost = TransformJson {
                            position: point,
                            rotation: [0.0, 0.0, 0.0, 1.0],
                            scale: [1.0, 1.0, 1.0],
                        };
                        if let Some(edges) =
                            vm::projected_box_edges(&ghost, &frame, rect.width(), rect.height())
                        {
                            for (a, b) in edges {
                                painter.line_segment(
                                    [
                                        rect.min + egui::vec2(a.0, a.1),
                                        rect.min + egui::vec2(b.0, b.1),
                                    ],
                                    egui::Stroke::new(1.0_f32, edge),
                                );
                            }
                        }
                    }
                }
                // 光标旁的标签：资产名 + 这次拖放会做什么。
                let name = payload.path.rsplit('/').next().unwrap_or(&payload.path);
                let text = format!("{name}  →  {hint}");
                let anchor = pos + egui::vec2(16.0, 18.0);
                let galley = painter.layout_no_wrap(
                    text,
                    egui::FontId::monospace(11.0),
                    egui::Color32::WHITE,
                );
                let chip = egui::Rect::from_min_size(anchor, galley.size() + egui::vec2(8.0, 4.0));
                painter.rect_filled(
                    chip,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_rgba_unmultiplied(18, 20, 24, 220),
                );
                painter.rect_stroke(
                    chip,
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(1.0_f32, edge),
                    egui::StrokeKind::Inside,
                );
                painter.galley(anchor + egui::vec2(4.0, 2.0), galley, egui::Color32::WHITE);
            }
        }
        if let Some(payload) = response.dnd_release_payload::<crate::widgets::AssetDrag>() {
            let path = payload.path.clone();
            let drop_local = response
                .interact_pointer_pos()
                .map(|pos| (pos.x - rect.min.x, pos.y - rect.min.y));
            if path.ends_with(".prefab.json") {
                match workspace.instantiate_prefab_asset(&path, None) {
                    Ok(_) => {
                        if let Some(local) = drop_local {
                            let frame = vm::camera_frame(&workspace.camera);
                            let ray = vm::ray_from_screen(
                                local.0,
                                local.1,
                                &frame,
                                rect.width(),
                                rect.height(),
                            );
                            if let Some(point) = vm::drop_point(&workspace.camera, ray) {
                                if let Some(index) = workspace.primary_selection() {
                                    let entity =
                                        workspace.document().doc.id_of_index(index).unwrap_or(0);
                                    let current =
                                        vm::world_position(&workspace.document().doc, index);
                                    let delta = [
                                        point[0] - current[0],
                                        point[1] - current[1],
                                        point[2] - current[2],
                                    ];
                                    workspace.translate_entity_world(entity, delta);
                                }
                            }
                        }
                        self.dirty = true;
                    }
                    Err(error) => self.error = Some(format!("instantiate failed: {error}")),
                }
            } else if path.ends_with(".scene.json") {
                if let Err(error) = workspace.open_project_file(&path) {
                    self.error = Some(format!("open {path} failed: {error}"));
                }
            }
        }

        if let Some(texture) = &self.texture {
            painter.image(
                texture.id,
                image_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(24, 26, 30));
            let text = self
                .error
                .clone()
                .unwrap_or_else(|| "渲染器未就绪".to_string());
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(13.0),
                egui::Color32::from_gray(150),
            );
        }

        // 2.5) Game 视图左上角：预览分辨率。面板再大也不改构图（#463）。
        if self.kind() == ViewKind::Game {
            let bar = Rect::from_min_size(
                image_rect.min + egui::vec2(6.0, 6.0),
                Vec2::new(190.0, 22.0),
            );
            let mut bar_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(bar)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            let mut chosen = self.target;
            egui::ComboBox::from_id_salt(("game-resolution", rect.min.x as i32))
                .selected_text(self.target.label())
                .width(120.0)
                .show_ui(&mut bar_ui, |ui| {
                    for preset in game_resolution_presets(self.project_resolution) {
                        if ui
                            .selectable_label(preset == self.target, preset.label())
                            .on_hover_text(match preset {
                                GameResolution::MatchWindow => {
                                    "Render at the panel size (old behaviour)"
                                }
                                GameResolution::Fixed { .. } => {
                                    "Render off-screen at this resolution and letterbox it"
                                }
                                GameResolution::Aspect { .. } => {
                                    "Keep this aspect ratio and fit it to the panel"
                                }
                            })
                            .clicked()
                        {
                            chosen = preset;
                        }
                    }
                });
            if chosen != self.target {
                self.target = chosen;
                // 新尺寸 = 新的离屏目标，下一帧必须重建。
                self.dirty = true;
            }
        }

        // 3) gizmo / 选中标记。
        let mut document_changed = false;
        let frame = vm::camera_frame(&workspace.camera);
        let mut icon_hits: Vec<(usize, egui::Pos2)> = Vec::new();
        if self.kind() == ViewKind::Scene {
            // 选中描边画在图标 / gizmo 之下：它属于对象本身，不该盖住手柄。
            if workspace.show_selection_outline {
                self.draw_selection_outline(&painter, rect, workspace, &frame);
            }
            // sprite 的真实矩形（#643）：AABB 描边对 2D 物体太大，这里按
            // `frame_size / pixels_per_unit` 与轴心画出真正会被绘制的那块。
            self.draw_sprite_rects(&painter, rect, workspace, &frame);
            // 引擎的 SceneView Gizmos：相机画视锥、光源画图标（点选图标 = 选中对象）。
            icon_hits = self.draw_component_icons(&painter, rect, workspace, &frame);
            self.draw_gizmo(&painter, rect, workspace, &frame);
            // 方向 gizmo 画在最上层：它只占右上角一小块，不遮住场景操作。
            draw_orientation_gizmo(&painter, rect, &frame);
        }

        // 4) 交互（常用习惯）：
        //    · LMB 拖 gizmo → 移动 / 旋转 / 缩放
        //    · LMB 拖空白 → 框选
        //    · Alt+LMB / RMB / MMB 拖 → 轨道
        //    · 滚轮 → 缩放；双击 → 聚焦选中项
        let alt = ui.input(|input| input.modifiers.alt);
        let local_of = |pos: egui::Pos2| (pos.x - rect.min.x, pos.y - rect.min.y);

        if response.dragged() {
            // 相机 / gizmo 一旦被动过，上一次点击的候选集就过期了。
            self.pick_cycle = None;
        }

        if response.drag_started_by(egui::PointerButton::Primary)
            && self.kind() == ViewKind::Scene
            && !alt
        {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = local_of(pos);
                if !self.begin_gizmo_drag(workspace, &frame, rect, local)
                    && !alt
                    && response.dragged_by(egui::PointerButton::Primary)
                {
                    self.box_start = Some(pos);
                    self.box_current = Some(pos);
                }
            }
        }

        let pointer_delta = ui.input(|i| i.pointer.delta());
        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
        if response.dragged_by(egui::PointerButton::Primary)
            && !(self.kind() == ViewKind::Scene && alt)
        {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = local_of(pos);
                if self.gizmo.is_some() {
                    if self.gizmo_drag_step(
                        workspace,
                        &frame,
                        rect,
                        local,
                        (pointer_delta.x, pointer_delta.y),
                        ctrl,
                    ) {
                        document_changed = true;
                    }
                } else if let Some(start) = self.box_start {
                    self.box_current = Some(pos);
                    painter.rect_stroke(
                        egui::Rect::from_two_pos(start, pos),
                        egui::CornerRadius::ZERO,
                        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
        if self.kind() == ViewKind::Scene {
            let right_held =
                ui.input(|input| input.pointer.button_down(egui::PointerButton::Secondary));
            self.flying_camera = response.hovered() && right_held;
            if self.flying_camera {
                let (forward, right, vertical) = ui.input(|input| {
                    let key = |key| if input.key_down(key) { 1.0 } else { 0.0 };
                    (
                        key(egui::Key::W) - key(egui::Key::S),
                        key(egui::Key::D) - key(egui::Key::A),
                        key(egui::Key::E) - key(egui::Key::Q),
                    )
                });
                if forward != 0.0 || right != 0.0 || vertical != 0.0 {
                    let dt = ui.input(|input| input.stable_dt);
                    workspace.camera.fly_move(forward, right, vertical, dt);
                    self.dirty = true;
                }
            }
            if response.dragged_by(egui::PointerButton::Middle) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let delta = self
                        .orbit_last_pos
                        .replace(pos)
                        .map(|last| pos - last)
                        .unwrap_or_default();
                    pan_camera(&mut workspace.camera, delta.x, delta.y);
                }
                self.dirty = true;
            } else if response.dragged_by(egui::PointerButton::Secondary) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let delta = self
                        .orbit_last_pos
                        .replace(pos)
                        .map(|last| pos - last)
                        .unwrap_or_default();
                    workspace.camera.look_in_place(delta.x, delta.y);
                }
                self.dirty = true;
            } else {
                self.orbit_last_pos = None;
            }
        } else {
            self.flying_camera = false;
        }
        if self.kind() == ViewKind::Scene
            && alt
            && response.dragged_by(egui::PointerButton::Primary)
        {
            workspace.camera.orbit(pointer_delta.x, pointer_delta.y);
            self.dirty = true;
            self.gizmo = None;
            self.box_start = None;
            self.box_current = None;
        }

        if response.drag_stopped_by(egui::PointerButton::Primary) {
            // 框选落定：一次写入选中集合（与引擎的矩形选择一致）。
            if let (Some(start), Some(end)) = (self.box_start.take(), self.box_current.take()) {
                let selection_rect = egui::Rect::from_two_pos(start, end);
                if selection_rect.width() > 3.0 || selection_rect.height() > 3.0 {
                    // `box_select` 要的是视口像素坐标，egui 给的是屏幕坐标（#422）。
                    let local =
                        vm::local_box((start.x, start.y), (end.x, end.y), (rect.min.x, rect.min.y));
                    let hits = vm::box_select(
                        local,
                        &frame,
                        rect.width(),
                        rect.height(),
                        &proxy_transforms(workspace),
                    );
                    let modifiers = ui.input(|input| input.modifiers);
                    let mode = SelectMode::from_modifiers(
                        modifiers.shift,
                        modifiers.ctrl || modifiers.command,
                    );
                    workspace.select_many_with(&hits, mode);
                    self.dirty = true;
                }
            }
            self.gizmo = None;
            // 交互结束：下一次拖动 / 字段输入必然开新的撤销步。
            workspace.end_interaction();
        }
        if response.drag_stopped_by(egui::PointerButton::Middle)
            || response.drag_stopped_by(egui::PointerButton::Secondary)
        {
            self.orbit_last_pos = None;
        }
        if response.hovered() {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll.abs() > 0.0 {
                // 缩放到光标（#511）：取光标射线上距离等于 `distance` 的世界点当锚点，
                // 缩放后它留在原地。相机可能在本帧前面的轨道 / 平移里已经动过，重新取帧。
                let frame = vm::camera_frame(&workspace.camera);
                let anchor = response.hover_pos().map(|pos| {
                    let (x, y) = local_of(pos);
                    let (origin, direction) =
                        vm::ray_from_screen(x, y, &frame, rect.width(), rect.height());
                    let distance = workspace.camera.distance;
                    [
                        origin[0] + direction[0] * distance,
                        origin[1] + direction[1] * distance,
                        origin[2] + direction[2] * distance,
                    ]
                });
                workspace.camera.zoom_towards(scroll, anchor);
                self.dirty = true;
            }
        }
        if response.double_clicked() && self.kind() == ViewKind::Scene {
            workspace.frame_selection();
            self.dirty = true;
        }
        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let local = local_of(pos);
                if self.kind() == ViewKind::Scene {
                    // 方向 gizmo 优先：点中右上角的轴 = 把相机对齐到该轴（#416），
                    // 这一下不参与实体拾取。
                    let aligned = vm::pick_orientation_axis(
                        (pos.x, pos.y),
                        orientation_gizmo_center(rect),
                        ORIENTATION_GIZMO_RADIUS,
                        &frame,
                    );
                    if let Some((axis, sign)) = aligned {
                        workspace.camera.align_to_axis(axis, sign);
                        self.pick_cycle = None;
                        self.dirty = true;
                    } else {
                        // 图标（相机 / 光源）比网格体优先命中，和 一致。
                        let icon = icon_hits
                            .iter()
                            .map(|(index, center)| {
                                let dx = local.0 - (center.x - rect.min.x);
                                let dy = local.1 - (center.y - rect.min.y);
                                (*index, (dx * dx + dy * dy).sqrt())
                            })
                            .filter(|(_, distance)| *distance <= 11.0)
                            .min_by(|a, b| a.1.total_cmp(&b.1))
                            .map(|(index, _)| index);
                        // 修饰键决定这次点击是替换 / 并入 / 移除（Shift 加、Ctrl-Cmd 减）。
                        let modifiers = ui.input(|input| input.modifiers);
                        let mode = SelectMode::from_modifiers(
                            modifiers.shift,
                            modifiers.ctrl || modifiers.command,
                        );
                        let candidates = pick_candidates(
                            workspace,
                            Some(&self.host),
                            local,
                            rect.size(),
                            &frame,
                        );
                        if let Some(index) = icon {
                            workspace.select_with(index, mode);
                            self.pick_cycle = None;
                        } else if matches!(mode, SelectMode::Replace) {
                            // 点选循环穿透（#416）：同一位置连点依次选下一层，
                            // 点空白回到清空。
                            match next_pick(self.pick_cycle.as_ref(), local, candidates) {
                                Some((index, cycle)) => {
                                    self.pick_cycle = Some(cycle);
                                    workspace.select_with(index, mode);
                                }
                                None => {
                                    workspace.clear_selection_in(mode);
                                    self.pick_cycle = None;
                                }
                            }
                        } else {
                            // Shift / Ctrl 的加选 / 减选不循环，永远取最近的一个。
                            match candidates.first() {
                                Some(index) => workspace.select_with(*index, mode),
                                None => workspace.clear_selection_in(mode),
                            }
                            self.pick_cycle = None;
                        }
                        self.dirty = true;
                    }
                }
            }
        }

        // 5) 角标：FPS / 帧耗时 / 分辨率 / 光源预算。
        let (lights_kept, lights_total) = self.host.local_light_stats();
        let mut stats = format!(
            "{} · {} · {:.1} ms · {:.1} FPS · {} frames",
            self.kind().label(),
            self.tool.label(),
            self.last_frame_ms,
            self.fps,
            self.frames
        );
        if let Some(label) = light_budget_label(lights_kept, lights_total) {
            stats.push_str(" · ");
            stats.push_str(&label);
        }
        let pos = rect.left_bottom() + egui::vec2(6.0, -6.0);
        painter.text(
            pos,
            egui::Align2::LEFT_BOTTOM,
            stats,
            egui::FontId::monospace(11.0),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 170),
        );

        ViewportResponse { document_changed }
    }

    /// 选中对象的包围盒线框（#487 的低成本版）。
    ///
    /// Unity 用 `Handles.DrawOutline` 做真正的描边（需要引擎侧 pass）；这里先用
    /// 世界代理立方体的 12 条边解决「看得出选中了谁」：
    /// - 直接选中的实体画强调色（与应用内选中色一致）；
    /// - 与选中项有父子关系、但自己没被选中的实体画更暗的辅助色，能看出层级方向；
    /// - 子级线框略细，父级略粗，多选时不会糊成一团。
    ///
    /// 任何一个角落在相机背后就整块跳过（`vm::projected_box_edges` 返回 `None`），
    /// 避免画出穿透屏幕的线。
    fn draw_selection_outline(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        workspace: &Workspace,
        frame: &vm::CameraFrame,
    ) {
        use egui::Color32;

        if workspace.selection.is_empty() {
            return;
        }
        let doc = &workspace.document().doc;
        let selected: std::collections::HashSet<usize> =
            workspace.selection.iter().copied().collect();

        // 父级 / 子级：给层级关系一个颜色区分（Unity 也同时画父与子）。
        let mut related: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for index in &selected {
            let mut current = doc.entities.get(*index).and_then(|entity| entity.parent);
            while let Some(id) = current {
                let Some(parent) = doc.index_of_id(id) else {
                    break;
                };
                if !selected.contains(&parent) {
                    related.insert(parent);
                }
                current = doc.entities.get(parent).and_then(|entity| entity.parent);
            }
            for child in doc.descendants(*index) {
                if !selected.contains(&child) {
                    related.insert(child);
                }
            }
        }

        let accent = Color32::from_rgb(255, 154, 60);
        let helper = Color32::from_rgb(255, 154, 60).gamma_multiply(0.45);

        for (index, transform, _) in proxies(workspace, None) {
            let is_selected = selected.contains(&index);
            let is_related = related.contains(&index);
            if !is_selected && !is_related {
                continue;
            }
            let Some(edges) =
                vm::projected_box_edges(&transform, frame, rect.width(), rect.height())
            else {
                continue;
            };
            let stroke = if is_selected {
                egui::Stroke::new(1.6_f32, accent)
            } else {
                egui::Stroke::new(1.0_f32, helper)
            };
            for (a, b) in edges {
                painter.line_segment(
                    [
                        rect.min + egui::vec2(a.0, a.1),
                        rect.min + egui::vec2(b.0, b.1),
                    ],
                    stroke,
                );
            }
        }
    }

    /// 选中 sprite 的矩形 gizmo（#643）。
    ///
    /// 只画选中项本身：sprite 的「盒子」就是它会被绘制的那块区域，
    /// 用 `frame_size / pixels_per_unit` 当世界尺寸、`pivot` 当轴心，
    /// 再乘实体自身的旋转 / 缩放。轴心额外点一个小实心方块，便于对齐。
    fn draw_sprite_rects(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        workspace: &Workspace,
        frame: &vm::CameraFrame,
    ) {
        use egui::Color32;

        const SPRITE: &str = "prism_engine::scene::SpriteRenderer";

        if workspace.selection.is_empty() {
            return;
        }
        let width = rect.width();
        let height = rect.height();
        let stroke = egui::Stroke::new(1.4_f32, Color32::from_rgb(90, 200, 255));
        let pivot_color = Color32::from_rgb(90, 200, 255);

        for (index, transform, _) in proxies(workspace, None) {
            if !workspace.selection.contains(&index) {
                continue;
            }
            let doc = &workspace.document().doc;
            let Some(entity) = doc.entities.get(index) else {
                continue;
            };
            let Some(sprite) = entity.component(SPRITE) else {
                continue;
            };
            let number = |field: &str| sprite.get(field).and_then(|value| value.as_f64());
            let axis = |field: &str, axis: usize| {
                sprite
                    .get(field)
                    .and_then(|value| value.as_array())
                    .and_then(|values| values.get(axis))
                    .and_then(|value| value.as_f64())
            };
            let ppu = number("pixels_per_unit").unwrap_or(100.0).max(0.0001);
            let frame_w = axis("frame_size", 0).unwrap_or(0.0);
            let frame_h = axis("frame_size", 1).unwrap_or(0.0);
            if frame_w <= 0.0 || frame_h <= 0.0 {
                continue;
            }
            let size = [(frame_w / ppu) as f32, (frame_h / ppu) as f32];
            let pivot = [
                axis("pivot", 0).unwrap_or(0.5) as f32,
                axis("pivot", 1).unwrap_or(0.5) as f32,
            ];
            let Some(edges) =
                vm::projected_rect_edges(&transform, size, pivot, frame, width, height)
            else {
                continue;
            };
            for (a, b) in edges {
                painter.line_segment(
                    [
                        rect.min + egui::vec2(a.0, a.1),
                        rect.min + egui::vec2(b.0, b.1),
                    ],
                    stroke,
                );
                // 角点小方块：2D 里比 3D 的线框更容易看清边界。
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        rect.min + egui::vec2(a.0, a.1),
                        egui::vec2(5.0, 5.0),
                    ),
                    0.0,
                    stroke.color,
                );
            }
            if let Some((x, y, _)) = vm::project(transform.position, frame, width, height) {
                painter.circle_filled(rect.min + egui::vec2(x, y), 3.0, pivot_color);
            }
        }
    }

    /// 引擎的 SceneView Gizmos：相机画视锥、光源画图标与范围。
    ///
    /// 引擎只画实体本身（网格 / 光栅化结果），编辑器侧的「组件图标」由 egui 覆盖层
    /// 画在视口图像上：世界坐标 → 屏幕坐标用和点选同一套 `vm::project`。
    fn draw_component_icons(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        workspace: &Workspace,
        frame: &vm::CameraFrame,
    ) -> Vec<(usize, egui::Pos2)> {
        use egui::Color32;

        const CAMERA: &str = "prism_engine::scene::Camera";
        const LIGHT: &str = "prism_engine::scene::Light";
        const POINT: &str = "prism_engine::scene::PointLight";
        const SPOT: &str = "prism_engine::scene::SpotLight";
        const DIRECTIONAL: &str = "prism_engine::scene::DirectionalLight";

        fn number(
            entity: &prism_editor_core::EntityJson,
            component: &str,
            field: &str,
        ) -> Option<f32> {
            entity
                .component(component)
                .and_then(|value| value.get(field))
                .and_then(|value| value.as_f64())
                .map(|value| value as f32)
        }
        fn flag(entity: &prism_editor_core::EntityJson, component: &str, field: &str) -> bool {
            entity
                .component(component)
                .and_then(|value| value.get(field))
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        }
        fn color(entity: &prism_editor_core::EntityJson, component: &str, alpha: u8) -> Color32 {
            let channel = |index: usize| {
                entity
                    .component(component)
                    .and_then(|value| value.get("color"))
                    .and_then(|value| value.get(index))
                    .and_then(|value| value.as_f64())
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0) as f32
            };
            // 场景里的光是线性 RGB，图标按 sRGB 近似，暗光也保持可见。
            let to_srgb = |c: f32| (c.max(0.05).powf(1.0 / 2.2) * 255.0).min(255.0) as u8;
            Color32::from_rgba_unmultiplied(
                to_srgb(channel(0)),
                to_srgb(channel(1)),
                to_srgb(channel(2)),
                alpha,
            )
        }

        let doc = &workspace.document().doc;
        let (width, height) = (rect.width(), rect.height());
        let world_len = vm::gizmo_world_length(&workspace.camera, rect.height());
        let mut hits = Vec::new();

        for (index, entity) in doc.entities.iter().enumerate() {
            if !entity.active() {
                continue;
            }
            let is_camera = entity.has_component(CAMERA);
            let is_light = entity.has_component(LIGHT)
                || entity.has_component(POINT)
                || entity.has_component(SPOT)
                || entity.has_component(DIRECTIONAL);
            if !is_camera && !is_light {
                continue;
            }
            if !icon_visible(workspace, is_camera, is_light) {
                continue;
            }
            let position = vm::world_position(doc, index);
            let Some((sx, sy, _)) = vm::project(position, frame, width, height) else {
                continue;
            };
            let center = rect.min + egui::vec2(sx, sy);
            if !rect.expand(48.0).contains(center) {
                continue;
            }
            let selected = workspace.selection.contains(&index);
            let alpha = if selected { 255 } else { 180 };
            hits.push((index, center));

            let rotation = vm::world_rotation(doc, index);
            let basis = vm::basis_from_quat(rotation);
            // 相机 / 聚光灯都看 -Z（与引擎的相机约定一致）。
            let forward = [-basis[2][0], -basis[2][1], -basis[2][2]];
            let stroke =
                |color: Color32| egui::Stroke::new(if selected { 1.6_f32 } else { 1.1_f32 }, color);

            if is_camera {
                let color = Color32::from_rgba_unmultiplied(150, 185, 225, alpha);
                // 用相机自己的 near/far 和投影模式画取景范围：
                // - 透视：near/far 两个矩形 + 四条侧棱；
                // - 正交：同样两个矩形，但尺寸由 orthographic_size 决定（#601）。
                let orthographic = flag(entity, CAMERA, "orthographic");
                let near = number(entity, CAMERA, "near")
                    .filter(|value| value.is_finite() && *value > 0.0)
                    .unwrap_or(0.1);
                let far = number(entity, CAMERA, "far")
                    .filter(|value| value.is_finite() && *value > near)
                    .unwrap_or(1000.0);
                let aspect = if width > 0.0 && height > 0.0 {
                    width / height
                } else {
                    16.0 / 9.0
                };
                let (near_hw, near_hh, far_hw, far_hh) = if orthographic {
                    let half_height = number(entity, CAMERA, "orthographic_size")
                        .filter(|value| value.is_finite() && *value > 0.0)
                        .unwrap_or(5.0);
                    (
                        half_height * aspect,
                        half_height,
                        half_height * aspect,
                        half_height,
                    )
                } else {
                    let fov = number(entity, CAMERA, "fov_y_degrees")
                        .filter(|value| value.is_finite() && *value > 0.0)
                        .unwrap_or(60.0);
                    let half_tan = (fov.to_radians() * 0.5).tan();
                    (
                        half_tan * near * aspect,
                        half_tan * near,
                        half_tan * far * aspect,
                        half_tan * far,
                    )
                };
                let corner = |distance: f32, hw: f32, hh: f32, du: f32, dv: f32| {
                    let point = [
                        position[0]
                            + forward[0] * distance
                            + basis[0][0] * hw * du
                            + basis[1][0] * hh * dv,
                        position[1]
                            + forward[1] * distance
                            + basis[0][1] * hw * du
                            + basis[1][1] * hh * dv,
                        position[2]
                            + forward[2] * distance
                            + basis[0][2] * hw * du
                            + basis[1][2] * hh * dv,
                    ];
                    vm::project(point, frame, width, height)
                        .map(|(x, y, _)| rect.min + egui::vec2(x, y))
                };
                let near_corners = [
                    corner(near, near_hw, near_hh, -1.0, -1.0),
                    corner(near, near_hw, near_hh, 1.0, -1.0),
                    corner(near, near_hw, near_hh, 1.0, 1.0),
                    corner(near, near_hw, near_hh, -1.0, 1.0),
                ];
                let far_corners = [
                    corner(far, far_hw, far_hh, -1.0, -1.0),
                    corner(far, far_hw, far_hh, 1.0, -1.0),
                    corner(far, far_hw, far_hh, 1.0, 1.0),
                    corner(far, far_hw, far_hh, -1.0, 1.0),
                ];
                for corners in [&near_corners, &far_corners] {
                    for pair in 0..4 {
                        if let (Some(a), Some(b)) = (corners[pair], corners[(pair + 1) % 4]) {
                            painter.line_segment([a, b], stroke(color));
                        }
                    }
                }
                for pair in 0..4 {
                    if let (Some(a), Some(b)) = (near_corners[pair], far_corners[pair]) {
                        painter.line_segment([a, b], stroke(color));
                    }
                }
                // 机身：一个小方框 + 镜头一侧的横线。
                let body = egui::Rect::from_center_size(center, egui::vec2(11.0, 8.0));
                painter.rect_filled(body, 0.0, Color32::from_rgba_unmultiplied(28, 34, 44, 220));
                painter.rect_stroke(
                    body,
                    egui::CornerRadius::ZERO,
                    stroke(color),
                    egui::StrokeKind::Inside,
                );
                painter.line_segment(
                    [body.right_top(), body.right_bottom()],
                    stroke(Color32::from_rgba_unmultiplied(120, 200, 150, alpha)),
                );
                continue;
            }

            // ---- 光源 ----
            let (light_type, component) = if entity.has_component(LIGHT) {
                let kind = entity
                    .component(LIGHT)
                    .and_then(|value| value.get("light_type"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("Point");
                let kind = match kind {
                    "Directional" | "Sun" => 0,
                    "Spot" => 2,
                    _ => 1,
                };
                (kind, LIGHT)
            } else if entity.has_component(DIRECTIONAL) {
                (0, DIRECTIONAL)
            } else if entity.has_component(SPOT) {
                (2, SPOT)
            } else {
                (1, POINT)
            };
            let color = color(entity, component, alpha);
            let radius = if selected { 7.5 } else { 6.0 };
            match light_type {
                0 => {
                    // 平行光：中间太阳 + 8 条光线 + 指向。
                    painter.circle_filled(center, radius, color);
                    for step in 0..8 {
                        let angle = step as f32 * std::f32::consts::TAU / 8.0;
                        let dir = egui::vec2(angle.cos(), angle.sin());
                        painter.line_segment(
                            [center + dir * (radius + 2.0), center + dir * (radius + 6.0)],
                            stroke(color),
                        );
                    }
                    if let Some(tip) = vm::project(
                        [
                            position[0] + forward[0] * world_len * 1.2,
                            position[1] + forward[1] * world_len * 1.2,
                            position[2] + forward[2] * world_len * 1.2,
                        ],
                        frame,
                        width,
                        height,
                    )
                    .map(|(x, y, _)| rect.min + egui::vec2(x, y))
                    {
                        painter.line_segment([center + (tip - center) * 0.12, tip], stroke(color));
                    }
                }
                2 => {
                    // 聚光灯：圆 + 锥（外锥角的一半）。
                    painter.circle_stroke(center, radius, stroke(color));
                    let half = number(entity, component, "spot_angle")
                        .or_else(|| {
                            number(entity, component, "outer_cone_angle").map(f32::to_degrees)
                        })
                        .unwrap_or(60.0)
                        .to_radians()
                        * 0.5;
                    let far = world_len * 1.2;
                    for sign in [-1.0_f32, 1.0] {
                        let (sin, cos) = (half * sign).sin_cos();
                        let point = [
                            position[0] + (forward[0] * cos + basis[0][0] * sin) * far,
                            position[1] + (forward[1] * cos + basis[0][1] * sin) * far,
                            position[2] + (forward[2] * cos + basis[0][2] * sin) * far,
                        ];
                        if let Some((x, y, _)) = vm::project(point, frame, width, height) {
                            painter
                                .line_segment([center, rect.min + egui::vec2(x, y)], stroke(color));
                        }
                    }
                }
                _ => {
                    // 点光：圆 + 中心点 + 四个方向短线（引擎的点光图标）。
                    painter.circle_stroke(center, radius, stroke(color));
                    painter.circle_filled(center, 2.0, color);
                    for dir in [
                        egui::vec2(0.0, -1.0),
                        egui::vec2(0.0, 1.0),
                        egui::vec2(-1.0, 0.0),
                        egui::vec2(1.0, 0.0),
                    ] {
                        painter.line_segment(
                            [center + dir * (radius + 2.0), center + dir * (radius + 5.0)],
                            stroke(color),
                        );
                    }
                }
            }
        }
        hits
    }

    fn draw_gizmo(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        workspace: &Workspace,
        frame: &vm::CameraFrame,
    ) {
        let Some(center) = workspace.gizmo_center_world() else {
            return;
        };
        let world_length = vm::gizmo_world_length(&workspace.camera, rect.height());
        let basis = workspace.gizmo_basis();
        let origin = rect.min;
        let (width, height) = (rect.width(), rect.height());
        let colors = [
            egui::Color32::from_rgb(232, 74, 74),
            egui::Color32::from_rgb(110, 200, 80),
            egui::Color32::from_rgb(80, 140, 235),
        ];
        for axis in 0..3 {
            let active = self
                .gizmo
                .as_ref()
                .map(|drag| drag.axis == axis)
                .unwrap_or(false);
            let width_px = if active { 3.5_f32 } else { 2.0_f32 };
            match self.tool {
                GizmoTool::Rotate => {
                    if let Some((cx, cy, radius)) = vm::ring_screen_radius_oriented(
                        frame,
                        center,
                        basis,
                        axis,
                        world_length,
                        width,
                        height,
                    ) {
                        if radius > 6.0 {
                            painter.circle_stroke(
                                origin + egui::vec2(cx, cy),
                                radius,
                                egui::Stroke::new(width_px, colors[axis]),
                            );
                        }
                    }
                }
                GizmoTool::Translate | GizmoTool::Scale => {
                    let Some((start, end, _)) = vm::axis_screen_segment_oriented(
                        frame,
                        center,
                        basis[axis],
                        world_length,
                        width,
                        height,
                    ) else {
                        continue;
                    };
                    let a = origin + egui::vec2(start.0, start.1);
                    let b = origin + egui::vec2(end.0, end.1);
                    painter.line_segment([a, b], egui::Stroke::new(width_px, colors[axis]));
                    if self.tool == GizmoTool::Scale {
                        let half = if active { 5.0 } else { 4.0 };
                        painter.rect_filled(
                            egui::Rect::from_center_size(b, egui::vec2(half * 2.0, half * 2.0)),
                            0.0,
                            colors[axis],
                        );
                    } else {
                        painter.circle_filled(b, if active { 5.0 } else { 4.0 }, colors[axis]);
                    }
                }
            }
        }
    }
}

impl ViewKind {
    pub fn label(self) -> &'static str {
        match self {
            ViewKind::Scene => "Scene",
            ViewKind::Game => "Game",
        }
    }
}

/// 文档实体的世界代理（单位立方体）：与 GPUI / Tauri 编辑器的点选规则一致。
/// 拖放标签里的动作说明（#646）。
fn drop_hint(path: &str) -> String {
    if path.ends_with(".prefab.json") {
        "instantiate at the marker".to_string()
    } else if path.ends_with(".scene.json") {
        "open scene".to_string()
    } else {
        "droppable".to_string()
    }
}

/// 精灵的局部 AABB：`frame_size / pixels_per_unit`，按 pivot 偏移（#600）。
///
/// 没有 `frame_size` 时无法知道纹理尺寸，返回 `None` 让调用方退回单位立方体。
fn sprite_bounds(
    doc: &prism_editor_core::SceneDoc,
    index: usize,
    host: Option<&EngineHost>,
) -> Option<vm::PickBounds> {
    const SPRITE: &str = "prism_engine::scene::SpriteRenderer";
    let sprite = doc.entities.get(index)?.component(SPRITE)?;
    let path = sprite.get("sprite_path")?.as_str()?;
    if path.is_empty() {
        return None;
    }
    let frame = sprite.get("frame_size");
    let mut frame_w = frame
        .and_then(|value| value.get(0))
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32;
    let mut frame_h = frame
        .and_then(|value| value.get(1))
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32;
    if frame_w <= 0.0 || frame_h <= 0.0 {
        // `frame_size` 是可选作者字段；资产上传后引擎知道纹理尺寸。
        let size = host.and_then(|host| host.sprite_texture_size(index))?;
        frame_w = size[0];
        frame_h = size[1];
    }
    if frame_w <= 0.0 || frame_h <= 0.0 {
        return None;
    }
    let ppu = sprite
        .get("pixels_per_unit")
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
        .filter(|value| *value > 0.0)
        .unwrap_or(100.0);
    let pivot = sprite.get("pivot");
    let pivot_x = pivot
        .and_then(|value| value.get(0))
        .and_then(|value| value.as_f64())
        .unwrap_or(0.5) as f32;
    let pivot_y = pivot
        .and_then(|value| value.get(1))
        .and_then(|value| value.as_f64())
        .unwrap_or(0.5) as f32;
    let width = frame_w / ppu;
    let height = frame_h / ppu;
    Some(vm::PickBounds {
        min: [-pivot_x * width, -pivot_y * height, -0.05],
        max: [(1.0 - pivot_x) * width, (1.0 - pivot_y) * height, 0.05],
    })
}

fn proxies(
    workspace: &Workspace,
    host: Option<&EngineHost>,
) -> Vec<(usize, TransformJson, Option<vm::PickBounds>)> {
    let doc = &workspace.document().doc;
    (0..doc.entities.len())
        .map(|index| {
            let bounds = sprite_bounds(doc, index, host).or_else(|| {
                host.and_then(|host| host.mesh_bounds(index))
                    .map(|(min, max)| vm::PickBounds { min, max })
            });
            (
                index,
                TransformJson {
                    position: vm::world_position(doc, index),
                    rotation: vm::world_rotation(doc, index),
                    scale: vm::world_scale(doc, index),
                },
                bounds,
            )
        })
        .collect()
}

/// 框选 / 描边仍用单位立方体代理（#600 只改了单击拾取）。
pub(crate) fn proxy_transforms(workspace: &Workspace) -> Vec<(usize, TransformJson)> {
    proxies(workspace, None)
        .into_iter()
        .map(|(index, transform, _)| (index, transform))
        .collect()
}

/// 最近的一个命中（控制通道的 `ui.click` / `ui.pick` 用；视口交互本身用候选集）。
pub(crate) fn pick(
    workspace: &Workspace,
    host: Option<&EngineHost>,
    point: (f32, f32),
    size: egui::Vec2,
    frame: &vm::CameraFrame,
) -> Option<usize> {
    pick_candidates(workspace, host, point, size, frame)
        .into_iter()
        .next()
}

pub(crate) fn pick_candidates(
    workspace: &Workspace,
    host: Option<&EngineHost>,
    point: (f32, f32),
    size: egui::Vec2,
    frame: &vm::CameraFrame,
) -> Vec<usize> {
    vm::pick_entities_with_bounds(point, frame, size.x, size.y, &proxies(workspace, host))
}

/// egui 键 → 引擎键（ABI 位编号由 `KeyCode::bit_index` 决定）。
fn engine_key(key: egui::Key) -> Option<KeyCode> {
    use egui::Key as EguiKey;
    use prism_engine::input::KeyCode as EngineKey;
    Some(match key {
        EguiKey::W => EngineKey::KeyW,
        EguiKey::A => EngineKey::KeyA,
        EguiKey::S => EngineKey::KeyS,
        EguiKey::D => EngineKey::KeyD,
        EguiKey::Q => EngineKey::KeyQ,
        EguiKey::E => EngineKey::KeyE,
        EguiKey::Space => EngineKey::Space,
        EguiKey::Escape => EngineKey::Escape,
        EguiKey::Tab => EngineKey::Tab,
        EguiKey::Enter => EngineKey::Enter,
        EguiKey::ArrowUp => EngineKey::ArrowUp,
        EguiKey::ArrowDown => EngineKey::ArrowDown,
        EguiKey::ArrowLeft => EngineKey::ArrowLeft,
        EguiKey::ArrowRight => EngineKey::ArrowRight,
        EguiKey::Num0 => EngineKey::Digit0,
        EguiKey::Num1 => EngineKey::Digit1,
        EguiKey::Num2 => EngineKey::Digit2,
        EguiKey::Num3 => EngineKey::Digit3,
        EguiKey::Num4 => EngineKey::Digit4,
        EguiKey::Num5 => EngineKey::Digit5,
        EguiKey::Num6 => EngineKey::Digit6,
        EguiKey::Num7 => EngineKey::Digit7,
        EguiKey::Num8 => EngineKey::Digit8,
        EguiKey::Num9 => EngineKey::Digit9,
        _ => return None,
    })
}

#[cfg(test)]
mod drop_hint_tests {
    /// #646：拖放标签要说清这次拖放会做什么。
    #[test]
    fn the_drop_label_names_the_action() {
        assert_eq!(
            super::drop_hint("assets/prefabs/crate.prefab.json"),
            "instantiate at the marker"
        );
        assert_eq!(
            super::drop_hint("assets/scenes/level.scene.json"),
            "open scene"
        );
        assert_eq!(
            super::drop_hint("assets/materials/red.mat.json"),
            "droppable"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Vec2};

    #[test]
    fn component_icon_filters_are_independent() {
        // #416：相机图标与光源图标各自开关，互不牵连。
        let mut workspace = Workspace::sample();
        assert!(icon_visible(&workspace, true, false));
        assert!(icon_visible(&workspace, false, true));
        workspace.show_camera_icons = false;
        assert!(!icon_visible(&workspace, true, false));
        assert!(icon_visible(&workspace, false, true));
        workspace.show_light_icons = false;
        assert!(!icon_visible(&workspace, false, true));
        assert!(!icon_visible(&workspace, true, true));
    }

    #[test]
    fn repeated_clicks_cycle_through_overlapping_entities() {
        // #416：同一位置连点依次选下一层，候选集或位置变了就回到第一层。
        let candidates = vec![3, 7, 9];
        let (first, cycle) = next_pick(None, (100.0, 50.0), candidates.clone()).unwrap();
        assert_eq!(first, 3);
        let (second, cycle) = next_pick(Some(&cycle), (101.0, 51.0), candidates.clone()).unwrap();
        assert_eq!(second, 7, "4px 内视为同一次点选");
        let (third, cycle) = next_pick(Some(&cycle), (100.0, 50.0), candidates.clone()).unwrap();
        assert_eq!(third, 9);
        // 回到第一层。
        let (again, _) = next_pick(Some(&cycle), (100.0, 50.0), candidates.clone()).unwrap();
        assert_eq!(again, 3);

        // 换位置 / 换候选集 → 重新从最近的一个开始。
        let (moved, _) = next_pick(Some(&cycle), (140.0, 50.0), candidates).unwrap();
        assert_eq!(moved, 3);
        let (changed, _) = next_pick(Some(&cycle), (100.0, 50.0), vec![7, 9]).unwrap();
        assert_eq!(changed, 7);

        // 点空白 = 没有候选。
        assert_eq!(next_pick(Some(&cycle), (0.0, 0.0), Vec::new()), None);
    }

    /// #468：超预算的局部光要在角标里明确报出来，而不是静默丢弃。
    #[test]
    fn the_stats_label_reports_dropped_lights() {
        assert_eq!(light_budget_label(0, 0), None, "没有局部光时不占位置");
        assert_eq!(light_budget_label(3, 3), Some("Lights 3".to_string()));
        assert_eq!(
            light_budget_label(8, 12),
            Some("Lights 8/12 (4 dropped)".to_string())
        );
    }

    #[test]
    fn ctrl_inverts_the_snap_toggle() {
        assert!(!snap_active(false, false), "关着时自由拖动");
        assert!(snap_active(true, false), "开着时吸附");
        assert!(snap_active(false, true), "按住 Ctrl 临时打开");
        assert!(!snap_active(true, true), "按住 Ctrl 临时关掉");
    }

    #[test]
    fn slow_snapped_drag_still_moves_once_a_step_is_reached() {
        let snap = prism_editor_core::workspace::SnapSettings::default();
        let mut applied = 0.0;
        let mut accum = 0.0;
        // 每帧 0.02 世界单位：6 帧共 0.12，还没过 0.125 的中点，不该动。
        for _ in 0..6 {
            accum += 0.02;
            applied += drag_translate_step(snap, true, 0.0, accum, applied);
        }
        assert_eq!(applied, 0.0, "没到半个步长之前不能动");
        // 第 7 帧到 0.14，按 0.25 取整 → 一次性跳到 0.25。
        accum += 0.02;
        applied += drag_translate_step(snap, true, 0.0, accum, applied);
        assert_eq!(applied, 0.25);
        // 继续拖到 0.34 仍然停在 0.25（round(1.36) = 1）。
        accum += 0.2;
        let extra = drag_translate_step(snap, true, 0.0, accum, applied);
        assert_eq!(extra, 0.0);
        // 拖到 0.4 才跨过 0.375 的中点，跳到 0.5。
        accum += 0.06;
        let extra = drag_translate_step(snap, true, 0.0, accum, applied);
        assert_eq!(extra, 0.25);
        // 关掉吸附就退回原始累计量。
        let free = drag_translate_step(snap, false, 0.0, 0.3, 0.25);
        assert!((free - 0.05).abs() < 1e-6);
    }

    #[test]
    fn game_keys_use_the_engine_abi_mapping() {
        assert_eq!(engine_key(egui::Key::W), Some(KeyCode::KeyW));
        assert_eq!(engine_key(egui::Key::Space), Some(KeyCode::Space));
        assert_eq!(engine_key(egui::Key::Num5), Some(KeyCode::Digit5));
        assert_eq!(engine_key(egui::Key::F1), None);
    }

    #[test]
    fn extent_follows_scale_and_dpi() {
        assert_eq!(viewport_extent(800.0, 600.0, 1.0, 1.0), (800, 600));
        assert_eq!(viewport_extent(800.0, 600.0, 2.0, 1.0), (1600, 1200));
        assert_eq!(viewport_extent(800.0, 600.0, 1.0, 0.5), (400, 300));
    }

    #[test]
    fn extent_clamps_degenerate_input() {
        assert_eq!(viewport_extent(0.0, 0.0, 1.0, 1.0), (1, 1));
        assert_eq!(viewport_extent(9000.0, 9000.0, 1.0, 2.0), (4096, 4096));
        // 倍率本身也被夹在 0.25..=2.0。
        assert_eq!(viewport_extent(100.0, 100.0, 1.0, 0.0), (25, 25));
    }

    #[test]
    fn a_fixed_resolution_ignores_the_panel_size() {
        // #463：钉住 1920x1080 之后，面板拖大拖小都出同样的图。
        let target = GameResolution::Fixed {
            width: 1920,
            height: 1080,
        };
        assert_eq!(target.extent((800, 600)), (1920, 1080));
        assert_eq!(target.extent((3840, 2160)), (1920, 1080));
        // Match Window 仍然跟着面板走（旧行为）。
        assert_eq!(GameResolution::MatchWindow.extent((800, 600)), (800, 600));
        // 超过离屏上限时夹到 4096，和 `viewport_extent` 一致。
        assert_eq!(
            GameResolution::Fixed {
                width: 8000,
                height: 9000
            }
            .extent((1, 1)),
            (4096, 4096)
        );
    }

    #[test]
    fn an_aspect_target_fits_the_panel_without_changing_its_ratio() {
        // #585：16:9 面板宽高比 2:1 时，高度吃满、左右留黑边。
        let target = GameResolution::Aspect {
            width: 16,
            height: 9,
        };
        assert_eq!(target.extent((1600, 800)), (1422, 800));
        // 4:3 面板时宽度吃满、上下留黑边。
        assert_eq!(target.extent((800, 800)), (800, 450));
        // 面板本身就是 16:9 时它完全不改动尺寸。
        assert_eq!(target.extent((1920, 1080)), (1920, 1080));
    }

    #[test]
    fn presets_put_the_project_resolution_first() {
        let presets = game_resolution_presets(Some((1600, 900)));
        assert_eq!(presets[0], GameResolution::MatchWindow);
        assert_eq!(
            presets[1],
            GameResolution::Fixed {
                width: 1600,
                height: 900
            }
        );
        assert_eq!(
            presets[2],
            GameResolution::Aspect {
                width: 16,
                height: 9
            }
        );
        assert!(game_resolution_presets(None)
            .iter()
            .all(|preset| !matches!(preset, GameResolution::Fixed { width: 1600, .. })));
    }

    #[test]
    fn letterbox_keeps_the_aspect_ratio_and_centres_the_image() {
        let outer = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1000.0, 1000.0));
        // 16:9 的图放进正方形面板：以宽度为准，上下留边。
        let fitted = letterbox(outer, (1920, 1080));
        assert!((fitted.width() - 1000.0).abs() < 1e-3);
        assert!((fitted.height() - 562.5).abs() < 1e-3);
        assert!((fitted.center() - outer.center()).length() < 1e-3);

        // 面板很宽：以高度为准，左右留边。
        let wide = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(2000.0, 500.0));
        let fitted = letterbox(wide, (1920, 1080));
        assert!((fitted.height() - 500.0).abs() < 1e-3);
        assert!((fitted.width() - 888.888).abs() < 0.01);

        // 退化尺寸不 panic，原样返回。
        assert_eq!(letterbox(outer, (0, 0)), outer);
    }

    #[test]
    fn drag_samples_walk_from_the_start_to_the_end() {
        let samples = drag_samples((10.0, 20.0), (50.0, 100.0), 4);
        assert_eq!(
            samples,
            vec![(20.0, 40.0), (30.0, 60.0), (40.0, 80.0), (50.0, 100.0)]
        );
        // 每步增量之和就是总位移（拖动逻辑按增量累积）。
        let mut previous = (10.0f32, 20.0f32);
        let mut total = (0.0f32, 0.0f32);
        for sample in &drag_samples((10.0, 20.0), (50.0, 100.0), 8) {
            total.0 += sample.0 - previous.0;
            total.1 += sample.1 - previous.1;
            previous = *sample;
        }
        assert!((total.0 - 40.0).abs() < 1e-4);
        assert!((total.1 - 80.0).abs() < 1e-4);
        // 退化输入（0 步）仍然给出终点，脚本不会静默什么都不做。
        assert_eq!(drag_samples((0.0, 0.0), (5.0, 5.0), 0), vec![(5.0, 5.0)]);
    }

    #[test]
    fn game_cursor_stays_inside_the_render_target() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
        let extent = viewport_extent(800.0, 600.0, 2.0, 1.0);
        assert_eq!(extent, (1600, 1200));

        assert_eq!(game_cursor(rect, rect.min, extent), [0.0, 0.0]);

        let corner = game_cursor(rect, rect.max, extent);
        assert_eq!(corner, [extent.0 as f32, extent.1 as f32]);

        let middle = game_cursor(rect, rect.center(), extent);
        assert_eq!(middle, [extent.0 as f32 / 2.0, extent.1 as f32 / 2.0]);
    }

    #[test]
    fn game_cursor_follows_the_clamped_extent() {
        // 2560 points × DPR 2 = 5120 物理像素，但离屏目标夹在 4096：
        // 光标必须跟 `screen` 用同一把尺子，否则最右侧会落在画面外（#405）。
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(2560.0, 1440.0));
        let extent = viewport_extent(rect.width(), rect.height(), 2.0, 1.0);
        assert_eq!(extent, (4096, 2880));

        let right_edge = game_cursor(rect, Pos2::new(rect.max.x, 0.0), extent);
        assert_eq!(right_edge[0], extent.0 as f32);
        assert!(
            right_edge[0] <= extent.0 as f32,
            "光标不该超过 InputSnapshot::screen（{} > {}）",
            right_edge[0],
            extent.0
        );
    }
}
