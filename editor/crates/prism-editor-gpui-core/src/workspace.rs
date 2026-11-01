//! 编辑器工作区状态：打开的文档、选择、dock 布局、主题、日志、Play 状态。
//!
//! 这一层**不依赖任何 UI 框架**，GPUI 只是它的一个视图。因此同一份状态可以
//! 单测，也可以在将来喂给别的宿主（例如自动化脚本）。

use crate::assets::{AssetDetails, AssetInfo, AssetKind, AssetQuery, AssetReference, AssetTree};
use crate::doc::{EntityJson, SceneDoc, TransformJson, PREFAB_LINK_COMPONENT};
use crate::dock::{
    self, collect_tabs, find_leaf, find_leaf_by_id, insert_panel, leaf, remove_panel, DockEdge,
    DockNode, DockZone, FloatPanel, LAYOUT_PRESETS,
};
use crate::i18n::{I18n, Locale};
use crate::keys::Command;
use crate::logbuf::{LogBuffer, LogLevel, LogSource};
use crate::manifest::SceneManifest;
use crate::project::{self, ProjectFile};
use crate::schema::{component_value_with_overrides, EntityPreset, Schema, TypeSchema};
use crate::theme::{ThemeColors, ThemeState, ThemeToken};
use crate::viewport_math::{
    basis_from_quat, parent_world_rotation, quat_axis_angle, quat_conjugate, quat_mul,
    quat_normalize, quat_rotate, world_bounds, world_position, world_rotation, world_scale,
    GizmoSpace,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

fn world_delta_to_parent_local(doc: &SceneDoc, index: usize, delta_world: [f32; 3]) -> [f32; 3] {
    let parent = doc.entities[index]
        .parent
        .and_then(|parent_id| doc.index_of_id(parent_id));
    let parent_rotation = parent
        .map(|parent| world_rotation(doc, parent))
        .unwrap_or(crate::viewport_math::IDENTITY_ROTATION);
    let scale = parent
        .map(|parent| world_scale(doc, parent))
        .unwrap_or([1.0; 3]);
    let unscaled = [
        delta_world[0] / scale[0].abs().max(1e-6),
        delta_world[1] / scale[1].abs().max(1e-6),
        delta_world[2] / scale[2].abs().max(1e-6),
    ];
    let rotated = quat_rotate(quat_conjugate(parent_rotation), unscaled);
    rotated
}

fn world_position_to_local(doc: &SceneDoc, index: usize, target_world: [f32; 3]) -> [f32; 3] {
    let Some(parent) = doc.entities[index]
        .parent
        .and_then(|parent_id| doc.index_of_id(parent_id))
    else {
        return target_world;
    };
    let parent_position = world_position(doc, parent);
    let delta = [
        target_world[0] - parent_position[0],
        target_world[1] - parent_position[1],
        target_world[2] - parent_position[2],
    ];
    world_delta_to_parent_local(doc, index, delta)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocKind {
    Scene,
    Prefab,
}

impl DocKind {
    pub fn folder(self) -> &'static str {
        match self {
            DocKind::Scene => "assets/scenes",
            DocKind::Prefab => "assets/prefabs",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            DocKind::Scene => ".scene.json",
            DocKind::Prefab => ".prefab.json",
        }
    }

    pub fn i18n_key(self) -> &'static str {
        match self {
            DocKind::Scene => "common.scene",
            DocKind::Prefab => "common.prefab",
        }
    }

    pub fn parse(text: &str) -> DocKind {
        if text == "prefab" {
            DocKind::Prefab
        } else {
            DocKind::Scene
        }
    }
}

/// 从场景实例打开预制体时记录的来源（#420）。
///
/// 只存在内存里，不进文档格式；面包屑用它切回场景并选中原来的实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefabOrigin {
    /// 来源场景在 `Workspace::docs` 里的下标。
    pub scene_index: usize,
    /// 场景里被打开的实例根实体 id（不是下标 —— 换场 / 重排后仍然有效）。
    pub entity_id: u32,
    /// 面包屑显示的场景名（下标失效时至少还有提示）。
    pub scene_title: String,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub kind: DocKind,
    pub path: Option<String>,
    pub doc: SceneDoc,
    pub undo: crate::undo::UndoStack,
    pub dirty: bool,
    /// 上一次读 / 写这份文件时的版本戳（mtime + 长度，#495）；`None` = 还没落过盘。
    pub saved_stamp: Option<project::FileStamp>,
    /// 磁盘上的版本已经和 `saved_stamp` 不一致，且用户还没处理（#452）。
    pub externally_modified: bool,
    /// 从场景实例打开预制体时的来源（#420）；普通打开 / 场景文档为 `None`。
    pub prefab_origin: Option<PrefabOrigin>,
    /// Scene 视图的会话级隐藏实体 id（#417）。
    ///
    /// 故意不放进 [`SceneDoc`]：可见性是编辑器查看状态，不是作者数据，保存不会
    /// 写进 `.scene.json`，也不会在场景之间串味。删除实体后残留的 id 无害，
    /// 解析时会被忽略。
    pub hidden_entities: HashSet<u32>,
}

impl Document {
    pub fn new(kind: DocKind, path: Option<String>, doc: SceneDoc) -> Self {
        Self {
            kind,
            path,
            doc,
            undo: crate::undo::UndoStack::new(),
            dirty: false,
            saved_stamp: None,
            externally_modified: false,
            prefab_origin: None,
            hidden_entities: HashSet::new(),
        }
    }

    /// 记下「刚读过 / 刚写过」这个文件的版本戳。
    pub fn note_disk_state(&mut self, root: Option<&std::path::Path>) {
        let (Some(root), Some(path)) = (root, self.path.as_deref()) else {
            self.saved_stamp = None;
            self.externally_modified = false;
            return;
        };
        self.saved_stamp = project::file_stamp(root, path);
        self.externally_modified = false;
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(path) => path.rsplit('/').next().unwrap_or(path).to_string(),
            None => match self.kind {
                DocKind::Scene => "Untitled.scene.json".to_string(),
                DocKind::Prefab => "Untitled.prefab.json".to_string(),
            },
        }
    }

    pub fn title_with_dirty(&self) -> String {
        if self.dirty {
            format!("{} *", self.title())
        } else {
            self.title()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PanelInfo {
    pub id: String,
    pub title: String,
    pub menu_path: String,
    pub shortcut: Option<String>,
    /// 辅助窗口（工具/设置/分析）默认并进检查器 leaf。
    pub utility: bool,
}

impl PanelInfo {
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            menu_path: String::new(),
            shortcut: None,
            utility: dock::UTILITY_TABS.contains(&id),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDoc {
    pub id: String,
    pub label: String,
    pub kind: DocKind,
    pub doc: SceneDoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComponentClipboard {
    pub component: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityClipboard {
    /// 每个顶层选中实体对应一棵独立子树（id 已从 1 重新编号）。
    pub trees: Vec<SceneDoc>,
    /// 是否来自剪切（仅用于界面提示，粘贴行为相同）。
    pub cut: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrbitCamera {
    pub target: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    /// 2D 视图（#641）：正交投影 + 视角锁在 XY 平面（`yaw = pitch = 0`，
    /// 看向 −Z，+X 右 / +Y 上），只允许平移与缩放。
    ///
    /// `distance` 在 2D 里同时充当正交半高，所以滚轮缩放的手感不变。
    pub orthographic: bool,
    /// 垂直视场角（度）。`project` / 拾取射线与引擎相机共用它（#416）。
    pub fov_y_degrees: f32,
    /// 近裁剪面（世界单位），同步给引擎相机。
    pub near: f32,
    /// 远裁剪面（世界单位），同步给引擎相机。
    pub far: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            distance: 12.0,
            orthographic: false,
            fov_y_degrees: 60.0,
            near: 0.1,
            far: 1000.0,
        }
    }
}

impl OrbitCamera {
    /// 切换 2D 视图。进入时把视角锁到 XY 正视，退出时保留当前平移/缩放。
    pub fn set_2d(&mut self, on: bool) {
        self.orthographic = on;
        if on {
            self.yaw = 0.0;
            self.pitch = 0.0;
        }
    }

    pub fn toggle_2d(&mut self) {
        self.set_2d(!self.orthographic);
    }

    /// 设置垂直视场角（度），夹在 10°–170°；非有限值忽略（#416）。
    pub fn set_fov_degrees(&mut self, degrees: f32) {
        if degrees.is_finite() {
            self.fov_y_degrees = degrees.clamp(10.0, 170.0);
        }
    }

    /// 设置近 / 远裁剪面；`near > 0`、`far` 至少比 `near` 大 1 cm，非法输入忽略（#416）。
    pub fn set_clip_planes(&mut self, near: f32, far: f32) {
        if !near.is_finite() || !far.is_finite() || near <= 0.0 {
            return;
        }
        let far = far.max(near + 0.01);
        self.near = near;
        self.far = far;
    }

    /// 正交投影的半高（世界单位）。透视相机没有这个概念。
    pub fn ortho_half_height(&self) -> Option<f32> {
        self.orthographic.then_some(self.distance.max(0.05))
    }

    /// 把相机摆到世界轴的正 / 负方向看回 `target`（#416 的方向 gizmo / Align To Axis）。
    ///
    /// `axis`：0 = X、1 = Y、2 = Z；`sign < 0` 表示从负半轴看过去。
    /// 与 [`Self::eye`] 的球面参数一致：先由 `side` 求 `pitch`，再由 XZ 分量求 `yaw`。
    /// 正交视图同样适用（`orthographic` 只锁定轨道拖拽，不阻止轴向对齐）。
    pub fn align_to_axis(&mut self, axis: usize, sign: f32) {
        if axis > 2 {
            return;
        }
        let sign = if sign < 0.0 { -1.0 } else { 1.0 };
        let mut side = [0.0f32; 3];
        side[axis] = sign;
        self.pitch = side[1].clamp(-1.0, 1.0).asin();
        // ±Y 轴的 `cos(pitch) == 0`，yaw 无意义，固定 0 让结果稳定。
        self.yaw = if side[0].abs() > 1e-6 || side[2].abs() > 1e-6 {
            side[0].atan2(side[2])
        } else {
            0.0
        };
    }

    pub fn eye(&self) -> [f32; 3] {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        [
            self.target[0] + self.distance * cos_pitch * sin_yaw,
            self.target[1] + self.distance * sin_pitch,
            self.target[2] + self.distance * cos_pitch * cos_yaw,
        ]
    }

    /// 拖动轨道（像素 → 弧度）。2D 视图锁定视角，直接忽略。
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        if self.orthographic {
            return;
        }
        self.yaw -= dx * 0.01;
        self.pitch = (self.pitch + dy * 0.01).clamp(-1.5, 1.5);
    }

    /// 飞行相机观察：保持相机位置不变，只改变视线方向。
    pub fn look_in_place(&mut self, dx: f32, dy: f32) {
        let eye = self.eye();
        self.orbit(dx, dy);
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        self.target = [
            eye[0] - self.distance * cos_pitch * sin_yaw,
            eye[1] - self.distance * sin_pitch,
            eye[2] - self.distance * cos_pitch * cos_yaw,
        ];
    }

    /// Move the eye along its horizontal view basis and world vertical axis.
    pub fn fly_move(&mut self, forward: f32, right: f32, vertical: f32, dt: f32) {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let speed = (self.distance * 0.5).clamp(2.0, 12.0) * dt.max(0.0);
        let forward_axis = [-cos_pitch * sin_yaw, -sin_pitch, -cos_pitch * cos_yaw];
        let right_axis = [cos_yaw, 0.0, -sin_yaw];
        let delta = [
            (forward_axis[0] * forward + right_axis[0] * right) * speed,
            (forward_axis[1] * forward + vertical) * speed,
            (forward_axis[2] * forward + right_axis[2] * right) * speed,
        ];
        self.target = [
            self.target[0] + delta[0],
            self.target[1] + delta[1],
            self.target[2] + delta[2],
        ];
    }

    pub fn zoom(&mut self, delta: f32) {
        self.distance = (self.distance * (1.0 - delta * 0.001)).clamp(0.2, 500.0);
    }

    /// 缩放到光标（#511）：`anchor` 是光标射线上的世界点，缩放后它仍在同一屏幕位置。
    ///
    /// 与 Unity `SceneViewMotion.HandleScrollWheel` 一致：先按 [`Self::zoom`] 改
    /// `distance`，再按 `1 - new/old` 的比例把 `target` 拉向锚点。推导上这是精确的
    /// ——锚点始终在自己的射线上（透视）或自己那条平行线上（正交），所以屏幕位置不变。
    ///
    /// `anchor` 为 `None` 或含非有限值时就退化成画面中心缩放。
    pub fn zoom_towards(&mut self, delta: f32, anchor: Option<[f32; 3]>) {
        let before = self.distance;
        self.zoom(delta);
        let Some(anchor) = anchor.filter(|anchor| anchor.iter().all(|value| value.is_finite()))
        else {
            return;
        };
        if !before.is_finite() || before <= 0.0 {
            return;
        }
        let percentage = 1.0 - self.distance / before;
        if !percentage.is_finite() {
            return;
        }
        for (target, anchor) in self.target.iter_mut().zip(anchor) {
            *target += (anchor - *target) * percentage;
        }
    }

    pub fn focus(&mut self, point: [f32; 3], radius: f32) {
        self.target = point;
        if self.orthographic {
            // 正交半高直接等于缩放半径，留 20% 余量。
            self.distance = (radius * 1.2).max(0.5);
            return;
        }
        // 垂直 fov（默认 60°）：距离 = 半径 / sin(fov/2) 留 20% 余量。
        let half_fov = (self.fov_y_degrees.clamp(10.0, 170.0) * 0.5).to_radians();
        self.distance = (radius / half_fov.sin() * 1.2).max(0.5);
    }
}

/// 变换吸附步长（Unity `SnapSettings` 的等价物）。
///
/// 默认值与 Unity 一致（网格 0.25、旋转 15°、缩放 1）。编辑器视口在拖动
/// gizmo 时把 [`Self::enabled`] 与按住 Ctrl 组合起来用（Ctrl 临时反转）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapSettings {
    /// 工具栏里的常驻开关；按住 Ctrl 拖动会临时反转它。
    pub enabled: bool,
    /// 平移步长（世界单位）。
    pub translate: f32,
    /// 旋转步长（度）。
    pub rotate_degrees: f32,
    /// 缩放步长。
    pub scale: f32,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            translate: 0.25,
            rotate_degrees: 15.0,
            scale: 1.0,
        }
    }
}

impl SnapSettings {
    fn snapped(value: f32, step: f32) -> f32 {
        if step <= 0.0 {
            value
        } else {
            (value / step).round() * step
        }
    }

    /// 吸附一个平移量（世界单位）。
    pub fn snapped_translate(&self, value: f32) -> f32 {
        Self::snapped(value, self.translate)
    }

    /// 吸附一个角度（弧度），按 [`Self::rotate_degrees`] 取整。
    pub fn snapped_radians(&self, radians: f32) -> f32 {
        let degrees = radians.to_degrees();
        Self::snapped(degrees, self.rotate_degrees).to_radians()
    }

    /// 吸附一个缩放分量；吸附后不会小于 0.01。
    pub fn snapped_scale(&self, value: f32) -> f32 {
        Self::snapped(value, self.scale).max(0.01)
    }
}

/// gizmo 的枢轴模式（#416，Unity `Tools.pivotMode`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PivotMode {
    /// 选择集包围中心：多选时 gizmo 在中心，旋转 / 缩放带动整个组（默认）。
    #[default]
    Center,
    /// 活动对象自己的轴心：gizmo 落在活动物体上，每个对象只绕自身变换。
    Pivot,
}

/// 一次点选 / 框选对选择集的作用方式。
///
/// 与 Unity 的 `SelectionType` 对齐：`Normal` = 替换，`Additive` = 并入，
/// `Subtractive` = 移除。修饰键如何映射由调用方决定（见 [`SelectMode::from_modifiers`]），
/// 数据层只负责集合运算，所以点空白在这种语义下也能「保留当前选择」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectMode {
    /// 覆盖选择：点空白时清空。
    #[default]
    Replace,
    /// 并入选择：点空白时不改变。
    Add,
    /// 从选择中移除：点空白时不改变。
    Remove,
}

impl SelectMode {
    /// Shift = 并入，Ctrl / Cmd = 移除，否则替换。
    pub fn from_modifiers(shift: bool, command: bool) -> Self {
        if command {
            Self::Remove
        } else if shift {
            Self::Add
        } else {
            Self::Replace
        }
    }
}

/// 组件预设（#418）：把某个组件当前的值存下来，之后一键套到别的实体上。
///
/// 存在 `Workspace` 里并随编辑器状态（`target/editor-egui/state.json`）持久化；
/// `component` 是完整的反射名（`prism_engine::scene::Camera`），同名预设后写覆盖前写。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentPreset {
    pub component: String,
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub docs: Vec<Document>,
    active: usize,
    pub schema: Schema,
    pub presets: Vec<EntityPreset>,
    /// 组件预设（#418）。
    pub component_presets: Vec<ComponentPreset>,
    pub templates: Vec<TemplateDoc>,
    pub assets: Vec<AssetInfo>,
    pub tree: AssetTree,
    pub asset_query: AssetQuery,
    pub expanded_assets: HashSet<String>,
    pub panels: Vec<PanelInfo>,
    pub dock: DockNode,
    pub floating: Vec<FloatPanel>,
    /// 被最大化显示的 tab（`None` = 正常布局）。
    ///
    /// 只影响绘制：dock tree 原样保留，所以还原时不会丢布局。
    pub maximized_tab: Option<String>,
    /// 按 Play 时自动最大化 Game 视图（Unity `maximizeOnPlay`，退出时还原）。
    pub maximize_on_play: bool,
    /// 进入 Play 前的最大化状态；`None` = 这次 Play 没有改动布局。
    ///
    /// 用 `Option<Option<String>>` 而不是 `Option<String>`：进入 Play 前本来没有
    /// 最大化任何 tab 时，退出 Play 必须回到「都不最大化」，不能把 `Some(tab)`
    /// 当成「没保存」。
    play_maximized_before: Option<Option<String>>,
    pub selection: Vec<usize>,
    /// 最后点击 / 右键的实体（检查器与 Add Component 的「活动对象」，#507）。
    ///
    /// 选择集仍按文档顺序保存用于批量操作，但 `primary_selection()` 优先返回它。
    pub selection_active: Option<usize>,
    pub expanded: HashSet<u32>,
    pub clipboard: Option<ComponentClipboard>,
    /// 实体剪贴板（Ctrl+C/X/V）：每项是一棵已重新编号的子树。
    pub entity_clipboard: Option<EntityClipboard>,
    pub theme: ThemeState,
    pub i18n: I18n,
    pub log: LogBuffer,
    pub camera: OrbitCamera,
    /// W/E/R gizmo 使用世界轴还是实体自身轴。
    pub gizmo_space: GizmoSpace,
    /// gizmo 拖动的吸附步长（工具栏 `Snap (Ctrl)` 开关）。
    pub snap: SnapSettings,
    /// gizmo 枢轴模式（工具栏 `Pivot` 开关，#416）。
    pub pivot_mode: PivotMode,
    /// Scene View 是否画世界网格。
    pub show_grid: bool,
    /// Scene View 是否给选中对象画轮廓（#487）。选中反馈弱时唯一的线索是 gizmo，
    /// 密集场景里容易改错对象。
    pub show_selection_outline: bool,
    /// Scene 视图是否处于 Isolate 模式（#417）：只渲染当前选择及其子树。
    ///
    /// 和 [`Document::hidden_entities`] 一样是会话级状态，不进 `.scene.json`；
    /// 切换文档时自动关闭，避免新文档因为空选择而整场变黑。
    pub isolate: bool,
    /// Scene View 是否画相机图标 / 视锥（#416）。
    pub show_camera_icons: bool,
    /// Scene View 是否画光源图标 / 范围（#416）。
    pub show_light_icons: bool,
    /// Scene View 是否用场景的天空盒当背景（关闭时退回纯色）。
    ///
    /// 对应引擎的 `SceneViewState.showSkybox`（默认开）。
    pub show_skybox: bool,
    pub playing: bool,
    pub paused: bool,
    pub play_time_s: f32,
    pub status: String,
    /// 保存被「磁盘已改动」挡下时记下文档下标，由 UI 弹冲突对话框（#452）。
    pub pending_save_conflict: Option<usize>,
    pub layout_preset: String,
    /// 项目根（含 `assets/`）；`None` = 纯 fixture / 内置样例模式。
    pub project_root: Option<PathBuf>,
    /// 磁盘上的场景 / 预制体（`assets/scenes`、`assets/prefabs`）。
    pub project_files: Vec<ProjectFile>,
    /// `assets/scenes.json` 的场景清单（顺序 = 构建 / 启动顺序）。
    pub scene_manifest: SceneManifest,
    /// 进入 Play 时的编辑态快照；`None` = 不在播放/没有快照（#500）。
    play_snapshot: Option<PlaySnapshot>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::sample()
    }
}

/// 进入 Play 时的编辑态快照（#500）。
///
/// 播放期间的编辑现在仍然直接写作者文档（运行时副本是后续条目），所以退出播放必须把
/// 文档、撤销栈、脏标记和选中一起放回去；否则试验性改动会跟着下一次保存落到磁盘，
/// 撤销栈里也会混进运行期的步骤。
#[derive(Debug, Clone)]
struct PlaySnapshot {
    /// 播放开始时的活动文档下标。播放中用户可能切到别的文档，恢复时必须回到这一份。
    doc: usize,
    scene: SceneDoc,
    undo: crate::undo::UndoStack,
    dirty: bool,
    selection: Vec<usize>,
    selection_active: Option<usize>,
}

/// 实例相对源预制体的一个字段差异（Unity 的 Prefab Override）。
#[derive(Debug, Clone, PartialEq)]
pub struct PrefabOverride {
    /// 实例里的实体下标（不是 id）。
    pub entity: usize,
    pub entity_name: String,
    /// `"transform"` 或组件名。
    pub component: String,
    pub field: String,
    pub instance_value: Value,
    pub prefab_value: Value,
}

/// 把一组下标映射成实体 id（#513）。没有 id 的实体跳过。
fn selection_ids_in(doc: &crate::doc::SceneDoc, selection: &[usize]) -> Vec<u32> {
    selection
        .iter()
        .filter_map(|index| doc.entities.get(*index).and_then(|entity| entity.id))
        .collect()
}

impl Workspace {
    /// 一个自包含的最小工作区（没有 fixture 时也能启动）。
    pub fn sample() -> Self {
        let mut doc = SceneDoc::default();
        let camera = doc.add_named_child(None, "Main Camera");
        doc.entities[camera].transform = TransformJson {
            position: [0.0, 1.0, -10.0],
            ..Default::default()
        };
        let cube = doc.add_named_child(None, "Cube");
        doc.entities[cube].transform.position = [0.0, 0.5, 0.0];
        let light = doc.add_named_child(None, "Directional Light");
        doc.entities[light].transform.rotation = [0.3, 0.2, 0.1, 0.9];
        let _ = doc.add_named_child(Some(cube), "Detail");

        let mut workspace = Self {
            docs: vec![Document::new(DocKind::Scene, None, doc)],
            active: 0,
            schema: Schema::default(),
            presets: Vec::new(),
            component_presets: Vec::new(),
            templates: Vec::new(),
            assets: Vec::new(),
            tree: AssetTree::default(),
            asset_query: AssetQuery::default(),
            expanded_assets: HashSet::new(),
            panels: default_panels(),
            dock: dock::default_dock(),
            floating: Vec::new(),
            maximized_tab: None,
            maximize_on_play: false,
            play_maximized_before: None,
            selection: Vec::new(),
            selection_active: None,
            expanded: HashSet::new(),
            clipboard: None,
            entity_clipboard: None,
            theme: ThemeState::default(),
            i18n: I18n::default(),
            log: LogBuffer::default(),
            camera: OrbitCamera::default(),
            gizmo_space: GizmoSpace::World,
            snap: SnapSettings::default(),
            pivot_mode: PivotMode::Center,
            show_grid: true,
            show_selection_outline: true,
            isolate: false,
            show_camera_icons: true,
            show_light_icons: true,
            show_skybox: true,
            playing: false,
            paused: false,
            play_time_s: 0.0,
            status: String::new(),
            pending_save_conflict: None,
            layout_preset: "default".to_string(),
            project_root: None,
            project_files: Vec::new(),
            scene_manifest: SceneManifest::default(),
            play_snapshot: None,
        };
        workspace.log(
            LogSource::Editor,
            LogLevel::Info,
            "PrismaRev GPUI 编辑器已启动",
        );
        workspace
    }

    /// 从编辑器 fixture（`editor/public/demo.json`）恢复全部数据。
    pub fn load_fixture(value: &Value) -> Self {
        let mut workspace = Self::sample();

        if let Some(schema) = value.get("schema").and_then(Schema::from_value) {
            workspace.schema = schema;
        }
        if let Some(presets) = value.get("presets") {
            workspace.presets = EntityPreset::from_list(presets);
        }
        if let Some(assets) = value.get("assetInfos") {
            workspace.assets = crate::assets::parse_assets(assets);
        }
        workspace.tree = AssetTree::build(&workspace.assets);

        // 模板文档（预制体 + 场景）。
        if let Some(template_docs) = value.get("templateDocs").and_then(|v| v.as_object()) {
            if let Some(labels) = value.get("templatesByKind").and_then(|v| v.as_object()) {
                for (kind_name, entries) in labels {
                    let kind = DocKind::parse(kind_name);
                    let Some(items) = entries.as_array() else {
                        continue;
                    };
                    for item in items {
                        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let label = item
                            .get("label")
                            .and_then(|v| v.as_str())
                            .unwrap_or(id)
                            .to_string();
                        if let Some(doc_value) =
                            template_docs.get(kind_name).and_then(|docs| docs.get(id))
                        {
                            if let Ok(mut doc) =
                                serde_json::from_value::<SceneDoc>(doc_value.clone())
                            {
                                doc.normalize();
                                workspace.templates.push(TemplateDoc {
                                    id: id.to_string(),
                                    label,
                                    kind,
                                    doc,
                                });
                            }
                        }
                    }
                }
            }
        }

        // 打开的文档（fixture 的 `docs` 是 path → SceneDoc）。
        let mut opened: Vec<Document> = Vec::new();
        if let Some(docs) = value.get("docs").and_then(|v| v.as_object()) {
            for (path, doc_value) in docs {
                let Ok(mut doc) = serde_json::from_value::<SceneDoc>(doc_value.clone()) else {
                    continue;
                };
                doc.normalize();
                let kind = if path.ends_with(".prefab.json") {
                    DocKind::Prefab
                } else {
                    DocKind::Scene
                };
                opened.push(Document::new(kind, Some(path.clone()), doc));
            }
        }
        if !opened.is_empty() {
            opened.sort_by(|a, b| {
                a.path
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.path.as_deref().unwrap_or(""))
            });
            workspace.docs = opened;
        }

        // 面板注册表来自后端声明的 EditorWindow 列表。
        if let Some(windows) = value.get("windows").and_then(|v| v.as_array()) {
            let mut panels = Vec::new();
            for window in windows {
                let Some(id) = window.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                panels.push(PanelInfo {
                    id: id.to_string(),
                    title: window
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or(id)
                        .to_string(),
                    menu_path: window
                        .get("menuPath")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    shortcut: window
                        .get("shortcut")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    utility: dock::UTILITY_TABS.contains(&id),
                });
            }
            if !panels.is_empty() {
                workspace.panels = panels;
            }
        }

        workspace.hydrate_documents();
        workspace.active = 0;
        workspace.log(
            LogSource::Editor,
            LogLevel::Info,
            format!(
                "已载入 fixture：{} 个文档 / {} 个组件类型 / {} 个资产",
                workspace.docs.len(),
                workspace.schema.types.len(),
                workspace.assets.len()
            ),
        );
        workspace
    }

    /// 用 schema 的 `defaultJson` 补齐组件缺失字段。
    ///
    /// Web 编辑器先 `from_object` 再回填，所以字段永远齐全；GPUI 直接操作作者 JSON，
    /// 必须在加载后补默认值，否则「拖动一个旧文档里没写过的字段」会静默失败。
    pub fn hydrate_documents(&mut self) {
        let schema = self.schema.clone();
        for document in &mut self.docs {
            hydrate_doc(&schema, &mut document.doc);
        }
        for template in &mut self.templates {
            hydrate_doc(&schema, &mut template.doc);
        }
    }

    // ── 项目文件 ────────────────────────────────────────────────────────

    /// 绑定项目根：扫描 `assets/scenes` / `assets/prefabs`，用磁盘文档替换 fixture 文档。
    /// 返回实际打开的文档数。
    pub fn load_project(&mut self, root: PathBuf) -> io::Result<usize> {
        let files = project::scan_documents(&root);
        let mut opened: Vec<Document> = Vec::new();
        for file in &files {
            match project::read_document(&root, &file.path) {
                Ok(doc) => {
                    let mut document = Document::new(file.kind, Some(file.path.clone()), doc);
                    document.note_disk_state(Some(&root));
                    opened.push(document);
                }
                Err(error) => self.log(
                    LogSource::Editor,
                    LogLevel::Warn,
                    format!("读取 {} 失败: {error}", file.path),
                ),
            }
        }

        self.project_root = Some(root);
        self.project_files = files;
        if let Some(root) = self.project_root.clone() {
            self.assets = project::scan_assets(&root);
            self.tree = AssetTree::build(&self.assets);
            for folder in project::scan_asset_dirs(&root) {
                self.tree.insert_dir(&folder);
            }
        }
        match SceneManifest::read(self.project_root.as_deref().expect("project_root 刚刚赋值"))
        {
            Ok(manifest) => self.scene_manifest = manifest,
            Err(error) => self.log(
                LogSource::Editor,
                LogLevel::Warn,
                format!("读取 assets/scenes.json 失败: {error}"),
            ),
        }
        if !opened.is_empty() {
            self.docs = opened;
            // 优先打开场景（而不是预制体）作为当前文档。
            self.active = self
                .docs
                .iter()
                .position(|document| document.kind == DocKind::Scene)
                .unwrap_or(0);
            self.clear_selection();
            self.expanded.clear();
        }
        self.hydrate_documents();
        let count = self.docs.len();
        self.log(
            LogSource::System,
            LogLevel::Info,
            format!(
                "项目：{} 个文档 / {} 个文件",
                count,
                self.project_files.len()
            ),
        );
        Ok(count)
    }

    /// 从资产面板实例化预制体到场景（拖预制体到层级）。
    /// 磁盘优先，fixture 模式回退同名模板；返回插入的实体数。
    pub fn instantiate_prefab_asset(
        &mut self,
        relative: &str,
        parent: Option<usize>,
    ) -> io::Result<usize> {
        let prefab = match &self.project_root {
            Some(root) if root.join(relative).exists() => project::read_document(root, relative)?,
            _ => {
                let stem = relative
                    .rsplit('/')
                    .next()
                    .unwrap_or(relative)
                    .trim_end_matches(".prefab.json");
                self.templates
                    .iter()
                    .find(|template| template.kind == DocKind::Prefab && template.id == stem)
                    .map(|template| template.doc.clone())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("找不到预制体 {relative}"))
                    })?
            }
        };

        let parent = parent.filter(|index| *index < self.document().doc.entities.len());
        let inserted = self.edit("实例化预制体", |document| {
            document.doc.instantiate_prefab(&prefab, parent, relative)
        });
        if let Some(root) = inserted.first() {
            self.selection = vec![*root];
            self.selection_active = Some(*root);
            if let Some(id) = self.document().doc.id_of_index(*root) {
                self.expanded.insert(id);
            }
        }
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已实例化 {relative}（{} 个实体）", inserted.len()),
        );
        Ok(inserted.len())
    }

    /// 把当前选中实体子树存成预制体文件（「另存为预制体」）。
    /// 重名自动追加 `-2`、`-3`；不修改当前场景。
    pub fn save_selection_as_prefab(&mut self, name: &str) -> io::Result<String> {
        self.save_selection_as_prefab_at(Some("assets/prefabs"), name)
    }

    /// 把当前选择存成预制体到指定 `assets/...` 目录（层级拖到资产目录用）。
    pub fn save_selection_as_prefab_at(
        &mut self,
        folder: Option<&str>,
        name: &str,
    ) -> io::Result<String> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法保存预制体")
        })?;
        let index = self
            .primary_selection()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "未选择实体"))?;
        let doc = self.document().doc.extract_subtree(&[index]);
        let doc = self.pruned_doc(doc);
        let stem = project::sanitize_stem(name);
        let folder = folder
            .filter(|folder| folder.starts_with("assets/"))
            .unwrap_or("assets/prefabs");
        let mut path = format!("{folder}/{stem}.prefab.json");
        let mut suffix = 2;
        while root.join(&path).exists() {
            path = format!("{folder}/{stem}-{suffix}.prefab.json");
            suffix += 1;
        }
        project::write_document(&root, &path, &doc)?;
        self.refresh_project_files();
        self.refresh_assets();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已存为预制体 {path}（{} 个实体）", doc.entities.len()),
        );
        Ok(path)
    }

    /// 把实例当前内容覆盖写回源预制体（`PrefabUtility.ApplyPrefabInstance`）。
    ///
    /// 只写预制体文件，不改变场景；根上的 `PrefabInstance` 链接会被剥掉，
    /// 避免预制体引用自己，嵌套实例的链接保持不变。
    pub fn apply_prefab_instance(&mut self, entity_id: u32) -> io::Result<String> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "没有绑定项目根，无法应用预制体修改",
            )
        })?;
        let index = self
            .document()
            .doc
            .index_of_id(entity_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "实例不存在"))?;
        let link = self
            .document()
            .doc
            .entities
            .get(index)
            .and_then(|entity| entity.prefab_link())
            .map(str::to_string)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "不是预制体实例"))?;
        if !link.starts_with("assets/") || !link.ends_with(".prefab.json") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("非法的预制体路径 {link}"),
            ));
        }

        let doc = self.document().doc.extract_subtree(&[index]);
        let doc = self.pruned_doc(doc);
        project::write_document(&root, &link, &doc)?;
        self.refresh_project_files();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已应用预制体修改 {link}（{} 个实体）", doc.entities.len()),
        );
        Ok(link)
    }

    /// 删除资产文件（只允许 `assets/` 内，场景/预制体走文档流程）。
    /// 引用不会自动清理（与 Web 编辑器一致：引用变成悬空值，由检查器提示）。
    pub fn delete_asset(&mut self, path: &str) -> io::Result<()> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法删除资产")
        })?;
        if !path.starts_with("assets/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只允许删除 assets/ 内的文件",
            ));
        }
        if path.ends_with(".scene.json") || path.ends_with(".prefab.json") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "场景 / 预制体请用文档面板",
            ));
        }
        let full = root.join(path);
        if !full.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path} 不存在"),
            ));
        }
        std::fs::remove_file(full)?;
        self.refresh_project_files();
        self.log(LogSource::Editor, LogLevel::Info, format!("已删除 {path}"));
        Ok(())
    }

    /// 删除场景 / 预制体文档：磁盘文件、打开文档、项目文件表与 `scenes.json` 条目一起清理。
    ///
    /// 不可撤销，调用方负责确认；预制体实例会保留（链接悬空，便于发现）。
    pub fn delete_document(&mut self, path: &str) -> io::Result<()> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法删除文档")
        })?;
        if !path.starts_with("assets/")
            || !(path.ends_with(".scene.json") || path.ends_with(".prefab.json"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只允许删除 assets/ 下的 .scene.json / .prefab.json",
            ));
        }
        let full = root.join(path);
        if !full.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path} 不存在"),
            ));
        }
        std::fs::remove_file(full)?;

        let removed = self
            .docs
            .iter()
            .position(|document| document.path.as_deref() == Some(path));
        self.docs
            .retain(|document| document.path.as_deref() != Some(path));
        if let Some(index) = removed {
            if index < self.active {
                self.active -= 1;
            }
        }
        self.active = self.active.min(self.docs.len().saturating_sub(1));
        self.clear_selection();
        self.expanded.clear();
        if self.docs.is_empty() {
            self.new_document(DocKind::Scene, None);
        }

        let doc_relative = path.trim_start_matches("assets/");
        if let Some(position) = self.scene_manifest.scenes.iter().position(|entry| {
            crate::manifest::doc_relative_path(&entry.path).as_deref() == Some(doc_relative)
        }) {
            self.scene_manifest.remove(position);
            self.persist_scene_manifest();
        }
        self.project_files.retain(|file| file.path != path);
        self.log(LogSource::Editor, LogLevel::Info, format!("已删除 {path}"));
        Ok(())
    }

    /// 资产重命名 / 移动：先改磁盘文件，再改写**所有**文档里的资产路径引用。
    ///
    /// 打开着的文档改内存（标脏，等用户保存）；没打开的文档直接读-改-写落盘，
    /// 否则关着的那份会留着断掉的旧路径（#465）。只碰资产路径字段，实体名 /
    /// UI 文本里恰好写着同一段路径的字符串不受影响。
    ///
    /// 返回 `(替换次数, 受影响文档)`。
    pub fn rename_asset(&mut self, from: &str, to: &str) -> io::Result<(usize, Vec<String>)> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法重命名资产")
        })?;
        if from.ends_with(".scene.json") || from.ends_with(".prefab.json") {
            // 文档走「另存为 / 打开」路径，避免和未保存内容打架。
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "场景 / 预制体请用另存为或文档面板",
            ));
        }
        project::rename_path(&root, from, to)?;

        let (references, affected) =
            self.rewrite_moved_references(&root, &[(from.to_string(), to.to_string())]);
        Ok((references, affected))
    }

    /// 资产移动（重命名 / 拖到目录 / 目录改名）之后，改写所有文档里的引用。
    ///
    /// 打开着的文档就地改并标脏；磁盘上没打开的文档读-改-写回。返回
    /// `(改写处数, 受影响文档)`，无法解析的引用只在日志里 warn（#465）。
    fn rewrite_moved_references(
        &mut self,
        root: &Path,
        moves: &[(String, String)],
    ) -> (usize, Vec<String>) {
        let mut references = 0;
        let mut affected = Vec::new();
        let mut ambiguous: Vec<String> = Vec::new();
        let mut open_paths: Vec<String> = Vec::new();

        for document in &mut self.docs {
            let doc_dir = document_dir(document.path.as_deref());
            for (from, to) in moves {
                let rewrite = document.doc.rewrite_asset_references(&doc_dir, from, to);
                ambiguous.extend(rewrite.ambiguous);
                if rewrite.rewritten > 0 {
                    document.dirty = true;
                    references += rewrite.rewritten;
                    affected.push(document.title());
                }
                if document.path.as_deref() == Some(from.as_str()) {
                    document.path = Some(to.clone());
                }
            }
            if let Some(path) = document.path.clone() {
                open_paths.push(path);
            }
        }

        let closed: Vec<String> = self
            .project_files
            .iter()
            .filter(|file| matches!(file.kind, DocKind::Scene | DocKind::Prefab))
            .map(|file| file.path.clone())
            .filter(|path| !open_paths.contains(path))
            .collect();
        for path in closed {
            let mut doc = match project::read_document(root, &path) {
                Ok(doc) => doc,
                Err(error) => {
                    log::warn!("rename: cannot read {path}: {error}");
                    continue;
                }
            };
            let doc_dir = document_dir(Some(&path));
            let mut rewritten = 0;
            for (from, to) in moves {
                let rewrite = doc.rewrite_asset_references(&doc_dir, from, to);
                ambiguous.extend(rewrite.ambiguous);
                rewritten += rewrite.rewritten;
            }
            if rewritten == 0 {
                continue;
            }
            let doc = self.pruned_doc(doc);
            match project::write_document(root, &path, &doc) {
                Ok(()) => {
                    references += rewritten;
                    affected.push(path);
                }
                Err(error) => log::warn!("rename: cannot write {path}: {error}"),
            }
        }

        affected.sort();
        affected.dedup();
        // 文件已经被搬走：文档列表与资产树都要重扫（`rename_asset` 以前在调用处做，
        // 现在三个入口共用这份逻辑）。
        self.refresh_project_files();
        self.refresh_assets();
        if !ambiguous.is_empty() {
            ambiguous.sort();
            ambiguous.dedup();
            self.log(
                LogSource::Editor,
                LogLevel::Warn,
                format!(
                    "{} 处引用无法自动改写（不是 `assets/` 或相对路径）：{}",
                    ambiguous.len(),
                    ambiguous.join(", ")
                ),
            );
        }
        (references, affected)
    }

    /// 当前文档所在的目录（`assets/...` 形式），用于解析相对引用。
    pub fn document_dir(&self) -> String {
        document_dir(self.document().path.as_deref())
    }

    /// 当前文档里**解析不到**的资产引用：`(字段, 值)`。
    ///
    /// 「解析不到」= 文件被删 / 改名 / 拼错。引擎侧只会往日志里写一句，
    /// 画面表现是物体消失或贴图变白，所以编辑器要主动标出来（#467）。
    pub fn missing_references(&self, index: usize) -> Vec<(String, String)> {
        let doc = &self.document().doc;
        if doc.entities.get(index).is_none() {
            return Vec::new();
        }
        let dir = self.document_dir();
        let paths = self.asset_paths();
        doc.reference_values()
            .into_iter()
            .filter(|(entity, _, _)| *entity == index)
            .filter_map(|(_, field, value)| {
                crate::assets::resolve_reference(&paths, &dir, &value)
                    .is_none()
                    .then_some((field, value))
            })
            .collect()
    }

    /// 资产表里的全部路径（`resolve_reference` 的查找集合）。
    pub fn asset_paths(&self) -> Vec<String> {
        self.assets.iter().map(|asset| asset.path.clone()).collect()
    }

    pub fn refresh_project_files(&mut self) {
        if let Some(root) = &self.project_root {
            self.project_files = project::scan_documents(root);
        }
    }

    /// 重新扫描 `assets/` 并重建资产树（新建目录 / 复制 / 移动后调用）。
    /// 读一个 JSON 资产（材质 `.mat.json` / `.asset.json`）。
    pub fn read_asset_json(&self, path: &str) -> io::Result<Value> {
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根"))?;
        project::read_json_asset(&root, path)
    }

    /// 写回一个 JSON 资产（原子写 + 回读校验），并刷新资产表。
    pub fn write_asset_json(&mut self, path: &str, value: &Value) -> io::Result<()> {
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根"))?;
        project::write_json_asset(&root, path, value)?;
        self.refresh_assets();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已保存材质 {path}"),
        );
        Ok(())
    }

    pub fn refresh_assets(&mut self) {
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.assets = project::scan_assets(&root);
        let tree = {
            let mut tree = AssetTree::build(&self.assets);
            for folder in project::scan_asset_dirs(&root) {
                tree.insert_dir(&folder);
            }
            tree
        };
        self.expanded_assets
            .retain(|path| tree.find(path).is_some());
        self.tree = tree;
    }

    /// 新建资产目录（相对项目根，必须位于 `assets/` 内）。
    pub fn create_asset_folder(&mut self, relative: &str) -> io::Result<String> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法新建目录")
        })?;
        let relative = relative.trim_end_matches('/');
        if let Some(problem) = project::asset_path_problem(relative) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("目录路径不合法：{problem}"),
            ));
        }
        if !relative.starts_with("assets/") || relative == "assets" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "目录必须位于 assets/ 内",
            ));
        }
        if root.join(relative).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{relative} 已存在"),
            ));
        }
        project::create_folder(&root, relative)?;
        if let Some(parent) = relative.rsplit_once('/').map(|(parent, _)| parent) {
            self.expanded_assets.insert(parent.to_string());
        }
        self.refresh_assets();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已新建目录 {relative}"),
        );
        Ok(relative.to_string())
    }

    /// 重命名 / 移动一个资产目录（#478）。
    ///
    /// 目录里的每个资产路径都变了，所以引用改写按「每个文件一对 from→to」走，
    /// 和 `rename_asset` 共用同一套改写逻辑；目录本身用 `project::rename_path`
    /// 一次 `fs::rename` 搬走。
    pub fn rename_folder(&mut self, from: &str, to: &str) -> io::Result<(usize, Vec<String>)> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法重命名目录")
        })?;
        let from = from.trim_end_matches('/');
        let to = to.trim_end_matches('/');
        if let Some(problem) = project::asset_path_problem(to) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("目录路径不合法：{problem}"),
            ));
        }
        if !from.starts_with("assets/") || !to.starts_with("assets/") || to == "assets" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "目录必须位于 assets/ 内",
            ));
        }
        if !root.join(from).is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from} 不是目录"),
            ));
        }
        if to == from || to.starts_with(&format!("{from}/")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "目标不能是目录自身或它的子目录",
            ));
        }

        // 目录里每个资产的新路径都要参与引用改写。
        let prefix = format!("{from}/");
        let mut moves: Vec<(String, String)> = self
            .assets
            .iter()
            .filter(|asset| asset.path.starts_with(&prefix))
            .map(|asset| {
                (
                    asset.path.clone(),
                    format!("{to}/{}", &asset.path[prefix.len()..]),
                )
            })
            .collect();
        // 目录里的文档也要跟着换路径（它们同样会被搬走）。
        for file in &self.project_files {
            if file.path.starts_with(&prefix) && !moves.iter().any(|(from, _)| from == &file.path) {
                moves.push((
                    file.path.clone(),
                    format!("{to}/{}", &file.path[prefix.len()..]),
                ));
            }
        }

        project::rename_path(&root, from, to)?;

        let (references, affected) = self.rewrite_moved_references(&root, &moves);

        // 展开状态跟着目录走。
        let expansions: Vec<String> = self.expanded_assets.iter().cloned().collect();
        for path in expansions {
            let next = if path == from {
                to.to_string()
            } else if let Some(rest) = path.strip_prefix(&prefix) {
                format!("{to}/{rest}")
            } else {
                continue;
            };
            self.expanded_assets.remove(&path);
            self.expanded_assets.insert(next);
        }

        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!(
                "已重命名目录 {from} → {to}（改写 {references} 处引用，{} 个文档）",
                affected.len()
            ),
        );
        Ok((references, affected))
    }

    /// 删除一个**空**资产目录（#478）。
    ///
    /// 非空时拒绝而不是递归删除：资产删除已经在文件行上逐个确认过，
    /// 目录级递归删除太容易误伤。
    pub fn delete_folder(&mut self, path: &str) -> io::Result<()> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法删除目录")
        })?;
        let path = path.trim_end_matches('/');
        if !path.starts_with("assets/") || path == "assets" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只允许删除 assets/ 内的子目录",
            ));
        }
        let full = root.join(path);
        if !full.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path} 不是目录"),
            ));
        }
        let count = std::fs::read_dir(&full)?.count();
        if count > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("目录非空（{count} 项），请先清空"),
            ));
        }
        std::fs::remove_dir(&full)?;
        self.expanded_assets.remove(path);
        self.refresh_assets();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已删除目录 {path}"),
        );
        Ok(())
    }

    /// 复制资产到同目录下的 `-2` / `-3` 路径。
    pub fn duplicate_asset(&mut self, path: &str) -> io::Result<String> {
        let root = self.project_root.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法复制资产")
        })?;
        let target = crate::assets::unique_asset_path(&self.assets, path);
        project::copy_file(&root, path, &target)?;
        self.refresh_project_files();
        self.refresh_assets();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已复制 {path} → {target}"),
        );
        Ok(target)
    }

    /// 把资产移动到某个目录（拖拽到目录行）。场景 / 预制体请用另存为。
    pub fn move_asset_to_folder(&mut self, from: &str, folder: Option<&str>) -> io::Result<String> {
        let name = from.rsplit('/').next().unwrap_or(from);
        let folder = folder.unwrap_or("assets").trim_end_matches('/');
        let to = format!("{folder}/{name}");
        if to == from {
            return Ok(to);
        }
        if from.ends_with(".scene.json") || from.ends_with(".prefab.json") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "场景 / 预制体请用另存为移动",
            ));
        }
        self.rename_asset(from, &to)?;
        Ok(to)
    }

    /// 资产详情：GUID、按内容类型生成的预览、所有打开 / 磁盘文档里的引用。
    pub fn asset_details(&self, path: &str) -> Option<AssetDetails> {
        let asset = self.assets.iter().find(|asset| asset.path == path)?.clone();
        let root = self.project_root.as_deref();
        let preview = crate::assets::asset_preview(root, &asset, 32 * 1024);

        let mut references = Vec::new();
        let collect = |document: &str, doc: &SceneDoc, out: &mut Vec<AssetReference>| {
            for (entity, field, _) in doc.field_references(&asset.path) {
                out.push(AssetReference {
                    document: document.to_string(),
                    entity: doc
                        .entities
                        .get(entity)
                        .and_then(|entity| entity.id)
                        .unwrap_or(0),
                    entity_name: doc
                        .entities
                        .get(entity)
                        .map(|entity| entity.display_name())
                        .unwrap_or_else(|| "(已删除)".to_string()),
                    field,
                });
            }
        };
        for document in &self.docs {
            let name = document.path.clone().unwrap_or_else(|| document.title());
            collect(&name, &document.doc, &mut references);
        }
        if let Some(root) = root {
            for file in &self.project_files {
                if self
                    .docs
                    .iter()
                    .any(|document| document.path.as_deref() == Some(file.path.as_str()))
                {
                    continue;
                }
                if let Ok(doc) = project::read_document(root, &file.path) {
                    collect(&file.path, &doc, &mut references);
                }
            }
        }
        references.sort_by(|a, b| {
            (&a.document, a.entity, &a.field).cmp(&(&b.document, b.entity, &b.field))
        });
        references.dedup();

        Some(AssetDetails {
            asset,
            guid: crate::assets::asset_guid(path),
            preview,
            references,
        })
    }

    /// 打开（或切到）磁盘上的文档。
    /// 从场景里的预制体实例打开它的源预制体，并记录返回路径（#420）。
    ///
    /// 打开后预制体文档的 `prefab_origin` 就是面包屑的数据来源；
    /// 实例必须真的带 `prism_engine::scene::PrefabInstance` 链接。
    pub fn open_prefab_from_instance(
        &mut self,
        scene_index: usize,
        entity_id: u32,
    ) -> io::Result<()> {
        let Some(scene) = self.docs.get(scene_index) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "文档不存在"));
        };
        if matches!(scene.kind, DocKind::Prefab) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只能从场景实例打开预制体",
            ));
        }
        let Some(link) = scene
            .doc
            .entities
            .iter()
            .find(|entity| entity.id == Some(entity_id))
            .and_then(|entity| entity.prefab_link())
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "这个实体不是预制体实例",
            ));
        };
        let link = link.to_string();
        let scene_title = scene.title();

        self.open_project_file(&link)?;
        let Some(prefab_index) = self
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some(link.as_str()))
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "预制体文档没有打开",
            ));
        };
        self.docs[prefab_index].prefab_origin = Some(PrefabOrigin {
            scene_index,
            entity_id,
            scene_title,
        });
        Ok(())
    }

    /// 从预制体返回来源场景（#420）：切文档、选中实例，必要时把预制体的改动同步回来。
    ///
    /// 只有预制体文档**没有未保存改动**时才自动 `revert_prefab_instance`：
    /// 否则磁盘上还是旧文件，还原会把刚编辑的内容看起来「丢掉」。
    /// 返回 `false` 表示这份文档不是从场景实例打开的（没有面包屑可回）。
    pub fn return_from_prefab(&mut self, prefab_index: usize) -> bool {
        let Some(origin) = self
            .docs
            .get(prefab_index)
            .and_then(|doc| doc.prefab_origin.clone())
        else {
            return false;
        };
        if origin.scene_index >= self.docs.len() {
            return false;
        }
        let clean = self
            .docs
            .get(prefab_index)
            .map(|doc| !doc.dirty)
            .unwrap_or(false);

        self.set_active(origin.scene_index);
        let mut target = origin.entity_id;
        if clean {
            // 失败只记日志：可能预制体文件已被删除 / 实例已不是预制体。
            // 还原会重新实例化，实体 id 会变 —— 用返回的新 id 选中。
            match self.revert_prefab_instance(origin.entity_id) {
                Ok(id) => target = id,
                Err(error) => self.log(
                    LogSource::Editor,
                    LogLevel::Warn,
                    format!("返回场景时同步实例失败: {error}"),
                ),
            }
        }
        if let Some(index) = self.document().doc.index_of_id(target) {
            self.select(index, false);
        }
        true
    }

    pub fn open_project_file(&mut self, relative: &str) -> io::Result<()> {
        if let Some(index) = self
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some(relative))
        {
            self.set_active(index);
            return Ok(());
        }
        let Some(root) = self.project_root.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "没有绑定项目根（纯 fixture 模式）",
            ));
        };
        let doc = project::read_document(&root, relative)?;
        let kind = if relative.ends_with(".prefab.json") {
            DocKind::Prefab
        } else {
            DocKind::Scene
        };
        let mut document = Document::new(kind, Some(relative.to_string()), doc);
        document.note_disk_state(Some(&root));
        self.docs.push(document);
        self.active = self.docs.len() - 1;
        self.clear_selection();
        self.expanded.clear();
        self.hydrate_documents();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已打开 {relative}"),
        );
        Ok(())
    }

    /// 保存当前文档；未命名文档落到 `assets/scenes|prefabs` 下的默认路径。
    pub fn save_active(&mut self) -> io::Result<String> {
        let index = self.active;
        self.save_document(index)
    }

    /// 保存指定文档（可以是未激活标签）；返回写入的相对路径。
    /// 保存前检查「磁盘上的这份文件是不是被别人改过了」（#452）。
    ///
    /// 返回冲突的文档路径。文件被删除也算冲突——盲写会把它变回我们的版本。
    pub fn save_conflict(&self, index: usize) -> Option<String> {
        let root = self.project_root.as_deref()?;
        let document = self.docs.get(index)?;
        let path = document.path.as_deref()?;
        if document.externally_modified {
            return Some(path.to_string());
        }
        let saved = document.saved_stamp?;
        match project::file_stamp(root, path) {
            Some(current) if current == saved => None,
            _ => Some(path.to_string()),
        }
    }

    /// 重新比对每个已落盘文档的版本戳；返回本次新发现「被外部改过」的文档数。
    ///
    /// 窗口回到前台时调用一次即可，不需要文件监听（#452 的建议 3）。
    pub fn check_external_changes(&mut self) -> usize {
        let Some(root) = self.project_root.clone() else {
            return 0;
        };
        let mut changed = 0;
        for document in &mut self.docs {
            if document.externally_modified {
                continue;
            }
            let Some(path) = document.path.clone() else {
                continue;
            };
            let differs = match (document.saved_stamp, project::file_stamp(&root, &path)) {
                (None, None) => false,
                (Some(saved), Some(current)) => saved != current,
                _ => true,
            };
            if differs {
                document.externally_modified = true;
                changed += 1;
            }
        }
        if changed > 0 {
            self.log(
                LogSource::Editor,
                LogLevel::Warn,
                format!("{changed} 个文档在磁盘上被修改过（保存前会提示）"),
            );
        }
        changed
    }

    /// 丢弃内存里的版本，按磁盘内容重载（冲突对话框的「重新载入」）。
    pub fn reload_document_from_disk(&mut self, index: usize) -> io::Result<()> {
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根"))?;
        let path = self
            .docs
            .get(index)
            .and_then(|document| document.path.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文档还没有路径"))?;
        let doc = project::read_document(&root, &path)?;
        {
            let document = self.docs.get_mut(index).expect("checked above");
            document.doc = doc;
            document.undo = crate::undo::UndoStack::new();
            document.dirty = false;
            document.note_disk_state(Some(&root));
        }
        self.hydrate_documents();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已按磁盘内容重新载入 {path}"),
        );
        Ok(())
    }

    /// 保存文档；磁盘上的版本变过就拒绝（由 UI 弹「覆盖 / 重新载入 / 取消」）。
    pub fn save_document(&mut self, index: usize) -> io::Result<String> {
        if let Some(path) = self.save_conflict(index) {
            self.pending_save_conflict = Some(index);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{path} 在磁盘上已被修改"),
            ));
        }
        self.write_document_now(index)
    }

    /// 忽略冲突，直接覆盖磁盘（冲突对话框的「覆盖保存」）。
    pub fn save_document_forced(&mut self, index: usize) -> io::Result<String> {
        self.write_document_now(index)
    }

    /// 写盘前按 schema 去掉默认值与只读字段（#636）。
    fn pruned_doc(&self, mut doc: SceneDoc) -> SceneDoc {
        doc.prune_defaults(&self.schema);
        doc
    }

    fn write_document_now(&mut self, index: usize) -> io::Result<String> {
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法保存"))?;

        let (path, doc) = {
            let document = self
                .docs
                .get(index)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文档不存在"))?;
            let path = match &document.path {
                Some(path) => path.clone(),
                None => project::default_save_path(&root, document.kind, &document.title()),
            };
            (path, document.doc.clone())
        };
        let doc = self.pruned_doc(doc);
        project::write_document(&root, &path, &doc)?;
        {
            let document = self.docs.get_mut(index).expect("checked above");
            document.path = Some(path.clone());
            document.dirty = false;
            document.note_disk_state(Some(&root));
        }
        self.refresh_project_files();
        self.log(LogSource::Editor, LogLevel::Info, format!("已保存 {path}"));
        Ok(path)
    }

    /// 自动保存用：只在文档有未保存修改时才写盘。
    pub fn save_active_if_dirty(&mut self) -> Option<io::Result<String>> {
        if !self.document().dirty {
            return None;
        }
        Some(self.save_active())
    }

    /// 把所有 dirty 文档写进退出快照（#619）。
    ///
    /// 返回写入的文档数；没有绑定项目根 / 没有 dirty 文档时为 0。快照路径固定在
    /// `target/editor-recovery/recovery.json`，不会覆盖原场景或预制体。
    pub fn write_recovery(&self) -> io::Result<usize> {
        let Some(root) = self.project_root.as_deref() else {
            return Ok(0);
        };
        let entries: Vec<crate::recovery::RecoveredDocument> = self
            .docs
            .iter()
            .filter(|document| document.dirty)
            .map(|document| crate::recovery::RecoveredDocument {
                path: document.path.clone(),
                kind: document.kind,
                document: document.doc.clone(),
            })
            .collect();
        crate::recovery::write(root, &entries)
    }

    /// 上次退出留下了几条待恢复文档（没有快照 = 0）。
    pub fn recovery_pending(&self) -> io::Result<usize> {
        let Some(root) = self.project_root.as_deref() else {
            return Ok(0);
        };
        crate::recovery::read(root).map(|entries| entries.len())
    }

    /// 把退出快照读回工作区。
    ///
    /// 同路径 / 同类型的文档**就地替换**（保留 tab 位置），其余追加到末尾；
    /// 恢复出来的文档一律是 dirty，用户确认后再保存。成功后删除快照。
    pub fn recover_unsaved(&mut self) -> io::Result<usize> {
        let Some(root) = self.project_root.clone() else {
            return Ok(0);
        };
        let entries = crate::recovery::read(&root)?;
        let count = entries.len();
        let mut first = None;
        for entry in entries {
            let existing = entry.path.as_deref().and_then(|path| {
                self.docs.iter().position(|document| {
                    document.kind == entry.kind && document.path.as_deref() == Some(path)
                })
            });
            let index = match existing {
                Some(index) => {
                    let document = &mut self.docs[index];
                    document.doc = entry.document;
                    document.undo = crate::undo::UndoStack::new();
                    document.dirty = true;
                    document.externally_modified = false;
                    index
                }
                None => {
                    let mut document = Document::new(entry.kind, entry.path, entry.document);
                    document.dirty = true;
                    self.docs.push(document);
                    self.docs.len() - 1
                }
            };
            first.get_or_insert(index);
        }
        if let Some(index) = first {
            self.set_active(index);
        }
        crate::recovery::clear(&root)?;
        if count > 0 {
            self.log(
                LogSource::Editor,
                LogLevel::Warn,
                format!("已恢复 {count} 个上次退出时未保存的文档（仍未保存）"),
            );
        }
        Ok(count)
    }

    /// 丢弃退出快照。
    pub fn discard_recovery(&mut self) -> io::Result<()> {
        let Some(root) = self.project_root.as_deref() else {
            return Ok(());
        };
        crate::recovery::clear(root)?;
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            "已丢弃退出时的未保存快照",
        );
        Ok(())
    }

    /// 另存为（路径相对项目根；空串/非法输入返回错误）。
    pub fn save_active_as(&mut self, input: &str) -> io::Result<String> {
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根，无法保存"))?;
        let kind = self.document().kind;
        let path = project::normalize_save_path(kind, input);
        if path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "路径不合法（不能为空、不能包含 .. 或绝对路径）",
            ));
        }
        if !path.starts_with("assets/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "只允许写到 assets/ 下",
            ));
        }
        let doc = self.document().doc.clone();
        let doc = self.pruned_doc(doc);
        project::write_document(&root, &path, &doc)?;
        {
            let document = self.document_mut();
            document.path = Some(path.clone());
            document.dirty = false;
        }
        self.refresh_project_files();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已另存为 {path}"),
        );
        Ok(path)
    }

    /// 重新从磁盘读取当前文档（丢弃未保存修改）。
    pub fn reload_active(&mut self) -> io::Result<()> {
        let Some(root) = self.project_root.clone() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根"));
        };
        let path = self
            .document()
            .path
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "文档尚未保存"))?;
        let doc = project::read_document(&root, &path)?;
        {
            let document = self.document_mut();
            document.doc = doc;
            document.undo.clear();
            document.dirty = false;
        }
        self.hydrate_documents();
        // 外部文件可能比当前文档短（git checkout / 脚本重写）。选中集存的是
        // 下标，必须在换完文档后裁掉越界项，否则检查器的 multi-select 路径会 panic。
        self.clamp_selection();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已重新读取 {path}"),
        );
        Ok(())
    }

    pub fn has_project(&self) -> bool {
        self.project_root.is_some()
    }

    pub fn save_suggestion(&self) -> String {
        let document = self.document();
        match &document.path {
            Some(path) => path.clone(),
            None => match &self.project_root {
                Some(root) => project::default_save_path(root, document.kind, &document.title()),
                None => format!("{}/{}", document.kind.folder(), document.title()),
            },
        }
    }

    // ── 场景清单（assets/scenes.json） ───────────────────────────────────

    /// 落盘失败只记日志：清单编辑不应打断编辑器的其它操作。
    fn persist_scene_manifest(&mut self) {
        let Some(root) = self.project_root.clone() else {
            return;
        };
        if let Err(error) = self.scene_manifest.write(&root) {
            self.log(
                LogSource::Editor,
                LogLevel::Error,
                format!("写入 assets/scenes.json 失败: {error}"),
            );
        }
    }

    pub fn reload_scene_manifest(&mut self) -> io::Result<()> {
        let Some(root) = self.project_root.clone() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "没有绑定项目根"));
        };
        self.scene_manifest = SceneManifest::read(&root)?;
        Ok(())
    }

    pub fn set_scene_enabled(&mut self, index: usize, enabled: bool) {
        if self.scene_manifest.set_enabled(index, enabled) {
            self.persist_scene_manifest();
        }
    }

    /// 上移 / 下移一条，返回新下标。
    pub fn move_scene_entry(&mut self, index: usize, delta: i32) -> Option<usize> {
        let moved = self.scene_manifest.move_entry(index, delta);
        if moved.is_some() {
            self.persist_scene_manifest();
        }
        moved
    }

    pub fn remove_scene_entry(&mut self, index: usize) -> bool {
        let removed = self.scene_manifest.remove(index).is_some();
        if removed {
            self.persist_scene_manifest();
        }
        removed
    }

    /// 把磁盘上的场景文档加入清单；已存在 / 无项目根时返回 `None`。
    pub fn add_scene_document(&mut self, doc_relative: &str) -> Option<usize> {
        self.project_root.as_ref()?;
        let added = self.scene_manifest.add_document(doc_relative);
        if added.is_some() {
            self.persist_scene_manifest();
        }
        added
    }

    /// `assets/scenes` 下尚未写进清单的场景文档（相对 `assets/`）。
    pub fn available_scene_documents(&self) -> Vec<String> {
        let mut docs: Vec<String> = self
            .project_files
            .iter()
            .filter(|file| file.kind == DocKind::Scene)
            .map(|file| file.path.trim_start_matches("assets/").to_string())
            .filter(|relative| !self.scene_manifest.contains_document(relative))
            .collect();
        docs.sort();
        docs
    }

    // ── 文档 ─────────────────────────────────────────────────────────────

    pub fn document(&self) -> &Document {
        &self.docs[self.active.min(self.docs.len() - 1)]
    }

    pub fn document_mut(&mut self) -> &mut Document {
        let index = self.active.min(self.docs.len() - 1);
        &mut self.docs[index]
    }

    pub fn active_index(&self) -> usize {
        self.active.min(self.docs.len().saturating_sub(1))
    }

    pub fn set_active(&mut self, index: usize) {
        if index < self.docs.len() {
            self.active = index;
            self.clear_selection();
            self.expanded.clear();
            self.isolate = false;
        }
    }

    pub fn open_path(&mut self, path: &str) {
        match self
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some(path))
        {
            Some(index) => self.set_active(index),
            None => {
                // 模板可以直接“实例化”成一个新文档。
                let template = self
                    .templates
                    .iter()
                    .find(|template| template.id == path)
                    .cloned();
                if let Some(template) = template {
                    let mut template = template;
                    hydrate_doc(&self.schema, &mut template.doc);
                    let document = Document::new(
                        template.kind,
                        Some(format!(
                            "{}/{}{}",
                            template.kind.folder(),
                            template.id,
                            template.kind.suffix()
                        )),
                        template.doc,
                    );
                    self.docs.push(document);
                    self.active = self.docs.len() - 1;
                    self.clear_selection();
                    self.log(
                        LogSource::Editor,
                        LogLevel::Info,
                        format!("已打开模板 {path}"),
                    );
                }
            }
        }
    }

    pub fn new_document(&mut self, kind: DocKind, template_id: Option<&str>) {
        let template = template_id
            .and_then(|id| self.templates.iter().find(|template| template.id == id))
            .filter(|template| template.kind == kind)
            .cloned();
        let mut doc = template
            .as_ref()
            .map(|template| template.doc.clone())
            .unwrap_or_default();
        hydrate_doc(&self.schema, &mut doc);
        // 模板 id 只是**建议标题**，落点要找不冲突的名字：以前直接拼成
        // `assets/scenes/<模板 id>.scene.json`，第二次用同一个模板新建就会在
        // 保存时静默覆盖上一个场景，或者两个标签页抢同一个路径（#449）。
        let path = template.as_ref().map(|template| {
            self.unique_document_path(kind, &template.id)
                // 没有项目根时落不了盘，保留建议名字让标签页有个标题。
                .unwrap_or_else(|| format!("{}/{}{}", kind.folder(), template.id, kind.suffix()))
        });
        self.docs.push(Document::new(kind, path, doc));
        self.active = self.docs.len() - 1;
        self.clear_selection();
        self.expanded.clear();
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("新建{}", kind.folder()),
        );
    }

    /// 新文档的落点：磁盘上没有同名文件，也没有被当前会话里的其它文档占用。
    ///
    /// 没有绑定项目根时返回 `None`（文档先不落盘，保存时会报「没有绑定项目根」）。
    fn unique_document_path(&self, kind: DocKind, title: &str) -> Option<String> {
        let root = self.project_root.as_ref()?;
        let taken: Vec<String> = self
            .docs
            .iter()
            .filter_map(|doc| doc.path.clone())
            .collect();
        Some(project::unique_save_path(root, kind, title, &taken))
    }

    pub fn close_document(&mut self, index: usize) {
        if self.docs.len() <= 1 || index >= self.docs.len() {
            return;
        }
        self.docs.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.docs.len() {
            self.active = self.docs.len() - 1;
        }
        self.clear_selection();
    }

    pub fn mark_saved(&mut self) {
        let title = {
            let document = self.document_mut();
            document.dirty = false;
            document.title()
        };
        self.log(LogSource::Editor, LogLevel::Info, format!("已保存 {title}"));
    }

    /// 结构性编辑：自动快照 → 执行 → 入撤销栈。
    pub fn edit<T>(
        &mut self,
        label: impl Into<String>,
        mutate: impl FnOnce(&mut Document) -> T,
    ) -> T {
        let before = self.document().doc.clone();
        let selection_before = self.selection_ids();
        let label = label.into();
        let result = {
            let document = self.document_mut();
            mutate(document)
        };
        let after = self.document().doc.clone();
        if before != after {
            let selection_after = selection_ids_in(&after, &self.selection);
            let document = self.document_mut();
            document.undo.push_snapshot_with_selection(
                label,
                before,
                Some(crate::undo::SelectionSnapshot {
                    before: selection_before,
                    after: selection_after,
                }),
                &after,
            );
            document.dirty = true;
        }
        result
    }

    /// 当前选择集对应的实体 id（#513）。实体没有 id 时跳过 —— 那种文档本来就不该存在。
    pub fn selection_ids(&self) -> Vec<u32> {
        selection_ids_in(&self.document().doc, &self.selection)
    }

    /// 按实体 id 恢复选择集：找不到的 id 直接丢弃，剩下的按下标排序。
    ///
    /// 返回真正被选中的实体数。
    pub fn select_ids(&mut self, ids: &[u32]) -> usize {
        let document = self.document();
        let mut selection: Vec<usize> = ids
            .iter()
            .filter_map(|id| document.doc.index_of_id(*id))
            .collect();
        selection.sort_unstable();
        selection.dedup();
        let count = selection.len();
        self.selection = selection;
        self.selection_active = self.selection.last().copied();
        count
    }

    /// 文档结构变了但没有选择快照（字段类命令之外的老快照）：把越界下标丢掉。
    fn clamp_selection(&mut self) {
        let len = self.document().doc.entities.len();
        self.selection.retain(|index| *index < len);
        self.selection.sort_unstable();
        self.selection.dedup();
        self.normalize_selection_active();
    }

    // ── 选择 / 层级 ─────────────────────────────────────────────────────

    pub fn select(&mut self, index: usize, extend: bool) {
        if !extend {
            self.clear_selection();
        }
        if let Some(position) = self.selection.iter().position(|item| *item == index) {
            if extend {
                self.selection.remove(position);
            }
        } else {
            self.selection.push(index);
        }
        self.selection.sort_unstable();
        if self.selection.contains(&index) {
            self.selection_active = Some(index);
        } else {
            self.normalize_selection_active();
        }
    }

    /// 右键菜单的目标（Unity 的 `TreeView` 行为，#524）。
    ///
    /// - 右键**未选中**的实体：先把选择换成它一个（菜单动作、检查器、`Parent to
    ///   Selection` 因此指向同一行）；
    /// - 右键**已选中**的实体：保留整个选择集合（多选时把右键项当作菜单目标）；
    /// - 右键空白处由调用方走 [`Workspace::clear_selection_in`]。
    ///
    /// 返回选择是否被改过。
    pub fn select_for_context(&mut self, index: usize) -> bool {
        if self.selection.contains(&index) {
            // 右键已选中的行：选择集不动，但把它设为活动对象（#507）。
            self.selection_active = Some(index);
            return false;
        }
        if index >= self.document().doc.entities.len() {
            return false;
        }
        self.select_with(index, SelectMode::Replace);
        true
    }

    /// 按 [`SelectMode`] 写入单选语义（视口单击、层级单击共用）。
    ///
    /// 与 [`Workspace::select`] 的区别：`Remove` 只删不加，`Add` 只加不删，
    /// 所以 Shift+点已选中项不会把它取消掉（Unity 的行为）。
    pub fn select_with(&mut self, index: usize, mode: SelectMode) {
        match mode {
            SelectMode::Replace => {
                self.clear_selection();
                self.selection.push(index);
                self.selection_active = Some(index);
            }
            SelectMode::Add => {
                if !self.selection.contains(&index) {
                    self.selection.push(index);
                }
                self.selection_active = Some(index);
            }
            SelectMode::Remove => {
                self.selection.retain(|item| *item != index);
                if self.selection_active == Some(index) {
                    self.selection_active = None;
                }
                self.normalize_selection_active();
            }
        }
        self.selection.sort_unstable();
    }

    pub fn select_range(&mut self, anchor: usize, index: usize) {
        let (start, end) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        self.selection = (start..=end).collect();
        self.selection_active = Some(index);
    }

    /// 按**当前可见行顺序**做 Shift 区间选择（层级树 / 列表的 `ordered`）。
    ///
    /// 与 [`Workspace::select_range`] 的区别：这里只选 `ordered` 里的下标，折叠 subtree
    /// 或搜索过滤掉的行不会被夹带进来。锚点或目标不在可见行里时退化成普通单击。
    pub fn select_visible_range(&mut self, ordered: &[usize], anchor: usize, index: usize) {
        let Some(anchor_pos) = ordered.iter().position(|row| *row == anchor) else {
            self.select(index, false);
            return;
        };
        let Some(target_pos) = ordered.iter().position(|row| *row == index) else {
            self.select(index, false);
            return;
        };
        let (start, end) = if anchor_pos <= target_pos {
            (anchor_pos, target_pos)
        } else {
            (target_pos, anchor_pos)
        };
        self.selection = ordered[start..=end].to_vec();
        self.selection.sort_unstable();
        self.selection_active = Some(index);
    }

    /// 框选：`additive = false` 覆盖选择，`true` 并入。
    pub fn select_many(&mut self, indices: &[usize], additive: bool) {
        self.select_many_with(
            indices,
            if additive {
                SelectMode::Add
            } else {
                SelectMode::Replace
            },
        );
    }

    /// 按 [`SelectMode`] 写入一组选中项（框选、反选、批量选中共用）。
    pub fn select_many_with(&mut self, indices: &[usize], mode: SelectMode) {
        match mode {
            SelectMode::Replace => {
                self.clear_selection();
                for index in indices {
                    if !self.selection.contains(index) {
                        self.selection.push(*index);
                    }
                }
                self.selection_active = indices.last().copied();
            }
            SelectMode::Add => {
                for index in indices {
                    if !self.selection.contains(index) {
                        self.selection.push(*index);
                    }
                }
                if let Some(index) = indices.last() {
                    self.selection_active = Some(*index);
                }
            }
            SelectMode::Remove => {
                self.selection.retain(|index| !indices.contains(index));
                if self
                    .selection_active
                    .is_some_and(|index| indices.contains(&index))
                {
                    self.selection_active = None;
                }
                self.normalize_selection_active();
            }
        }
        self.selection.sort_unstable();
    }

    /// 空选择框 / 点空白：只有 `Replace` 才清空，`Add` / `Remove` 保持现状。
    pub fn clear_selection_in(&mut self, mode: SelectMode) {
        if mode == SelectMode::Replace {
            self.selection.clear();
            self.selection_active = None;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.selection_active = None;
    }

    // ── Scene 可见性 / Isolate（#417） ──────────────────────────────────

    /// 隐藏 / 显示一个实体及其整棵子树（Unity `SceneVisibilityManager.Hide/Show`）。
    ///
    /// 只写编辑器会话状态，不改文档、不产生撤销步、不标脏。隐藏父级时子树一起
    /// 记入集合，这样之后单独显示子级不会被父级再次挡住。
    pub fn set_entity_hidden(&mut self, index: usize, hidden: bool) {
        let ids: Vec<u32> = {
            let doc = &self.document().doc;
            std::iter::once(index)
                .chain(doc.descendants(index))
                .filter_map(|item| doc.id_of_index(item))
                .collect()
        };
        let set = &mut self.document_mut().hidden_entities;
        for id in ids {
            if hidden {
                set.insert(id);
            } else {
                set.remove(&id);
            }
        }
    }

    /// 清空隐藏集合并退出 Isolate。
    pub fn show_all_entities(&mut self) {
        self.document_mut().hidden_entities.clear();
        self.isolate = false;
    }

    /// 打开 / 关闭 Isolate；空选择时不允许打开（否则整场会变黑）。
    pub fn set_isolate(&mut self, isolate: bool) {
        if isolate && self.selection.is_empty() {
            return;
        }
        self.isolate = isolate;
    }

    /// 当前文档里实际不渲染的实体 id（显式隐藏 ∪ Isolate 过滤）。
    pub fn hidden_entity_ids(&self) -> Vec<u32> {
        let document = self.document();
        let doc = &document.doc;
        let isolate_visible: Option<HashSet<u32>> = self.isolate.then(|| {
            self.selection
                .iter()
                .flat_map(|root| std::iter::once(*root).chain(doc.descendants(*root)))
                .filter_map(|index| doc.id_of_index(index))
                .collect()
        });
        doc.entities
            .iter()
            .filter_map(|entity| entity.id)
            .filter(|id| {
                document.hidden_entities.contains(id)
                    || isolate_visible
                        .as_ref()
                        .is_some_and(|visible| !visible.contains(id))
            })
            .collect()
    }

    /// 一个实体当前是否被 Scene 视图隐藏（含 Isolate）。
    pub fn is_entity_hidden(&self, index: usize) -> bool {
        let document = self.document();
        let doc = &document.doc;
        let Some(id) = doc.id_of_index(index) else {
            return false;
        };
        if document.hidden_entities.contains(&id) {
            return true;
        }
        if !self.isolate {
            return false;
        }
        !self
            .selection
            .iter()
            .flat_map(|root| std::iter::once(*root).chain(doc.descendants(*root)))
            .filter_map(|item| doc.id_of_index(item))
            .any(|visible| visible == id)
    }

    /// 活动对象：最后点击 / 右键的实体；不在选择集里时退回选择集末尾。
    pub fn primary_selection(&self) -> Option<usize> {
        self.selection_active
            .filter(|index| self.selection.contains(index))
            .or_else(|| self.selection.last().copied())
    }

    /// 选择集被别处直接改写后，保证活动对象仍然属于它。
    fn normalize_selection_active(&mut self) {
        if self
            .selection_active
            .is_some_and(|index| !self.selection.contains(&index))
        {
            self.selection_active = self.selection.last().copied();
        }
    }

    pub fn toggle_expanded(&mut self, index: usize) {
        if let Some(id) = self.document().doc.id_of_index(index) {
            if !self.expanded.insert(id) {
                self.expanded.remove(&id);
            }
        }
    }

    pub fn expand_all(&mut self) {
        let ids: Vec<u32> = self
            .document()
            .doc
            .entities
            .iter()
            .filter_map(|entity| entity.id)
            .collect();
        self.expanded.extend(ids);
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    /// Alt+点击箭头：连同全部后代一起展开 / 收起。
    pub fn set_subtree_expanded(&mut self, index: usize, expanded: bool) {
        let doc = &self.document().doc;
        let mut indices = vec![index];
        indices.extend(doc.descendants(index));
        let ids: Vec<u32> = indices
            .iter()
            .filter_map(|index| doc.id_of_index(*index))
            .collect();
        for id in ids {
            if expanded {
                self.expanded.insert(id);
            } else {
                self.expanded.remove(&id);
            }
        }
    }

    pub fn add_entity(&mut self, preset_id: &str) {
        let preset = self
            .presets
            .iter()
            .find(|preset| preset.id == preset_id)
            .cloned();
        let schema = self.schema.clone();
        let parent = self.primary_selection();
        self.edit("新建实体", |document| {
            let entity = build_entity(&schema, preset.as_ref(), parent, &document.doc);
            document.doc.add_entity(entity);
        });
    }

    pub fn delete_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        let indices = self.selection.clone();
        self.edit("删除实体", |document| {
            document.doc.remove_entities(&indices);
        });
        self.clear_selection();
    }

    /// 顶层选中实体：去掉“父级也在选中集合里”的项，避免复制整棵子树两次。
    pub fn selection_roots(&self) -> Vec<usize> {
        Self::roots_of(&self.document().doc, &self.selection)
    }

    fn roots_of(doc: &SceneDoc, selection: &[usize]) -> Vec<usize> {
        let ids: HashSet<u32> = selection
            .iter()
            .filter_map(|index| doc.entities.get(*index).and_then(|entity| entity.id))
            .collect();
        let mut roots = Vec::new();
        for index in selection {
            let Some(entity) = doc.entities.get(*index) else {
                continue;
            };
            let mut parent = entity.parent;
            let mut nested = false;
            let mut depth = 0;
            while let Some(parent_id) = parent {
                if ids.contains(&parent_id) {
                    nested = true;
                    break;
                }
                depth += 1;
                if depth > 64 {
                    break;
                }
                parent = doc
                    .index_of_id(parent_id)
                    .and_then(|parent_index| doc.entities.get(parent_index))
                    .and_then(|parent| parent.parent);
            }
            if !nested {
                roots.push(*index);
            }
        }
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    /// 复制选中实体到内部剪贴板（不写系统剪贴板）。返回实体数。
    pub fn copy_selection(&mut self) -> usize {
        let roots = self.selection_roots();
        if roots.is_empty() {
            return 0;
        }
        let trees: Vec<SceneDoc> = roots
            .iter()
            .map(|index| {
                self.document()
                    .doc
                    .extract_subtree_preserving_links(&[*index])
            })
            .collect();
        let count: usize = trees.iter().map(|tree| tree.entities.len()).sum();
        self.entity_clipboard = Some(EntityClipboard { trees, cut: false });
        self.log(
            LogSource::Editor,
            LogLevel::Debug,
            format!("已复制 {count} 个实体"),
        );
        count
    }

    /// 剪切 = 复制 + 删除（一次删除撤销步）。
    pub fn cut_selection(&mut self) -> usize {
        let count = self.copy_selection();
        if count > 0 {
            if let Some(clipboard) = &mut self.entity_clipboard {
                clipboard.cut = true;
            }
            self.delete_selection();
        }
        count
    }

    /// 粘贴实体剪贴板：`parent = None` 落到根级，否则挂到该实体下。
    /// 返回粘贴出来的根实体下标（并设为当前选择）。
    pub fn paste_entities(&mut self, parent: Option<usize>) -> Vec<usize> {
        let Some(clipboard) = self.entity_clipboard.clone() else {
            return Vec::new();
        };
        if clipboard.trees.is_empty() {
            return Vec::new();
        }
        let parent_valid = parent.filter(|index| *index < self.document().doc.entities.len());
        let roots = self.edit("粘贴实体", |document| {
            let mut roots = Vec::new();
            for tree in &clipboard.trees {
                let inserted = document.doc.instantiate_prefab(tree, parent_valid, "");
                if let Some(root) = inserted.first() {
                    roots.push(*root);
                }
            }
            roots
        });
        if roots.is_empty() {
            return roots;
        }
        self.selection = roots.clone();
        self.selection_active = roots.last().copied();
        for index in &roots {
            if let Some(id) = self.document().doc.id_of_index(*index) {
                self.expanded.insert(id);
            }
        }
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已粘贴 {} 个实体", roots.len()),
        );
        roots
    }

    pub fn duplicate_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        let roots = self.selection_roots();
        let mut inserted = self.edit("复制实体", |document| {
            let mut inserted = Vec::new();
            // 从后往前复制，前面的下标不会被新追加的实体挤动。
            for index in roots.iter().rev() {
                if let Some(new_index) = document.doc.duplicate_subtree(*index) {
                    inserted.push(new_index);
                }
            }
            inserted
        });
        if inserted.is_empty() {
            return;
        }
        inserted.sort_unstable();
        self.selection = inserted.clone();
        self.selection_active = inserted.last().copied();
        for index in &inserted {
            if let Some(id) = self.document().doc.id_of_index(*index) {
                self.expanded.insert(id);
            }
        }
    }

    pub fn reparent(&mut self, index: usize, new_parent: Option<usize>, position: Option<usize>) {
        let result = self.edit("调整层级", |document| {
            document.doc.reparent(index, new_parent, position).is_ok()
        });
        if !result {
            self.log(
                LogSource::Editor,
                LogLevel::Warn,
                "层级调整被拒绝（自父 / 成环）",
            );
        }
    }

    /// 多选拖拽：把选中集合的顶层实体一起挂到 `new_parent` 下（一次快照撤销）。
    pub fn reparent_selection(&mut self, new_parent: Option<usize>) -> usize {
        let roots = self.selection_roots();
        if roots.is_empty() {
            return 0;
        }
        let result = self.edit("调整层级", |document| {
            document.doc.reparent_many(&roots, new_parent)
        });
        match result {
            Ok(moved) => moved,
            Err(reason) => {
                self.log(LogSource::Editor, LogLevel::Warn, reason);
                0
            }
        }
    }

    /// 多选拖拽到某行的上/下边缘：整组（选择集合的顶层）插到目标前后（#525）。
    ///
    /// 组内保持相对顺序，一次拖拽 = 一个撤销步。
    pub fn reparent_selection_at(&mut self, target: usize, before: bool) -> usize {
        let roots = self.selection_roots();
        if roots.is_empty() {
            return 0;
        }
        let result = self.edit("调整层级", |document| {
            document.doc.reparent_many_at(&roots, target, before)
        });
        match result {
            Ok(moved) => moved,
            Err(reason) => {
                self.log(LogSource::Editor, LogLevel::Warn, reason);
                0
            }
        }
    }

    pub fn set_active_entity(&mut self, index: usize, active: bool) {
        self.edit(
            if active {
                "启用实体"
            } else {
                "禁用实体"
            },
            |document| {
                document.doc.set_active(index, active);
            },
        );
    }

    /// 预制体实例的还原 / 同步（`PrefabUtility.RevertPrefabInstance`）。
    ///
    /// 用预制体当前内容覆盖实例子树：保留实例根的名字与父级，重新生成根上的链接组件。
    /// 数据来源优先磁盘（项目根 + 链接路径），纯 fixture 模式下回退到同名模板。
    /// 返回**新的**实例根实体 id（旧的实体被移除后重新实例化，id 会变）。
    pub fn revert_prefab_instance(&mut self, entity_id: u32) -> io::Result<u32> {
        let prefab = self.prefab_document_for(entity_id)?;
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "实例不存在"));
        };
        let (name, parent_id, link) = {
            let entity = &self.document().doc.entities[index];
            (
                entity.display_name(),
                entity.parent,
                entity.prefab_link().unwrap_or_default().to_string(),
            )
        };

        let root = self.edit("还原预制体实例", |document| {
            document.doc.remove_entities(&[index]);
            let parent = parent_id.and_then(|id| document.doc.index_of_id(id));
            let inserted = document.doc.instantiate_prefab(&prefab, parent, &link);
            let root = inserted.first().copied();
            if let Some(root) = root {
                if let Some(entity) = document.doc.entities.get_mut(root) {
                    entity.name = Some(name.clone());
                }
            }
            root
        });
        let Some(root) = root else {
            return Err(io::Error::other("预制体没有生成任何实体"));
        };
        let root_id = self
            .document()
            .doc
            .entities
            .get(root)
            .and_then(|entity| entity.id)
            .ok_or_else(|| io::Error::other("新实例没有实体 id"))?;
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已还原预制体实例 {name}"),
        );
        Ok(root_id)
    }

    /// 同步所有同源实例（按实体 id 从后往前处理，避免下标漂移）。
    pub fn sync_prefab_instances(&mut self, prefab_path: &str) -> io::Result<usize> {
        let ids: Vec<u32> = self
            .document()
            .doc
            .entities
            .iter()
            .filter(|entity| entity.prefab_link() == Some(prefab_path))
            .filter_map(|entity| entity.id)
            .collect();
        let mut synced = 0;
        for id in ids.into_iter().rev() {
            if self.revert_prefab_instance(id).is_ok() {
                synced += 1;
            }
        }
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("同步 {synced} 个 {prefab_path} 实例"),
        );
        Ok(synced)
    }

    /// 实例对应的预制体文档：磁盘优先，fixture 模式回退到模板（按文件名）。
    pub fn prefab_document_for(&self, entity_id: u32) -> io::Result<SceneDoc> {
        let path = self
            .document()
            .doc
            .entities
            .iter()
            .find(|entity| entity.id == Some(entity_id))
            .and_then(|entity| entity.prefab_link())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不是预制体实例"))?
            .to_string();

        if let Some(root) = &self.project_root {
            return project::read_document(root, &path);
        }

        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or(&path)
            .trim_end_matches(".prefab.json");
        self.templates
            .iter()
            .find(|template| template.kind == DocKind::Prefab && template.id == stem)
            .map(|template| template.doc.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("找不到预制体 {path}（纯 fixture 模式下需要同名模板）"),
                )
            })
    }

    /// 解除预制体实例链接（移除编辑器元数据组件，场景内容保持不变）。
    pub fn unlink_prefab(&mut self, index: usize) -> bool {
        let linked = self
            .document()
            .doc
            .entities
            .get(index)
            .map(|entity| entity.has_component(PREFAB_LINK_COMPONENT))
            .unwrap_or(false);
        if !linked {
            return false;
        }
        self.edit("解除预制体链接", |document| {
            document.doc.remove_component(index, PREFAB_LINK_COMPONENT);
        });
        true
    }

    /// 内联重命名的实时预览：只改文档，不产生撤销步（避免每敲一个字符一步）。
    pub fn preview_rename(&mut self, index: usize, name: &str) {
        if let Some(entity) = self.document_mut().doc.entities.get_mut(index) {
            if entity.name.as_deref() != Some(name) {
                entity.name = Some(name.to_string());
                self.document_mut().dirty = true;
            }
        }
    }

    /// 重命名结束：用开始编辑前的快照补一个撤销步。
    ///
    /// `was_dirty` 是开始编辑时文档的脏标记：如果这次编辑最终没改变内容
    /// （输入后又删回原样），文档要回到原来的状态，而不是因为中途预览过
    /// 一次就永远带着 `*`（#473）。
    pub fn finish_rename(&mut self, base: SceneDoc, was_dirty: bool) {
        let after = self.document().doc.clone();
        if base == after {
            self.document_mut().dirty = was_dirty;
            return;
        }
        let document = self.document_mut();
        document.undo.push_snapshot("重命名实体", base, &after);
        document.dirty = true;
    }

    /// 放弃就地重命名：把文档还原成开始编辑前的快照（含脏标记），不记撤销步。
    ///
    /// `preview_rename` 会实时改动文档，按 `Esc` 时要把这些预览还原掉（#473）。
    pub fn cancel_rename(&mut self, base: SceneDoc, was_dirty: bool) {
        let document = self.document_mut();
        document.doc = base;
        document.dirty = was_dirty;
    }

    pub fn rename_entity(&mut self, index: usize, name: &str) {
        self.edit("重命名实体", |document| {
            document.doc.rename(index, name);
        });
    }

    // ── 组件 ────────────────────────────────────────────────────────────

    pub fn component_types(&self) -> Vec<&TypeSchema> {
        let indices: Vec<usize> = self.primary_selection().into_iter().collect();
        self.component_types_for(&indices)
    }

    /// 这些实体**还没全部挂上**的组件类型（Add Component 菜单的候选）。
    ///
    /// 多选时以「至少一个选中项缺失」为条件：给一排灯补 `Light` 时，
    /// 已经挂上的那几盏不应该让整个选项消失。
    pub fn component_types_for(&self, indices: &[usize]) -> Vec<&TypeSchema> {
        let entities = &self.document().doc.entities;
        self.schema
            .components()
            .into_iter()
            .filter(|component| {
                indices.iter().any(|index| {
                    entities
                        .get(*index)
                        .map(|entity| !entity.has_component(&component.name))
                        .unwrap_or(false)
                })
            })
            .collect()
    }

    pub fn add_component(&mut self, component_name: &str) {
        let indices: Vec<usize> = self.primary_selection().into_iter().collect();
        self.add_component_multi(&indices, component_name);
    }

    /// 给一组实体加同一个组件（多选检查器、批量操作），一个撤销步。
    ///
    /// 已经有该组件的实体跳过（与单选时的行为一致：不把参数静默重置成默认值），
    /// 返回真正加上组件的实体数。
    pub fn add_component_multi(&mut self, indices: &[usize], component_name: &str) -> usize {
        let Some(component) = self.schema.component(component_name) else {
            return 0;
        };
        let mut targets: Vec<usize> = indices.to_vec();
        targets.sort_unstable();
        targets.dedup();
        let entities = &self.document().doc.entities;
        let missing: Vec<usize> = targets
            .into_iter()
            .filter(|index| {
                entities
                    .get(*index)
                    .map(|entity| !entity.has_component(component_name))
                    .unwrap_or(false)
            })
            .collect();
        if missing.is_empty() {
            if !indices.is_empty() {
                self.log(
                    LogSource::Editor,
                    LogLevel::Warn,
                    format!("{} 已有该组件，未做修改", component.display_name()),
                );
            }
            return 0;
        }
        let name = component.name.clone();
        let value = component_value_with_overrides(&component.default_value(), None);
        let added = self.edit("添加组件", |document| {
            for index in &missing {
                let entity = &mut document.doc.entities[*index];
                entity.components.insert(name.clone(), value.clone());
            }
            missing.len()
        });
        added
    }

    pub fn remove_component(&mut self, component_name: &str) {
        let Some(index) = self.primary_selection() else {
            return;
        };
        if component_name == "Transform" {
            return;
        }
        self.edit("移除组件", |document| {
            document.doc.remove_component(index, component_name);
        });
    }

    /// 在检查器里上下移动一个组件（#512）。
    ///
    /// 顺序是文档的一部分（会写进 `.scene.json` 的对象键序），所以这是**一次
    /// 撤销步**；移动到头 / 到尾或组件不存在时什么都不做，也不污染撤销栈。
    /// 返回是否真的移动过。
    pub fn move_component_at(&mut self, index: usize, component_name: &str, delta: isize) -> bool {
        if component_name == "Transform" || delta == 0 {
            return false;
        }
        self.edit("调整组件顺序", |document| {
            let Some(entity) = document.doc.entities.get_mut(index) else {
                return false;
            };
            entity.components.move_by(component_name, delta)
        })
    }

    /// [`Self::move_component_at`] 的主选中项包装。
    pub fn move_component(&mut self, component_name: &str, delta: isize) -> bool {
        let Some(index) = self.primary_selection() else {
            return false;
        };
        self.move_component_at(index, component_name, delta)
    }

    /// 一个实体当前的组件顺序（检查器的显示顺序）。
    pub fn component_order(&self, index: usize) -> Vec<String> {
        self.document()
            .doc
            .entities
            .get(index)
            .map(|entity| entity.components.order())
            .unwrap_or_default()
    }

    pub fn copy_component(&mut self, component_name: &str) {
        let Some(index) = self.primary_selection() else {
            return;
        };
        self.copy_component_at(index, component_name);
    }

    pub fn copy_component_at(&mut self, index: usize, component_name: &str) {
        let Some(value) = self.document().doc.entities[index].component(component_name) else {
            return;
        };
        self.clipboard = Some(ComponentClipboard {
            component: component_name.to_string(),
            value: value.clone(),
        });
        self.log(
            LogSource::Editor,
            LogLevel::Debug,
            format!("已复制组件 {component_name}"),
        );
    }

    pub fn paste_component(&mut self) {
        let Some(clipboard) = self.clipboard.clone() else {
            return;
        };
        let Some(index) = self.primary_selection() else {
            return;
        };
        self.edit("粘贴组件", |document| {
            document.doc.entities[index]
                .components
                .insert(clipboard.component.clone(), clipboard.value.clone());
        });
    }

    /// 把剪贴板里的组件值覆盖到这些实体（仅当类型一致且实体本来就有该组件）。
    ///
    /// 与 [`Self::paste_component`] 的区别：后者按主选中项**插入**组件，这里只覆盖
    /// 已有组件的值，并且一次撤销步覆盖整个选中集合（#418 的组件 `…` 菜单）。
    /// 返回实际应用到的实体数。
    pub fn paste_component_values(&mut self, indices: &[usize], component_name: &str) -> usize {
        let Some(clipboard) = self.clipboard.clone() else {
            return 0;
        };
        if clipboard.component != component_name {
            return 0;
        }
        let indices = indices.to_vec();
        let component_name = component_name.to_string();
        self.edit("粘贴组件值", |document| {
            let mut applied = 0;
            for index in indices {
                let Some(component) = document
                    .doc
                    .entities
                    .get_mut(index)
                    .and_then(|entity| entity.components.get_mut(&component_name))
                else {
                    continue;
                };
                *component = clipboard.value.clone();
                applied += 1;
            }
            applied
        })
    }

    /// 多选时都能添加的组件（交集）：检查器「多对象编辑」只显示公共组件。
    pub fn common_components(&self, indices: &[usize]) -> Vec<TypeSchema> {
        let doc = &self.document().doc;
        let first = indices
            .iter()
            .filter_map(|index| doc.entities.get(*index))
            .next();
        let Some(first) = first else {
            return Vec::new();
        };
        let mut common: Vec<TypeSchema> = first
            .components
            .keys()
            .filter_map(|name| self.schema.component(name).cloned())
            .collect();
        for index in indices.iter().skip(1) {
            let Some(entity) = doc.entities.get(*index) else {
                continue;
            };
            common.retain(|component| entity.has_component(&component.name));
        }
        common
    }

    /// 多选字段写入：一次快照、一个撤销步，应用到每个存在的实体。
    /// 返回实际写入的实体数。
    pub fn set_field_multi(
        &mut self,
        entity_ids: &[u32],
        component: &str,
        path: &str,
        value: Value,
    ) -> usize {
        if entity_ids.is_empty() {
            return 0;
        }
        if entity_ids.len() == 1 {
            self.set_field(entity_ids[0], component, path, value);
            return 1;
        }
        let component_name = component.to_string();
        let path = path.to_string();
        self.edit("批量修改字段", |document| {
            let mut applied = 0;
            for id in entity_ids {
                let Some(index) = document.doc.index_of_id(*id) else {
                    continue;
                };
                let Some(root) = document.doc.entities[index]
                    .components
                    .get_mut(&component_name)
                else {
                    continue;
                };
                if crate::schema::set_at_path(root, &path, value.clone()) {
                    applied += 1;
                }
            }
            applied
        })
    }

    /// 从多选实体上移除同一个组件（不存在的跳过）。
    pub fn remove_component_multi(&mut self, indices: &[usize], component: &str) {
        let indices = indices.to_vec();
        let component_name = component.to_string();
        self.edit("批量移除组件", |document| {
            for index in &indices {
                document.doc.remove_component(*index, &component_name);
            }
        });
    }

    /// 检查器字段写入：反射写 → 撤销步（同字段合并）。
    pub fn set_field(&mut self, entity_id: u32, component: &str, path: &str, value: Value) {
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let Some(root) = self.document().doc.entities[index]
            .components
            .get(component)
        else {
            return;
        };
        let Some(before) = crate::schema::get_at(root, path) else {
            return;
        };
        if before == value {
            return;
        }
        let now_ms = now_ms();
        let document = self.document_mut();
        document.undo.push_field(
            &mut document.doc,
            entity_id,
            component,
            path,
            before,
            value,
            now_ms,
        );
        document.dirty = true;
    }

    /// 某个组件的预设名（按保存顺序）。
    pub fn component_preset_names(&self, component: &str) -> Vec<String> {
        self.component_presets
            .iter()
            .filter(|preset| preset.component == component)
            .map(|preset| preset.name.clone())
            .collect()
    }

    /// 取一个组件预设的值。
    pub fn component_preset_value(&self, component: &str, name: &str) -> Option<Value> {
        self.component_presets
            .iter()
            .find(|preset| preset.component == component && preset.name == name)
            .map(|preset| preset.value.clone())
    }

    /// 保存 / 覆盖一个组件预设（同名后写覆盖前写）。
    pub fn save_component_preset(&mut self, component: &str, name: &str, value: Value) {
        let name = name.trim();
        if component.is_empty() || name.is_empty() {
            return;
        }
        if let Some(preset) = self
            .component_presets
            .iter_mut()
            .find(|preset| preset.component == component && preset.name == name)
        {
            preset.value = value;
            return;
        }
        self.component_presets.push(ComponentPreset {
            component: component.to_string(),
            name: name.to_string(),
            value,
        });
    }

    /// 绕 gizmo 轴旋转实体（旋转 gizmo）；按实体合并撤销。
    pub fn rotate_entity(&mut self, entity_id: u32, axis: usize, delta_radians: f32) {
        if !delta_radians.is_finite() || delta_radians.abs() < 1e-6 {
            return;
        }
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let before = self.document().doc.entities[index].transform;
        // 世界空间的旋转先换算到父级空间；否则旋转父级下的子实体会被父级
        // 旋转再次偏转。局部空间仍然直接绕实体自身轴旋转。
        let after_rotation = match self.gizmo_space {
            GizmoSpace::World => {
                let delta_world = quat_axis_angle(axis, delta_radians);
                let parent_rotation = parent_world_rotation(&self.document().doc, index);
                let parent_delta = quat_mul(
                    quat_mul(quat_conjugate(parent_rotation), delta_world),
                    parent_rotation,
                );
                quat_mul(parent_delta, before.rotation)
            }
            GizmoSpace::Local => quat_mul(before.rotation, quat_axis_angle(axis, delta_radians)),
        };
        let after = TransformJson {
            rotation: quat_normalize(after_rotation),
            ..before
        };
        let now = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now);
        document.dirty = true;
    }

    /// 多选 gizmo 旋转：每个实体绕自身位置旋转（局部模式各用自身轴），
    /// 整次拖动只产生一个撤销步。
    pub fn rotate_entities(&mut self, entity_ids: &[u32], axis: usize, delta_radians: f32) {
        if entity_ids.is_empty() || !delta_radians.is_finite() || delta_radians.abs() < 1e-6 {
            return;
        }
        let mut ids: Vec<u32> = entity_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let ids = Self::prune_selected_descendants(&self.document().doc, &ids);
        let space = self.gizmo_space;
        let doc = &self.document().doc;
        let center = if self.pivot_mode == PivotMode::Center
            && space == GizmoSpace::World
            && ids.len() > 1
        {
            let mut center = [0.0; 3];
            let mut count = 0.0;
            for entity_id in &ids {
                if let Some(index) = doc.index_of_id(*entity_id) {
                    let position = world_position(doc, index);
                    for axis in 0..3 {
                        center[axis] += position[axis];
                    }
                    count += 1.0;
                }
            }
            (count > 0.0).then(|| center.map(|value| value / count))
        } else {
            None
        };
        let mut changes = Vec::new();
        for entity_id in &ids {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let delta_world = quat_axis_angle(axis, delta_radians);
            let after_rotation = match space {
                GizmoSpace::World => {
                    let parent_rotation = parent_world_rotation(doc, index);
                    let parent_delta = quat_mul(
                        quat_mul(quat_conjugate(parent_rotation), delta_world),
                        parent_rotation,
                    );
                    quat_mul(parent_delta, before.rotation)
                }
                GizmoSpace::Local => {
                    quat_mul(before.rotation, quat_axis_angle(axis, delta_radians))
                }
            };
            let after_position = center.map_or(before.position, |center| {
                let world = world_position(doc, index);
                let offset = [
                    world[0] - center[0],
                    world[1] - center[1],
                    world[2] - center[2],
                ];
                let rotated = quat_rotate(delta_world, offset);
                let target_world = [
                    center[0] + rotated[0],
                    center[1] + rotated[1],
                    center[2] + rotated[2],
                ];
                world_position_to_local(doc, index, target_world)
            });
            changes.push((
                *entity_id,
                before,
                TransformJson {
                    position: after_position,
                    rotation: quat_normalize(after_rotation),
                    ..before
                },
            ));
        }
        if changes.is_empty() {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("rotate-group/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 选中实体的 gizmo 基准（世界空间三轴）。世界模式恒为单位基。
    pub fn gizmo_basis(&self) -> [[f32; 3]; 3] {
        match self.gizmo_space {
            GizmoSpace::World => crate::viewport_math::IDENTITY_BASIS,
            GizmoSpace::Local => self
                .primary_selection()
                .map(|index| basis_from_quat(world_rotation(&self.document().doc, index)))
                .unwrap_or(crate::viewport_math::IDENTITY_BASIS),
        }
    }

    /// 将 gizmo 的轴索引映射到实体实际的局部缩放分量。
    ///
    /// 缩放数据只能表达沿实体局部轴的缩放，无法在没有 shear 的 Transform 中
    /// 表达任意世界轴缩放。因此 World 模式选择与世界轴最对齐的局部轴，避免
    /// 旋转实体后仍然错误地固定修改 `scale[axis]`。
    pub fn gizmo_scale_axis(&self, entity_id: u32, gizmo_axis: usize) -> usize {
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return gizmo_axis.min(2);
        };
        if self.gizmo_space == GizmoSpace::Local {
            return gizmo_axis.min(2);
        }
        let basis = basis_from_quat(world_rotation(&self.document().doc, index));
        (0..3)
            .max_by(|&left, &right| {
                basis[left][gizmo_axis.min(2)]
                    .abs()
                    .total_cmp(&basis[right][gizmo_axis.min(2)].abs())
            })
            .unwrap_or(gizmo_axis.min(2))
    }

    pub fn toggle_gizmo_space(&mut self) {
        self.gizmo_space = self.gizmo_space.toggled();
        self.log(
            LogSource::Editor,
            LogLevel::Debug,
            format!("gizmo 坐标空间：{}", self.gizmo_space.label()),
        );
    }

    pub fn toggle_grid(&mut self) {
        self.show_grid = !self.show_grid;
    }

    /// Scene View 天空盒开关（引擎的 Scene view 默认画天空盒）。
    pub fn toggle_skybox(&mut self) {
        self.show_skybox = !self.show_skybox;
    }

    /// 把世界空间位移换算到父级空间后加到实体 `position` 上（gizmo 拖动用）。
    pub fn translate_entity_world(&mut self, entity_id: u32, delta_world: [f32; 3]) {
        if delta_world.iter().all(|value| value.abs() < 1e-6) {
            return;
        }
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let delta_local = world_delta_to_parent_local(&self.document().doc, index, delta_world);
        let before = self.document().doc.entities[index].transform;
        let mut after = before;
        for (position, delta) in after.position.iter_mut().zip(delta_local) {
            *position += delta;
        }
        let now = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now);
        document.dirty = true;
    }

    /// 去掉「祖先也在本次操作集合里」的后代。
    ///
    /// 父级和子级同时被选中时，父级的世界变换会经层级传播带到子级；如果子级自己
    /// 再吃一份同样的 delta，它的世界位移就是两倍（#404）。这里只保留「集合里没有
    /// 祖先」的实体，其余交给层级传播。
    fn prune_selected_descendants(doc: &SceneDoc, ids: &[u32]) -> Vec<u32> {
        let selected: HashSet<u32> = ids.iter().copied().collect();
        ids.iter()
            .copied()
            .filter(|id| {
                let Some(index) = doc.index_of_id(*id) else {
                    return true;
                };
                let mut parent = doc.entities[index].parent;
                while let Some(parent_id) = parent {
                    if selected.contains(&parent_id) {
                        return false;
                    }
                    parent = doc
                        .index_of_id(parent_id)
                        .and_then(|parent_index| doc.entities[parent_index].parent);
                }
                true
            })
            .collect()
    }

    /// 多选 gizmo 平移：每个实体各自换算父空间位移，但整次拖动只产生一个撤销步。
    pub fn translate_entities_world(&mut self, entity_ids: &[u32], delta_world: [f32; 3]) {
        if entity_ids.is_empty() || delta_world.iter().all(|value| value.abs() < 1e-6) {
            return;
        }
        let mut ids: Vec<u32> = entity_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let ids = Self::prune_selected_descendants(&self.document().doc, &ids);
        let doc = &self.document().doc;
        let mut changes = Vec::new();
        for entity_id in &ids {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let delta_local = world_delta_to_parent_local(doc, index, delta_world);
            let before = doc.entities[index].transform;
            let mut after = before;
            for (position, delta) in after.position.iter_mut().zip(delta_local) {
                *position += delta;
            }
            changes.push((*entity_id, before, after));
        }
        if changes.is_empty() {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("transform-group/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 缩放 gizmo：只改一个轴（下限 0.01，避免退化矩阵）。
    pub fn set_scale_axis(&mut self, entity_id: u32, axis: usize, value: f32) {
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let before = self.document().doc.entities[index].transform;
        let mut after = before;
        after.scale[axis] = value.clamp(0.01, 1000.0);
        if before == after {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now);
        document.dirty = true;
    }

    /// 多选 gizmo 缩放：`targets` 是 (实体 id, 拖动开始时的轴缩放值)，`ratio` 为整体倍数。
    /// 每个实体保留自己的起始比例，整次拖动只产生一个撤销步。
    pub fn scale_entities_axis(&mut self, targets: &[(u32, f32)], axis: usize, ratio: f32) {
        if targets.is_empty() || !ratio.is_finite() || ratio <= 0.0 {
            return;
        }
        let mut sorted: Vec<(u32, f32)> = targets.to_vec();
        sorted.sort_by_key(|(entity_id, _)| *entity_id);
        sorted.dedup_by_key(|(entity_id, _)| *entity_id);
        let keep: Vec<u32> = sorted.iter().map(|(entity_id, _)| *entity_id).collect();
        let keep = Self::prune_selected_descendants(&self.document().doc, &keep);
        sorted.retain(|(entity_id, _)| keep.contains(entity_id));
        let doc = &self.document().doc;
        let mut changes = Vec::new();
        for (entity_id, start_value) in &sorted {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let mut after = before;
            after.scale[axis] = (*start_value * ratio).clamp(0.01, 1000.0);
            if before != after {
                changes.push((*entity_id, before, after));
            }
        }
        if changes.is_empty() {
            return;
        }
        let ids: Vec<u32> = sorted.iter().map(|(entity_id, _)| *entity_id).collect();
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("scale-group/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 多选 gizmo 缩放（#416）：`targets` = `(实体 id, 本地缩放轴, 起始缩放)`。
    ///
    /// - `Center` 模式 + 世界空间：位置也按同一比例沿 gizmo 轴相对选择中心缩放
    ///   （Unity 的群组缩放）；
    /// - `Pivot` 模式（或局部空间）：只在各自轴心上改缩放，位置不动。
    ///
    /// 整次拖动一个撤销步。
    pub fn scale_entities_gizmo(
        &mut self,
        targets: &[(u32, usize, f32)],
        gizmo_axis: usize,
        ratio: f32,
    ) {
        if targets.is_empty() || !ratio.is_finite() || ratio <= 0.0 {
            return;
        }
        let mut sorted: Vec<(u32, usize, f32)> = targets.to_vec();
        sorted.sort_by_key(|(entity_id, _, _)| *entity_id);
        sorted.dedup_by_key(|(entity_id, _, _)| *entity_id);
        let keep: Vec<u32> = sorted.iter().map(|(entity_id, _, _)| *entity_id).collect();
        let keep = Self::prune_selected_descendants(&self.document().doc, &keep);
        sorted.retain(|(entity_id, _, _)| keep.contains(entity_id));
        let doc = &self.document().doc;
        let center = if self.pivot_mode == PivotMode::Center
            && self.gizmo_space == GizmoSpace::World
            && sorted.len() > 1
        {
            let mut center = [0.0; 3];
            let mut count = 0.0;
            for (entity_id, _, _) in &sorted {
                if let Some(index) = doc.index_of_id(*entity_id) {
                    let position = world_position(doc, index);
                    for axis in 0..3 {
                        center[axis] += position[axis];
                    }
                    count += 1.0;
                }
            }
            (count > 0.0).then(|| center.map(|value| value / count))
        } else {
            None
        };
        let mut axis_unit = [0.0f32; 3];
        if gizmo_axis < 3 {
            axis_unit[gizmo_axis] = 1.0;
        }
        let mut changes = Vec::new();
        for (entity_id, local_axis, start) in &sorted {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let mut after = before;
            after.scale[*local_axis] = (*start * ratio).clamp(0.01, 1000.0);
            if let Some(center) = center {
                let world = world_position(doc, index);
                let offset = [
                    world[0] - center[0],
                    world[1] - center[1],
                    world[2] - center[2],
                ];
                let along = (offset[0] * axis_unit[0]
                    + offset[1] * axis_unit[1]
                    + offset[2] * axis_unit[2])
                    * (ratio - 1.0);
                let target_world = [
                    world[0] + axis_unit[0] * along,
                    world[1] + axis_unit[1] * along,
                    world[2] + axis_unit[2] * along,
                ];
                after.position = world_position_to_local(doc, index, target_world);
            }
            if before != after {
                changes.push((*entity_id, before, after));
            }
        }
        if changes.is_empty() {
            return;
        }
        let ids: Vec<u32> = sorted.iter().map(|(entity_id, _, _)| *entity_id).collect();
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("scale-group/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 检查器里拖动 Transform 的一个分量（按实体合并撤销步）。
    pub fn set_transform_axis(&mut self, entity_id: u32, field: &str, axis: usize, value: f32) {
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let before = self.document().doc.entities[index].transform;
        let mut after = before;
        match field {
            "position" => after.position[axis] = value,
            "rotation" => after.rotation[axis] = value,
            "scale" => after.scale[axis] = value,
            _ => return,
        }
        let now_ms = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now_ms);
        document.dirty = true;
    }

    /// 整体写一个向量分量（`position` / `scale`）。
    ///
    /// 拖动检查器里的 `Position` / `Scale` 行标签时三个轴一起动；逐轴调
    /// [`Self::set_transform_axis`] 的合并键里带 axis，会被拆成三个撤销步（#479）。
    pub fn set_transform_vector(&mut self, entity_id: u32, field: &str, values: [f32; 3]) {
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let before = self.document().doc.entities[index].transform;
        let mut after = before;
        match field {
            "position" => after.position = values,
            "scale" => after.scale = values,
            _ => return,
        }
        if before == after {
            return;
        }
        let now_ms = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now_ms);
        document.dirty = true;
    }

    /// 多选：整体写向量分量，整次拖动一个撤销步。
    pub fn set_transform_vector_many(&mut self, entity_ids: &[u32], field: &str, values: [f32; 3]) {
        if !matches!(field, "position" | "scale") {
            return;
        }
        let mut ids: Vec<u32> = entity_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let doc = &self.document().doc;
        let mut changes = Vec::new();
        for entity_id in &ids {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let mut after = before;
            if field == "position" {
                after.position = values;
            } else {
                after.scale = values;
            }
            if before != after {
                changes.push((*entity_id, before, after));
            }
        }
        if changes.is_empty() {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("transform-vector/{field}/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 多选 Transform 字段：把同一个分量值写到所有选中实体（整次拖动一个撤销步）。
    pub fn set_transform_axis_many(
        &mut self,
        entity_ids: &[u32],
        field: &str,
        axis: usize,
        value: f32,
    ) {
        let mut ids: Vec<u32> = entity_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let doc = &self.document().doc;
        let mut changes = Vec::new();
        for entity_id in &ids {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let mut after = before;
            match field {
                "position" => after.position[axis] = value,
                "rotation" => after.rotation[axis] = value,
                "scale" => after.scale[axis] = value,
                _ => return,
            }
            if before != after {
                changes.push((*entity_id, before, after));
            }
        }
        if changes.is_empty() {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("transform-axis/{field}/{axis}/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    /// 整体写一个实体的 `rotation`（检查器按欧拉角编辑后换回四元数）。
    pub fn set_transform_rotation(&mut self, entity_id: u32, rotation: [f32; 4]) {
        let Some(rotation) = normalize_quat(rotation) else {
            return;
        };
        let Some(index) = self.document().doc.index_of_id(entity_id) else {
            return;
        };
        let before = self.document().doc.entities[index].transform;
        let mut after = before;
        after.rotation = rotation;
        if before == after {
            return;
        }
        let now_ms = now_ms();
        let document = self.document_mut();
        document
            .undo
            .push_transform(&mut document.doc, entity_id, before, after, now_ms);
        document.dirty = true;
    }

    /// 多选：把同一个 `rotation` 写到所有选中实体（一次撤销步）。
    pub fn set_transform_rotation_many(&mut self, entity_ids: &[u32], rotation: [f32; 4]) {
        let Some(rotation) = normalize_quat(rotation) else {
            return;
        };
        let mut ids: Vec<u32> = entity_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let doc = &self.document().doc;
        let mut changes = Vec::new();
        for entity_id in &ids {
            let Some(index) = doc.index_of_id(*entity_id) else {
                continue;
            };
            let before = doc.entities[index].transform;
            let mut after = before;
            after.rotation = rotation;
            if before != after {
                changes.push((*entity_id, before, after));
            }
        }
        if changes.is_empty() {
            return;
        }
        let now = now_ms();
        let document = self.document_mut();
        if changes.len() == 1 {
            let (entity_id, before, after) = changes[0];
            document
                .undo
                .push_transform(&mut document.doc, entity_id, before, after, now);
        } else {
            let key = format!("transform-rotation/{ids:?}");
            document
                .undo
                .push_transform_batch(&mut document.doc, changes, key, now);
        }
        document.dirty = true;
    }

    // ── 撤销 ────────────────────────────────────────────────────────────

    pub fn undo(&mut self) {
        let outcome = {
            let document = self.document_mut();
            document.undo.undo(&mut document.doc)
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.document_mut().dirty = true;
        self.restore_selection(outcome.selection);
    }

    pub fn redo(&mut self) {
        let outcome = {
            let document = self.document_mut();
            document.undo.redo(&mut document.doc)
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.document_mut().dirty = true;
        self.restore_selection(outcome.selection);
    }

    /// 撤销 / 重做之后把选择集校正好（#513）。
    fn restore_selection(&mut self, ids: Option<Vec<u32>>) {
        match ids {
            Some(ids) => {
                self.select_ids(&ids);
            }
            None => self.clamp_selection(),
        }
    }

    pub fn can_undo(&self) -> bool {
        self.document().undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.document().undo.can_redo()
    }

    /// 开始一次交互（拖动 gizmo / 拖拽输入框）：期间所有 Transform 改动并成一步，
    /// 不受 800ms 合并窗口影响 —— 中途停顿不会把一次拖动拆成两步。
    pub fn begin_interaction(&mut self) {
        self.document_mut().undo.begin_interaction();
    }

    /// 结束一次交互：把本次交互压成一步，下一次修改必然开新步 ——
    /// 两次相邻拖动不会合并成一步。
    pub fn end_interaction(&mut self) {
        self.document_mut().undo.commit();
    }

    /// 当前文档的撤销历史（最近 → 最早），供「撤销历史」面板使用。
    pub fn undo_history(&self) -> Vec<String> {
        self.document()
            .undo
            .history()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// 回退到历史面板中第 `position` 条（0 = 最近一条）之前：撤销 `position + 1` 步。
    /// 返回实际撤销的步数。
    pub fn undo_to(&mut self, position: usize) -> usize {
        let mut undone = 0;
        for _ in 0..=position {
            if !self.can_undo() {
                break;
            }
            self.undo();
            undone += 1;
        }
        if undone > 0 {
            self.log(
                LogSource::Editor,
                LogLevel::Info,
                format!("撤销 {undone} 步"),
            );
        }
        undone
    }

    /// 清空当前文档的撤销 / 重做历史。
    pub fn clear_history(&mut self) {
        self.document_mut().undo.clear();
        self.log(LogSource::Editor, LogLevel::Info, "已清空撤销历史");
    }

    /// 完整历史（最早 → 最新），`bool` 表示该步骤当前处于「已撤销」状态。
    pub fn history_entries(&self) -> Vec<(String, bool)> {
        self.document().undo.history_entries()
    }

    /// 当前状态在 `history_entries()` 中的位置（= 已执行步骤数）。
    pub fn history_cursor(&self) -> usize {
        self.document().undo.history_cursor()
    }

    /// 跳到历史中的绝对位置（`target` ∈ `0..=history_entries().len()`）。
    ///
    /// 与 `undo_to` 不同：往前是撤销，往后是重做，所以撤销过的步骤还能点回去；
    /// 返回实际移动的步数。
    pub fn jump_history(&mut self, target: usize) -> usize {
        let cursor = self.history_cursor();
        let mut moved = 0;
        if target < cursor {
            for _ in target..cursor {
                if !self.can_undo() {
                    break;
                }
                self.undo();
                moved += 1;
            }
        } else {
            for _ in cursor..target {
                if !self.can_redo() {
                    break;
                }
                self.redo();
                moved += 1;
            }
        }
        if moved > 0 {
            self.log(
                LogSource::Editor,
                LogLevel::Info,
                format!("history jump: {} step(s)", moved),
            );
        }
        moved
    }

    // ── 面板 / 布局 ─────────────────────────────────────────────────────

    pub fn panel(&self, id: &str) -> Option<&PanelInfo> {
        self.panels.iter().find(|panel| panel.id == id)
    }

    pub fn panel_title(&self, id: &str) -> String {
        match id {
            "sceneView" => self.i18n.t1("menu.sceneView"),
            "gameView" => self.i18n.t1("menu.gameView"),
            "docs" => self.i18n.t1("menu.docs"),
            "hierarchy" => self.i18n.t1("menu.hierarchy"),
            "assets" => self.i18n.t1("menu.assets"),
            "inspector" => self.i18n.t1("menu.inspector"),
            "log" => self.i18n.t1("menu.log"),
            "renderGraph" => self.i18n.t1("menu.renderGraph"),
            "projectSettings" => self.i18n.t1("menu.projectSettings"),
            "preferences" => self.i18n.t1("menu.preferences"),
            "theme" => self.i18n.t1("menu.theme"),
            "build" => self.i18n.t1("menu.buildPanel"),
            "tools" => self.i18n.t1("menu.tools"),
            "history" => self.i18n.t1("menu.history"),
            "shortcuts" => self.i18n.t1("menu.shortcuts"),
            "about" => self.i18n.t1("menu.about"),
            other => self
                .panel(other)
                .map(|panel| panel.title.clone())
                .unwrap_or_else(|| other.to_string()),
        }
    }

    pub fn known_panels(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.panels.iter().map(|panel| panel.id.clone()).collect();
        for id in dock::UTILITY_TABS {
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
        ids
    }

    pub fn show_panel(&mut self, id: &str) {
        self.dock = dock::show_panel(&self.dock, id);
    }

    /// `window/toggle/<id>`：已经在某个 leaf 里就切到它，否则放回默认锚点。
    pub fn toggle_panel(&mut self, id: &str) {
        if let Some(leaf) = find_leaf(&self.dock, id) {
            let leaf_id = leaf.id().to_string();
            self.dock = dock::activate_tab(&self.dock, &leaf_id, id);
            return;
        }
        self.dock = dock::show_panel(&self.dock, id);
    }

    pub fn activate_panel(&mut self, leaf_id: &str, tab: &str) {
        self.dock = dock::activate_tab(&self.dock, leaf_id, tab);
    }

    /// 最大化 / 还原一个 tab（tab 双击与右键菜单共用）。
    ///
    /// 只改 [`Workspace::maximized_tab`]，不动 dock tree。
    pub fn toggle_maximized(&mut self, tab: &str) {
        self.maximized_tab = if self.maximized_tab.as_deref() == Some(tab) {
            None
        } else {
            Some(tab.to_string())
        };
    }

    pub fn restore_maximized(&mut self) {
        self.maximized_tab = None;
    }

    /// 关掉 dock 里的一个 tab（浮动窗口里的同一面板也一起关）。
    ///
    /// 关掉**最后**一个面板是 no-op：否则整块布局会变成空白。
    pub fn close_panel(&mut self, tab: &str) {
        if dock::collect_tabs(&self.dock).len() <= 1 {
            return;
        }
        if let Some(next) = remove_panel(&self.dock, tab) {
            self.dock = next;
        }
        self.floating.retain(|panel| panel.tab != tab);
        if self.maximized_tab.as_deref() == Some(tab) {
            self.maximized_tab = None;
        }
    }

    /// 只留这一个 tab：其它面板从 dock 里移除，浮窗一并清掉。
    pub fn close_other_panels(&mut self, tab: &str) {
        if dock::find_leaf(&self.dock, tab).is_none() {
            return;
        }
        self.dock = dock::leaf([tab]);
        self.floating.clear();
        if self.maximized_tab.as_deref() != Some(tab) {
            self.maximized_tab = None;
        }
    }

    /// 把面板加进指定 leaf（`Add Tab` 菜单项）。
    pub fn add_tab_to_leaf(&mut self, leaf_id: &str, tab: &str) {
        self.dock = insert_panel(&self.dock, leaf_id, tab, DockZone::Center, 0.5, None);
        self.floating.retain(|panel| panel.tab != tab);
    }

    /// 拖拽结束：中心 = 合并成 tab；边缘 = 新开分屏。
    pub fn dock_tab(&mut self, tab: &str, target_leaf: &str, zone: DockZone) {
        self.dock = insert_panel(&self.dock, target_leaf, tab, zone, 0.5, None);
        self.floating.retain(|panel| panel.tab != tab);
    }

    /// 拖到主窗口左/右/底边。
    pub fn dock_tab_at_edge(&mut self, tab: &str, edge: DockEdge) {
        self.dock = dock::dock_panel_at_edge(Some(&self.dock), tab, edge, 0.25);
        self.floating.retain(|panel| panel.tab != tab);
    }

    pub fn float_panel(&mut self, tab: &str, x: f32, y: f32) {
        self.dock = remove_panel(&self.dock, tab).unwrap_or_else(|| leaf([tab]));
        let z = self.floating.iter().map(|panel| panel.z).max().unwrap_or(0) + 1;
        let panel = dock::clamp_float_panel(
            FloatPanel {
                tab: tab.to_string(),
                x,
                y,
                width: 340.0,
                height: 380.0,
                z,
            },
            1600.0,
            900.0,
        );
        self.floating.push(panel);
    }

    pub fn close_floating(&mut self, tab: &str) {
        self.floating.retain(|panel| panel.tab != tab);
    }

    pub fn raise_floating(&mut self, tab: &str) {
        let z = self.floating.iter().map(|panel| panel.z).max().unwrap_or(0) + 1;
        if let Some(panel) = self.floating.iter_mut().find(|panel| panel.tab == tab) {
            panel.z = z;
        }
    }

    pub fn set_layout_preset(&mut self, id: &str) {
        if let Some(node) = dock::layout_preset(id) {
            self.dock = node;
            self.floating.clear();
            self.maximized_tab = None;
            self.layout_preset = id.to_string();
        }
    }

    pub fn layout_json(&self) -> Value {
        dock::DockLayout {
            version: dock::DockLayout::VERSION,
            dock: self.dock.clone(),
            floating: self.floating.clone(),
        }
        .to_json()
    }

    // ── 资产 ────────────────────────────────────────────────────────────

    pub fn filtered_assets(&self) -> Vec<&AssetInfo> {
        crate::assets::filter_assets(&self.assets, &self.asset_query)
    }

    /// 检查器「选择资产」候选：按字段名推断资产类别，同类优先（全部按路径排序）。
    pub fn asset_picker_candidates(&self, field: &str) -> Vec<AssetInfo> {
        let leaf = field
            .rsplit('.')
            .next()
            .unwrap_or(field)
            .to_ascii_lowercase();
        let kind = if leaf == "env_map"
            || leaf.contains("hdr")
            || leaf.contains("env")
            || leaf.contains("sky")
        {
            Some(AssetKind::Env)
        } else if leaf.contains("mesh") || leaf.contains("model") {
            Some(AssetKind::Model)
        } else if leaf.contains("sprite")
            || leaf.contains("texture")
            || leaf.contains("icon")
            || leaf.contains("tex")
        {
            Some(AssetKind::Texture)
        } else if leaf.contains("audio") || leaf.contains("sound") || leaf.contains("music") {
            Some(AssetKind::Audio)
        } else if leaf.contains("material") {
            Some(AssetKind::Material)
        } else if leaf == "font" || leaf.contains("font") {
            Some(AssetKind::Font)
        } else if leaf.contains("prefab") {
            Some(AssetKind::Prefab)
        } else if leaf.contains("scene") {
            Some(AssetKind::Scene)
        } else {
            None
        };
        let mut candidates: Vec<AssetInfo> = self
            .assets
            .iter()
            .filter(|asset| kind.is_none_or(|kind| asset.kind == kind))
            .cloned()
            .collect();
        candidates.sort_by(|a, b| a.path.cmp(&b.path));
        candidates
    }

    pub fn toggle_asset_kind(&mut self, kind: AssetKind) {
        match self.asset_query.kinds.iter().position(|item| *item == kind) {
            Some(index) => {
                self.asset_query.kinds.remove(index);
            }
            None => self.asset_query.kinds.push(kind),
        }
    }

    pub fn toggle_asset_folder(&mut self, path: &str) {
        if !self.expanded_assets.insert(path.to_string()) {
            self.expanded_assets.remove(path);
        }
    }

    // ── 主题 / 语言 ─────────────────────────────────────────────────────

    pub fn colors(&self) -> ThemeColors {
        self.theme.colors()
    }

    pub fn color(&self, token: ThemeToken) -> crate::theme::Rgba {
        self.colors().color(token)
    }

    pub fn set_theme_preset(&mut self, id: &str) {
        self.theme.set_preset(id);
    }

    pub fn set_locale(&mut self, locale: Locale) {
        self.i18n.set_locale(locale);
    }

    pub fn t(&self, key: &str) -> String {
        self.i18n.t1(key)
    }

    // ── 日志 / Play ─────────────────────────────────────────────────────

    pub fn log(&mut self, source: LogSource, level: LogLevel, text: impl Into<String>) {
        self.log.push(source, level, text);
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        // 进入播放前先留住编辑态：Stop 要原样还回去（#500）。
        self.play_snapshot = Some(self.take_play_snapshot());
        // 播放中自动最大化 Game 视图（#421）；Stop 时连「原本有没有最大化」
        // 一起还原，所以用户自己最大化过的面板不会被吞掉。
        if self.maximize_on_play {
            self.play_maximized_before = Some(self.maximized_tab.clone());
            self.maximized_tab = Some("gameView".to_string());
        }
        self.playing = true;
        self.paused = false;
        self.play_time_s = 0.0;
        self.log(LogSource::Editor, LogLevel::Info, "进入 Play 模式");
    }

    pub fn stop(&mut self) {
        let restored = self.play_snapshot.take().map(|snapshot| {
            self.restore_play_snapshot(snapshot);
        });
        if let Some(previous) = self.play_maximized_before.take() {
            self.maximized_tab = previous;
        }
        self.playing = false;
        self.paused = false;
        self.play_time_s = 0.0;
        if restored.is_some() {
            self.log(
                LogSource::Editor,
                LogLevel::Info,
                "停止 Play：已丢弃播放期间的编辑，恢复进入前的文档",
            );
        } else {
            self.log(LogSource::Editor, LogLevel::Info, "停止 Play");
        }
    }

    /// 播放期间的编辑是否会在 Stop 时被丢弃（UI 用它提示）。
    pub fn has_play_snapshot(&self) -> bool {
        self.play_snapshot.is_some()
    }

    fn take_play_snapshot(&self) -> PlaySnapshot {
        let document = &self.docs[self.active];
        PlaySnapshot {
            doc: self.active,
            scene: document.doc.clone(),
            undo: document.undo.clone(),
            dirty: document.dirty,
            selection: self.selection.clone(),
            selection_active: self.selection_active,
        }
    }

    /// 把文档 / 撤销栈 / 脏标记 / 选中还原成进入 Play 前的样子。
    ///
    /// 只碰快照记录的那份文档：播放中切到别的文档时，别的文档保持原样。
    fn restore_play_snapshot(&mut self, snapshot: PlaySnapshot) {
        let Some(document) = self.docs.get_mut(snapshot.doc) else {
            return;
        };
        document.doc = snapshot.scene;
        document.undo = snapshot.undo;
        document.dirty = snapshot.dirty;
        self.active = snapshot.doc;
        let entity_count = document.doc.entities.len();
        let mut selection = snapshot.selection;
        selection.retain(|index| *index < entity_count);
        self.selection = selection;
        self.selection_active = snapshot
            .selection_active
            .filter(|index| self.selection.contains(index))
            .or_else(|| self.selection.last().copied());
    }

    pub fn pause(&mut self) {
        if self.playing {
            self.paused = true;
        }
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn step(&mut self) {
        if self.playing {
            self.paused = true;
            self.play_time_s += 1.0 / 60.0;
        }
    }

    /// 每帧推进（暂停时不推进模拟）。
    pub fn tick(&mut self, dt: f32) {
        if self.playing && !self.paused {
            self.play_time_s += dt;
        }
    }

    pub fn play_state_label(&self) -> &'static str {
        if !self.playing {
            "编辑"
        } else if self.paused {
            "暂停"
        } else {
            "运行"
        }
    }

    // ── 视口取景 ────────────────────────────────────────────────────────

    /// 框住整个文档：用世界包围盒（父链 + 缩放），不再只看局部 `position`。
    pub fn frame_all(&mut self) {
        let indices: Vec<usize> = (0..self.document().doc.entities.len()).collect();
        let bounds = world_bounds(&self.document().doc, &indices);
        if let Some((center, radius)) = bounds {
            self.camera.focus(center, radius);
        }
    }

    /// 框住选中项：世界包围球 + 按相机 FOV 推导的距离（`#505`）。
    ///
    /// 半径来自 [`Self::selection_world_bounds`]（含父链变换与缩放），所以
    /// 大物体不再穿进去、小物体不再离得太远。
    pub fn frame_selection(&mut self) {
        let Some((center, radius)) = self.selection_world_bounds() else {
            return;
        };
        self.camera.focus(center, radius);
    }

    /// 选中集合的世界包围球 `(center, radius)`；空选返回 `None`。
    pub fn selection_world_bounds(&self) -> Option<([f32; 3], f32)> {
        world_bounds(&self.document().doc, &self.selection)
    }

    /// gizmo 的落点（#416）：Center = 选择集中心；Pivot = 活动对象的轴心。
    ///
    /// `Pivot` 模式没有活动对象时退回选择集中心，不会让 gizmo 突然消失。
    pub fn gizmo_center_world(&self) -> Option<[f32; 3]> {
        let doc = &self.document().doc;
        match self.pivot_mode {
            PivotMode::Pivot => self
                .primary_selection()
                .and_then(|index| doc.entities.get(index).map(|_| world_position(doc, index)))
                .or_else(|| self.selection_center_world()),
            PivotMode::Center => self.selection_center_world(),
        }
    }

    /// 当前选择集的实体 id（文档顺序，未去重；batch API 自己会去重并裁剪后代）。
    pub fn selection_entity_ids(&self) -> Vec<u32> {
        let doc = &self.document().doc;
        self.selection
            .iter()
            .filter_map(|index| doc.id_of_index(*index))
            .collect()
    }

    /// 选择集在世界空间的中心（父链累积位置）。
    pub fn selection_center_world(&self) -> Option<[f32; 3]> {
        let doc = &self.document().doc;
        let mut center = [0.0; 3];
        let mut count = 0;
        for index in &self.selection {
            let position = world_position(doc, *index);
            center[0] += position[0];
            center[1] += position[1];
            center[2] += position[2];
            count += 1;
        }
        (count > 0).then(|| {
            [
                center[0] / count as f32,
                center[1] / count as f32,
                center[2] / count as f32,
            ]
        })
    }

    /// 「Align View to Selected」：把轨道中心对准选中项（保留距离与角度）。
    pub fn align_view_to_selection(&mut self) -> bool {
        let Some(center) = self.selection_center_world() else {
            return false;
        };
        self.camera.target = center;
        true
    }

    /// 「Align Selected to View」：把选中项平移到当前轨道中心。
    pub fn align_selection_to_view(&mut self) -> bool {
        let Some(center) = self.selection_center_world() else {
            return false;
        };
        let delta = [
            self.camera.target[0] - center[0],
            self.camera.target[1] - center[1],
            self.camera.target[2] - center[2],
        ];
        let ids: Vec<u32> = self
            .selection
            .iter()
            .filter_map(|index| self.document().doc.entities.get(*index)?.id)
            .collect();
        self.translate_entities_world(&ids, delta);
        true
    }

    /// 方向 gizmo：沿世界轴看过去（`axis` 0/1/2，`sign` ±1）。
    pub fn look_along_axis(&mut self, axis: usize, sign: f32) {
        let direction = match axis {
            0 => [sign, 0.0, 0.0],
            1 => [0.0, sign, 0.0],
            _ => [0.0, 0.0, sign],
        };
        self.camera.yaw = direction[0].atan2(direction[2]);
        self.camera.pitch = direction[1].asin().clamp(-1.55, 1.55);
    }

    /// 选中项贴地：每个实体把自己的世界 AABB 底面移到 y = 0（一次撤销步）。
    pub fn snap_selection_to_ground(&mut self) -> usize {
        let doc = self.document().doc.clone();
        let mut changes = Vec::new();
        for index in &self.selection {
            let Some(entity) = doc.entities.get(*index) else {
                continue;
            };
            let Some(entity_id) = entity.id else {
                continue;
            };
            let world = world_position(&doc, *index);
            let half_height = (world_scale(&doc, *index)[1] * 0.5).abs();
            let delta_y = half_height - world[1];
            if delta_y.abs() < 1e-4 {
                continue;
            }
            let delta_local = quat_rotate(
                quat_conjugate(parent_world_rotation(&doc, *index)),
                [0.0, delta_y, 0.0],
            );
            let before = entity.transform;
            let mut after = before;
            for (position, delta) in after.position.iter_mut().zip(delta_local) {
                *position += delta;
            }
            changes.push((entity_id, before, after));
        }
        if changes.is_empty() {
            return 0;
        }
        let ids: Vec<u32> = changes.iter().map(|(id, _, _)| *id).collect();
        let count = changes.len();
        let now = now_ms();
        let document = self.document_mut();
        document.undo.push_transform_batch(
            &mut document.doc,
            changes,
            format!("snap-ground/{ids:?}"),
            now,
        );
        document.dirty = true;
        self.log(
            LogSource::Editor,
            LogLevel::Info,
            format!("已把 {count} 个实体贴到地面"),
        );
        count
    }

    // ── 命令分发（菜单 / 快捷键共用） ──────────────────────────────────

    pub fn run(&mut self, command: Command) {
        match command {
            Command::NewScene => self.new_document(DocKind::Scene, None),
            Command::NewPrefab => self.new_document(DocKind::Prefab, None),
            Command::OpenDocument => self.toggle_panel("docs"),
            Command::Save => match self.save_active() {
                Ok(path) => self.status = format!("已保存 {path}"),
                Err(error) => self.log(
                    LogSource::Editor,
                    LogLevel::Error,
                    format!("保存失败：{error}"),
                ),
            },
            Command::ReloadDocument => {
                if let Err(error) = self.reload_active() {
                    self.log(
                        LogSource::Editor,
                        LogLevel::Error,
                        format!("重新读取失败：{error}"),
                    );
                }
            }
            Command::RenameDocument | Command::DeleteDocument => {
                self.log(
                    LogSource::Editor,
                    LogLevel::Warn,
                    "重命名 / 删除请在资产面板或文件系统里做（GPUI 壳暂不提供破坏性操作）",
                );
            }
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::DuplicateEntity => self.duplicate_selection(),
            Command::DeleteSelection => self.delete_selection(),
            Command::SelectAll => {
                self.selection = (0..self.document().doc.entities.len()).collect();
                self.selection_active = self.selection.last().copied();
            }
            Command::PasteComponent => self.paste_component(),
            Command::ClearComponentClipboard => {
                self.clipboard = None;
                self.status = "已清空组件剪贴板".to_string();
            }
            Command::ResetLayout => self.set_layout_preset("default"),
            Command::CopyEntity => {
                self.copy_selection();
            }
            Command::CutEntity => {
                self.cut_selection();
            }
            Command::PasteEntity => {
                self.paste_entities(None);
            }
            Command::BuildProject => {
                // 真正可用的构建入口是 build 面板（编辑器里由 `ToggleBuild` 打开）。
                self.toggle_panel("build");
            }
            Command::CookScenes => {
                self.toggle_panel("build");
            }
            Command::Play => self.play(),
            Command::Pause => {
                if self.paused {
                    self.resume();
                } else {
                    self.pause();
                }
            }
            Command::Stop => self.stop(),
            Command::Step => self.step(),
            Command::FrameAll => self.frame_all(),
            Command::FrameSelection => self.frame_selection(),
            Command::ResetSceneCamera => self.camera = OrbitCamera::default(),
            other => {
                if let Some(panel) = other.panel_id() {
                    self.toggle_panel(panel);
                }
            }
        }
    }

    /// 菜单里显示一项是否可用。
    pub fn is_enabled(&self, command: Command) -> bool {
        match command {
            Command::Undo => self.can_undo(),
            Command::Redo => self.can_redo(),
            Command::DuplicateEntity
            | Command::DeleteSelection
            | Command::CopyEntity
            | Command::CutEntity
            | Command::FrameSelection => !self.selection.is_empty(),
            Command::PasteEntity => self.entity_clipboard.is_some(),
            Command::PasteComponent => self.clipboard.is_some() && !self.selection.is_empty(),
            Command::ClearComponentClipboard => self.clipboard.is_some(),
            Command::Pause | Command::Stop | Command::Step => self.playing,
            Command::Play => !self.playing,
            Command::BuildProject | Command::CookScenes | Command::OfflineTools => true,
            _ => true,
        }
    }

    /// 只统计停靠在树里的 tab（不含浮动窗口）。
    pub fn docked_tabs(&self) -> Vec<String> {
        collect_tabs(&self.dock)
    }

    pub fn dock_tabs(&self) -> Vec<String> {
        let mut tabs = collect_tabs(&self.dock);
        tabs.extend(self.floating.iter().map(|panel| panel.tab.clone()));
        tabs
    }

    /// 已停靠面板的 leaf id（拖拽命中测试用）。
    pub fn leaf_of(&self, tab: &str) -> Option<String> {
        find_leaf(&self.dock, tab).map(|leaf| leaf.id().to_string())
    }

    pub fn leaf_by_id(&self, id: &str) -> Option<&DockNode> {
        find_leaf_by_id(&self.dock, id)
    }

    pub fn component_value(&self, entity: usize, component: &str) -> Option<&Value> {
        self.document()
            .doc
            .entities
            .get(entity)?
            .components
            .get(component)
    }

    pub fn prefab_links(&self) -> Vec<(usize, String)> {
        self.document().doc.prefab_links()
    }

    pub fn prefab_component_name() -> &'static str {
        PREFAB_LINK_COMPONENT
    }

    /// 实例（含子级）与源预制体的逐字段差异。
    ///
    /// 惰性计算：不往 `.scene.json` 里存快照，打开场景时按 `PrefabInstance`
    /// 指向的文件实时比对，所以文档格式零改动。
    ///
    /// 实体配对靠 id 平移（`instantiate_prefab` 用 `base + 原 id` 复制整棵子树），
    /// 不依赖数组顺序：用户在实例里增删兄弟节点不会让后面的实体全部错位。
    pub fn prefab_overrides(&self, root_index: usize) -> io::Result<Vec<PrefabOverride>> {
        let doc = &self.document().doc;
        let root = doc
            .entities
            .get(root_index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "实体不存在"))?;
        let root_id = root
            .id
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "实例根没有 id"))?;
        let prefab = self.prefab_document_for(root_id)?;
        let Some(prefab_root_id) = prefab.entities.first().and_then(|entity| entity.id) else {
            return Ok(Vec::new());
        };
        let base = root_id as i64 - prefab_root_id as i64;

        let mut out = Vec::new();
        for (index, source) in prefab.entities.iter().enumerate() {
            let Some(source_id) = source.id else { continue };
            let instance_id = (source_id as i64 + base).try_into().unwrap_or(u32::MAX);
            let Some(instance_index) = doc.index_of_id(instance_id) else {
                continue;
            };
            let instance = &doc.entities[instance_index];
            out.extend(overrides_between(instance, instance_index, source, index));
        }
        Ok(out)
    }

    /// 回滚一个 override：把源预制体的值写回实例（一个撤销步）。
    pub fn revert_prefab_override(&mut self, target: &PrefabOverride) -> bool {
        let Some(id) = self.document().doc.id_of_index(target.entity) else {
            return false;
        };
        let mut changed = false;
        self.edit("回滚预制体重写", |document| {
            let Some(index) = document.doc.index_of_id(id) else {
                return;
            };
            let entity = &mut document.doc.entities[index];
            if target.component == "transform" {
                let value = &target.prefab_value;
                match target.field.as_str() {
                    "position" | "scale" => {
                        if let Some(array) = json_vec3(value) {
                            if target.field == "position" {
                                entity.transform.position = array;
                            } else {
                                entity.transform.scale = array;
                            }
                            changed = true;
                        }
                    }
                    "rotation" => {
                        if let Some(array) = json_quat(value) {
                            entity.transform.rotation = array;
                            changed = true;
                        }
                    }
                    _ => {}
                }
            } else if let Some(component) = entity.components.get_mut(&target.component) {
                set_json_field(component, &target.field, target.prefab_value.clone());
                changed = true;
            }
        });
        changed
    }

    /// 把一个 override 应用到源预制体（写回文件或已打开的预制体文档）。
    pub fn apply_prefab_override(
        &mut self,
        root_index: usize,
        target: &PrefabOverride,
    ) -> io::Result<()> {
        let root_id = self
            .document()
            .doc
            .entities
            .get(root_index)
            .and_then(|entity| entity.id)
            .and_then(|id| {
                self.document()
                    .doc
                    .entities
                    .iter()
                    .find(|e| e.id == Some(id))
            })
            .and_then(|entity| entity.prefab_link())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "不是预制体实例"))?
            .to_string();

        // 实例实体 → 预制体实体：同样按 id 平移配对。
        let prefab = self.prefab_document_for(root_id_of(self, root_index)?)?;
        let Some(prefab_root_id) = prefab.entities.first().and_then(|entity| entity.id) else {
            return Ok(());
        };
        let base = root_id_of(self, root_index)? as i64 - prefab_root_id as i64;
        let instance_id = self
            .document()
            .doc
            .id_of_index(target.entity)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "实体没有 id"))?;
        let prefab_id = (instance_id as i64 - base) as u32;

        let mut updated = prefab.clone();
        let index = updated
            .index_of_id(prefab_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "预制体里找不到对应实体"))?;
        let entity = &mut updated.entities[index];
        if target.component == "transform" {
            match target.field.as_str() {
                "position" => {
                    if let Some(array) = json_vec3(&target.instance_value) {
                        entity.transform.position = array;
                    }
                }
                "scale" => {
                    if let Some(array) = json_vec3(&target.instance_value) {
                        entity.transform.scale = array;
                    }
                }
                "rotation" => {
                    if let Some(array) = json_quat(&target.instance_value) {
                        entity.transform.rotation = array;
                    }
                }
                _ => {}
            }
        } else if let Some(component) = entity.components.get_mut(&target.component) {
            set_json_field(component, &target.field, target.instance_value.clone());
        }

        // 预制体正开着时同步内存文档：干净就直接落盘（用户点 Apply 就是要写文件），
        // 有未保存改动就留在内存里，让用户自己决定保存时机。
        let open_clean = self
            .docs
            .iter()
            .find(|document| document.path.as_deref() == Some(root_id.as_str()))
            .map(|document| !document.dirty);
        if let Some(document) = self
            .docs
            .iter_mut()
            .find(|document| document.path.as_deref() == Some(root_id.as_str()))
        {
            document.doc = updated.clone();
            document.dirty = true;
        }
        if open_clean == Some(false) {
            return Ok(());
        }
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有项目根"))?;
        let disk = self.pruned_doc(updated);
        project::write_document(&root, &root_id, &disk)?;
        Ok(())
    }

    /// 完全解包：移除实例（以及子树里嵌套实例）的预制体链接，保留场景内容。
    ///
    /// 返回被移除的链接数量，整体算一个撤销步。
    pub fn unpack_prefab(&mut self, root_index: usize) -> usize {
        let doc = &self.document().doc;
        let mut subtree = doc.descendants(root_index);
        subtree.push(root_index);
        let linked: Vec<usize> = subtree
            .into_iter()
            .filter(|index| {
                doc.entities
                    .get(*index)
                    .map(|entity| entity.has_component(PREFAB_LINK_COMPONENT))
                    .unwrap_or(false)
            })
            .collect();
        if linked.is_empty() {
            return 0;
        }
        self.edit("解包预制体", |document| {
            for index in &linked {
                document.doc.remove_component(*index, PREFAB_LINK_COMPONENT);
            }
        });
        linked.len()
    }
}

/// 选中实体的 id（没有 id 直接报错：场景文档在 `normalize` 之后一定都有）。
fn root_id_of(workspace: &Workspace, index: usize) -> io::Result<u32> {
    workspace
        .document()
        .doc
        .id_of_index(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "实体没有 id"))
}

fn json_vec3(value: &Value) -> Option<[f32; 3]> {
    let items = value.as_array()?;
    if items.len() != 3 {
        return None;
    }
    Some([
        items[0].as_f64()? as f32,
        items[1].as_f64()? as f32,
        items[2].as_f64()? as f32,
    ])
}

fn json_quat(value: &Value) -> Option<[f32; 4]> {
    let items = value.as_array()?;
    if items.len() != 4 {
        return None;
    }
    Some([
        items[0].as_f64()? as f32,
        items[1].as_f64()? as f32,
        items[2].as_f64()? as f32,
        items[3].as_f64()? as f32,
    ])
}

fn set_json_field(component: &mut Value, field: &str, value: Value) {
    if let Some(map) = component.as_object_mut() {
        if value.is_null() {
            map.remove(field);
        } else {
            map.insert(field.to_string(), value);
        }
    }
}

/// 浮点比较：JSON 往返和欧拉角编辑会带来 1e-6 级噪声，差 1e-4 以内算相同。
fn values_close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
            (a - b).abs() <= 1e-4 * a.abs().max(b.abs()).max(1.0)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| values_close(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| values_close(value, other)))
        }
        _ => a == b,
    }
}

/// 两个实体之间的字段差异（`transform` 的三个字段 + 每个组件的每个字段）。
fn overrides_between(
    instance: &EntityJson,
    instance_index: usize,
    source: &EntityJson,
    _source_index: usize,
) -> Vec<PrefabOverride> {
    let mut out = Vec::new();
    let name = instance.display_name();
    for (field, left, right) in [
        (
            "position",
            serde_json::json!(instance.transform.position),
            serde_json::json!(source.transform.position),
        ),
        (
            "rotation",
            serde_json::json!(instance.transform.rotation),
            serde_json::json!(source.transform.rotation),
        ),
        (
            "scale",
            serde_json::json!(instance.transform.scale),
            serde_json::json!(source.transform.scale),
        ),
    ] {
        if !values_close(&left, &right) {
            out.push(PrefabOverride {
                entity: instance_index,
                entity_name: name.clone(),
                component: "transform".to_string(),
                field: field.to_string(),
                instance_value: left,
                prefab_value: right,
            });
        }
    }

    let mut components: Vec<&String> = instance
        .components
        .keys()
        .chain(source.components.keys())
        .filter(|name| name.as_str() != PREFAB_LINK_COMPONENT)
        .collect();
    components.sort();
    components.dedup();
    for component in components {
        let left = instance.components.get(component);
        let right = source.components.get(component);
        let mut fields: Vec<String> = left
            .and_then(|value| value.as_object())
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(map) = right.and_then(|value| value.as_object()) {
            fields.extend(map.keys().cloned());
        }
        fields.sort();
        fields.dedup();
        if fields.is_empty() {
            // 数组 / 标量组件：整个值比一次。
            let (left, right) = (
                left.cloned().unwrap_or(Value::Null),
                right.cloned().unwrap_or(Value::Null),
            );
            if !values_close(&left, &right) {
                out.push(PrefabOverride {
                    entity: instance_index,
                    entity_name: name.clone(),
                    component: component.clone(),
                    field: String::new(),
                    instance_value: left,
                    prefab_value: right,
                });
            }
            continue;
        }
        for field in fields {
            let left = left
                .and_then(|value| value.get(&field))
                .cloned()
                .unwrap_or(Value::Null);
            let right = right
                .and_then(|value| value.get(&field))
                .cloned()
                .unwrap_or(Value::Null);
            if !values_close(&left, &right) {
                out.push(PrefabOverride {
                    entity: instance_index,
                    entity_name: name.clone(),
                    component: component.clone(),
                    field,
                    instance_value: left,
                    prefab_value: right,
                });
            }
        }
    }
    out
}

/// 文档所在的目录（`assets/scenes` 这种），没有路径时按 `assets` 处理。
fn document_dir(path: Option<&str>) -> String {
    path.and_then(|path| path.rsplit_once('/'))
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_else(|| "assets".to_string())
}

fn hydrate_doc(schema: &Schema, doc: &mut SceneDoc) {
    for entity in &mut doc.entities {
        for (component_name, value) in entity.components.iter_mut() {
            let Some(component) = schema.component(component_name) else {
                continue;
            };
            let defaults = component.default_value();
            merge_missing(value, &defaults);
        }
    }
}

fn merge_missing(target: &mut Value, defaults: &Value) {
    if let (Value::Object(target_map), Value::Object(default_map)) = (target, defaults) {
        for (key, value) in default_map {
            match target_map.get_mut(key) {
                Some(existing) => merge_missing(existing, value),
                None => {
                    target_map.insert(key.clone(), value.clone());
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn build_entity(
    schema: &Schema,
    preset: Option<&EntityPreset>,
    parent: Option<usize>,
    doc: &SceneDoc,
) -> EntityJson {
    let parent_id = parent.and_then(|index| doc.id_of_index(index));
    let (name, components, position, rotation) = match preset {
        Some(preset) => (
            preset.name.clone(),
            preset.components.clone(),
            preset.position,
            preset.rotation,
        ),
        None => (
            "GameObject".to_string(),
            Vec::new(),
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
        ),
    };
    let mut entity = EntityJson {
        name: Some(doc.unique_name(&name, parent_id)),
        parent: parent_id,
        transform: TransformJson {
            position,
            rotation,
            scale: [1.0; 3],
        },
        ..Default::default()
    };
    for component_name in components {
        let Some(component) = schema.component(&component_name) else {
            continue;
        };
        let override_value = preset.and_then(|preset| preset.overrides.get(&component_name));
        let value = component_value_with_overrides(&component.default_value(), override_value);
        entity.components.insert(component_name, value);
    }
    entity
}

fn default_panels() -> Vec<PanelInfo> {
    let mut panels: Vec<PanelInfo> = [
        ("sceneView", "场景视图"),
        ("gameView", "游戏视图"),
        ("docs", "文档"),
        ("hierarchy", "层级"),
        ("assets", "资产"),
        ("inspector", "检查器"),
        ("log", "日志"),
        ("renderGraph", "渲染图"),
        ("projectSettings", "项目设置"),
        ("preferences", "首选项"),
        ("theme", "界面主题"),
        ("build", "构建项目"),
        ("tools", "离线工具"),
        ("history", "撤销历史"),
        ("shortcuts", "键盘快捷键"),
        ("about", "关于 PrismaRev Editor"),
    ]
    .iter()
    .map(|(id, title)| PanelInfo::new(id, title))
    .collect();
    panels.sort_by(|a, b| a.id.cmp(&b.id));
    panels
}

pub fn layout_preset_ids() -> Vec<(&'static str, &'static str)> {
    LAYOUT_PRESETS
        .iter()
        .map(|preset| (preset.id, preset.label))
        .collect()
}

/// 归一化四元数；长度接近 0（非法输入）时返回 `None`。
fn normalize_quat(quat: [f32; 4]) -> Option<[f32; 4]> {
    let length =
        (quat[0] * quat[0] + quat[1] * quat[1] + quat[2] * quat[2] + quat[3] * quat[3]).sqrt();
    if !length.is_finite() || length < 1e-6 {
        return None;
    }
    Some([
        quat[0] / length,
        quat[1] / length,
        quat[2] / length,
        quat[3] / length,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_path_replaces_the_whole_component_in_one_step() {
        // 组件菜单的 Reset 走 `set_field(.., "", defaultJson)`：整份组件替换、一步撤销。
        let mut workspace = Workspace::sample();
        let id = workspace.document().doc.id_of_index(0).unwrap();
        workspace.document_mut().doc.entities[0]
            .components
            .insert("test::Rig".to_string(), serde_json::json!({"speed": 9.0}));
        let undo_before = workspace.undo_history().len();

        workspace.set_field(id, "test::Rig", "", serde_json::json!({"speed": 1.0}));

        assert_eq!(
            workspace.document().doc.entities[0].components["test::Rig"],
            serde_json::json!({"speed": 1.0})
        );
        assert_eq!(workspace.undo_history().len(), undo_before + 1);
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].components["test::Rig"]["speed"],
            serde_json::json!(9.0)
        );
    }

    #[test]
    fn pasting_component_values_overwrites_matching_components_in_one_step() {
        let mut workspace = Workspace::sample();
        workspace.document_mut().doc.entities[0]
            .components
            .insert("test::Rig".to_string(), serde_json::json!({"speed": 1.0}));
        workspace.document_mut().doc.entities[1]
            .components
            .insert("test::Rig".to_string(), serde_json::json!({"speed": 2.0}));

        workspace.copy_component_at(0, "test::Rig");
        let undo_before = workspace.undo_history().len();
        assert_eq!(
            workspace.paste_component_values(&[1], "test::Rig"),
            1,
            "同类型组件才覆盖"
        );
        assert_eq!(
            workspace.document().doc.entities[1].components["test::Rig"]["speed"],
            serde_json::json!(1.0)
        );
        assert_eq!(workspace.undo_history().len(), undo_before + 1);

        // 剪贴板类型与目标不一致 / 目标没有该组件时不写。
        assert_eq!(workspace.paste_component_values(&[0], "test::Other"), 0);
        assert_eq!(workspace.paste_component_values(&[2], "test::Rig"), 0);
    }

    #[test]
    fn maximize_on_play_restores_the_previous_layout() {
        // #421：Play 自动最大化 Game 视图，Stop 回到进入前的状态。
        let mut workspace = Workspace::sample();
        workspace.maximize_on_play = true;
        assert!(workspace.maximized_tab.is_none());

        workspace.play();
        assert_eq!(workspace.maximized_tab.as_deref(), Some("gameView"));

        workspace.stop();
        assert!(
            workspace.maximized_tab.is_none(),
            "进入 Play 前没有最大化任何面板，Stop 后也必须回到都没有最大化"
        );

        // 用户自己最大化过 Scene 视图：Play / Stop 之后仍应保持 Scene 最大化。
        workspace.maximized_tab = Some("sceneView".to_string());
        workspace.play();
        assert_eq!(workspace.maximized_tab.as_deref(), Some("gameView"));
        workspace.stop();
        assert_eq!(workspace.maximized_tab.as_deref(), Some("sceneView"));
    }

    #[test]
    fn maximize_on_play_off_leaves_the_layout_alone() {
        let mut workspace = Workspace::sample();
        workspace.maximized_tab = Some("hierarchy".to_string());
        workspace.play();
        assert_eq!(workspace.maximized_tab.as_deref(), Some("hierarchy"));
        workspace.stop();
        assert_eq!(workspace.maximized_tab.as_deref(), Some("hierarchy"));
    }

    #[test]
    fn camera_fov_and_clip_planes_are_clamped() {
        // #416：View… 面板的输入都经过夹取，坏值不会污染渲染 / 拾取。
        let mut camera = OrbitCamera::default();
        assert!((camera.fov_y_degrees - 60.0).abs() < 1e-6);
        camera.set_fov_degrees(5.0);
        assert!((camera.fov_y_degrees - 10.0).abs() < 1e-6);
        camera.set_fov_degrees(400.0);
        assert!((camera.fov_y_degrees - 170.0).abs() < 1e-6);
        let before = camera.fov_y_degrees;
        camera.set_fov_degrees(f32::NAN);
        assert_eq!(camera.fov_y_degrees, before, "非有限值忽略");

        camera.set_clip_planes(0.0, 100.0);
        assert!((camera.near - 0.1).abs() < 1e-6, "非法 near 不动");
        camera.set_clip_planes(1.0, 0.5);
        assert!((camera.near - 1.0).abs() < 1e-6);
        assert!((camera.far - 1.01).abs() < 1e-6, "far 至少比 near 大 1cm");
        camera.set_clip_planes(2.0, 500.0);
        assert!((camera.near - 2.0).abs() < 1e-6);
        assert!((camera.far - 500.0).abs() < 1e-6);
    }

    #[test]
    fn focus_distance_follows_the_configured_fov() {
        // 视场角越窄，同样半径要退得越远。
        let mut wide = OrbitCamera::default();
        wide.set_fov_degrees(90.0);
        wide.focus([0.0, 0.0, 0.0], 1.0);
        let mut narrow = OrbitCamera::default();
        narrow.set_fov_degrees(30.0);
        narrow.focus([0.0, 0.0, 0.0], 1.0);
        assert!(
            narrow.distance > wide.distance,
            "30° {} 应该比 90° {} 更远",
            narrow.distance,
            wide.distance
        );
        assert!((wide.distance - 1.2 / 45.0f32.to_radians().sin()).abs() < 1e-3);
    }

    #[test]
    fn aligning_the_camera_to_an_axis_puts_the_eye_on_that_side() {
        // #416：方向 gizmo 点轴之后，eye 必须落在该轴的正 / 负方向上。
        let mut camera = OrbitCamera::default();
        camera.target = [1.0, 2.0, 3.0];
        for (axis, sign) in [
            (0, 1.0),
            (0, -1.0),
            (1, 1.0),
            (1, -1.0),
            (2, 1.0),
            (2, -1.0),
        ] {
            camera.align_to_axis(axis, sign);
            let eye = camera.eye();
            let offset = [
                eye[0] - camera.target[0],
                eye[1] - camera.target[1],
                eye[2] - camera.target[2],
            ];
            let length =
                (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
            let expected = camera.distance;
            assert!(
                (length - expected).abs() < 1e-3,
                "axis {axis} sign {sign}: eye 距离 {} != {}",
                length,
                expected
            );
            for other in 0..3 {
                let want = if other == axis { sign } else { 0.0 };
                let normalized = offset[other] / length;
                assert!(
                    (normalized - want).abs() < 1e-3,
                    "axis {axis} sign {sign} component {other}: {normalized} != {want}"
                );
            }
        }
        // 非法轴不动相机。
        let before = camera;
        camera.align_to_axis(9, 1.0);
        assert_eq!(camera, before);
    }

    #[test]
    fn component_presets_replace_by_name_and_keep_values() {
        // #418：同名后写覆盖前写；不同组件互不影响；空名字不落库。
        let mut workspace = Workspace::sample();
        workspace.save_component_preset("a::B", "hero", serde_json::json!({"x": 1}));
        workspace.save_component_preset("a::C", "hero", serde_json::json!({"y": 2}));
        workspace.save_component_preset("a::B", "hero", serde_json::json!({"x": 3}));

        assert_eq!(
            workspace.component_preset_names("a::B"),
            vec!["hero".to_string()]
        );
        assert_eq!(
            workspace.component_preset_value("a::B", "hero"),
            Some(serde_json::json!({"x": 3}))
        );
        assert_eq!(
            workspace.component_preset_value("a::C", "hero"),
            Some(serde_json::json!({"y": 2}))
        );
        assert_eq!(workspace.component_preset_value("a::B", "missing"), None);

        workspace.save_component_preset("a::B", "   ", serde_json::json!({}));
        assert_eq!(workspace.component_preset_names("a::B").len(), 1);
    }

    #[test]
    fn orbit_applies_horizontal_and_vertical_deltas_independently() {
        let mut camera = OrbitCamera::default();
        let (yaw, pitch) = (camera.yaw, camera.pitch);
        camera.orbit(10.0, 0.0);
        assert!((camera.yaw - (yaw - 0.1)).abs() < 1e-6);
        assert_eq!(camera.pitch, pitch);

        camera = OrbitCamera::default();
        let (yaw, pitch) = (camera.yaw, camera.pitch);
        camera.orbit(0.0, 10.0);
        assert_eq!(camera.yaw, yaw);
        assert!((camera.pitch - (pitch + 0.1)).abs() < 1e-6);

        camera = OrbitCamera::default();
        let (yaw, pitch) = (camera.yaw, camera.pitch);
        camera.orbit(10.0, -20.0);
        assert!((camera.yaw - (yaw - 0.1)).abs() < 1e-6);
        assert!((camera.pitch - (pitch - 0.2)).abs() < 1e-6);
    }

    /// #641：2D 视图锁视角 —— 进入时归零，之后轨道/飞行观察都不再改变朝向。
    #[test]
    fn the_2d_view_locks_the_camera_to_the_xy_plane() {
        let mut camera = OrbitCamera::default();
        camera.orbit(30.0, 20.0);
        assert!(camera.yaw.abs() > 1e-6 || camera.pitch.abs() > 1e-6);

        camera.set_2d(true);
        assert!(camera.orthographic);
        assert_eq!((camera.yaw, camera.pitch), (0.0, 0.0));

        camera.orbit(50.0, 50.0);
        camera.look_in_place(50.0, 50.0);
        assert_eq!((camera.yaw, camera.pitch), (0.0, 0.0), "2D 视图不该被旋转");
        // 正视：eye 在 target 的 +Z 上，看 −Z，+X 右 / +Y 上。
        let eye = camera.eye();
        assert!((eye[2] - camera.distance).abs() < 1e-5);
        assert!(camera.ortho_half_height().unwrap() > 0.0);

        camera.toggle_2d();
        assert!(!camera.orthographic);
        assert!(camera.ortho_half_height().is_none());
        camera.orbit(50.0, 0.0);
        assert!(camera.yaw.abs() > 1e-6, "退出 2D 后轨道恢复");
    }

    /// #511：滚轮缩放到光标 —— 锚点（光标射线上的世界点）缩放前后屏幕位置不变。
    fn anchor_point(camera: &OrbitCamera, x: f32, y: f32, width: f32, height: f32) -> [f32; 3] {
        let frame = crate::viewport_math::camera_frame(camera);
        let (origin, direction) =
            crate::viewport_math::ray_from_screen(x, y, &frame, width, height);
        let distance = camera.distance;
        [
            origin[0] + direction[0] * distance,
            origin[1] + direction[1] * distance,
            origin[2] + direction[2] * distance,
        ]
    }

    fn screen_of(camera: &OrbitCamera, point: [f32; 3], width: f32, height: f32) -> (f32, f32) {
        let frame = crate::viewport_math::camera_frame(camera);
        let (x, y, _depth) =
            crate::viewport_math::project(point, &frame, width, height).expect("on screen");
        (x, y)
    }

    #[test]
    fn zooming_towards_the_cursor_keeps_the_anchor_on_screen() {
        let mut camera = OrbitCamera {
            target: [1.0, 0.5, -2.0],
            yaw: 0.4,
            pitch: 0.3,
            distance: 14.0,
            ..OrbitCamera::default()
        };
        let before = camera;
        let anchor = anchor_point(&camera, 640.0, 180.0, 800.0, 600.0);
        let (x0, y0) = screen_of(&camera, anchor, 800.0, 600.0);

        camera.zoom_towards(240.0, Some(anchor));

        assert!(camera.distance < before.distance, "wheel forward zooms in");
        // pivot 朝锚点移动了 1 - new/old 的比例。
        let percentage = 1.0 - camera.distance / before.distance;
        for axis in 0..3 {
            let expected = before.target[axis] + (anchor[axis] - before.target[axis]) * percentage;
            assert!((camera.target[axis] - expected).abs() < 1e-4);
        }

        let (x1, y1) = screen_of(&camera, anchor, 800.0, 600.0);
        assert!(
            (x1 - x0).abs() < 0.05 && (y1 - y0).abs() < 0.05,
            "anchor moved on screen: ({x0}, {y0}) -> ({x1}, {y1})"
        );
    }

    #[test]
    fn zooming_towards_the_cursor_works_in_the_2d_view() {
        let mut camera = OrbitCamera::default();
        camera.set_2d(true);
        let before = camera;
        let anchor = anchor_point(&camera, 720.0, 120.0, 800.0, 600.0);
        let (x0, y0) = screen_of(&camera, anchor, 800.0, 600.0);

        camera.zoom_towards(300.0, Some(anchor));

        assert!(camera.distance < before.distance);
        let (x1, y1) = screen_of(&camera, anchor, 800.0, 600.0);
        assert!(
            (x1 - x0).abs() < 0.05 && (y1 - y0).abs() < 0.05,
            "2D anchor moved on screen: ({x0}, {y0}) -> ({x1}, {y1})"
        );
    }

    #[test]
    fn zooming_without_a_cursor_keeps_the_target() {
        let mut camera = OrbitCamera::default();
        let before = camera;
        camera.zoom_towards(200.0, None);
        assert_eq!(camera.target, before.target);
        assert!(camera.distance < before.distance);

        // 非有限锚点（射线退化）同样退回画面中心缩放。
        let mut camera = OrbitCamera::default();
        camera.zoom_towards(200.0, Some([f32::NAN, 0.0, 0.0]));
        assert_eq!(camera.target, before.target);
    }

    #[test]
    fn zooming_towards_the_cursor_respects_the_distance_limits() {
        let mut camera = OrbitCamera {
            distance: 500.0,
            ..OrbitCamera::default()
        };
        // 继续拉远也只会停在 500，此时 percentage == 0，target 不动。
        camera.zoom_towards(-500.0, Some([10.0, 10.0, 0.0]));
        assert_eq!(camera.distance, 500.0);
        assert_eq!(camera.target, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn focus_fits_the_radius_in_the_ortho_size() {
        let mut camera = OrbitCamera::default();
        camera.set_2d(true);
        camera.focus([1.0, 2.0, 0.0], 5.0);
        assert_eq!(camera.target, [1.0, 2.0, 0.0]);
        assert!((camera.distance - 6.0).abs() < 1e-4);
    }

    #[test]
    fn orbit_keeps_pitch_inside_pole_limits() {
        let mut camera = OrbitCamera::default();
        camera.orbit(0.0, 1000.0);
        assert_eq!(camera.pitch, 1.5);
        camera.orbit(0.0, -1000.0);
        assert_eq!(camera.pitch, -1.5);
    }

    #[test]
    fn look_in_place_rotates_view_without_moving_eye() {
        let mut camera = OrbitCamera::default();
        let eye = camera.eye();
        camera.look_in_place(10.0, -5.0);
        let next_eye = camera.eye();
        for axis in 0..3 {
            assert!((next_eye[axis] - eye[axis]).abs() < 1e-5);
        }
        assert!((camera.yaw + 0.1).abs() < 1e-6);
        assert!((camera.pitch + 0.05).abs() < 1e-6);
    }

    #[test]
    fn fly_move_translates_eye_and_target_together() {
        let mut camera = OrbitCamera::default();
        let eye = camera.eye();
        let target = camera.target;
        camera.fly_move(1.0, 0.0, 0.0, 1.0);
        let next_eye = camera.eye();
        let delta = [
            camera.target[0] - target[0],
            camera.target[1] - target[1],
            camera.target[2] - target[2],
        ];
        assert!(delta[2] < 0.0);
        for axis in 0..3 {
            assert!((next_eye[axis] - eye[axis] - delta[axis]).abs() < 1e-5);
        }
    }

    #[test]
    fn reset_scene_camera_restores_default_orbit() {
        let mut workspace = Workspace::sample();
        workspace.camera.target = [9.0, -4.0, 2.0];
        workspace.camera.yaw = -1.2;
        workspace.camera.pitch = 1.1;
        workspace.camera.distance = 42.0;
        workspace.run(Command::ResetSceneCamera);
        assert_eq!(workspace.camera, OrbitCamera::default());
        assert_eq!(workspace.camera.yaw, 0.0);
        assert_eq!(workspace.camera.pitch, 0.0);
    }

    fn fixture() -> Value {
        json!({
            "schema": { "types": [
                {
                    "name": "prism_engine::scene::Camera",
                    "typeId": "c",
                    "version": 1,
                    "isComponent": true,
                    "defaultJson": { "fov_y_degrees": 60.0 },
                    "fields": [
                        { "name": "fov_y_degrees", "kind": "f32", "structType": null, "inner": null,
                          "arrayLen": null, "variants": null, "readOnly": false }
                    ]
                }
            ]},
            "presets": [
                { "id": "camera", "label": "相机", "name": "Camera",
                  "components": ["prism_engine::scene::Camera"],
                  "overrides": {}, "position": [0.0, 0.0, 10.0], "rotation": [0.0, 0.0, 0.0, 1.0] },
                { "id": "empty", "label": "空实体", "name": "entity",
                  "overrides": {}, "position": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0] }
            ],
            "assetInfos": [
                { "path": "assets/scenes/a.scene.json", "sizeBytes": 10, "kind": "scene", "importer": "TextScriptImporter" }
            ],
            "docs": {
                "assets/scenes/a.scene.json": {
                    "version": 3,
                    "entities": [
                        { "id": 1, "name": "Camera", "parent": null,
                          "transform": {"position":[0,1,-10],"rotation":[0,0,0,1],"scale":[1,1,1]},
                          "components": { "prism_engine::scene::Camera": { "fov_y_degrees": 60.0 } } },
                        { "id": 2, "name": "Cube", "parent": 1,
                          "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                          "components": {} }
                    ]
                }
            },
            "templateDocs": {
                "prefab": { "camera": { "version": 3, "entities": [
                    { "id": 1, "name": "Camera", "parent": null,
                      "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                      "components": {} }
                ]}}
            },
            "templatesByKind": { "prefab": [ { "id": "camera", "label": "相机" } ] },
            "windows": [
                { "id": "sceneView", "title": "场景视图", "menuPath": "窗口/场景视图", "shortcut": null }
            ]
        })
    }

    #[test]
    fn loads_fixture_documents_templates_and_panels() {
        let workspace = Workspace::load_fixture(&fixture());
        assert_eq!(workspace.docs.len(), 1);
        assert_eq!(workspace.docs[0].title(), "a.scene.json");
        assert_eq!(workspace.docs[0].doc.entities.len(), 2);
        assert_eq!(workspace.templates.len(), 1);
        assert_eq!(workspace.presets.len(), 2);
        assert_eq!(workspace.assets.len(), 1);
        assert!(workspace.panel("sceneView").is_some());
        assert_eq!(workspace.panel_title("sceneView"), "场景视图");
    }

    #[test]
    fn add_entity_uses_preset_and_parent() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select(0, false);
        workspace.add_entity("camera");
        let doc = &workspace.document().doc;
        assert_eq!(doc.entities.len(), 3);
        let added = doc.entities.last().unwrap();
        assert_eq!(added.display_name(), "Camera");
        assert_eq!(added.parent, Some(1), "挂在选中的相机下面");
        assert!(added.has_component("prism_engine::scene::Camera"));
        assert!(workspace.can_undo());
    }

    #[test]
    fn add_entity_names_stay_unique_among_siblings() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.clear_selection();
        workspace.add_entity("");
        workspace.add_entity("");
        let doc = &workspace.document().doc;
        let names: Vec<String> = doc
            .entities
            .iter()
            .map(|entity| entity.display_name())
            .collect();
        assert!(names.contains(&"GameObject".to_string()));
        assert!(names.contains(&"GameObject (1)".to_string()));
    }

    #[test]
    fn multi_field_edit_writes_all_selected_in_one_step() {
        let mut workspace = Workspace::load_fixture(&fixture());
        // 给两个实体都加 Camera 组件。
        workspace.select(0, false);
        workspace.add_component("prism_engine::scene::Camera");
        workspace.select(1, false);
        workspace.add_component("prism_engine::scene::Camera");

        let ids: Vec<u32> = workspace
            .document()
            .doc
            .entities
            .iter()
            .filter_map(|entity| entity.id)
            .collect();
        let applied = workspace.set_field_multi(
            &ids,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(55.0),
        );
        assert_eq!(applied, 2);
        for entity in &workspace.document().doc.entities {
            assert_eq!(
                entity.components["prism_engine::scene::Camera"]["fov_y_degrees"],
                json!(55.0)
            );
        }
        // 一次撤销回退全部。
        workspace.undo();
        for entity in &workspace.document().doc.entities {
            assert_eq!(
                entity.components["prism_engine::scene::Camera"]["fov_y_degrees"],
                json!(60.0)
            );
        }

        let common = workspace.common_components(&[0, 1]);
        assert!(common
            .iter()
            .any(|component| component.name == "prism_engine::scene::Camera"));
        workspace.remove_component_multi(&[0, 1], "prism_engine::scene::Camera");
        for entity in &workspace.document().doc.entities {
            assert!(!entity.has_component("prism_engine::scene::Camera") || entity.id != Some(1));
        }
    }

    #[test]
    fn rotate_and_scale_gizmo_operations_are_undoable() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let id = workspace.document().doc.entities[0].id.unwrap();
        workspace.rotate_entity(id, 1, std::f32::consts::FRAC_PI_2);
        let rotation = workspace.document().doc.entities[0].transform.rotation;
        // 绕 Y 90°：w = cos45。
        assert!((rotation[3] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4);
        assert!(workspace.can_undo());
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].transform.rotation,
            [0.0, 0.0, 0.0, 1.0]
        );

        workspace.set_scale_axis(id, 0, 2.5);
        assert_eq!(workspace.document().doc.entities[0].transform.scale[0], 2.5);
        workspace.set_scale_axis(id, 0, -5.0);
        assert_eq!(
            workspace.document().doc.entities[0].transform.scale[0],
            0.01,
            "负数被夹到最小值"
        );
        // 无效 delta 不产生撤销步。
        let history = workspace.document().undo.history().len();
        workspace.rotate_entity(id, 0, 0.0);
        assert_eq!(workspace.document().undo.history().len(), history);
    }

    #[test]
    fn local_gizmo_rotation_uses_entity_axis() {
        use crate::viewport_math::{basis_from_quat, quat_axis_angle, world_rotation};

        let mut workspace = Workspace::sample();
        let index = workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == "Cube")
            .expect("sample 里有 Cube");
        let id = workspace.document().doc.entities[index].id.unwrap();
        let base_rotation = quat_axis_angle(1, std::f32::consts::FRAC_PI_2);

        workspace.document_mut().doc.entities[index]
            .transform
            .rotation = base_rotation;
        workspace.gizmo_space = GizmoSpace::World;
        workspace.rotate_entity(id, 0, std::f32::consts::FRAC_PI_2);
        let world_x = basis_from_quat(world_rotation(&workspace.document().doc, index))[0];
        assert!(
            (world_x[1] - 1.0).abs() < 1e-3,
            "世界轴绕 X 旋后 X 轴应指向 +Y，实际 {world_x:?}"
        );

        workspace.document_mut().doc.entities[index]
            .transform
            .rotation = base_rotation;
        workspace.gizmo_space = GizmoSpace::Local;
        workspace.rotate_entity(id, 0, std::f32::consts::FRAC_PI_2);
        let local_x = basis_from_quat(world_rotation(&workspace.document().doc, index))[0];
        assert!(
            (local_x[2] + 1.0).abs() < 1e-3,
            "局部轴绕 X 旋后 X 轴应指向 −Z，实际 {local_x:?}"
        );
    }

    #[test]
    fn gizmo_basis_follows_parent_rotation_and_space() {
        let mut workspace = Workspace::sample();
        let cube = workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == "Cube")
            .expect("sample 里有 Cube");
        let detail = workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == "Detail")
            .expect("sample 里有 Detail");
        workspace.document_mut().doc.entities[cube]
            .transform
            .rotation = crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        workspace.select(detail, false);

        assert_eq!(
            workspace.gizmo_basis(),
            crate::viewport_math::IDENTITY_BASIS,
            "默认是世界空间"
        );
        workspace.toggle_gizmo_space();
        let basis = workspace.gizmo_basis();
        assert!(
            (basis[0][2] + 1.0).abs() < 1e-3,
            "父级绕 Y 90° 后子级局部 X 指向 −Z，实际 {:?}",
            basis[0]
        );
    }

    #[test]
    fn gizmo_scale_axis_distinguishes_world_and_local_space() {
        let mut workspace = Workspace::sample();
        let index = index_named(&workspace, "Cube");
        let id = workspace.document().doc.entities[index].id.unwrap();
        workspace.document_mut().doc.entities[index]
            .transform
            .rotation = crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2);

        workspace.gizmo_space = GizmoSpace::Local;
        assert_eq!(workspace.gizmo_scale_axis(id, 0), 0);

        workspace.gizmo_space = GizmoSpace::World;
        assert_eq!(workspace.gizmo_scale_axis(id, 0), 2);
    }

    #[test]
    fn translate_entity_world_converts_into_parent_space() {
        let mut workspace = Workspace::sample();
        let cube = workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == "Cube")
            .expect("sample 里有 Cube");
        let detail = workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == "Detail")
            .expect("sample 里有 Detail");
        workspace.document_mut().doc.entities[cube]
            .transform
            .rotation = crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        let id = workspace.document().doc.entities[detail].id.unwrap();
        let delta = [0.25, 0.5, -0.75];

        workspace.translate_entity_world(id, delta);

        let parent_rotation = crate::viewport_math::world_rotation(&workspace.document().doc, cube);
        let position = workspace.document().doc.entities[detail].transform.position;
        let world_offset = crate::viewport_math::quat_rotate(parent_rotation, position);
        for axis in 0..3 {
            assert!(
                (world_offset[axis] - delta[axis]).abs() < 1e-3,
                "世界位移应等于 {delta:?}，实际 {world_offset:?}"
            );
        }
        assert!(workspace.can_undo());
    }

    #[test]
    fn translate_entity_world_inverts_parent_scale_and_rotation() {
        let mut workspace = Workspace::sample();
        let parent = index_named(&workspace, "Cube");
        let child = index_named(&workspace, "Detail");
        workspace.document_mut().doc.entities[parent]
            .transform
            .scale = [2.0, 3.0, 4.0];
        workspace.document_mut().doc.entities[parent]
            .transform
            .rotation = crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        let child_id = workspace.document().doc.entities[child].id.unwrap();
        let before = crate::viewport_math::world_position(&workspace.document().doc, child);
        let delta = [0.8, -1.2, 2.0];

        workspace.translate_entity_world(child_id, delta);

        let after = crate::viewport_math::world_position(&workspace.document().doc, child);
        for axis in 0..3 {
            assert!((after[axis] - before[axis] - delta[axis]).abs() < 1e-3);
        }
    }

    #[test]
    fn multi_select_translate_skips_descendants_of_selected_parents() {
        // 回归 #404：父级 + 子级同时选中时，子级只能跟着父级走一份 delta。
        let mut workspace = Workspace::sample();
        let parent = index_named(&workspace, "Cube");
        let child = index_named(&workspace, "Detail");
        let parent_id = workspace.document().doc.entities[parent].id.unwrap();
        let child_id = workspace.document().doc.entities[child].id.unwrap();
        workspace.document_mut().doc.entities[child]
            .transform
            .position = [0.0, 0.0, 0.0];
        let child_before = world_position(&workspace.document().doc, child);

        workspace.translate_entities_world(&[parent_id, child_id], [1.0, 0.0, 0.0]);

        let child_after = world_position(&workspace.document().doc, child);
        let moved = child_after[0] - child_before[0];
        assert!(
            (moved - 1.0).abs() < 1e-4,
            "子级应只跟随父级移动 1 个单位，实际 {moved}"
        );
        assert_eq!(
            workspace.document().doc.entities[child].transform.position,
            [0.0, 0.0, 0.0],
            "子级的局部位置不该被单独改动"
        );
    }

    #[test]
    fn multi_select_scale_skips_descendants_of_selected_parents() {
        let mut workspace = Workspace::sample();
        let parent = index_named(&workspace, "Cube");
        let child = index_named(&workspace, "Detail");
        let parent_id = workspace.document().doc.entities[parent].id.unwrap();
        let child_id = workspace.document().doc.entities[child].id.unwrap();
        let parent_start = workspace.document().doc.entities[parent].transform.scale[0];
        let child_start = workspace.document().doc.entities[child].transform.scale[0];

        workspace.scale_entities_axis(
            &[(parent_id, parent_start), (child_id, child_start)],
            0,
            2.0,
        );

        let parent_after = workspace.document().doc.entities[parent].transform.scale[0];
        let child_after = workspace.document().doc.entities[child].transform.scale[0];
        assert!((parent_after - parent_start * 2.0).abs() < 1e-4);
        assert!(
            (child_after - child_start).abs() < 1e-4,
            "子级自己的缩放不该再乘一次，实际 {child_after}，起始 {child_start}"
        );
    }

    #[test]
    fn world_rotation_is_converted_through_parent_rotation() {
        let mut workspace = Workspace::sample();
        let parent = index_named(&workspace, "Cube");
        let child = index_named(&workspace, "Detail");
        workspace.document_mut().doc.entities[parent]
            .transform
            .rotation = crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        let child_id = workspace.document().doc.entities[child].id.unwrap();
        let before = crate::viewport_math::world_rotation(&workspace.document().doc, child);
        let delta = crate::viewport_math::quat_axis_angle(0, std::f32::consts::FRAC_PI_2);

        workspace.rotate_entity(child_id, 0, std::f32::consts::FRAC_PI_2);

        let after = crate::viewport_math::world_rotation(&workspace.document().doc, child);
        let expected = crate::viewport_math::quat_mul(delta, before);
        let expected_x = crate::viewport_math::quat_rotate(expected, [1.0, 0.0, 0.0]);
        let actual_x = crate::viewport_math::quat_rotate(after, [1.0, 0.0, 0.0]);
        for axis in 0..3 {
            assert!((actual_x[axis] - expected_x[axis]).abs() < 1e-3);
        }
    }

    #[test]
    fn gizmo_center_switches_between_the_group_and_the_active_pivot() {
        // #416：Center = 选择集中心；Pivot = 活动对象自己的轴心。
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        workspace.selection = vec![cube, light];
        workspace.selection_active = Some(light);

        let center = workspace.selection_center_world().unwrap();
        let pivot = world_position(&workspace.document().doc, light);
        assert_eq!(workspace.gizmo_center_world(), Some(center));

        workspace.pivot_mode = PivotMode::Pivot;
        assert_eq!(workspace.gizmo_center_world(), Some(pivot));

        // 没有显式活动对象时退回「最后选中」的那个，gizmo 不会消失。
        workspace.selection_active = None;
        assert_eq!(workspace.gizmo_center_world(), Some(pivot));
        workspace.selection.clear();
        assert_eq!(workspace.gizmo_center_world(), None);
    }

    #[test]
    fn pivot_mode_rotates_in_place_while_center_mode_orbits() {
        // #416：Pivot 模式只转自身，位置不动；Center 模式绕中心公转。
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let ids = [cube_id, light_id];
        let before = [
            world_position(&workspace.document().doc, cube),
            world_position(&workspace.document().doc, light),
        ];

        workspace.pivot_mode = PivotMode::Pivot;
        workspace.rotate_entities(&ids, 1, std::f32::consts::FRAC_PI_2);
        for (index, expected) in [(cube, before[0]), (light, before[1])] {
            let actual = world_position(&workspace.document().doc, index);
            for axis in 0..3 {
                assert!(
                    (actual[axis] - expected[axis]).abs() < 1e-4,
                    "Pivot 模式不应移动实体: {actual:?} != {expected:?}"
                );
            }
        }
        assert_ne!(
            workspace.document().doc.entities[cube].transform.rotation,
            [0.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn center_mode_scale_moves_objects_away_from_the_centre() {
        // #416：Center 模式沿 gizmo 轴把位置也按比例推开；Pivot 模式只改缩放。
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        workspace.set_transform_vector(cube_id, "position", [-1.0, 0.0, 0.0]);
        workspace.set_transform_vector(light_id, "position", [1.0, 0.0, 0.0]);
        let targets = [(cube_id, 0usize, 1.0f32), (light_id, 0, 1.0)];

        workspace.scale_entities_gizmo(&targets, 0, 2.0);
        assert!((world_position(&workspace.document().doc, cube)[0] + 2.0).abs() < 1e-4);
        assert!((world_position(&workspace.document().doc, light)[0] - 2.0).abs() < 1e-4);
        assert!((workspace.document().doc.entities[cube].transform.scale[0] - 2.0).abs() < 1e-4);
        assert_eq!(
            workspace.document().undo.history().len(),
            3,
            "两次 set + 一次群组缩放"
        );

        // 撤销回原位，再用 Pivot 模式：位置不动，只放大。
        workspace.undo();
        workspace.pivot_mode = PivotMode::Pivot;
        workspace.scale_entities_gizmo(&targets, 0, 2.0);
        assert!((world_position(&workspace.document().doc, cube)[0] + 1.0).abs() < 1e-4);
        assert!((world_position(&workspace.document().doc, light)[0] - 1.0).abs() < 1e-4);
        assert!((workspace.document().doc.entities[light].transform.scale[0] - 2.0).abs() < 1e-4);
    }

    #[test]
    fn batch_world_rotation_moves_entities_around_selection_center() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let cube_before = crate::viewport_math::world_position(&workspace.document().doc, cube);
        let light_before = crate::viewport_math::world_position(&workspace.document().doc, light);
        let center = [
            (cube_before[0] + light_before[0]) * 0.5,
            (cube_before[1] + light_before[1]) * 0.5,
            (cube_before[2] + light_before[2]) * 0.5,
        ];

        workspace.rotate_entities(&[cube_id, light_id], 1, std::f32::consts::FRAC_PI_2);

        for (index, before) in [(cube, cube_before), (light, light_before)] {
            let offset = [
                before[0] - center[0],
                before[1] - center[1],
                before[2] - center[2],
            ];
            let rotated = crate::viewport_math::quat_rotate(
                crate::viewport_math::quat_axis_angle(1, std::f32::consts::FRAC_PI_2),
                offset,
            );
            let expected = [
                center[0] + rotated[0],
                center[1] + rotated[1],
                center[2] + rotated[2],
            ];
            let actual = crate::viewport_math::world_position(&workspace.document().doc, index);
            for axis in 0..3 {
                assert!((actual[axis] - expected[axis]).abs() < 1e-3);
            }
        }
        assert_eq!(workspace.document().undo.history().len(), 1);
    }

    #[test]
    fn batch_translate_moves_all_entities_in_one_undo_step() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let cube_before = workspace.document().doc.entities[cube].transform.position;
        let light_before = workspace.document().doc.entities[light].transform.position;

        workspace.translate_entities_world(&[cube_id, light_id], [1.0, 0.0, 0.0]);
        workspace.translate_entities_world(&[cube_id, light_id], [1.0, 0.0, 0.0]);

        assert!(
            (workspace.document().doc.entities[cube].transform.position[0] - cube_before[0] - 2.0)
                .abs()
                < 1e-4
        );
        assert!(
            (workspace.document().doc.entities[light].transform.position[0]
                - light_before[0]
                - 2.0)
                .abs()
                < 1e-4
        );
        assert_eq!(workspace.document().undo.history().len(), 1);

        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[cube].transform.position,
            cube_before
        );
        assert_eq!(
            workspace.document().doc.entities[light].transform.position,
            light_before
        );
        assert!(!workspace.can_undo());
    }

    #[test]
    fn batch_transform_axis_sets_all_and_one_undo_step() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let cube_before = workspace.document().doc.entities[cube].transform.position[1];
        let light_before = workspace.document().doc.entities[light].transform.position[1];

        workspace.set_transform_axis_many(&[cube_id, light_id], "position", 1, 2.5);
        workspace.set_transform_axis_many(&[cube_id, light_id], "position", 1, 3.0);

        assert_eq!(
            workspace.document().doc.entities[cube].transform.position[1],
            3.0
        );
        assert_eq!(
            workspace.document().doc.entities[light].transform.position[1],
            3.0
        );
        assert_eq!(workspace.document().undo.history().len(), 1);

        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[cube].transform.position[1],
            cube_before
        );
        assert_eq!(
            workspace.document().doc.entities[light].transform.position[1],
            light_before
        );
    }

    #[test]
    fn batch_rotate_turns_every_selection_in_one_undo_step() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let cube_before = workspace.document().doc.entities[cube].transform.rotation;
        let light_before = workspace.document().doc.entities[light].transform.rotation;

        workspace.rotate_entities(&[cube_id, light_id], 1, 0.5);
        workspace.rotate_entities(&[cube_id, light_id], 1, 0.5);

        let cube_after = workspace.document().doc.entities[cube].transform.rotation;
        let light_after = workspace.document().doc.entities[light].transform.rotation;
        assert_ne!(cube_after, cube_before);
        assert_ne!(light_after, light_before);
        assert_eq!(workspace.document().undo.history().len(), 1);

        workspace.undo();
        let cube_restored = workspace.document().doc.entities[cube].transform.rotation;
        let light_restored = workspace.document().doc.entities[light].transform.rotation;
        for (restored, before) in [(cube_restored, cube_before), (light_restored, light_before)] {
            for (a, b) in restored.iter().zip(before) {
                assert!((a - b).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn history_entries_keep_undone_steps_and_jump_both_ways() {
        let mut workspace = Workspace::sample();
        let initial = workspace.document().doc.entities.len();
        workspace.add_entity("camera");
        workspace.add_entity("camera");
        workspace.add_entity("camera");
        assert_eq!(workspace.history_cursor(), 3);
        let entries = workspace.history_entries();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|(_, undone)| !undone));

        // 撤销两步：位置回到 1，但两步仍然留在列表里，并且被标记为已撤销。
        assert_eq!(workspace.jump_history(1), 2);
        assert_eq!(workspace.history_cursor(), 1);
        assert_eq!(workspace.document().doc.entities.len(), initial + 1);
        let entries = workspace.history_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().filter(|(_, undone)| *undone).count(),
            2,
            "撤销过的步骤不能从历史里消失"
        );

        // 点回去（redo）恢复。
        assert_eq!(workspace.jump_history(3), 2);
        assert_eq!(workspace.history_cursor(), 3);
        assert_eq!(workspace.document().doc.entities.len(), initial + 3);
        assert!(workspace
            .history_entries()
            .iter()
            .all(|(_, undone)| !undone));

        // 跳回开头。
        assert_eq!(workspace.jump_history(0), 3);
        assert_eq!(workspace.history_cursor(), 0);
        assert_eq!(workspace.document().doc.entities.len(), initial);
        assert_eq!(workspace.jump_history(0), 0, "原地跳转是空操作");
    }

    #[test]
    fn undo_history_can_jump_back_and_clear() {
        let mut workspace = Workspace::sample();
        let initial = workspace.document().doc.entities.len();
        workspace.add_entity("camera");
        workspace.add_entity("camera");
        workspace.add_entity("camera");
        assert_eq!(workspace.undo_history().len(), 3);
        assert_eq!(
            workspace.undo_history().first().map(String::as_str),
            Some("新建实体")
        );

        assert_eq!(workspace.undo_to(1), 2, "回退第 2 条之前的操作");
        assert_eq!(workspace.undo_history().len(), 1);
        assert_eq!(workspace.document().doc.entities.len(), initial + 1);

        assert_eq!(workspace.undo_to(0), 1);
        assert!(workspace.undo_history().is_empty());
        assert!(!workspace.can_undo());
        assert_eq!(workspace.undo_to(3), 0, "没有历史时是空操作");

        workspace.add_entity("camera");
        assert!(workspace.can_undo());
        workspace.clear_history();
        assert!(!workspace.can_undo());
        assert!(!workspace.can_redo());
        assert!(workspace.undo_history().is_empty());
    }

    #[test]
    fn batch_scale_uses_each_start_value_and_one_undo_step() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();
        let cube_before = workspace.document().doc.entities[cube].transform.scale;
        let light_before = workspace.document().doc.entities[light].transform.scale;

        let targets = [(cube_id, cube_before[0]), (light_id, light_before[0])];
        workspace.scale_entities_axis(&targets, 0, 2.0);
        workspace.scale_entities_axis(&targets, 0, 3.0);

        assert!(
            (workspace.document().doc.entities[cube].transform.scale[0] - cube_before[0] * 3.0)
                .abs()
                < 1e-4
        );
        assert!(
            (workspace.document().doc.entities[light].transform.scale[0] - light_before[0] * 3.0)
                .abs()
                < 1e-4
        );
        assert_eq!(workspace.document().undo.history().len(), 1);

        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[cube].transform.scale,
            cube_before
        );
        assert_eq!(
            workspace.document().doc.entities[light].transform.scale,
            light_before
        );
    }

    #[test]
    fn preview_rename_creates_one_undo_step_on_finish() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let base = workspace.document().doc.clone();
        workspace.preview_rename(0, "C");
        workspace.preview_rename(0, "Ca");
        workspace.preview_rename(0, "Camera2");
        assert_eq!(
            workspace.document().doc.entities[0].display_name(),
            "Camera2"
        );
        assert!(!workspace.can_undo(), "预览阶段不产生撤销步");

        workspace.finish_rename(base, false);
        assert_eq!(workspace.document().undo.history().len(), 1);
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].display_name(),
            "Camera"
        );
    }

    /// #473：预览不产生撤销步；提交后恰好一步，且撤销能回到原名。
    #[test]
    fn finish_rename_pushes_exactly_one_undo_step() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let base = workspace.document().doc.clone();
        let was_dirty = workspace.document().dirty;
        assert!(!was_dirty);

        workspace.preview_rename(0, "F");
        workspace.preview_rename(0, "Floor");
        assert!(workspace.document().dirty, "预览阶段就要有未保存标记");
        assert!(!workspace.can_undo(), "预览阶段不产生撤销步");

        workspace.finish_rename(base, was_dirty);
        assert_eq!(workspace.document().undo.history().len(), 1);
        assert!(workspace.document().dirty);
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].display_name(),
            "Camera"
        );
    }

    /// 输入后又删回原样：既不该留下撤销步，也不该把文档标脏。
    #[test]
    fn finishing_an_unchanged_rename_leaves_the_document_clean() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let base = workspace.document().doc.clone();

        workspace.preview_rename(0, "Floor");
        workspace.preview_rename(0, "Camera");
        workspace.finish_rename(base, false);

        assert!(!workspace.document().dirty);
        assert!(!workspace.can_undo());
    }

    /// Esc：预览过的名字要还原，脏标记也回到编辑前的状态。
    #[test]
    fn cancel_rename_restores_the_previous_name_and_dirty_flag() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let base = workspace.document().doc.clone();

        workspace.preview_rename(0, "Floor");
        assert!(workspace.document().dirty);
        workspace.cancel_rename(base, false);

        assert_eq!(
            workspace.document().doc.entities[0].display_name(),
            "Camera"
        );
        assert!(!workspace.document().dirty);
        assert!(!workspace.can_undo());
    }

    #[test]
    fn duplicate_selection_copies_subtree() {
        let mut workspace = Workspace::load_fixture(&fixture());
        // fixture: Camera(1) → Cube(2)。
        workspace.select(0, false);
        workspace.duplicate_selection();
        assert_eq!(workspace.document().doc.entities.len(), 4);
        let root = workspace.selection[0];
        assert_eq!(
            workspace.document().doc.entities[root].display_name(),
            "Camera (1)"
        );
        let root_id = workspace.document().doc.entities[root].id.unwrap();
        assert!(workspace
            .document()
            .doc
            .entities
            .iter()
            .any(|entity| entity.parent == Some(root_id) && entity.display_name() == "Cube"));
        workspace.undo();
        assert_eq!(workspace.document().doc.entities.len(), 2);
    }

    #[test]
    fn selection_range_and_toggle_follow_anchor() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_range(1, 0);
        assert_eq!(workspace.selection, vec![0, 1], "反向区间也规范化");
        workspace.select(0, true);
        assert_eq!(workspace.selection, vec![1], "已有项被切换掉");
        workspace.select(0, true);
        assert_eq!(workspace.selection, vec![0, 1]);
    }

    #[test]
    fn visible_range_selection_skips_rows_hidden_by_filters() {
        let mut workspace = Workspace::sample();
        // 可见行顺序与实体下标顺序不一致：中间那行被折叠 / 搜索过滤掉了。
        let ordered = [0usize, 2, 3];
        workspace.select_visible_range(&ordered, 0, 3);
        assert_eq!(workspace.selection, vec![0, 2, 3]);
        assert_eq!(workspace.selection_active, Some(3));

        workspace.select_visible_range(&ordered, 3, 0);
        assert_eq!(workspace.selection, vec![0, 2, 3], "反向选择同样规范化");

        // 锚点不在可见列表里：退化成普通单击，不选整段。
        workspace.select_visible_range(&ordered, 1, 2);
        assert_eq!(workspace.selection, vec![2]);
        assert_eq!(workspace.selection_active, Some(2));
    }

    #[test]
    fn select_many_overwrites_or_extends() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_many(&[1, 0], false);
        assert_eq!(workspace.selection, vec![0, 1], "排序且覆盖旧选择");
        workspace.select_many(&[1, 2], true);
        assert_eq!(workspace.selection, vec![0, 1, 2], "附加选择去重");
        workspace.select_many(&[], false);
        assert!(workspace.selection.is_empty());
    }

    #[test]
    fn the_last_clicked_entity_stays_active_in_a_multi_selection() {
        // #507：选择集按文档顺序保存，但检查器/Add Component 用最后点击的那个。
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_with(0, SelectMode::Replace);
        workspace.select_with(2, SelectMode::Add);
        workspace.select_with(1, SelectMode::Add);
        assert_eq!(workspace.selection, vec![0, 1, 2], "集合仍按文档顺序");
        assert_eq!(
            workspace.primary_selection(),
            Some(1),
            "活动对象是最后点击的实体"
        );

        workspace.select_with(1, SelectMode::Remove);
        assert_eq!(
            workspace.primary_selection(),
            Some(2),
            "移除活动对象后回退到选择集末尾"
        );
    }

    #[test]
    fn right_clicking_a_selected_entity_makes_it_active_without_changing_the_set() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_many(&[0, 1, 2], false);
        assert_eq!(workspace.primary_selection(), Some(2));
        assert!(!workspace.select_for_context(0));
        assert_eq!(workspace.selection, vec![0, 1, 2]);
        assert_eq!(workspace.primary_selection(), Some(0));
    }

    #[test]
    fn shift_adds_and_ctrl_removes_in_the_viewport() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_with(0, SelectMode::Replace);
        workspace.select_with(1, SelectMode::Add);
        assert_eq!(workspace.selection, vec![0, 1], "Shift 并入且保留原选择");
        workspace.select_with(1, SelectMode::Add);
        assert_eq!(workspace.selection, vec![0, 1], "重复并入不切换掉已有项");
        workspace.select_with(0, SelectMode::Remove);
        assert_eq!(workspace.selection, vec![1], "Ctrl 只移除，不影响其它项");
        workspace.select_with(0, SelectMode::Remove);
        assert_eq!(workspace.selection, vec![1], "移除未选中项是空操作");
        workspace.select_with(2, SelectMode::Replace);
        assert_eq!(workspace.selection, vec![2], "普通点击替换");
    }

    /// #524：右键未选中的行先把它选成唯一目标，菜单动作与检查器才一致。
    #[test]
    fn right_clicking_an_unselected_entity_selects_only_it() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_with(0, SelectMode::Replace);
        assert!(workspace.select_for_context(1), "未选中 → 换选择");
        assert_eq!(workspace.selection, vec![1]);
        assert_eq!(workspace.primary_selection(), Some(1));
    }

    /// 右键多选里的某项：保留整个集合，右键项就是菜单目标。
    #[test]
    fn right_clicking_a_selected_entity_keeps_the_whole_selection() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_many(&[0, 1, 2], false);
        assert!(!workspace.select_for_context(1), "已选中 → 不动选择");
        assert_eq!(workspace.selection, vec![0, 1, 2]);
    }

    #[test]
    fn right_clicking_a_stale_index_leaves_the_selection_alone() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_with(0, SelectMode::Replace);
        assert!(!workspace.select_for_context(999));
        assert_eq!(workspace.selection, vec![0]);
    }

    #[test]
    fn clicking_empty_space_only_clears_on_replace() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_many(&[0, 1], false);
        workspace.clear_selection_in(SelectMode::Add);
        assert_eq!(workspace.selection, vec![0, 1], "Shift+点空白保留选择");
        workspace.clear_selection_in(SelectMode::Remove);
        assert_eq!(workspace.selection, vec![0, 1], "Ctrl+点空白保留选择");
        workspace.clear_selection_in(SelectMode::Replace);
        assert!(workspace.selection.is_empty(), "普通点空白才清空");
    }

    #[test]
    fn marquee_modes_add_remove_and_replace() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select_many_with(&[0], SelectMode::Replace);
        workspace.select_many_with(&[1, 2], SelectMode::Add);
        assert_eq!(workspace.selection, vec![0, 1, 2], "Shift 框选并入");
        workspace.select_many_with(&[0, 2], SelectMode::Remove);
        assert_eq!(workspace.selection, vec![1], "Ctrl 框选只减去命中项");
        workspace.select_many_with(&[2], SelectMode::Replace);
        assert_eq!(workspace.selection, vec![2], "普通框选覆盖");
        workspace.select_many_with(&[], SelectMode::Add);
        assert_eq!(workspace.selection, vec![2], "空框 + Shift 不清空");
        workspace.select_many_with(&[], SelectMode::Replace);
        assert!(workspace.selection.is_empty(), "空框 + 普通点击清空");
    }

    #[test]
    fn select_mode_follows_the_modifiers() {
        assert_eq!(
            SelectMode::from_modifiers(false, false),
            SelectMode::Replace
        );
        assert_eq!(SelectMode::from_modifiers(true, false), SelectMode::Add);
        assert_eq!(SelectMode::from_modifiers(false, true), SelectMode::Remove);
        assert_eq!(SelectMode::from_modifiers(true, true), SelectMode::Remove);
    }

    #[test]
    fn selection_duplicate_delete_roundtrip() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select(1, false);
        workspace.duplicate_selection();
        assert_eq!(workspace.document().doc.entities.len(), 3);
        assert_eq!(workspace.selection, vec![2]);

        workspace.delete_selection();
        assert_eq!(workspace.document().doc.entities.len(), 2);
        assert!(workspace.selection.is_empty());
    }

    #[test]
    fn field_edit_goes_through_undo() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.set_field(
            1,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(75.0),
        );
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(75.0)
        );
        assert!(workspace.document().dirty);
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(60.0)
        );
    }

    #[test]
    fn add_and_remove_component() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select(0, false);
        // 相机已有 Camera，列表里不应再出现。
        let available: Vec<&str> = workspace
            .component_types()
            .iter()
            .map(|component| component.name.as_str())
            .collect();
        assert!(!available.contains(&"prism_engine::scene::Camera"));

        workspace.select(1, false);
        assert!(workspace
            .component_types()
            .iter()
            .any(|component| component.name == "prism_engine::scene::Camera"));
        workspace.add_component("prism_engine::scene::Camera");
        assert!(workspace.document().doc.entities[1].has_component("prism_engine::scene::Camera"));
        workspace.remove_component("prism_engine::scene::Camera");
        assert!(!workspace.document().doc.entities[1].has_component("prism_engine::scene::Camera"));
    }

    #[test]
    fn add_component_multi_writes_every_entity_in_one_step() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let path = "prism_engine::scene::Camera";
        // fixture 的 0 号实体本来就带 Camera，1 号没有。
        assert!(workspace.document().doc.entities[0].has_component(path));
        let undo_before = workspace.undo_history().len();

        let added = workspace.add_component_multi(&[0, 1], path);

        assert_eq!(added, 1, "已经有该组件的实体被跳过");
        assert!(workspace.document().doc.entities[1].has_component(path));
        assert_eq!(
            workspace.undo_history().len(),
            undo_before + 1,
            "批量添加只产生一个撤销步"
        );
        workspace.undo();
        assert!(
            !workspace.document().doc.entities[1].has_component(path),
            "一次撤销就回退整批"
        );
    }

    /// #514：资产详情把「被谁引用」也算出来（打开文档 + 磁盘上的文档）。
    #[test]
    fn asset_details_reports_references_from_open_documents() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let path = "assets/scenes/a.scene.json";
        workspace.document_mut().doc.entities[0].components.insert(
            "prism_engine::scene::MeshRenderer".to_string(),
            serde_json::json!({ "mesh_path": "../../models/cube.mesh" }),
        );

        let details = workspace.asset_details(path).expect("资产在索引里");
        assert_eq!(details.asset.path, path);
        assert_eq!(details.asset.kind, crate::assets::AssetKind::Scene);
        assert_eq!(details.asset.size_bytes, 10);
        assert!(!details.guid.is_empty());
        // 引用扫描看的是资产表里真实存在的路径，这里没有对应文件，所以只断言不 panic。
        assert!(details.references.iter().all(|r| !r.document.is_empty()));
    }

    /// 没有这个资产时返回 `None`，检查器据此显示占位而不是 panic。
    #[test]
    fn asset_details_for_an_unknown_path_is_none() {
        let workspace = Workspace::load_fixture(&fixture());
        assert!(workspace
            .asset_details("assets/does-not-exist.png")
            .is_none());
    }

    /// #513：删除后撤销，原实体重新被选中（按 id 找回，不是按下标）。
    #[test]
    fn undoing_a_delete_restores_the_selection() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let target = 1;
        let id = workspace.document().doc.entities[target]
            .id
            .expect("fixture 有 id");
        workspace.selection = vec![target];

        workspace.delete_selection();
        assert!(
            workspace.document().doc.index_of_id(id).is_none(),
            "删除后实体不在了"
        );
        assert!(workspace.selection.is_empty());

        workspace.undo();
        let restored = workspace
            .document()
            .doc
            .index_of_id(id)
            .expect("撤销后实体回来");
        assert_eq!(workspace.selection, vec![restored]);
        assert_eq!(workspace.selection_ids(), vec![id]);
    }

    /// 重做删除：选择按「删除后」的快照还原（空的），不会指向越界下标。
    #[test]
    fn redoing_a_delete_leaves_an_empty_selection() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.selection = vec![1];
        workspace.delete_selection();
        workspace.undo();
        assert_eq!(workspace.selection.len(), 1);

        workspace.redo();
        assert!(workspace.selection.is_empty());
        let len = workspace.document().doc.entities.len();
        assert!(workspace.selection.iter().all(|index| *index < len));
    }

    /// #513：复制后撤销，选择回到复制前的实体，而不是已经不存在的新下标。
    #[test]
    fn undoing_a_duplicate_returns_to_the_original_selection() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let original = 1;
        let id = workspace.document().doc.entities[original]
            .id
            .expect("fixture 有 id");
        workspace.selection = vec![original];

        workspace.duplicate_selection();
        assert!(workspace.selection.len() >= 1);
        assert!(
            workspace.selection.iter().any(|index| *index != original),
            "复制后选中新实体"
        );

        workspace.undo();
        assert_eq!(workspace.selection_ids(), vec![id]);
        assert_eq!(workspace.selection, vec![original]);
    }

    /// 选择快照里的 id 已经不存在时（文档被外部改动过）只丢弃，不 panic、不越界。
    #[test]
    fn restoring_a_selection_drops_entities_that_are_gone() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let kept = workspace.document().doc.entities[0].id.unwrap();
        let selected = workspace.select_ids(&[kept, 9999]);
        assert_eq!(selected, 1);
        assert_eq!(workspace.selection, vec![0]);

        assert_eq!(workspace.select_ids(&[9999]), 0);
        assert!(workspace.selection.is_empty());
    }

    /// 字段类命令（没有选择快照）撤销后 SELECTION 保持现状。
    #[test]
    fn undoing_a_field_edit_keeps_the_selection() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.selection = vec![0];
        workspace.set_field(
            0,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(75.0),
        );
        assert_eq!(workspace.selection, vec![0]);

        workspace.undo();
        assert_eq!(workspace.selection, vec![0]);
    }

    /// `fixture()` 的 0 号实体只有一个组件；插入两个哑组件才好测顺序。
    fn with_extra_components(workspace: &mut Workspace) {
        for name in ["zz_extra", "mm_extra"] {
            workspace.document_mut().doc.entities[0]
                .components
                .insert(name.to_string(), serde_json::json!({ "value": 0 }));
        }
    }

    /// #512：检查器顺序 = 文档顺序，可以上下移动，一次撤销回退。
    #[test]
    fn components_can_be_reordered_and_the_move_is_one_undo_step() {
        let mut workspace = Workspace::load_fixture(&fixture());
        with_extra_components(&mut workspace);
        let entity = 0;
        let before = workspace.component_order(entity);
        let camera = "prism_engine::scene::Camera";
        let position = before
            .iter()
            .position(|name| name == camera)
            .expect("fixture 的 0 号实体带 Camera");
        assert!(position + 1 < before.len(), "需要下面还有组件才谈得上上移");

        let undo_before = workspace.undo_history().len();
        assert!(workspace.move_component_at(entity, camera, 1));
        let mut expected = before.clone();
        expected.swap(position, position + 1);
        assert_eq!(workspace.component_order(entity), expected);
        assert_eq!(
            workspace.undo_history().len(),
            undo_before + 1,
            "移动组件只产生一个撤销步"
        );

        workspace.undo();
        assert_eq!(workspace.component_order(entity), before, "撤销恢复原顺序");
    }

    #[test]
    fn moving_a_component_past_the_edges_is_a_no_op() {
        let mut workspace = Workspace::load_fixture(&fixture());
        with_extra_components(&mut workspace);
        let entity = 0;
        let order = workspace.component_order(entity);
        let first = order.first().cloned().expect("至少一个组件");
        let last = order.last().cloned().expect("至少一个组件");
        let undo_before = workspace.undo_history().len();

        assert!(!workspace.move_component_at(entity, &first, -1));
        assert!(!workspace.move_component_at(entity, &last, 1));
        assert!(!workspace.move_component_at(entity, "not-a-component", 1));
        assert!(!workspace.move_component_at(entity, "Transform", 1));

        assert_eq!(workspace.component_order(entity), order);
        assert_eq!(
            workspace.undo_history().len(),
            undo_before,
            "没有移动就不该产生撤销步"
        );
    }

    #[test]
    fn the_inspector_order_survives_a_save_and_reload() {
        let mut workspace = Workspace::load_fixture(&fixture());
        with_extra_components(&mut workspace);
        let entity = 0;
        let order = workspace.component_order(entity);
        let last = order.last().cloned().expect("至少一个组件");
        assert!(workspace.move_component_at(entity, &last, -1));

        let saved = serde_json::to_string(&workspace.document().doc).unwrap();
        let reloaded: crate::doc::SceneDoc = serde_json::from_str(&saved).unwrap();
        let reloaded_order: Vec<String> = reloaded.entities[entity].components.order();
        assert_eq!(reloaded_order, workspace.component_order(entity));
    }

    #[test]
    fn add_component_multi_ignores_duplicates_and_repeats() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let path = "prism_engine::scene::Camera";
        let added = workspace.add_component_multi(&[1, 1, 1], path);
        assert_eq!(added, 1, "重复下标只算一次");

        let undo_before = workspace.undo_history().len();
        assert_eq!(workspace.add_component_multi(&[1], path), 0);
        assert_eq!(
            workspace.undo_history().len(),
            undo_before,
            "全都已有组件时不产生撤销步"
        );
        assert_eq!(workspace.add_component_multi(&[], path), 0);
        assert_eq!(
            workspace.add_component_multi(&[999], path),
            0,
            "越界下标跳过"
        );
    }

    #[test]
    fn the_add_component_menu_lists_what_one_entity_is_missing() {
        let workspace = Workspace::load_fixture(&fixture());
        let path = "prism_engine::scene::Camera";
        let names = |workspace: &Workspace, indices: &[usize]| -> Vec<String> {
            workspace
                .component_types_for(indices)
                .into_iter()
                .map(|component| component.name.clone())
                .collect()
        };
        assert!(!names(&workspace, &[0]).contains(&path.to_string()));
        assert!(names(&workspace, &[1]).contains(&path.to_string()));
        assert!(
            names(&workspace, &[0, 1]).contains(&path.to_string()),
            "至少一个选中项缺该组件时仍然列出",
        );
        assert!(names(&workspace, &[]).is_empty());
    }

    #[test]
    fn add_component_does_not_reset_an_attached_component() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.select(0, false);
        let path = "prism_engine::scene::Camera";
        workspace.set_field(1, path, "fov_y_degrees", json!(30.0));
        let undo_before = workspace.undo_history().len();

        // 再「添加」一次：不能把调好的参数重置成默认值，也不该产生撤销步。
        workspace.add_component(path);

        assert_eq!(
            workspace.document().doc.entities[0].components[path]["fov_y_degrees"],
            json!(30.0)
        );
        assert_eq!(workspace.undo_history().len(), undo_before);
    }

    #[test]
    fn maximize_toggles_without_touching_the_dock_tree() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let before = workspace.dock.clone();

        workspace.toggle_maximized("inspector");
        assert_eq!(workspace.maximized_tab.as_deref(), Some("inspector"));
        assert_eq!(
            workspace.dock, before,
            "maximize must not rewrite the layout"
        );

        // 再切到别的面板 = 转移最大化，不是取消。
        workspace.toggle_maximized("log");
        assert_eq!(workspace.maximized_tab.as_deref(), Some("log"));

        workspace.toggle_maximized("log");
        assert_eq!(workspace.maximized_tab, None);

        // 切换布局预设 / 还原都要清掉最大化。
        workspace.toggle_maximized("inspector");
        workspace.restore_maximized();
        assert_eq!(workspace.maximized_tab, None);
        workspace.toggle_maximized("inspector");
        workspace.set_layout_preset("2by3");
        assert_eq!(workspace.maximized_tab, None);
    }

    #[test]
    fn close_panel_drops_the_tab_and_clears_maximize() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.show_panel("problems");
        assert!(workspace.docked_tabs().iter().any(|tab| tab == "problems"));

        workspace.toggle_maximized("problems");
        workspace.close_panel("problems");

        assert!(!workspace.docked_tabs().iter().any(|tab| tab == "problems"));
        assert_eq!(workspace.maximized_tab, None);
        assert!(
            workspace.leaf_of("inspector").is_some(),
            "dock must survive"
        );

        // 关掉最后一个面板是 no-op（否则整块布局会变成空白）。
        workspace.close_other_panels("inspector");
        assert_eq!(workspace.docked_tabs(), vec!["inspector".to_string()]);
        workspace.close_panel("inspector");
        assert_eq!(
            workspace.docked_tabs(),
            vec!["inspector".to_string()],
            "closing the only remaining panel must be a no-op"
        );
    }

    #[test]
    fn close_other_panels_keeps_only_the_chosen_tab() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.float_panel("log", 10.0, 10.0);
        workspace.toggle_maximized("inspector");

        workspace.close_other_panels("inspector");

        assert_eq!(workspace.docked_tabs(), vec!["inspector".to_string()]);
        assert!(workspace.floating.is_empty());
        // 关掉的是「别的面板」，被留下的那个仍然最大化。
        assert_eq!(workspace.maximized_tab.as_deref(), Some("inspector"));
    }

    #[test]
    fn add_tab_to_leaf_merges_instead_of_splitting() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let leaf = workspace.leaf_of("inspector").unwrap();
        workspace.add_tab_to_leaf(&leaf, "shortcuts");

        let target = dock::find_leaf(&workspace.dock, "shortcuts").unwrap();
        assert_eq!(target.id(), leaf);
        assert_eq!(
            workspace
                .docked_tabs()
                .iter()
                .filter(|tab| *tab == "shortcuts")
                .count(),
            1
        );
    }

    #[test]
    fn panel_toggle_and_dock_operations_keep_single_instance() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.toggle_panel("log");
        assert_eq!(
            workspace
                .dock_tabs()
                .iter()
                .filter(|tab| *tab == "log")
                .count(),
            1
        );
        workspace.toggle_panel("log");
        assert_eq!(
            workspace
                .dock_tabs()
                .iter()
                .filter(|tab| *tab == "log")
                .count(),
            1
        );

        // 浮动后从 dock 摘掉，重新停靠再回来。
        workspace.float_panel("log", 100.0, 120.0);
        assert_eq!(workspace.floating.len(), 1);
        assert!(!workspace.docked_tabs().iter().any(|tab| *tab == "log"));
        let inspector_leaf = workspace.leaf_of("inspector").unwrap();
        workspace.dock_tab("log", &inspector_leaf, DockZone::Center);
        assert!(workspace.docked_tabs().iter().any(|tab| *tab == "log"));
        assert!(workspace.floating.is_empty());
    }

    #[test]
    fn layout_presets_switch_and_clear_floating() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.float_panel("log", 10.0, 10.0);
        workspace.set_layout_preset("2by3");
        assert!(workspace.floating.is_empty());
        assert!(workspace.dock_tabs().iter().any(|tab| tab == "gameView"));
        assert_eq!(workspace.layout_preset, "2by3");
    }

    #[test]
    fn grid_toggle_flips_flag() {
        let mut workspace = Workspace::sample();
        assert!(workspace.show_grid);
        workspace.toggle_grid();
        assert!(!workspace.show_grid);
        workspace.toggle_grid();
        assert!(workspace.show_grid);
    }

    #[test]
    fn skybox_toggle_flips_flag_and_defaults_on() {
        let mut workspace = Workspace::sample();
        assert!(workspace.show_skybox, "Scene view 默认画天空盒");
        workspace.toggle_skybox();
        assert!(!workspace.show_skybox);
        workspace.toggle_skybox();
        assert!(workspace.show_skybox);
    }

    #[test]
    fn play_state_machine() {
        let mut workspace = Workspace::load_fixture(&fixture());
        assert!(!workspace.playing);
        workspace.play();
        assert!(workspace.playing);
        workspace.tick(0.5);
        assert!((workspace.play_time_s - 0.5).abs() < 1e-6);
        workspace.pause();
        workspace.tick(0.5);
        assert!((workspace.play_time_s - 0.5).abs() < 1e-6);
        workspace.step();
        assert!(workspace.play_time_s > 0.5);
        workspace.stop();
        assert!(!workspace.playing);
        assert_eq!(workspace.play_time_s, 0.0);
    }

    /// #500：进入 Play 前的文档、撤销栈、脏标记和选中都要在 Stop 时原样回来。
    #[test]
    fn stopping_play_discards_the_edits_made_while_playing() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let index = 1; // fixture 里第二个实体：Cube
        workspace.selection = vec![index];
        let id = workspace.document().doc.entities[index]
            .id
            .expect("fixture 实体都有稳定 id");
        let before = workspace.document().doc.entities[index].transform;
        let entities_before = workspace.document().doc.entities.len();
        let selection_before = workspace.selection.clone();

        workspace.play();
        assert!(workspace.has_play_snapshot());
        // 播放期间的两类编辑：改 Transform + 删实体，都直接写作者文档。
        workspace.set_transform_axis(id, "position", 0, 99.0);
        workspace.selection = vec![index];
        workspace.delete_selection();
        assert!(workspace.document().doc.entities.len() < entities_before);
        assert!(workspace.document().dirty);

        workspace.stop();
        assert!(!workspace.playing);
        assert!(!workspace.has_play_snapshot());
        assert_eq!(workspace.document().doc.entities.len(), entities_before);
        let restored = &workspace.document().doc.entities[index];
        assert_eq!(
            restored.transform, before,
            "Transform 要回到进入 Play 前的值"
        );
        assert!(!workspace.document().dirty, "运行期编辑不能把文档标脏");
        assert_eq!(workspace.selection, selection_before);
        assert!(!workspace.can_undo(), "撤销栈不能混进运行期的步骤");
    }

    /// 播放中切到别的文档再 Stop：快照只盖回被播放的那一份，别的文档不受影响。
    #[test]
    fn stopping_play_only_restores_the_document_that_was_playing() {
        let mut workspace = Workspace::sample();
        let mut other = SceneDoc::default();
        other.add_named_child(None, "Other");
        workspace.docs.push(Document::new(
            DocKind::Scene,
            Some("other.scene.json".into()),
            other.clone(),
        ));
        let playing_index = workspace.active_index();

        workspace.play();
        workspace.set_active(1);
        workspace.docs[1]
            .doc
            .add_named_child(None, "Added while other doc is active");

        workspace.stop();
        assert_eq!(
            workspace.active_index(),
            playing_index,
            "Stop 要回到被播放的文档"
        );
        assert_eq!(
            workspace.docs[1].doc.entities.len(),
            other.entities.len() + 1
        );
    }

    struct TempProject {
        root: std::path::PathBuf,
    }

    impl TempProject {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "prism-editor-gpui-ws-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("assets/scenes")).unwrap();
            std::fs::create_dir_all(root.join("assets/prefabs")).unwrap();
            Self { root }
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn saving_after_an_external_edit_is_refused_until_forced() {
        let project = TempProject::new("save-conflict");
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        let index = workspace
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some("assets/scenes/main.scene.json"))
            .unwrap();

        // 头一次保存：磁盘上的版本就是我们读进来的那份，允许写。
        workspace.docs[index].dirty = true;
        workspace.save_document(index).unwrap();

        // 模拟 `git checkout`：别人改了文件。
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[{"id":1,"name":"Theirs","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}}]}"#,
        )
        .unwrap();

        assert!(workspace.check_external_changes() >= 1);
        assert!(workspace.save_conflict(index).is_some());
        assert!(workspace.save_document(index).is_err());

        // 覆盖保存：确实把我们的版本写回去了。
        workspace.docs[index].dirty = true;
        workspace.save_document_forced(index).unwrap();
        let text =
            std::fs::read_to_string(project.root.join("assets/scenes/main.scene.json")).unwrap();
        assert!(!text.contains("Theirs"), "{text}");
    }

    #[test]
    fn reloading_a_conflicted_document_takes_the_disk_version() {
        let project = TempProject::new("save-reload");
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        let index = workspace
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some("assets/scenes/main.scene.json"))
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[{"id":1,"name":"Theirs","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}}]}"#,
        )
        .unwrap();
        assert!(workspace.check_external_changes() >= 1);

        workspace.reload_document_from_disk(index).unwrap();
        let names: Vec<String> = workspace.docs[index]
            .doc
            .entities
            .iter()
            .map(|entity| entity.name.clone().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["Theirs".to_string()]);
        assert!(!workspace.docs[index].dirty);
        assert!(!workspace.docs[index].externally_modified);
        assert!(workspace.save_conflict(index).is_none());
    }

    #[test]
    fn a_missing_file_counts_as_an_external_change() {
        let project = TempProject::new("save-missing");
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        let index = workspace
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some("assets/scenes/main.scene.json"))
            .unwrap();

        assert!(workspace.save_conflict(index).is_none());
        std::fs::remove_file(project.root.join("assets/scenes/main.scene.json")).unwrap();
        // 文件被删掉也是「改了」：盲写会把它变回我们的版本。
        assert!(workspace.save_conflict(index).is_some());
    }

    #[test]
    fn project_scan_assets_populates_tree_and_picker() {
        let project = TempProject::new("scan-assets");
        std::fs::create_dir_all(project.root.join("assets/meshes")).unwrap();
        std::fs::write(project.root.join("assets/meshes/cube.fbx"), b"fbx").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/a.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert!(workspace
            .assets
            .iter()
            .any(|asset| asset.path == "assets/meshes/cube.fbx" && asset.kind == AssetKind::Model));
        assert!(workspace
            .assets
            .iter()
            .any(|asset| asset.path == "assets/scenes/a.scene.json"
                && asset.kind == AssetKind::Scene));
        let meshes = workspace.asset_picker_candidates("mesh_path");
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].path, "assets/meshes/cube.fbx");
        let expanded = workspace.expanded_assets.clone();
        assert!(workspace
            .tree
            .visible_rows(&expanded)
            .iter()
            .any(|(node, _)| node.name == "meshes"));
    }

    #[test]
    fn material_json_saves_and_reloads_through_the_workspace() {
        let project = TempProject::new("material-json");
        std::fs::create_dir_all(project.root.join("assets/materials")).unwrap();
        std::fs::write(
            project.root.join("assets/materials/wood.mat.json"),
            r#"{"type":"material","metallic":0.0,"base_color":[1,1,1,1]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        let mut value = workspace
            .read_asset_json("assets/materials/wood.mat.json")
            .unwrap();
        value["metallic"] = serde_json::json!(0.75);
        workspace
            .write_asset_json("assets/materials/wood.mat.json", &value)
            .unwrap();

        let reread = workspace
            .read_asset_json("assets/materials/wood.mat.json")
            .unwrap();
        assert_eq!(reread["metallic"], serde_json::json!(0.75));
        // 写盘后资产表里的分类也应该是「材质」而不是「数据」。
        assert!(workspace
            .assets
            .iter()
            .any(|asset| asset.path == "assets/materials/wood.mat.json"
                && asset.kind == crate::assets::AssetKind::Material));
    }

    #[test]
    fn asset_folders_copy_move_and_details() {
        let project = TempProject::new("asset-ops");
        std::fs::create_dir_all(project.root.join("assets/meshes")).unwrap();
        std::fs::write(project.root.join("assets/meshes/cube.fbx"), b"fbx").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/a.scene.json"),
            r#"{"version":3,"entities":[{"id":1,"name":"Mesh","components":{"prism_engine::scene::MeshRenderer":{"mesh_path":"assets/meshes/cube.fbx"}}}]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        workspace.create_asset_folder("assets/meshes/new").unwrap();
        assert!(project.root.join("assets/meshes/new").is_dir());
        assert!(workspace.tree.find("assets/meshes/new").is_some());

        let copy = workspace.duplicate_asset("assets/meshes/cube.fbx").unwrap();
        assert_eq!(copy, "assets/meshes/cube-2.fbx");
        assert!(project.root.join(&copy).is_file());

        let moved = workspace
            .move_asset_to_folder(&copy, Some("assets/meshes/new"))
            .unwrap();
        assert_eq!(moved, "assets/meshes/new/cube-2.fbx");
        assert!(project.root.join(&moved).is_file());
        assert!(workspace.assets.iter().any(|asset| asset.path == moved));

        let details = workspace.asset_details("assets/meshes/cube.fbx").unwrap();
        assert_eq!(details.guid.len(), 32);
        assert_eq!(details.references.len(), 1);
        assert_eq!(details.references[0].entity_name, "Mesh");
        assert!(details
            .references
            .iter()
            .any(|reference| reference.field.contains("mesh_path")));

        // 场景 / 预制体不能通过资产拖拽移动（走另存为）。
        assert!(workspace
            .move_asset_to_folder("assets/scenes/a.scene.json", Some("assets"))
            .is_err());
    }

    /// #471：把资产拖进目录（`move_asset_to_folder`）也要像重命名一样改写引用。
    #[test]
    fn moving_an_asset_rewrites_the_references_of_open_documents() {
        let project = TempProject::new("asset-move-references");
        std::fs::create_dir_all(project.root.join("assets/meshes")).unwrap();
        std::fs::write(project.root.join("assets/meshes/cube.fbx"), b"fbx").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/a.scene.json"),
            r#"{"version":3,"entities":[{"id":1,"name":"Mesh","components":{"prism_engine::scene::MeshRenderer":{"mesh_path":"assets/meshes/cube.fbx"}}}]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.create_asset_folder("assets/moved").unwrap();

        let moved = workspace
            .move_asset_to_folder("assets/meshes/cube.fbx", Some("assets/moved"))
            .unwrap();
        assert_eq!(moved, "assets/moved/cube.fbx");
        assert!(project.root.join(&moved).is_file());
        assert!(!project.root.join("assets/meshes/cube.fbx").exists());

        let document = workspace.document();
        let value = document
            .doc
            .entities
            .iter()
            .find(|entity| entity.name.as_deref() == Some("Mesh"))
            .and_then(|entity| entity.component("prism_engine::scene::MeshRenderer"))
            .and_then(|component| component.get("mesh_path"))
            .and_then(|value| value.as_str());
        assert_eq!(value, Some("assets/moved/cube.fbx"));
        assert!(document.dirty, "文档被改写了就该是 dirty");
    }

    /// #478：目录改名要把目录里每个资产的引用都跟着改写。
    #[test]
    fn renaming_a_folder_rewrites_references_to_its_contents() {
        let project = TempProject::new("folder-rename");
        std::fs::create_dir_all(project.root.join("assets/tex")).unwrap();
        std::fs::write(project.root.join("assets/tex/wood.png"), b"png").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/a.scene.json"),
            r#"{"version":3,"entities":[{"id":1,"name":"Floor","components":{"prism_engine::scene::SpriteRenderer":{"sprite_path":"assets/tex/wood.png"}}}]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        let (references, affected) = workspace
            .rename_folder("assets/tex", "assets/wood")
            .unwrap();

        assert_eq!(references, 1);
        assert_eq!(affected.len(), 1);
        assert!(project.root.join("assets/wood/wood.png").is_file());
        assert!(!project.root.join("assets/tex").exists());
        let value = workspace
            .document()
            .doc
            .entities
            .iter()
            .find(|entity| entity.name.as_deref() == Some("Floor"))
            .and_then(|entity| entity.component("prism_engine::scene::SpriteRenderer"))
            .and_then(|component| component.get("sprite_path"))
            .and_then(|value| value.as_str());
        assert_eq!(value, Some("assets/wood/wood.png"));
        assert!(workspace.document().dirty);
        assert!(workspace
            .assets
            .iter()
            .any(|a| a.path == "assets/wood/wood.png"));
    }

    /// 目录改名不能搬进自己的子目录（`fs::rename` 会失败得很隐晦）。
    #[test]
    fn renaming_a_folder_rejects_its_own_subtree() {
        let project = TempProject::new("folder-rename-subtree");
        std::fs::create_dir_all(project.root.join("assets/tex/inner")).unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        assert!(workspace
            .rename_folder("assets/tex", "assets/tex/inner")
            .is_err());
        assert!(project.root.join("assets/tex/inner").is_dir());
    }

    /// #478：只删空目录；非空时明确报错而不是递归删掉别人的资产。
    #[test]
    fn deleting_a_folder_requires_it_to_be_empty() {
        let project = TempProject::new("folder-delete");
        std::fs::create_dir_all(project.root.join("assets/empty")).unwrap();
        std::fs::create_dir_all(project.root.join("assets/full")).unwrap();
        std::fs::write(project.root.join("assets/full/keep.png"), b"png").unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        let error = workspace.delete_folder("assets/full").unwrap_err();
        assert!(error.to_string().contains("非空"), "{error}");

        workspace.delete_folder("assets/empty").unwrap();
        assert!(!project.root.join("assets/empty").exists());
        assert!(workspace.tree.find("assets/empty").is_none());

        // `assets/` 根自身不能删。
        assert!(workspace.delete_folder("assets").is_err());
    }

    #[test]
    fn snap_ground_and_align_operations() {
        let mut workspace = Workspace::sample();
        // sample：0 Main Camera、1 Cube、2 Directional Light、3 Detail。
        let cube = 1;
        workspace.selection = vec![cube];
        workspace.document_mut().doc.entities[cube]
            .transform
            .position = [0.0, 3.0, 0.0];
        workspace.document_mut().doc.entities[cube].transform.scale = [2.0, 2.0, 2.0];
        assert_eq!(workspace.snap_selection_to_ground(), 1);
        let position = workspace.document().doc.entities[cube].transform.position;
        assert!(
            (position[1] - 1.0).abs() < 1e-3,
            "半高 1 → y=1，实际 {position:?}"
        );
        assert!(workspace.can_undo());

        workspace.camera.target = [9.0, 9.0, 9.0];
        assert!(workspace.align_selection_to_view());
        let position = workspace.document().doc.entities[cube].transform.position;
        assert!(
            (position[0] - 9.0).abs() < 1e-3,
            "应平移到轨道中心: {position:?}"
        );

        assert!(workspace.align_view_to_selection());
        assert!((workspace.camera.target[0] - 9.0).abs() < 1e-3);
        assert!((workspace.camera.target[1] - 9.0).abs() < 1e-3);

        workspace.look_along_axis(0, 1.0);
        assert!((workspace.camera.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3);
    }

    #[test]
    fn project_scene_manifest_is_loaded_and_saved() {
        let project = TempProject::new("manifest-load");
        std::fs::write(
            project.root.join("assets/scenes.json"),
            r#"{"scenes": [
                {"name": "intro", "path": "scenes/intro.rscn", "enabled": true},
                {"name": "level", "path": "scenes/level.rscn", "enabled": false}
            ]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert_eq!(workspace.scene_manifest.scenes.len(), 2);
        assert_eq!(workspace.scene_manifest.active().unwrap().name, "intro");

        workspace.set_scene_enabled(0, false);
        let reloaded = SceneManifest::read(&project.root).unwrap();
        assert!(!reloaded.scenes[0].enabled);
        assert!(reloaded.active().is_none(), "level 本来就没启用");

        workspace.set_scene_enabled(1, true);
        let reloaded = SceneManifest::read(&project.root).unwrap();
        assert_eq!(reloaded.active().unwrap().name, "level");
    }

    #[test]
    fn manifest_add_move_remove_and_available_documents() {
        let project = TempProject::new("manifest-edit");
        let scene = r#"{"version":3,"entities":[]}"#;
        std::fs::write(project.root.join("assets/scenes/a.scene.json"), scene).unwrap();
        std::fs::write(project.root.join("assets/scenes/b.scene.json"), scene).unwrap();
        std::fs::write(
            project.root.join("assets/scenes.json"),
            r#"{"scenes": [{"name": "a", "path": "scenes/a.rscn"}]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        assert_eq!(
            workspace.available_scene_documents(),
            vec!["scenes/b.scene.json".to_string()]
        );
        let index = workspace
            .add_scene_document("assets/scenes/b.scene.json")
            .expect("b 应能加入");
        assert_eq!(index, 1);
        assert!(workspace.available_scene_documents().is_empty());
        assert!(workspace
            .add_scene_document("scenes/b.scene.json")
            .is_none());

        assert_eq!(workspace.move_scene_entry(1, -1), Some(0));
        assert_eq!(
            SceneManifest::read(&project.root).unwrap().scenes[0].name,
            "b"
        );
        assert!(workspace.remove_scene_entry(0));
        assert_eq!(SceneManifest::read(&project.root).unwrap().scenes.len(), 1);
        assert!(!workspace.remove_scene_entry(9));
    }

    #[test]
    fn delete_document_cleans_file_manifest_and_open_tab() {
        let project = TempProject::new("doc-delete");
        let scene = r#"{"version":3,"entities":[]}"#;
        std::fs::write(project.root.join("assets/scenes/a.scene.json"), scene).unwrap();
        std::fs::write(project.root.join("assets/scenes/b.scene.json"), scene).unwrap();
        std::fs::write(
            project.root.join("assets/scenes.json"),
            r#"{"scenes": [
                {"name": "a", "path": "scenes/a.rscn"},
                {"name": "b", "path": "scenes/b.rscn"}
            ]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert_eq!(workspace.docs.len(), 2);
        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/scenes/a.scene.json")
        );

        workspace
            .delete_document("assets/scenes/a.scene.json")
            .unwrap();
        assert!(!project.root.join("assets/scenes/a.scene.json").exists());
        assert_eq!(workspace.docs.len(), 1);
        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/scenes/b.scene.json")
        );
        assert_eq!(workspace.project_files.len(), 1);
        let manifest = SceneManifest::read(&project.root).unwrap();
        assert_eq!(manifest.scenes.len(), 1);
        assert_eq!(manifest.scenes[0].name, "b");

        assert!(workspace
            .delete_document("assets/scenes/a.scene.json")
            .is_err());
        assert!(workspace.delete_document("assets/settings.ini").is_err());
    }

    #[test]
    fn save_document_can_target_an_inactive_tab() {
        let project = TempProject::new("save-inactive");
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.new_document(DocKind::Scene, None);
        let index = workspace.docs.len() - 1;
        workspace.set_active(0);
        workspace.docs[index].dirty = true;

        let path = workspace.save_document(index).unwrap();
        assert_eq!(workspace.active, 0);
        assert_eq!(workspace.docs[index].path.as_deref(), Some(path.as_str()));
        assert!(!workspace.docs[index].dirty);
        assert!(project.root.join(&path).exists());
    }

    /// #459：Save As 不写后缀也要落到 `assets/scenes/<name>.scene.json`，
    /// 并且 `..` 不能再跑出 `assets/`。
    #[test]
    fn save_as_completes_the_suffix_and_rejects_traversal() {
        let project = TempProject::new("saveas");
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        let saved = workspace.save_active_as("level2").unwrap();
        assert_eq!(saved, "assets/scenes/level2.scene.json");
        assert!(project.root.join(&saved).exists());
        assert!(workspace
            .project_files
            .iter()
            .any(|file| file.path == saved));

        let saved = workspace
            .save_active_as("assets/scenes/nested/level3.json")
            .unwrap();
        assert_eq!(saved, "assets/scenes/nested/level3.scene.json");
        assert!(project.root.join(&saved).exists());

        assert!(workspace.save_active_as("../evil").is_err());
        assert!(workspace.save_active_as("assets/../evil").is_err());
        assert!(workspace.save_active_as("/tmp/evil").is_err());
        assert!(!project.root.join("assets/evil.scene.json").exists());
        assert!(workspace.create_asset_folder("assets/../evil").is_err());
    }

    #[test]
    fn closing_documents_keeps_active_index_valid() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.new_document(DocKind::Scene, None);
        workspace.new_document(DocKind::Scene, None);
        assert!(workspace.docs.len() >= 3);
        workspace.set_active(2);

        workspace.close_document(0);
        assert_eq!(workspace.active, 1);
        workspace.close_document(1);
        assert!(workspace.active < workspace.docs.len());
        assert!(!workspace.docs.is_empty());
    }

    #[test]
    fn reloading_a_shrunken_document_clamps_selection() {
        let project = TempProject::new("reload-selection-clamp");
        let path = "assets/scenes/a.scene.json";
        let five = r#"{
            "version": 3,
            "entities": [
                {"id":1,"name":"A","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}},
                {"id":2,"name":"B","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}},
                {"id":3,"name":"C","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}},
                {"id":4,"name":"D","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}},
                {"id":5,"name":"E","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}}
            ]
        }"#;
        std::fs::write(project.root.join(path), five).unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert_eq!(workspace.document().doc.entities.len(), 5);
        workspace.selection = (0..5).collect();

        // 模拟 `git checkout` / 外部工具把文档换短。
        std::fs::write(
            project.root.join(path),
            r#"{
                "version": 3,
                "entities": [
                    {"id":1,"name":"A","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}},
                    {"id":2,"name":"B","parent":null,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},"components":{}}
                ]
            }"#,
        )
        .unwrap();

        workspace.reload_active().unwrap();
        assert_eq!(workspace.document().doc.entities.len(), 2);
        assert_eq!(workspace.selection, vec![0, 1]);
    }

    #[test]
    fn saving_a_scene_omits_default_and_read_only_component_fields() {
        let project = TempProject::new("prune-defaults");
        std::fs::write(
            project.root.join("assets/scenes/a.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Camera","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::Camera":{"fov_y_degrees":60.0}}}
            ]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.save_document(0).unwrap();

        let saved =
            std::fs::read_to_string(project.root.join("assets/scenes/a.scene.json")).unwrap();
        assert!(
            !saved.contains("fov_y_degrees"),
            "默认值不应写进作者文档: {saved}"
        );

        workspace.reload_active().unwrap();
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(60.0),
            "读取时默认值必须补回来"
        );
    }

    #[test]
    fn project_roundtrip_save_and_reload() {
        let project = TempProject::new("roundtrip");
        let disk_doc = r#"{
            "version": 3,
            "entities": [
                {"id": 1, "name": "Disk Cube", "parent": null,
                 "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components": {"prism_engine::scene::Camera": {}}}
            ]
        }"#;
        std::fs::write(project.root.join("assets/scenes/disk.scene.json"), disk_doc).unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        let opened = workspace.load_project(project.root.clone()).unwrap();
        assert_eq!(opened, 1);
        assert_eq!(workspace.project_files.len(), 1);
        assert_eq!(workspace.document().title(), "disk.scene.json");
        // 打开时补齐 schema 默认值。
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(60.0)
        );

        // 改字段 → 保存 → dirty 清掉、文件落盘。
        workspace.set_field(
            1,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(42.0),
        );
        assert!(workspace.document().dirty);
        let path = workspace.save_active().unwrap();
        assert_eq!(path, "assets/scenes/disk.scene.json");
        assert!(!workspace.document().dirty);

        // 再改（不保存）→ reload 丢弃修改。
        workspace.set_field(
            1,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(90.0),
        );
        workspace.reload_active().unwrap();
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(42.0)
        );

        // 另存为到新路径，并出现在项目文件列表里。
        let saved = workspace
            .save_active_as("assets/scenes/copy.scene.json")
            .unwrap();
        assert_eq!(saved, "assets/scenes/copy.scene.json");
        assert!(project.root.join(&saved).exists());
        assert!(workspace
            .project_files
            .iter()
            .any(|file| file.path == saved));
        assert!(workspace.save_active_as("   ").is_err());
        assert!(workspace.save_active_as("outside.scene.json").is_ok());
        assert!(project
            .root
            .join("assets/scenes/outside.scene.json")
            .exists());
    }

    #[test]
    fn instantiate_prefab_asset_from_disk_and_template() {
        let project = TempProject::new("instantiate");
        std::fs::write(
            project.root.join("assets/prefabs/box.prefab.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Box","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}},
                {"id":2,"name":"Child","parent":1,
                 "transform":{"position":[0,1,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Root","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.select(0, false);

        let inserted = workspace
            .instantiate_prefab_asset("assets/prefabs/box.prefab.json", Some(0))
            .unwrap();
        assert_eq!(inserted, 2);
        let doc = &workspace.document().doc;
        assert_eq!(doc.entities.len(), 3);
        let instance = &doc.entities[1];
        assert_eq!(instance.parent, Some(1), "挂到选中实体下");
        assert_eq!(
            instance.prefab_link(),
            Some("assets/prefabs/box.prefab.json")
        );
        assert_eq!(workspace.selection, vec![1]);
        assert!(workspace.can_undo());

        // fixture 模式回退模板。
        let mut fixture_workspace = Workspace::load_fixture(&fixture());
        let count = fixture_workspace
            .instantiate_prefab_asset("assets/prefabs/camera.prefab.json", None)
            .unwrap();
        assert_eq!(count, 1);
        assert!(fixture_workspace
            .document()
            .doc
            .entities
            .iter()
            .any(|entity| entity.name.as_deref() == Some("Camera (1)")));

        // 不存在的预制体报错。
        assert!(workspace
            .instantiate_prefab_asset("assets/prefabs/nope.prefab.json", None)
            .is_err());
    }

    #[test]
    fn save_selection_as_prefab_writes_subtree_and_dedupes_names() {
        let project = TempProject::new("prefab-save");
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Root","parent":null,
                 "transform":{"position":[1,2,3],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}},
                {"id":2,"name":"Child","parent":1,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.select(0, false);

        let path = workspace.save_selection_as_prefab("My Prefab").unwrap();
        assert_eq!(path, "assets/prefabs/My Prefab.prefab.json");
        let written = project::read_document(&project.root, &path).unwrap();
        assert_eq!(written.entities.len(), 2, "包含子树");
        assert!(written.entities[0].parent.is_none(), "根无父级");

        let second = workspace.save_selection_as_prefab("My Prefab").unwrap();
        assert_eq!(second, "assets/prefabs/My Prefab-2.prefab.json");
        assert!(workspace
            .project_files
            .iter()
            .any(|file| file.path == second));

        assert!(
            workspace.save_selection_as_prefab("   ").is_ok(),
            "空名字回退 Untitled"
        );
        assert!(project
            .root
            .join("assets/prefabs/Untitled.prefab.json")
            .exists());
    }

    #[test]
    fn delete_asset_only_removes_regular_assets() {
        let project = TempProject::new("delete-asset");
        std::fs::create_dir_all(project.root.join("assets/textures")).unwrap();
        std::fs::write(project.root.join("assets/textures/a.png"), b"png").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();

        assert!(workspace.delete_asset("outside.txt").is_err());
        assert!(workspace
            .delete_asset("assets/textures/missing.png")
            .is_err());
        assert!(workspace
            .delete_asset("assets/scenes/main.scene.json")
            .is_err());
        workspace.delete_asset("assets/textures/a.png").unwrap();
        assert!(!project.root.join("assets/textures/a.png").exists());
    }

    #[test]
    fn rename_asset_moves_file_and_rewrites_references() {
        let project = TempProject::new("rename");
        std::fs::create_dir_all(project.root.join("assets/textures")).unwrap();
        std::fs::write(project.root.join("assets/textures/a.png"), b"png").unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Sprite","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::SpriteRenderer":{"sprite_path":"assets/textures/a.png"}}}
            ]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert!(
            workspace
                .document()
                .doc
                .field_references("assets/textures/a.png")
                .len()
                == 1
        );

        let (references, affected) = workspace
            .rename_asset("assets/textures/a.png", "assets/textures/b.png")
            .unwrap();
        assert_eq!(references, 1);
        assert_eq!(affected.len(), 1);
        assert!(!project.root.join("assets/textures/a.png").exists());
        assert!(project.root.join("assets/textures/b.png").exists());
        assert_eq!(
            workspace
                .document()
                .doc
                .field_references("assets/textures/b.png")
                .len(),
            1
        );
        assert!(workspace.document().dirty, "引用改写后应标记未保存");

        // 场景文档不允许走资产重命名。
        assert!(workspace
            .rename_asset(
                "assets/scenes/main.scene.json",
                "assets/scenes/other.scene.json"
            )
            .is_err());
        // 目标必须存在 / 在 assets 内。
        assert!(workspace
            .rename_asset("assets/textures/b.png", "outside.png")
            .is_err());
    }

    #[test]
    fn missing_references_only_flags_broken_asset_fields() {
        let mut workspace = Workspace::sample();
        // 资产表里只有这一个。
        workspace.assets = vec![
            AssetInfo {
                path: "assets/meshes/ok.rmes".to_string(),
                size_bytes: 1,
                kind: AssetKind::Model,
                importer: String::new(),
            },
            AssetInfo {
                path: "assets/textures/wood.png".to_string(),
                size_bytes: 1,
                kind: AssetKind::Texture,
                importer: String::new(),
            },
        ];
        workspace.document_mut().doc.entities[0].components.insert(
            "prism_engine::scene::MeshRenderer".to_string(),
            json!({
                "mesh_path": "assets/meshes/ok.rmes",
                "materials": ["assets/textures/wood.png", "assets/textures/gone.png"],
            }),
        );
        // 实体名长得像路径，但不是资产字段，不能被当成断引用。
        workspace.document_mut().doc.entities[0].name = Some("assets/nope.png".to_string());

        let missing = workspace.missing_references(0);
        let values: Vec<&str> = missing.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(values, vec!["assets/textures/gone.png"]);
        assert!(
            missing[0].0.contains("materials"),
            "字段路径要能认出是哪一格: {:?}",
            missing[0]
        );
        assert!(workspace.missing_references(1).is_empty());
    }

    #[test]
    fn renaming_an_asset_updates_scenes_that_are_not_open() {
        let project = TempProject::new("rename-closed");
        std::fs::create_dir_all(project.root.join("assets/textures")).unwrap();
        std::fs::write(project.root.join("assets/textures/wood.png"), b"png").unwrap();
        let scene = |texture: &str| {
            format!(
                r#"{{"version":3,"entities":[{{"id":1,"name":"Sprite","parent":null,"transform":{{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}},"components":{{"prism_engine::scene::SpriteRenderer":{{"sprite_path":"{texture}"}}}}}}]}}"#
            )
        };
        for name in ["a", "b"] {
            std::fs::write(
                project
                    .root
                    .join(format!("assets/scenes/{name}.scene.json")),
                scene("assets/textures/wood.png"),
            )
            .unwrap();
        }

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        // 关掉 b：它仍在 project_files 里，但不在打开的文档里。
        let index = workspace
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some("assets/scenes/b.scene.json"))
            .expect("b 应该被 load_project 打开");
        workspace.close_document(index);
        assert!(!workspace
            .docs
            .iter()
            .any(|doc| doc.path.as_deref() == Some("assets/scenes/b.scene.json")));

        let (references, affected) = workspace
            .rename_asset("assets/textures/wood.png", "assets/textures/oak.png")
            .unwrap();

        // 两份场景各 1 处引用：打开的那份在内存里标脏，关着的那份直接落盘。
        assert_eq!(references, 2);
        assert_eq!(affected.len(), 2);

        let on_disk =
            std::fs::read_to_string(project.root.join("assets/scenes/b.scene.json")).unwrap();
        assert!(on_disk.contains("assets/textures/oak.png"), "{on_disk}");
        assert!(!on_disk.contains("wood.png"), "{on_disk}");

        let open = workspace
            .docs
            .iter()
            .find(|doc| doc.path.as_deref() == Some("assets/scenes/a.scene.json"))
            .expect("a 仍然开着");
        assert_eq!(
            open.doc.field_references("assets/textures/oak.png").len(),
            1
        );
        assert!(open.dirty, "内存里的引用改完要标脏");
    }

    /// 建一个「预制体在磁盘上 + 场景里实例化一份」的工程。
    fn prefab_project(name: &str) -> (TempProject, Workspace) {
        let project = TempProject::new(name);
        std::fs::create_dir_all(project.root.join("assets/prefabs")).unwrap();
        std::fs::write(
            project.root.join("assets/prefabs/box.prefab.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Box","parent":null,
                 "transform":{"position":[5,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::Camera":{"fov_y_degrees":33.0}}},
                {"id":2,"name":"Child","parent":1,
                 "transform":{"position":[0,1,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace
            .instantiate_prefab_asset("assets/prefabs/box.prefab.json", None)
            .unwrap();
        (project, workspace)
    }

    #[test]
    fn prefab_overrides_report_only_changed_fields() {
        let (_project, mut workspace) = prefab_project("overrides");
        let root = workspace.selection[0];
        assert!(
            workspace.prefab_overrides(root).unwrap().is_empty(),
            "刚实例化时应该没有 override"
        );

        workspace.document_mut().doc.entities[root]
            .transform
            .position = [1.0, 2.0, 3.0];
        workspace.document_mut().doc.entities[root].components["prism_engine::scene::Camera"]
            ["fov_y_degrees"] = serde_json::json!(60.0);

        let list = workspace.prefab_overrides(root).unwrap();
        let fields: Vec<String> = list
            .iter()
            .map(|entry| format!("{}.{}", entry.component, entry.field))
            .collect();
        assert_eq!(
            fields,
            [
                "transform.position".to_string(),
                "prism_engine::scene::Camera.fov_y_degrees".to_string()
            ]
        );
        assert_eq!(list[0].instance_value, serde_json::json!([1.0, 2.0, 3.0]));
        assert_eq!(list[0].prefab_value, serde_json::json!([5.0, 0.0, 0.0]));
    }

    #[test]
    fn prefab_overrides_survive_a_sibling_inserted_into_the_instance() {
        let (_project, mut workspace) = prefab_project("overrides-insert");
        let root = workspace.selection[0];
        // 在实例子树里插一个新实体（id 不在「base + 预制体 id」的集合里），
        // 后面的实体不能因此错位。
        let child = workspace.document().doc.children(root)[0];
        workspace
            .document_mut()
            .doc
            .add_named_child(Some(child), "Extra");
        workspace.document_mut().doc.entities[child]
            .transform
            .position = [0.0, 9.0, 0.0];

        let list = workspace.prefab_overrides(root).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].field, "position");
        assert_eq!(list[0].instance_value, serde_json::json!([0.0, 9.0, 0.0]));
    }

    #[test]
    fn reverting_a_prefab_override_restores_the_prefab_value() {
        let (_project, mut workspace) = prefab_project("overrides-revert");
        let root = workspace.selection[0];
        workspace.document_mut().doc.entities[root]
            .transform
            .position = [1.0, 2.0, 3.0];
        let entry = workspace.prefab_overrides(root).unwrap().remove(0);

        assert!(workspace.revert_prefab_override(&entry));
        assert_eq!(
            workspace.document().doc.entities[root].transform.position,
            [5.0, 0.0, 0.0]
        );
        assert!(workspace.prefab_overrides(root).unwrap().is_empty());
        workspace.undo();
        assert_eq!(
            workspace.undo_history().first().map(|entry| entry.as_str()),
            Some("实例化预制体"),
            "回滚是一个独立的撤销步（撤销一次就回到实例化那一步之前）"
        );
        assert_eq!(
            workspace.document().doc.entities[root].transform.position,
            [1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn applying_a_prefab_override_writes_the_prefab_file() {
        let (project, mut workspace) = prefab_project("overrides-apply");
        let root = workspace.selection[0];
        workspace.document_mut().doc.entities[root]
            .transform
            .position = [1.0, 2.0, 3.0];
        let entry = workspace.prefab_overrides(root).unwrap().remove(0);

        workspace.apply_prefab_override(root, &entry).unwrap();
        let prefab =
            crate::project::read_document(&project.root, "assets/prefabs/box.prefab.json").unwrap();
        assert_eq!(prefab.entities[0].transform.position, [1.0, 2.0, 3.0]);
        assert!(
            workspace.prefab_overrides(root).unwrap().is_empty(),
            "写回之后实例和源一致"
        );
    }

    #[test]
    fn unpack_prefab_drops_every_link_in_the_subtree_in_one_step() {
        let (_project, mut workspace) = prefab_project("unpack");
        let root = workspace.selection[0];
        let before = workspace.document().doc.entities.len();
        assert_eq!(workspace.unpack_prefab(root), 1);
        assert!(workspace.document().doc.entities[root]
            .prefab_link()
            .is_none());
        assert_eq!(workspace.document().doc.entities.len(), before);
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[root].prefab_link(),
            Some("assets/prefabs/box.prefab.json")
        );
    }

    /// 写一个最小工程：crate 预制体 + 引用它的 level 场景（实例 id = 7）。
    fn breadcrumb_project(tag: &str) -> (TempProject, Workspace) {
        let project = TempProject::new(tag);
        std::fs::write(
            project.root.join("assets/prefabs/crate.prefab.json"),
            r#"{"version":3,"entities":[
                {"id":0,"name":"Crate","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            project.root.join("assets/scenes/level.scene.json"),
            r#"{"version":3,"entities":[
                {"id":7,"name":"Crate Instance","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::PrefabInstance":{"prefab":"assets/prefabs/crate.prefab.json"}}}
            ]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        (project, workspace)
    }

    fn breadcrumb_scene_index(workspace: &Workspace) -> usize {
        workspace
            .docs
            .iter()
            .position(|doc| doc.path.as_deref() == Some("assets/scenes/level.scene.json"))
            .expect("level scene")
    }

    #[test]
    fn opening_a_prefab_from_an_instance_remembers_the_origin() {
        let (_project, mut workspace) = breadcrumb_project("prefab-origin");
        let scene = breadcrumb_scene_index(&workspace);
        workspace.set_active(scene);

        workspace.open_prefab_from_instance(scene, 7).unwrap();
        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/prefabs/crate.prefab.json")
        );
        assert_eq!(
            workspace.document().prefab_origin,
            Some(PrefabOrigin {
                scene_index: scene,
                entity_id: 7,
                scene_title: "level.scene.json".to_string(),
            })
        );

        // 普通场景文档没有面包屑；从预制体打开预制体会被拒绝。
        workspace.set_active(scene);
        assert!(workspace.document().prefab_origin.is_none());
        assert!(workspace.open_prefab_from_instance(scene, 999).is_err());
    }

    #[test]
    fn returning_from_a_clean_prefab_reinstantiates_and_selects_the_instance() {
        let (_project, mut workspace) = breadcrumb_project("prefab-return-clean");
        let scene = breadcrumb_scene_index(&workspace);
        workspace.set_active(scene);
        workspace.open_prefab_from_instance(scene, 7).unwrap();

        // 改预制体并保存：返回时干净文档要重新实例化，把新位置同步进场景。
        workspace.document_mut().doc.entities[0].transform.position = [5.0, 1.0, 0.0];
        workspace.save_active().unwrap();
        let prefab = workspace.active_index();
        assert!(workspace.return_from_prefab(prefab));

        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/scenes/level.scene.json")
        );
        let selected = workspace.primary_selection().expect("返回后选中原来的实例");
        let entity = &workspace.document().doc.entities[selected];
        assert_eq!(
            entity.prefab_link(),
            Some("assets/prefabs/crate.prefab.json")
        );
        assert_eq!(
            entity.transform.position,
            [5.0, 1.0, 0.0],
            "干净预制体返回时应把内容同步回实例"
        );
        // 重新实例化会换 id，但选中的仍是场景里的那个实例。
        assert_ne!(entity.id, Some(7));
    }

    #[test]
    fn returning_from_a_dirty_prefab_keeps_the_instance_untouched() {
        let (_project, mut workspace) = breadcrumb_project("prefab-return-dirty");
        let scene = breadcrumb_scene_index(&workspace);
        workspace.set_active(scene);
        workspace.open_prefab_from_instance(scene, 7).unwrap();

        workspace.document_mut().doc.entities[0].transform.position = [9.0, 0.0, 0.0];
        workspace.document_mut().dirty = true;
        let prefab = workspace.active_index();
        assert!(workspace.return_from_prefab(prefab));

        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/scenes/level.scene.json")
        );
        let selected = workspace.primary_selection().expect("仍然选中实例");
        let entity = &workspace.document().doc.entities[selected];
        assert_eq!(entity.id, Some(7), "脏预制体不重新实例化，实体 id 不变");
        assert_ne!(entity.transform.position, [9.0, 0.0, 0.0]);
    }

    #[test]
    fn returning_without_an_origin_is_a_noop() {
        let (_project, mut workspace) = breadcrumb_project("prefab-return-none");
        let scene = breadcrumb_scene_index(&workspace);
        workspace.set_active(scene);
        assert!(!workspace.return_from_prefab(scene));
        assert_eq!(workspace.active_index(), scene);
    }

    #[test]
    fn revert_prefab_instance_from_disk_keeps_name_and_parent() {
        let project = TempProject::new("revert");
        std::fs::write(
            project.root.join("assets/prefabs/box.prefab.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Box","parent":null,
                 "transform":{"position":[5,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::Camera":{"fov_y_degrees":33.0}}},
                {"id":2,"name":"Child","parent":1,
                 "transform":{"position":[0,1,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Parent","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}},
                {"id":2,"name":"My Box","parent":1,
                 "transform":{"position":[1,2,3],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::PrefabInstance":{"prefab":"assets/prefabs/box.prefab.json"}}}
            ]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        assert_eq!(workspace.document().doc.entities.len(), 2);

        workspace.revert_prefab_instance(2).unwrap();
        let doc = &workspace.document().doc;
        assert_eq!(doc.entities.len(), 3, "实例根 + 预制体子级");
        let root = doc
            .entities
            .iter()
            .find(|entity| entity.name.as_deref() == Some("My Box"))
            .expect("保留实例名字");
        assert_eq!(root.parent, Some(1), "保留父级");
        assert_eq!(
            root.transform.position,
            [5.0, 0.0, 0.0],
            "使用预制体当前内容"
        );
        assert_eq!(
            root.prefab_link(),
            Some("assets/prefabs/box.prefab.json"),
            "链接保留"
        );
        assert!(doc
            .entities
            .iter()
            .any(|entity| entity.display_name() == "Child"));

        workspace.undo();
        assert_eq!(workspace.document().doc.entities.len(), 2);
        assert_eq!(
            workspace.document().doc.entities[1].transform.position,
            [1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn apply_prefab_instance_writes_subtree_and_strips_root_link() {
        let project = TempProject::new("apply-prefab");
        std::fs::write(
            project.root.join("assets/prefabs/box.prefab.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Box","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}},
                {"id":2,"name":"Child","parent":1,
                 "transform":{"position":[0,1,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}}
            ]}"#,
        )
        .unwrap();
        std::fs::write(
            project.root.join("assets/scenes/main.scene.json"),
            r#"{"version":3,"entities":[
                {"id":1,"name":"Root","parent":null,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{}},
                {"id":2,"name":"My Box","parent":1,
                 "transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                 "components":{"prism_engine::scene::PrefabInstance":{"prefab":"assets/prefabs/box.prefab.json"}}}
            ]}"#,
        )
        .unwrap();

        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.revert_prefab_instance(2).unwrap();
        let root_id = workspace
            .document()
            .doc
            .entities
            .iter()
            .find(|entity| entity.display_name() == "My Box")
            .and_then(|entity| entity.id)
            .expect("实例根");
        {
            let doc = &mut workspace.document_mut().doc;
            let child = doc
                .entities
                .iter_mut()
                .find(|entity| entity.display_name() == "Child")
                .expect("实例子级");
            child.transform.position = [7.0, 0.0, 0.0];
            child.name = Some("Renamed Child".to_string());
        }

        let path = workspace.apply_prefab_instance(root_id).unwrap();
        assert_eq!(path, "assets/prefabs/box.prefab.json");
        let prefab = project::read_document(&project.root, &path).unwrap();
        assert_eq!(prefab.entities.len(), 2);
        assert!(
            prefab
                .entities
                .iter()
                .all(|entity| entity.prefab_link().is_none()),
            "预制体根不应带实例链接"
        );
        let child = prefab
            .entities
            .iter()
            .find(|entity| entity.display_name() == "Renamed Child")
            .expect("修改写回预制体");
        assert_eq!(child.transform.position, [7.0, 0.0, 0.0]);
        assert!(workspace.apply_prefab_instance(1).is_err(), "非实例应报错");
    }

    #[test]
    fn prefab_falls_back_to_template_in_fixture_mode() {
        let mut workspace = Workspace::load_fixture(&fixture());
        // fixture 的模板是 camera；伪造一个指向它的实例。
        workspace.document_mut().doc.entities[1].components.insert(
            PREFAB_LINK_COMPONENT.to_string(),
            serde_json::json!({"prefab": "assets/prefabs/camera.prefab.json"}),
        );
        let doc = workspace.prefab_document_for(2).unwrap();
        assert_eq!(doc.entities.len(), 1);
        assert_eq!(doc.entities[0].display_name(), "Camera");

        workspace.revert_prefab_instance(2).unwrap();
        let instance = workspace
            .document()
            .doc
            .entities
            .iter()
            .find(|entity| entity.name.as_deref() == Some("Cube"))
            .expect("实例根");
        assert_eq!(
            instance.prefab_link(),
            Some("assets/prefabs/camera.prefab.json")
        );
        // 非实例实体不能被还原。
        assert!(workspace.revert_prefab_instance(1).is_err());
    }

    #[test]
    fn saving_without_project_fails_cleanly() {
        let mut workspace = Workspace::load_fixture(&fixture());
        assert!(!workspace.has_project());
        assert!(workspace.save_active().is_err());
        assert!(workspace.reload_active().is_err());
    }

    #[test]
    fn hydration_fills_missing_component_fields() {
        let mut fixture = fixture();
        // 去掉 Camera 的 fov_y_degrees，模拟旧文档。
        fixture["docs"]["assets/scenes/a.scene.json"]["entities"][0]["components"]
            ["prism_engine::scene::Camera"] = json!({});
        let mut workspace = Workspace::load_fixture(&fixture);
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(60.0)
        );
        workspace.set_field(
            1,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(80.0),
        );
        assert_eq!(
            workspace.document().doc.entities[0].components["prism_engine::scene::Camera"]
                ["fov_y_degrees"],
            json!(80.0)
        );
    }

    #[test]
    fn unlink_prefab_removes_link_only_once() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.document_mut().doc.entities[1].components.insert(
            PREFAB_LINK_COMPONENT.to_string(),
            serde_json::json!({ "prefab": "assets/prefabs/camera.prefab.json" }),
        );
        assert!(workspace.unlink_prefab(1));
        assert!(!workspace.document().doc.entities[1].has_component(PREFAB_LINK_COMPONENT));
        assert!(!workspace.unlink_prefab(1), "没有链接时返回 false");
    }

    #[test]
    fn asset_path_fields_are_detected_and_filtered() {
        use crate::assets::{is_asset_path_field, AssetKind};
        assert!(is_asset_path_field(
            "prism_engine::scene::MeshRenderer",
            "mesh_path"
        ));
        assert!(is_asset_path_field(
            "prism_engine::scene::Skybox",
            "hdr_path"
        ));
        assert!(is_asset_path_field(
            "prism_engine::scene::EnvironmentLighting",
            "env_map"
        ));
        assert!(is_asset_path_field(
            "prism_engine::scene::PrefabInstance",
            "prefab"
        ));
        assert!(!is_asset_path_field("prism_engine::scene::Camera", "name"));
        assert!(!is_asset_path_field(
            "prism_engine::scene::Camera",
            "fov_y_degrees"
        ));

        let mut workspace = Workspace::sample();
        use crate::assets::AssetInfo;
        workspace.assets = vec![
            AssetInfo {
                path: "assets/meshes/cube.rmesh".into(),
                size_bytes: 1,
                kind: AssetKind::Model,
                importer: "ModelImporter".into(),
            },
            AssetInfo {
                path: "assets/textures/grid.png".into(),
                size_bytes: 1,
                kind: AssetKind::Texture,
                importer: "TextureImporter".into(),
            },
        ];
        let meshes = workspace.asset_picker_candidates("mesh_path");
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].path, "assets/meshes/cube.rmesh");
        let textures = workspace.asset_picker_candidates("sprite_path");
        assert_eq!(textures.len(), 1);
        assert_eq!(textures[0].kind, AssetKind::Texture);
        let all = workspace.asset_picker_candidates("some_unknown_field");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn theme_and_locale_switch() {
        let mut workspace = Workspace::load_fixture(&fixture());
        assert_eq!(workspace.t("common.save"), "保存");
        workspace.set_locale(Locale::EnUs);
        assert_eq!(workspace.t("common.save"), "Save");
        workspace.set_theme_preset("paper");
        assert_eq!(workspace.color(ThemeToken::Bg).to_hex(), "#f2f4f7");
    }

    /// #472：层级里双击实体 = `Frame`，相机轨道中心要落到它身上。
    #[test]
    fn framing_the_selected_entity_centres_the_camera_on_it() {
        let mut workspace = Workspace::sample();
        // sample 里 Cube 的本地位置是 (0, 0.5, 0)。
        workspace.select(1, false);
        workspace.camera.target = [99.0, 99.0, 99.0];

        workspace.frame_selection();

        assert!((workspace.camera.target[0]).abs() < 1e-4);
        assert!((workspace.camera.target[1] - 0.5).abs() < 1e-4);
        assert!((workspace.camera.target[2]).abs() < 1e-4);
        assert!(workspace.camera.distance > 0.0);
    }

    #[test]
    fn frame_all_moves_camera_target() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.frame_all();
        assert!(workspace.camera.distance > 0.5);
        workspace.select(0, false);
        workspace.frame_selection();
        assert_eq!(workspace.camera.target[1], 1.0);
    }

    /// `fixture()` + 自定义实体表：用来测世界变换相关的取景逻辑。
    fn scene_fixture(entities: serde_json::Value) -> Value {
        let mut value = fixture();
        value["docs"]["assets/scenes/a.scene.json"]["entities"] = entities;
        value
    }

    #[test]
    fn framing_a_child_uses_its_world_position() {
        // 回归 #505：子物体按**局部** position 取景，父级在 x=100 时镜头飞到原点。
        let mut workspace = Workspace::load_fixture(&scene_fixture(json!([
            { "id": 1, "name": "Root", "parent": null,
              "transform": {"position":[100,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
              "components": {} },
            { "id": 2, "name": "Child", "parent": 1,
              "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
              "components": {} }
        ])));
        workspace.select(1, false);
        workspace.frame_selection();
        assert!(
            (workspace.camera.target[0] - 100.0).abs() < 1e-4,
            "子物体应在世界 x=100 取景，实际 {:?}",
            workspace.camera.target
        );
    }

    #[test]
    fn framing_a_large_object_pulls_the_camera_back() {
        // 100 单位宽的墙：旧实现固定按半径 2 算距离（≈4.8），会直接穿进去。
        let mut workspace = Workspace::load_fixture(&scene_fixture(json!([
            { "id": 1, "name": "Wall", "parent": null,
              "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[100,1,1]},
              "components": {} }
        ])));
        workspace.select(0, false);
        workspace.frame_selection();
        assert!(
            workspace.camera.distance > 100.0,
            "大物体要退到能框住它，实际 {}",
            workspace.camera.distance
        );
    }

    #[test]
    fn framing_a_tiny_object_keeps_a_workable_distance() {
        let mut workspace = Workspace::load_fixture(&scene_fixture(json!([
            { "id": 1, "name": "Dot", "parent": null,
              "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[0.01,0.01,0.01]},
              "components": {} }
        ])));
        workspace.select(0, false);
        workspace.frame_selection();
        assert!(
            (0.4..2.0).contains(&workspace.camera.distance),
            "小物体不该离得太远，实际 {}",
            workspace.camera.distance
        );
    }

    #[test]
    fn frame_all_uses_world_bounds_not_local_positions() {
        // 父在 x=100、子局部 x=50（世界 150）：并集中心应是 125，而不是 75。
        let mut workspace = Workspace::load_fixture(&scene_fixture(json!([
            { "id": 1, "name": "Root", "parent": null,
              "transform": {"position":[100,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
              "components": {} },
            { "id": 2, "name": "Child", "parent": 1,
              "transform": {"position":[50,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
              "components": {} }
        ])));
        workspace.frame_all();
        assert!(
            (workspace.camera.target[0] - 125.0).abs() < 1e-3,
            "world bounds 中心应为 125，实际 {:?}",
            workspace.camera.target
        );
    }

    #[test]
    fn framing_an_empty_selection_does_not_move_the_camera() {
        let mut workspace = Workspace::load_fixture(&fixture());
        let before = workspace.camera.target;
        workspace.clear_selection();
        workspace.frame_selection();
        assert_eq!(workspace.camera.target, before);
        assert!(workspace.selection_world_bounds().is_none());
    }

    #[test]
    fn new_document_from_template() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.new_document(DocKind::Prefab, Some("camera"));
        assert_eq!(workspace.docs.len(), 2);
        assert_eq!(workspace.document().kind, DocKind::Prefab);
        assert!(workspace
            .document()
            .path
            .as_deref()
            .unwrap()
            .ends_with("camera.prefab.json"));
    }

    #[test]
    fn two_documents_from_the_same_template_never_share_a_path() {
        let project = TempProject::new("template-paths");
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.new_document(DocKind::Prefab, Some("camera"));
        workspace.new_document(DocKind::Prefab, Some("camera"));

        let first = workspace.docs[workspace.docs.len() - 2]
            .path
            .clone()
            .unwrap();
        let second = workspace.docs[workspace.docs.len() - 1]
            .path
            .clone()
            .unwrap();
        assert_eq!(first, "assets/prefabs/camera.prefab.json");
        assert_eq!(second, "assets/prefabs/camera-2.prefab.json");
    }

    #[test]
    fn a_template_document_never_lands_on_an_existing_file() {
        let project = TempProject::new("template-existing");
        std::fs::write(
            project.root.join("assets/prefabs/camera.prefab.json"),
            r#"{"version":3,"entities":[]}"#,
        )
        .unwrap();
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.load_project(project.root.clone()).unwrap();
        workspace.new_document(DocKind::Prefab, Some("camera"));

        assert_eq!(
            workspace.document().path.as_deref(),
            Some("assets/prefabs/camera-2.prefab.json")
        );
        // 原文件没被读进来当模板产物，内容也没被碰过。
        let saved = std::fs::read_to_string(project.root.join("assets/prefabs/camera.prefab.json"))
            .unwrap();
        assert!(saved.contains("\"entities\""));
    }

    #[test]
    fn commands_respect_enabled_state() {
        let mut workspace = Workspace::load_fixture(&fixture());
        assert!(!workspace.is_enabled(Command::Undo));
        workspace.run(Command::SelectAll);
        assert_eq!(workspace.selection.len(), 2);
        workspace.run(Command::DuplicateEntity);
        assert!(workspace.can_undo());
        assert!(workspace.is_enabled(Command::Undo));
        workspace.run(Command::Play);
        assert!(workspace.is_enabled(Command::Stop));
        assert!(!workspace.is_enabled(Command::Play));
    }

    #[test]
    fn undo_snapshot_roundtrip_for_reparent() {
        let mut workspace = Workspace::load_fixture(&fixture());
        workspace.reparent(1, None, Some(1));
        assert!(workspace.document().doc.entities[1].parent.is_none());
        workspace.undo();
        assert_eq!(workspace.document().doc.entities[1].parent, Some(1));
    }

    fn index_named(workspace: &Workspace, name: &str) -> usize {
        workspace
            .document()
            .doc
            .entities
            .iter()
            .position(|entity| entity.display_name() == name)
            .unwrap_or_else(|| panic!("missing entity {name}"))
    }

    #[test]
    fn selection_roots_drops_nested_entities() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let detail = index_named(&workspace, "Detail");
        workspace.select(cube, false);
        workspace.select(detail, true);
        assert_eq!(workspace.selection_roots(), vec![cube]);
    }

    #[test]
    fn subtree_expand_toggles_all_descendant_ids() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let detail = index_named(&workspace, "Detail");
        let light = index_named(&workspace, "Directional Light");
        let cube_id = workspace.document().doc.entities[cube].id.unwrap();
        let detail_id = workspace.document().doc.entities[detail].id.unwrap();
        let light_id = workspace.document().doc.entities[light].id.unwrap();

        workspace.set_subtree_expanded(cube, true);
        assert!(workspace.expanded.contains(&cube_id));
        assert!(workspace.expanded.contains(&detail_id));
        assert!(!workspace.expanded.contains(&light_id));

        workspace.set_subtree_expanded(cube, false);
        assert!(!workspace.expanded.contains(&cube_id));
        assert!(!workspace.expanded.contains(&detail_id));
    }

    #[test]
    fn reparent_selection_moves_all_roots_in_one_step() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let camera = index_named(&workspace, "Main Camera");
        let camera_id = workspace.document().doc.entities[camera].id;
        let cube_parent = workspace.document().doc.entities[cube].parent;
        let light_parent = workspace.document().doc.entities[light].parent;

        workspace.select(cube, false);
        workspace.select(light, true);
        assert_eq!(workspace.reparent_selection(Some(camera)), 2);
        assert_eq!(workspace.document().doc.entities[cube].parent, camera_id);
        assert_eq!(workspace.document().doc.entities[light].parent, camera_id);
        assert_eq!(workspace.document().undo.history().len(), 1);

        workspace.undo();
        assert_eq!(workspace.document().doc.entities[cube].parent, cube_parent);
        assert_eq!(
            workspace.document().doc.entities[light].parent,
            light_parent
        );
    }

    #[test]
    fn reparent_selection_rejects_dropping_into_own_descendant() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let detail = index_named(&workspace, "Detail");
        workspace.select(cube, false);
        workspace.select(detail, true);
        assert_eq!(workspace.reparent_selection(Some(detail)), 0);
        assert!(!workspace.can_undo());
    }

    /// #525：拖到某行的下边缘时整组一起插到后面，且保持相对顺序。
    #[test]
    fn dragging_a_group_below_a_row_keeps_the_group_order() {
        let mut workspace = Workspace::sample();
        // 末尾加一个独立的根实体当落点：Cube 组本来就在相机后面，插回去是空操作。
        workspace.add_entity("empty");
        let anchor = workspace.document().doc.entities.len() - 1;
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        workspace.select(cube, false);
        workspace.select(light, true);

        let names = |workspace: &Workspace| -> Vec<String> {
            workspace
                .document()
                .doc
                .entities
                .iter()
                .map(|entity| entity.display_name())
                .collect()
        };
        let anchor_parent = workspace.document().doc.entities[anchor].parent;
        let before = names(&workspace);
        let steps = workspace.document().undo.history().len();

        assert_eq!(workspace.reparent_selection_at(anchor, false), 2);
        // 两个根都挂到落点所在层级。
        let moved_cube = index_named(&workspace, "Cube");
        let moved_light = index_named(&workspace, "Directional Light");
        assert_eq!(
            workspace.document().doc.entities[moved_cube].parent,
            anchor_parent
        );
        assert_eq!(
            workspace.document().doc.entities[moved_light].parent,
            anchor_parent
        );
        // 整组在落点之后，顺序不变（Cube 仍在 Light 前）。
        let moved_anchor = index_named(&workspace, "GameObject");
        assert!(
            moved_cube > moved_anchor && moved_light > moved_anchor,
            "cube={moved_cube} light={moved_light} anchor={moved_anchor} order={:?}",
            names(&workspace)
        );
        assert!(moved_cube < moved_light, "组内顺序必须保留");
        // 子树跟着走：Detail 仍是 Cube 的子级。
        let detail = index_named(&workspace, "Detail");
        let cube_id = workspace.document().doc.entities[moved_cube].id;
        assert_eq!(workspace.document().doc.entities[detail].parent, cube_id);
        assert_eq!(
            workspace.document().undo.history().len(),
            steps + 1,
            "一次拖拽 = 一步撤销"
        );

        workspace.undo();
        assert_eq!(names(&workspace), before, "撤销要还原顺序与父级");
    }

    #[test]
    fn dragging_a_group_above_a_row_inserts_before_it() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        let camera = index_named(&workspace, "Main Camera");
        workspace.select(cube, false);
        workspace.select(light, true);

        assert_eq!(workspace.reparent_selection_at(camera, true), 2);
        let moved_cube = index_named(&workspace, "Cube");
        let moved_light = index_named(&workspace, "Directional Light");
        let moved_camera = index_named(&workspace, "Main Camera");
        assert!(moved_cube < moved_camera && moved_light < moved_camera);
        assert!(moved_cube < moved_light);
    }

    #[test]
    fn a_group_cannot_be_dropped_between_its_own_children() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let detail = index_named(&workspace, "Detail");
        workspace.select(cube, false);
        assert_eq!(workspace.reparent_selection_at(detail, true), 0);
        assert!(!workspace.can_undo());
    }

    #[test]
    fn copy_paste_keeps_subtree_and_attaches_parent() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        workspace.select(cube, false);
        assert_eq!(workspace.copy_selection(), 2, "Cube + Detail");

        let before = workspace.document().doc.entities.len();
        let pasted = workspace.paste_entities(None);
        assert_eq!(pasted.len(), 1);
        assert_eq!(workspace.document().doc.entities.len(), before + 2);
        assert_eq!(workspace.selection, pasted);
        let root = &workspace.document().doc.entities[pasted[0]];
        assert_eq!(root.display_name(), "Cube (1)");
        assert!(root.parent.is_none());

        let pasted = workspace.paste_entities(Some(light));
        let light_id = workspace.document().doc.entities[light].id;
        assert_eq!(
            workspace.document().doc.entities[pasted[0]].parent,
            light_id
        );
    }

    #[test]
    fn cut_removes_source_then_paste_restores() {
        let mut workspace = Workspace::sample();
        let before = workspace.document().doc.entities.len();
        let detail = index_named(&workspace, "Detail");
        workspace.select(detail, false);
        assert_eq!(workspace.cut_selection(), 1);
        assert!(workspace
            .document()
            .doc
            .entities
            .iter()
            .all(|entity| entity.display_name() != "Detail"));
        assert!(workspace.entity_clipboard.as_ref().unwrap().cut);

        let pasted = workspace.paste_entities(None);
        assert_eq!(
            workspace.document().doc.entities[pasted[0]].display_name(),
            "Detail"
        );
        workspace.undo();
        workspace.undo();
        assert_eq!(workspace.document().doc.entities.len(), before);
        assert_eq!(
            workspace.document().doc.entities[detail].display_name(),
            "Detail"
        );
    }

    #[test]
    fn copy_preserves_prefab_instance_link() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        workspace.document_mut().doc.entities[cube]
            .components
            .insert(
                PREFAB_LINK_COMPONENT.to_string(),
                serde_json::json!({ "prefab": "assets/prefabs/cube.prefab.json" }),
            );
        workspace.select(cube, false);
        workspace.copy_selection();
        let pasted = workspace.paste_entities(None);
        assert_eq!(
            workspace.document().doc.entities[pasted[0]].prefab_link(),
            Some("assets/prefabs/cube.prefab.json")
        );
    }

    #[test]
    fn duplicate_multiple_roots_copies_each_subtree() {
        let mut workspace = Workspace::sample();
        let cube = index_named(&workspace, "Cube");
        let light = index_named(&workspace, "Directional Light");
        workspace.select(cube, false);
        workspace.select(light, true);
        let before = workspace.document().doc.entities.len();
        workspace.run(Command::DuplicateEntity);
        assert_eq!(workspace.document().doc.entities.len(), before + 3);
        assert_eq!(workspace.selection.len(), 2);
    }

    #[test]
    fn entity_clipboard_commands_enable_state() {
        let mut workspace = Workspace::sample();
        assert!(!workspace.is_enabled(Command::PasteEntity));
        workspace.select(0, false);
        assert!(workspace.is_enabled(Command::CopyEntity));
        workspace.run(Command::CopyEntity);
        assert!(workspace.is_enabled(Command::PasteEntity));
        workspace.run(Command::CutEntity);
        assert!(workspace.entity_clipboard.as_ref().unwrap().cut);
    }

    #[test]
    fn snap_settings_round_to_the_configured_steps() {
        let snap = SnapSettings::default();
        assert!(!snap.enabled, "吸附默认关，靠工具栏或 Ctrl 临时开");
        assert_eq!(snap.snapped_translate(0.31), 0.25);
        assert_eq!(snap.snapped_translate(0.38), 0.5);
        assert_eq!(snap.snapped_translate(-0.38), -0.5);
        let thirty = 30.0f32.to_radians();
        assert!((snap.snapped_radians(thirty) - thirty).abs() < 1e-6);
        assert!((snap.snapped_radians(thirty + 5.0f32.to_radians()) - thirty).abs() < 1e-6);
        assert_eq!(snap.snapped_scale(1.4), 1.0);
        assert_eq!(snap.snapped_scale(1.6), 2.0);
    }

    #[test]
    fn snap_settings_never_round_to_zero_scale() {
        let snap = SnapSettings {
            scale: 0.0,
            ..SnapSettings::default()
        };
        assert_eq!(snap.snapped_scale(0.3), 0.3, "步长为 0 时原样返回");
        let coarse = SnapSettings {
            scale: 10.0,
            ..SnapSettings::default()
        };
        assert_eq!(coarse.snapped_scale(0.2), 0.01, "吸附到 0 时夹到 0.01");
    }

    #[test]
    fn scrubbing_a_whole_vector_is_one_undo_step() {
        let mut workspace = Workspace::sample();
        let entity = workspace.document().doc.id_of_index(1).unwrap();
        workspace.set_transform_vector(entity, "position", [1.0, 2.0, 3.0]);
        let transform = workspace.document().doc.entities[1].transform;
        assert_eq!(transform.position, [1.0, 2.0, 3.0]);
        workspace.undo();
        assert_ne!(
            workspace.document().doc.entities[1].transform.position,
            [1.0, 2.0, 3.0],
            "一次撤销就要回到拖动前"
        );
        assert!(
            !workspace.can_undo(),
            "整行拖动只能留一个撤销步，实际 {:?}",
            workspace.undo_history()
        );
    }

    #[test]
    fn set_transform_rotation_normalizes_and_undoes_in_one_step() {
        let mut workspace = Workspace::sample();
        let entity = workspace.document().doc.id_of_index(0).unwrap();
        let before = workspace.document().doc.entities[0].transform.rotation;
        workspace.set_transform_rotation(entity, [0.0, 0.0, 3.0, 3.0]);
        let after = workspace.document().doc.entities[0].transform.rotation;
        let length = (after.iter().map(|v| v * v).sum::<f32>()).sqrt();
        assert!((length - 1.0).abs() < 1e-5, "写入前归一化: {after:?}");
        workspace.undo();
        assert_eq!(
            workspace.document().doc.entities[0].transform.rotation,
            before
        );
    }

    #[test]
    fn set_transform_rotation_ignores_degenerate_input() {
        let mut workspace = Workspace::sample();
        let entity = workspace.document().doc.id_of_index(0).unwrap();
        let before = workspace.document().doc.entities[0].transform.rotation;
        workspace.set_transform_rotation(entity, [0.0; 4]);
        assert_eq!(
            workspace.document().doc.entities[0].transform.rotation,
            before
        );
        assert!(!workspace.can_undo(), "非法输入不该产生撤销步");
    }

    #[test]
    fn set_transform_rotation_many_writes_every_selection() {
        let mut workspace = Workspace::sample();
        let ids: Vec<u32> = (0..2)
            .map(|index| workspace.document().doc.id_of_index(index).unwrap())
            .collect();
        let quat = crate::viewport_math::euler_degrees_to_quat([0.0, 90.0, 0.0]);
        workspace.set_transform_rotation_many(&ids, quat);
        for index in 0..2 {
            let rotation = workspace.document().doc.entities[index].transform.rotation;
            for axis in 0..4 {
                assert!((rotation[axis] - quat[axis]).abs() < 1e-5, "{rotation:?}");
            }
        }
        workspace.undo();
        let restored = workspace.document().doc.entities[0].transform.rotation;
        assert!(
            (restored[3] - 1.0).abs() < 1e-5,
            "一次 undo 回到两个实体都未改的状态"
        );
    }

    #[test]
    fn recovery_snapshot_round_trips_only_dirty_documents() {
        let project = TempProject::new("recovery-roundtrip");
        let mut workspace = Workspace::sample();
        workspace.project_root = Some(project.root.clone());
        workspace.document_mut().path = Some("assets/scenes/recovery.scene.json".to_string());
        workspace
            .document_mut()
            .doc
            .add_named_child(None, "recovered");
        workspace.document_mut().dirty = true;
        let expected = workspace.document().doc.clone();
        // 干净文档不进快照。
        workspace.docs.push(Document::new(
            DocKind::Scene,
            Some("assets/scenes/clean.scene.json".to_string()),
            SceneDoc::default(),
        ));

        assert_eq!(workspace.write_recovery().unwrap(), 1);

        let mut fresh = Workspace::sample();
        fresh.project_root = Some(project.root.clone());
        assert_eq!(fresh.recovery_pending().unwrap(), 1);
        assert_eq!(fresh.recover_unsaved().unwrap(), 1);
        assert_eq!(fresh.recovery_pending().unwrap(), 0);
        let recovered = fresh
            .docs
            .iter()
            .find(|document| document.path.as_deref() == Some("assets/scenes/recovery.scene.json"))
            .expect("恢复出来的文档要出现在工作区");
        assert!(recovered.dirty, "恢复的文档必须保持 dirty");
        assert_eq!(recovered.doc, expected);
    }

    #[test]
    fn recovery_replaces_an_already_open_document_without_a_second_tab() {
        let project = TempProject::new("recovery-replace");
        let mut workspace = Workspace::sample();
        workspace.project_root = Some(project.root.clone());
        workspace.document_mut().path = Some("assets/scenes/open.scene.json".to_string());
        workspace
            .document_mut()
            .doc
            .add_named_child(None, "from-recovery");
        workspace.document_mut().dirty = true;
        workspace.write_recovery().unwrap();

        let mut fresh = Workspace::sample();
        fresh.project_root = Some(project.root.clone());
        fresh.document_mut().path = Some("assets/scenes/open.scene.json".to_string());
        let tabs = fresh.docs.len();

        assert_eq!(fresh.recover_unsaved().unwrap(), 1);
        assert_eq!(
            fresh.docs.len(),
            tabs,
            "同路径文档应就地替换，不能多开一个 tab"
        );
        assert!(fresh.document().dirty);
        assert!(fresh
            .document()
            .doc
            .entities
            .iter()
            .any(|entity| entity.name.as_deref() == Some("from-recovery")));
    }

    #[test]
    fn discarding_recovery_removes_the_snapshot() {
        let project = TempProject::new("recovery-discard");
        let mut workspace = Workspace::sample();
        workspace.project_root = Some(project.root.clone());
        workspace.document_mut().dirty = true;
        workspace.write_recovery().unwrap();
        assert_eq!(workspace.recovery_pending().unwrap(), 1);

        workspace.discard_recovery().unwrap();
        assert_eq!(workspace.recovery_pending().unwrap(), 0);
        assert!(!project
            .root
            .join(crate::recovery::RECOVERY_DIR)
            .join(crate::recovery::RECOVERY_FILE)
            .exists());
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    #[test]
    fn hiding_a_parent_hides_its_subtree_without_touching_the_document() {
        let mut workspace = Workspace::sample();
        // sample: 0 = Main Camera, 1 = Cube, 2 = Directional Light, 3 = Cube/Detail。
        workspace.set_entity_hidden(1, true);

        assert!(workspace.is_entity_hidden(1));
        assert!(workspace.is_entity_hidden(3));
        assert!(!workspace.is_entity_hidden(0));
        assert!(!workspace.document().dirty, "可见性不是作者数据");

        workspace.set_entity_hidden(3, false);
        assert!(workspace.is_entity_hidden(1));
        assert!(!workspace.is_entity_hidden(3), "子级可以单独重新显示");
    }

    #[test]
    fn isolate_keeps_the_selection_and_its_subtree() {
        let mut workspace = Workspace::sample();
        workspace.select(1, false);
        workspace.set_isolate(true);

        assert!(!workspace.is_entity_hidden(1));
        assert!(!workspace.is_entity_hidden(3), "选中父级时子树一起保留");
        assert!(workspace.is_entity_hidden(0));
        assert!(workspace.is_entity_hidden(2));

        workspace.show_all_entities();
        assert!(!workspace.isolate);
        assert!(workspace.hidden_entity_ids().is_empty());
    }

    #[test]
    fn isolate_requires_a_selection_and_resets_when_switching_documents() {
        let mut workspace = Workspace::sample();
        workspace.set_isolate(true);
        assert!(!workspace.isolate, "空选择不能把整场隔离成黑色");

        workspace.select(1, false);
        workspace.set_isolate(true);
        assert!(workspace.isolate);
        workspace.docs.push(Document::new(
            DocKind::Scene,
            Some("assets/scenes/other.scene.json".to_string()),
            SceneDoc::default(),
        ));
        workspace.set_active(1);
        assert!(!workspace.isolate, "切文档自动退出 Isolate");
    }
}
