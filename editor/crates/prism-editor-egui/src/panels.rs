//! 面板主体：层级 / 检查器 / 资产 / 文档 / 日志 / 撤销历史 / 设置类窗口。
//!
//! 数据与命令全部走 `prism-editor-core`（与 Web 编辑器同一份
//! 文档模型 / schema / 撤销栈），这里只负责画 egui 控件。

use egui::{Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use prism_editor_core::settings::{
    matching_fields, SettingsCache, PROFILE_CHOICES, TARGET_CHOICES,
};
use prism_editor_core::{
    format_bytes, AssetKind, AssetPreview, EntityJson, Locale, LogLevel, LogSource, Preferences,
    ProjectSettings, SceneDoc, Workspace, THEME_PRESETS,
};
use prism_engine::scene::systems::terrain::SculptMode;
use serde_json::{json, Value};

use crate::viewport::{set_terrain_brush_settings, terrain_brush_settings};
use crate::{diagnostics::Diagnostic, theme, widgets};
use egui_phosphor::regular as icons;

/// 在系统文件管理器里定位一个项目内资产（引擎的 `Reveal in Finder` / `Show in Explorer`）。
///
/// Windows 的 `explorer /select,<path>` 成功时也可能返回非零退出码，所以这里只检查能否
/// 启动进程，不检查退出码。
pub fn reveal_asset(workspace: &Workspace, path: &str) -> Result<(), String> {
    let root = workspace
        .project_root
        .clone()
        .ok_or_else(|| "no project root; cannot reveal assets".to_string())?;
    let absolute = root.join(path);
    if !absolute.exists() {
        return Err(format!("{} does not exist", absolute.display()));
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        command.arg(format!("/select,{}", absolute.display()));
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(&absolute);
        command
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(absolute.parent().unwrap_or(absolute.as_path()));
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot open file manager: {error}"))
}

/// 资产类型 → Phosphor 图标。
///
/// 引擎的 Project 窗口按类型换图标；之前这里直接写 `AssetKind::label()`
/// 的中文类别词（"模型" / "数据" / "包"…），窄面板下会和文件名挤在一起，
/// 看起来像乱码。
fn asset_kind_icon(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Model => icons::CUBE,
        AssetKind::Texture => icons::IMAGE,
        AssetKind::Env => icons::GLOBE,
        AssetKind::Scene => icons::TREE_STRUCTURE,
        AssetKind::Prefab => icons::PACKAGE,
        AssetKind::Package => icons::ARCHIVE_BOX,
        AssetKind::Material => icons::PALETTE,
        AssetKind::Audio => icons::MUSIC_NOTES,
        AssetKind::Font => icons::TEXT_AA,
        AssetKind::Data => icons::FILE_TEXT,
        AssetKind::Other => icons::FILE,
    }
}

/// 层级树行高：比资产 / 文档列表更紧凑（引擎的 Hierarchy 也是）。
pub fn tree_row_height(compact: bool) -> f32 {
    if compact {
        15.0
    } else {
        18.0
    }
}

/// 列表行高：Preferences 的「Compact layout」直接决定它。
pub fn row_height(compact: bool) -> f32 {
    if compact {
        16.0
    } else {
        20.0
    }
}

/// 列表行（桌面 / Web 风格）：整行底色 + 文本，用于资产 / 文档这类列表。
fn list_row(
    ui: &mut Ui,
    colors: &prism_editor_core::ThemeColors,
    selected: bool,
    compact: bool,
    text: &str,
) -> egui::Response {
    use prism_editor_core::ThemeToken;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_height(compact)),
        egui::Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            Some(theme::color32(colors, ThemeToken::Selection))
        } else if response.hovered() {
            Some(theme::color32(colors, ThemeToken::RowHover))
        } else {
            None
        };
        if let Some(bg) = bg {
            ui.painter().rect_filled(rect, 0.0, bg);
        }
    }
    let text_color = theme::color32(
        colors,
        if selected {
            ThemeToken::OnAccent
        } else {
            ThemeToken::Text
        },
    );
    // 用 painter 直接画文字：`Label` 会参与命中测试，把整行的点击吃掉。
    ui.painter().text(
        egui::pos2(rect.left() + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(11.5),
        text_color,
    );
    response
}

/// 无底色的图标按钮（层级的眼睛 / 展开箭头：只有图标，没有按钮框）。
fn flat_icon(ui: &mut Ui, glyph: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(glyph).size(11.0)).frame(false))
}

/// 同 [`flat_icon`]，但可以画成灰色（层级里的「祖先被关掉」提示）。
fn flat_icon_dimmed(ui: &mut Ui, glyph: &str, dimmed: bool) -> egui::Response {
    let mut text = RichText::new(glyph).size(11.0);
    if dimmed {
        text = text.weak();
    }
    ui.add(egui::Button::new(text).frame(false))
}

/// 资产面板的排序列（Project 窗口的列头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AssetSort {
    #[default]
    Name,
    Kind,
    Size,
}

/// 资产列表排序：目录由调用方先画，这里只排文件。
///
/// 排序键相同时回落到「文件名 → 完整路径」，保证同一份目录每次刷新顺序一样
/// （`scan_assets` 的顺序取决于文件系统，不能依赖）。
pub fn sort_assets(files: &mut [(String, AssetKind, u64)], sort: AssetSort, descending: bool) {
    files.sort_by(|a, b| {
        let name = |entry: &(String, AssetKind, u64)| {
            entry
                .0
                .rsplit('/')
                .next()
                .unwrap_or(&entry.0)
                .to_lowercase()
        };
        let primary = match sort {
            AssetSort::Name => std::cmp::Ordering::Equal,
            AssetSort::Kind => (a.1 as u8).cmp(&(b.1 as u8)),
            AssetSort::Size => a.2.cmp(&b.2),
        };
        let order = primary
            .then_with(|| name(a).cmp(&name(b)))
            .then_with(|| a.0.cmp(&b.0));
        if descending {
            order.reverse()
        } else {
            order
        }
    });
}

/// 就地重命名的目标路径：同一个目录 + 新的文件名。
fn rename_target(from: &str, file_name: &str) -> Option<String> {
    let file_name = file_name.trim();
    if file_name.is_empty() || file_name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        return None;
    }
    match from.rsplit_once('/') {
        Some((folder, _)) => Some(format!("{folder}/{file_name}")),
        None => Some(file_name.to_string()),
    }
}

/// Offline Tools 面板的表单（运行参数，不进文档、不持久化）。
#[derive(Debug, Clone)]
pub struct OfflineToolsState {
    pub gi_output: String,
    pub gi_rays: u32,
    pub gi_bounces: u32,
    pub heightmap_output: String,
    pub heightmap_width: u32,
    pub heightmap_height: u32,
    pub heightmap_seed: u64,
    pub heightmap_iterations: u32,
    pub heightmap_sea_level: f32,
    pub pack_output: String,
    pub pack_compression: u32,
}

impl Default for OfflineToolsState {
    fn default() -> Self {
        // 默认值与 `prism-build-pipeline --help` 一致：照着默认值点下去就是文档里的产物。
        Self {
            gi_output: "assets/gi/probe_volume.bin".to_string(),
            gi_rays: 64,
            gi_bounces: 3,
            heightmap_output: "assets/heightmaps/terrain.raw".to_string(),
            heightmap_width: 512,
            heightmap_height: 512,
            heightmap_seed: 0x5EED,
            heightmap_iterations: 100,
            heightmap_sea_level: 0.7,
            pack_output: "assets/scenes.pak".to_string(),
            pack_compression: 3,
        }
    }
}

/// 层级搜索的匹配方式（Unity 的 `SearchMode`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HierarchySearchMode {
    /// 按实体名字子串匹配（默认）。
    #[default]
    Name,
    /// 按组件类型短名匹配，例如 `PointLight` / `camera`。
    Component,
}

impl HierarchySearchMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Component => "Type",
        }
    }

    fn matches(self, entity: &EntityJson, filter: &str) -> bool {
        match self {
            Self::Name => entity.display_name().to_lowercase().contains(filter),
            Self::Component => entity.components.keys().any(|component| {
                // schema 里存的是完整 Rust 路径；用户搜的是短名。
                let short = component.rsplit("::").next().unwrap_or(component);
                short.to_lowercase().contains(filter)
            }),
        }
    }
}

/// egui 侧的临时 UI 状态（不进文档、不进撤销栈）。
#[derive(Default)]
/// 检查器里显示骨骼用的一行（名字 + 父级下标）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkeletonJointRow {
    pub name: String,
    pub parent: Option<usize>,
}

#[derive(Default)]
pub struct UiState {
    pub inspector_search: String,
    /// 已加载剪辑的时长（`Animator.clip_path` → 秒），由 app 每帧从引擎的
    /// `SkeletonLibrary` 填进来，检查器据此给时间轴划范围（#560 P0-4）。
    pub clip_durations: std::collections::HashMap<String, f32>,
    /// 已加载剪辑的关键帧时间点（`clip_path` → 升序秒数）。时间轴据此画刻度，
    /// 一眼看出这个剪辑在哪些时刻会变（#560）。
    pub clip_keys: std::collections::HashMap<String, Vec<f32>>,
    /// 已加载剪辑的 Notify（`clip_path` → `(时间, 名字)`，按时间升序）。
    pub clip_notifies: std::collections::HashMap<String, Vec<(f32, String)>>,
    /// 已加载骨架的关节列表（`SkeletonRoot.skeleton_path` → 关节）。检查器
    /// 用它画骨骼树；没有数据的路径不显示（拒绝画一个猜出来的层次）。
    pub skeleton_joints: std::collections::HashMap<String, Vec<SkeletonJointRow>>,
    /// 检查器锁定：按**实体 id** 钉住内容（Unity 的 Inspector Lock，#418）。
    ///
    /// 存 id 而不是下标：删除 / 重排实体后仍然指向同一个对象；对象全被删掉时
    /// 下一帧自动解锁、回落到文档选择。
    pub inspector_lock: Option<Vec<u32>>,
    pub hierarchy_search: String,
    /// 层级搜索按名字还是按组件类型匹配（#417）。
    pub hierarchy_search_mode: HierarchySearchMode,
    /// 层级 Shift 区间选择的锚点（按实体下标；与资产面板的 `asset_anchor` 同理）。
    pub hierarchy_anchor: Option<usize>,
    /// 层级锁定：按**实体 id** 钉住高亮的行（Unity 的 Hierarchy Lock，#417）。
    ///
    /// 只锁可见的选中高亮，不锁文档 / 展开状态：在 Scene、Project 或资产面板
    /// 里换选择时，层级仍停在锁定时的那几行，便于对照。层级内部点选会同步更新
    /// 这份快照；锁住的对象全被删掉时自动解锁。
    pub hierarchy_lock: Option<Vec<u32>>,
    /// 下一帧把这一行滚到可视区域中央（Find in Hierarchy / 资产 Ping）。
    pub hierarchy_reveal: Option<u32>,
    pub asset_search: String,
    /// 历史面板的排序：`true` = 最新在上（引擎的 `m_ShowLatestFirst`）。
    pub history_latest_first: bool,
    /// 视口渲染分辨率倍率（高 DPI 屏上可降到 0.5 省 GPU）。
    pub viewport_scale: f32,
    /// 瓦片地图格子编辑器的调色板大小（左键循环 `0..=palette`，#529）。
    pub tilemap_palette: u32,
    /// Scene 视图左上角的浮层工具条是否折叠（#421）。
    pub scene_overlay_collapsed: bool,
    /// 快捷键改键：正在等待用户按键的命令（#421）。
    pub shortcut_capture: Option<prism_editor_core::Command>,
    /// Project 面板的列表 / 网格视图（#419）。
    pub asset_view: AssetView,
    /// Project 面板的一列 / 两列（左目录树 + 右内容，#419）。
    pub asset_columns: AssetColumns,
    pub log_levels: Vec<LogLevel>,
    pub log_sources: Vec<LogSource>,
    pub log_search: String,
    pub log_autoscroll: bool,
    /// Console 把重复消息折叠成一行 + 计数（#421）。
    pub log_collapse: bool,
    /// 进入 Play 时清空 Console（#421）。
    pub log_clear_on_play: bool,
    /// Play 中出现 Error / Panic 时自动暂停（#421）。
    pub log_error_pause: bool,
    /// 项目设置面板的搜索词 + 读盘缓存（`#428`：不要每帧读盘）。
    pub settings_search: String,
    pub settings_cache: Option<SettingsCache>,
    pub rename: Option<(usize, String)>,
    /// 就地重命名开始前的文档快照 + 当时的脏标记。
    ///
    /// `preview_rename` 会实时改动文档，提交时用这份快照补**恰好一个**撤销步，
    /// 取消时用它把预览还原（#473）。
    pub rename_base: Option<(SceneDoc, bool)>,
    /// 检查器 Name 框的缓冲（延迟提交，不逐字符写文档）。
    pub name_edit: Option<(usize, String)>,
    /// 重命名刚打开的那一帧要抢一次焦点；之后不能每帧抢，否则
    /// TextEdit 永远保持焦点、点空白处不会退出编辑。
    pub rename_focus: bool,
    pub add_component_query: String,
    pub preset_choice: usize,
    pub status: String,
    pub about_open: bool,
    /// 当前打开的输入框（保存 / 新建目录 / 预制体命名…），None = 关闭。
    pub prompt: Option<Prompt>,
    /// 资产面板里当前选中的资产（引擎的 Project 选择，支持多选）。
    ///
    /// 最后一项是「主项」（Inspector / 详情 / 单路径操作看它）。
    /// 选中资产上一次算好的详情 + 时间戳：详情会扫磁盘上的文档，不能每帧重算（#514）。
    pub asset_details: Option<(String, prism_editor_core::AssetDetails)>,
    pub asset_details_at: Option<std::time::Instant>,
    /// Animation Graph 面板的节点布局缓存：(图路径, 布局)。
    pub anim_graph_layout: Option<(String, prism_editor_core::AnimGraphLayout)>,
    /// Animation Graph 面板的跨帧输入缓冲（新增节点 / 正在编辑的骨骼掩码）。
    pub anim_graph_edit: AnimGraphEditState,
    /// 状态机面板「+ Condition」正在填的条件草稿（None = 表单关闭）。
    pub anim_sm_condition: Option<StateConditionDraft>,
    pub asset_selection: Vec<String>,
    /// 区间选择的锚点：Shift+单击从它选到目标（#486）。
    pub asset_anchor: Option<String>,
    /// 资产面板当前目录（`assets` 开头）、后退栈与游标。
    pub asset_folder: String,
    pub asset_history: Vec<String>,
    pub asset_history_pos: usize,
    /// 资产面板的排序：先按哪一列、是否降序（目录永远排在文件前面）。
    pub asset_sort: AssetSort,
    pub asset_sort_desc: bool,
    /// 就地重命名：(资产路径, 正在编辑的文件名)。`None` = 没有在改名。
    pub asset_rename: Option<(String, String)>,
    /// 就地重命名刚打开的那一帧抢焦点（同层级树的 `rename_focus`）。
    pub asset_rename_focus: bool,
    /// 资产右键菜单请求 App 执行的操作（面板拿不到 App 的方法）。
    pub pending_find_references: Option<(String, bool)>,
    /// 正在拖的 docking 标签页：(tab, 源 leaf id)。
    pub tab_drag: Option<(String, String)>,
    /// 拖拽落点：(目标 leaf id, 区域, 比例)。
    pub tab_drop: Option<(String, prism_editor_core::dock::DockZone, f32)>,
    /// 变换剪贴板：(字段, 分量值) —— 引擎的 Copy / Paste Transform。
    pub transform_clip: Option<(String, Vec<f32>)>,
    /// Preferences 的镜像：列表行高（紧凑布局）。
    pub compact: bool,
    /// Preferences 的镜像：删除实体前是否弹确认。
    pub confirm_delete: bool,
    /// 等待确认删除的实体下标（None = 没有待确认的删除）。
    pub pending_entity_delete: Option<Vec<usize>>,
    /// 层级拖拽：正在拖的实体下标 + 悬停目标。
    pub dragging: Option<usize>,
    pub drop_target: Option<(usize, DropZone)>,
    pub shortcuts_open: bool,
    /// 等待确认删除的场景 / 预制体（相对项目根路径）。删除不可撤销，
    /// 资产面板与文档面板共用同一个确认框。
    pub pending_doc_delete: Option<String>,
    /// 保存时发现磁盘上的版本变了，等用户选「覆盖 / 重新载入 / 取消」（#452）。
    pub pending_save_conflict: Option<usize>,
    /// Build 面板里点击的构建请求，由 `EditorApp` 在同一帧取走并真正启动。
    pub pending_build: Option<crate::build::BuildKind>,
    /// 当前是否有构建子进程在跑（由 `EditorApp` 每帧同步，用来禁用按钮）。
    pub build_running: bool,
    /// Offline Tools 的表单（GI bake / heightmap / 自定义打包参数）。
    pub offline: OfflineToolsState,
    /// 待确认的破坏性操作（引擎的 `EditorUtility.DisplayDialog`）。
    pub confirm: Option<Confirm>,
    /// 最后点过的面板（引擎的 `focusedWindow`）：快捷键按它分派。
    pub focused_panel: Option<String>,
    /// 局部光源预算（#468）：实际画出来的 / 场景里启用的。
    pub lights_kept: usize,
    pub lights_total: usize,
    /// 被 `LIGHT_MAX` 挤掉的局部光实体 id。
    pub dropped_light_ids: Vec<u32>,
    /// 等待确认删除的资产路径（批量删除时有多项）。
    pub pending_asset_delete: Option<Vec<String>>,
    /// 正在编辑的材质 / JSON 资产（相对路径）与编辑缓冲（#488）。
    pub material_asset: Option<String>,
    pub material_value: Option<serde_json::Value>,
    /// 磁盘上那一份（Revert / dirty 判定用）。
    pub material_saved: Option<serde_json::Value>,
    pub material_error: Option<String>,
    /// 等待用户回答的关闭请求（有未保存改动时才会出现）。
    pub pending_close: Option<CloseTarget>,
    /// 上次非正常退出留下的未保存快照数量（#619）；0 = 没有待恢复内容。
    pub pending_recovery: usize,
    /// Render Graph / Frame Debugger 是否冻结在某一帧（#518）。
    pub render_graph_paused: bool,
    /// 暂停时冻结的报告 JSON；`None` = 还没收到第一份报告。
    pub render_graph_frozen: Option<String>,
    /// Frame Debugger 当前选中的 draw call 下标（#518）。
    pub render_graph_call: Option<usize>,
    /// 用户配置的整帧 GPU 预算（ms）；`None` = 默认 [`FRAME_BUDGET_MS`]（#518）。
    pub render_graph_budget_ms: Option<f32>,
    /// Project 面板请求的音频试听动作（#508），由 `EditorApp` 在同一帧消费。
    pub pending_audio_preview: Option<AudioPreviewAction>,
    /// 当前正在试听的音频资产路径（`None` = 没在播）。
    pub audio_preview_path: Option<String>,
}

/// Project 面板 → `EditorApp` 的音频试听请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPreviewAction {
    Play(String),
    Stop,
}

/// 关闭前确认的目标：某个文档标签，或整个编辑器窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseTarget {
    Document(usize),
    App,
}

/// 需要二次确认的破坏性操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// `File → Clean Build`：只清玩家 workspace 的 target。
    CleanPlayer,
    /// `File → Clean Engine Cache…`：清仓库根 workspace 的 target。
    CleanEngine,
}

/// 一个待确认的对话框（引擎的 `EditorUtility.DisplayDialog`）。
#[derive(Clone)]
pub struct Confirm {
    pub kind: ConfirmKind,
    pub title: String,
    pub message: String,
    pub ok: String,
}

impl Confirm {
    pub fn new(kind: ConfirmKind, title: &str, message: impl Into<String>, ok: &str) -> Self {
        Self {
            kind,
            title: title.to_string(),
            message: message.into(),
            ok: ok.to_string(),
        }
    }
}

/// 二级 / 三级菜单和对话框复用的输入框种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    SaveAs,
    NewFolder,
    SavePrefab,
    PrefabVariant,
    /// 命名布局（Window → Layouts → Save Layout…）。
    LayoutName,
    /// 布局文件路径（Load / Save Layout to File…）。
    LayoutFileSave,
    LayoutFileLoad,
}

#[derive(Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub hint: String,
    pub value: String,
}

impl Prompt {
    pub fn new(kind: PromptKind, title: &str, hint: &str, value: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.to_string(),
            hint: hint.to_string(),
            value: value.into(),
        }
    }
}

/// 层级拖拽的落点：兄弟插入（上 / 下）或挂成子级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    Before,
    After,
    Child,
}

impl UiState {
    /// 选中的多个资产里的「主项」（最后点击的那个）。
    pub fn asset_primary(&self) -> Option<&String> {
        self.asset_selection.last()
    }

    pub fn asset_selected(&self, path: &str) -> bool {
        self.asset_selection.iter().any(|item| item == path)
    }

    /// 普通单击：只选这一个，并把它记成区间锚点。
    pub fn select_asset_only(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.asset_anchor = Some(path.clone());
        self.asset_selection = vec![path];
    }

    /// Ctrl/Cmd+单击：切换单个条目（做法同 Unity 的 `GetNewSelection`）。
    pub fn toggle_asset(&mut self, path: &str) {
        if let Some(index) = self.asset_selection.iter().position(|item| item == path) {
            self.asset_selection.remove(index);
        } else {
            self.asset_selection.push(path.to_string());
        }
        self.asset_anchor = Some(path.to_string());
    }

    /// Shift+单击：从锚点到目标整段选中（按面板里可见的顺序）。
    ///
    /// 锚点不在可见列表里（切过目录 / 被删掉）时退化成普通单击。
    pub fn select_asset_range(&mut self, ordered: &[String], path: &str) {
        let Some(target) = ordered.iter().position(|item| item == path) else {
            self.select_asset_only(path);
            return;
        };
        let anchor = self
            .asset_anchor
            .as_ref()
            .and_then(|anchor| ordered.iter().position(|item| item == anchor))
            .unwrap_or(target);
        let (start, end) = if anchor <= target {
            (anchor, target)
        } else {
            (target, anchor)
        };
        self.asset_selection = ordered[start..=end].to_vec();
    }

    pub fn clear_asset_selection(&mut self) {
        self.asset_selection.clear();
        self.asset_anchor = None;
    }

    /// 删掉一个路径后同步选中状态（保留其它项与锚点）。
    pub fn forget_asset(&mut self, path: &str) {
        self.asset_selection.retain(|item| item != path);
        if self.asset_anchor.as_deref() == Some(path) {
            self.asset_anchor = None;
        }
    }

    pub fn new() -> Self {
        Self {
            log_autoscroll: true,
            // 三个开关默认关：升级后 Console 的显示与行为保持原样。
            log_collapse: false,
            log_clear_on_play: false,
            log_error_pause: false,
            viewport_scale: 1.0,
            asset_folder: "assets".to_string(),
            asset_history: vec!["assets".to_string()],
            asset_history_pos: 0,
            tilemap_palette: 8,
            asset_view: AssetView::List,
            asset_columns: AssetColumns::One,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// 层级
// ---------------------------------------------------------------------------

enum TreeAction {
    Select(usize, bool),
    Toggle(usize),
    ToggleExpand(usize),
    ToggleActive(usize),
    ToggleHidden(usize),
    Delete(usize),
    Duplicate(usize),
    CommitRename(usize, String),
    BeginRename(usize, String),
    /// 双击层级行 = 选中并在 Scene 视图里框住它（Unity 的 `FrameSelected`）。
    Frame(usize),
    /// 重命名输入框每敲一个字符：实时预览 + 标脏（不产生撤销步）。
    PreviewRename(usize, String),
    /// Esc：把预览还原回开始编辑前的状态。
    CancelRename,
    /// 右键空白处：清空选择。
    ClearSelection,
}

/// 层级搜索命中项及其祖先（`path_of` 已包含自身）。
///
/// 搜索必须能看见折叠父级下的对象，否则会出现「明明有这个实体却搜不到」。
fn search_matches(
    doc: &SceneDoc,
    filter: &str,
    mode: HierarchySearchMode,
) -> std::collections::HashSet<usize> {
    let mut keep = std::collections::HashSet::new();
    if filter.is_empty() {
        return keep;
    }
    for (index, entity) in doc.entities.iter().enumerate() {
        if mode.matches(entity, filter) {
            keep.extend(doc.path_of(index));
        }
    }
    keep
}

/// 层级实际高亮的行（#417）。
///
/// 未锁定时就是 `workspace.selection`；锁定时按实体 id 解析，外部选择变化不会
/// 改动层级高亮。锁住的对象全被删掉后自动解锁并回落到当前选择。
fn hierarchy_indices(workspace: &Workspace, state: &mut UiState) -> Vec<usize> {
    let Some(ids) = state.hierarchy_lock.clone() else {
        return workspace.selection.clone();
    };
    let resolved: Vec<usize> = ids
        .iter()
        .filter_map(|id| workspace.document().doc.index_of_id(*id))
        .collect();
    if resolved.is_empty() {
        state.hierarchy_lock = None;
        return workspace.selection.clone();
    }
    resolved
}

/// 锁定 / 解锁层级高亮：锁的是当前选择里的实体 id（与检查器锁一致）。
fn toggle_hierarchy_lock(workspace: &Workspace, state: &mut UiState) {
    if state.hierarchy_lock.is_some() {
        state.hierarchy_lock = None;
        return;
    }
    state.hierarchy_lock = Some(
        workspace
            .selection
            .iter()
            .filter_map(|index| workspace.document().doc.id_of_index(*index))
            .collect(),
    );
}

/// 层级里发生了点选 / 删除 / 复制：把锁定的快照同步到新的选择。
///
/// 只由层级自己的动作调用，外部 Scene / Project 的选择变化不会走到这里，
/// 否则锁就失效了。
fn sync_hierarchy_lock(workspace: &Workspace, state: &mut UiState) {
    if state.hierarchy_lock.is_some() {
        state.hierarchy_lock = Some(
            workspace
                .selection
                .iter()
                .filter_map(|index| workspace.document().doc.id_of_index(*index))
                .collect(),
        );
    }
}

/// 在层级里定位一个实体：清掉会把它过滤掉的搜索、展开祖先、请求下一帧滚动。
pub(crate) fn reveal_in_hierarchy(workspace: &mut Workspace, state: &mut UiState, index: usize) {
    state.hierarchy_search.clear();
    // 定位的目标是「当前选择」；继续锁着旧选择会让它看不见。
    state.hierarchy_lock = None;
    let ids: Vec<u32> = workspace
        .document()
        .doc
        .path_of(index)
        .into_iter()
        .filter_map(|path_index| workspace.document().doc.id_of_index(path_index))
        .collect();
    workspace.expanded.extend(ids);
    state.hierarchy_reveal = workspace.document().doc.id_of_index(index);
}

#[allow(dead_code)] // Responses are read by the headless interaction tests.
struct HierarchyToolbar {
    search: egui::Response,
    mode: egui::Response,
    create: egui::Response,
    show_all: Option<egui::Response>,
    isolate: egui::Response,
    lock: egui::Response,
    find: egui::Response,
}

/// Hierarchy toolbar: search / search mode / create / visibility / lock / find.
///
/// Returns the widget responses so interaction tests can click the real
/// controls without duplicating the layout.
fn hierarchy_toolbar(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
) -> HierarchyToolbar {
    let mut toolbar = None;
    ui.horizontal(|ui| {
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.hierarchy_search)
                .hint_text(
                    if state.hierarchy_search_mode == HierarchySearchMode::Component {
                        "Search type"
                    } else {
                        "Search"
                    },
                )
                .desired_width(110.0),
        );
        let mode = egui::ComboBox::from_id_salt("hierarchy-search-mode")
            .width(56.0)
            .selected_text(state.hierarchy_search_mode.label())
            .show_ui(ui, |ui| {
                for mode in [HierarchySearchMode::Name, HierarchySearchMode::Component] {
                    ui.selectable_value(&mut state.hierarchy_search_mode, mode, mode.label());
                }
            })
            .response;
        let presets: Vec<(String, String)> = workspace
            .presets
            .iter()
            .map(|preset| (preset.id.clone(), preset.label.clone()))
            .collect();
        let create = if presets.is_empty() {
            let create = ui.button("Create");
            if create.clicked() {
                workspace.add_entity("");
            }
            create
        } else {
            let current = state.preset_choice.min(presets.len().saturating_sub(1));
            ui.menu_button("Create", |ui| {
                for (index, (id, label)) in presets.iter().enumerate() {
                    if ui.selectable_label(index == current, label).clicked() {
                        state.preset_choice = index;
                        workspace.add_entity(id);
                        ui.close();
                    }
                }
            })
            .response
        };
        let visibility_changed =
            !workspace.document().hidden_entities.is_empty() || workspace.isolate;
        let show_all = visibility_changed.then(|| {
            let response = ui.button(format!("{} Show All", icons::EYE));
            if response.clicked() {
                workspace.show_all_entities();
            }
            response
        });
        let isolate = ui.add_enabled(
            workspace.isolate || !workspace.selection.is_empty(),
            egui::Button::new(format!("{} Isolate", icons::TARGET)).selected(workspace.isolate),
        );
        let isolate = isolate.on_hover_text("Only show the selected entities and their children");
        if isolate.clicked() {
            workspace.set_isolate(!workspace.isolate);
        }
        // #417：锁定层级高亮，不让 Scene / Project 的选择把它带走。
        let hierarchy_locked = state.hierarchy_lock.is_some();
        let lock = ui.add_enabled(
            hierarchy_locked || !workspace.selection.is_empty(),
            egui::Button::new(if hierarchy_locked {
                icons::LOCK_SIMPLE
            } else {
                icons::LOCK_SIMPLE_OPEN
            })
            .selected(hierarchy_locked),
        );
        let lock = if hierarchy_locked {
            lock.on_hover_text("Unlock the hierarchy selection")
        } else {
            lock.on_hover_text("Lock the hierarchy selection")
        };
        if lock.clicked() {
            toggle_hierarchy_lock(workspace, state);
        }
        let find = ui.add_enabled(
            workspace.primary_selection().is_some(),
            egui::Button::new(icons::CROSSHAIR),
        );
        let find = find.on_hover_text("Find the selected object in the hierarchy");
        if find.clicked() {
            if let Some(index) = workspace.primary_selection() {
                reveal_in_hierarchy(workspace, state, index);
            }
        }
        toolbar = Some(HierarchyToolbar {
            search,
            mode,
            create,
            show_all,
            isolate,
            lock,
            find,
        });
    });
    toolbar.expect("horizontal layout always runs")
}

/// Apply one hierarchy row click (also used by the interaction tests).
///
/// `extend` is Shift; Ctrl/Cmd uses [`apply_hierarchy_toggle`] instead. The
/// range walks `visible_rows`, so collapsed/filtered entities never sneak in.
fn apply_hierarchy_select(
    workspace: &mut Workspace,
    state: &mut UiState,
    index: usize,
    extend: bool,
    visible_rows: &[usize],
) {
    if extend {
        let anchor = state
            .hierarchy_anchor
            .filter(|anchor| visible_rows.contains(anchor))
            .or_else(|| workspace.primary_selection());
        match anchor {
            Some(anchor) => workspace.select_visible_range(visible_rows, anchor, index),
            None => {
                workspace.select(index, false);
                state.hierarchy_anchor = Some(index);
            }
        }
    } else {
        workspace.select(index, false);
        state.hierarchy_anchor = Some(index);
    }
}

/// Ctrl/Cmd-click one hierarchy row: add or remove it from the selection.
fn apply_hierarchy_toggle(workspace: &mut Workspace, state: &mut UiState, index: usize) {
    state.hierarchy_anchor = Some(index);
    if workspace.selection.contains(&index) {
        let remaining: Vec<usize> = workspace
            .selection
            .iter()
            .copied()
            .filter(|selected| *selected != index)
            .collect();
        workspace.select_many(&remaining, false);
    } else {
        workspace.select_many(&[index], true);
    }
}

pub fn hierarchy(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    // ---- 顶部工具条：搜索 + 新建实体（引擎的 Create 下拉） ----
    let _ = hierarchy_toolbar(ui, workspace, state);
    ui.separator();

    let filter = state.hierarchy_search.to_lowercase();
    let searching = !filter.is_empty();
    // 命中项 + 祖先：折叠父级下的对象也要能被搜到（会展开祖先并显示路径）。
    let search = search_matches(
        &workspace.document().doc,
        &filter,
        state.hierarchy_search_mode,
    );
    if searching {
        let ids: Vec<u32> = {
            let doc = &workspace.document().doc;
            search
                .iter()
                .filter_map(|index| doc.id_of_index(*index))
                .collect()
        };
        workspace.expanded.extend(ids);
    }
    let expanded = workspace.expanded.clone();
    let rows = workspace.document().doc.visible_rows(&expanded);
    // #417：锁定时高亮停在锁定时的实体 id；外部选择变化不改这里。
    let display_selection = hierarchy_indices(workspace, state);
    // #417：可见性是会话级状态，每帧算一次，行循环里只查集合。
    let hidden_ids: std::collections::HashSet<u32> =
        workspace.hidden_entity_ids().into_iter().collect();
    // Shift 区间选择按**当前可见行**的顺序走：折叠子树与搜索过滤掉的实体不能混进来。
    let visible_rows: Vec<usize> = rows
        .iter()
        .filter(|(index, _)| !searching || search.contains(index))
        .map(|(index, _)| *index)
        .collect();
    // 引用断掉的实体（文件被删 / 改名 / 拼错）：行尾标一个警告图标。
    // 每帧只算一次；GUID 仍能解析的引用不算断（#466 / #467）。
    let broken: std::collections::HashSet<usize> = workspace
        .broken_references()
        .into_iter()
        .map(|(entity, _, _)| entity)
        .collect();
    let mut action: Option<TreeAction> = None;
    // 资产拖到实体上 = 实例化预制体（引擎的 Project → Hierarchy 拖拽）。
    let mut asset_drop: Option<(usize, String)> = None;
    let mut hovered_drop_target = false;
    // 这一帧有没有右键命中某一行（决定「右键空白清空选择」要不要生效，#524）。
    let mut row_context_open = false;

    let area_rect = egui::ScrollArea::vertical()
        .id_salt("hierarchy-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let area_rect = ui.max_rect();
            for &(index, _id) in &rows {
                let doc = &workspace.document().doc;
                let Some(entity) = doc.entities.get(index) else {
                    continue;
                };
                let name = entity.display_name();
                if searching && !search.contains(&index) {
                    continue;
                }
                let depth = doc.path_of(index).len().saturating_sub(1);
                let selected = display_selection.contains(&index);
                let expandable = !doc.children(index).is_empty();
                let expanded_here = entity
                    .id
                    .map(|id| workspace.expanded.contains(&id))
                    .unwrap_or(false);
                let active = entity.active();
                let renaming = matches!(&state.rename, Some((target, _)) if *target == index);

                // 整行选中（点行任意位置都选中，只有文字可点会很别扭），
                // 行高固定 18px，用来画整行底色 / 拖拽插入线。
                let (row_rect, row) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), tree_row_height(state.compact)),
                    egui::Sense::click_and_drag(),
                );
                if state.hierarchy_reveal.is_some() && state.hierarchy_reveal == entity.id {
                    row.scroll_to_me(Some(Align::Center));
                    state.hierarchy_reveal = None;
                }
                if ui.is_rect_visible(row_rect) {
                    use prism_editor_core::ThemeToken;
                    let colors = workspace.colors();
                    let bg = if selected {
                        Some(theme::color32(&colors, ThemeToken::Selection))
                    } else if row.hovered() {
                        Some(theme::color32(&colors, ThemeToken::RowHover))
                    } else {
                        None
                    };
                    if let Some(bg) = bg {
                        ui.painter().rect_filled(row_rect, 0.0, bg);
                    }
                }
                let mut row_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(row_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                row_ui.add_space(6.0 + depth as f32 * 12.0);
                if expandable {
                    let arrow = if expanded_here {
                        icons::CARET_DOWN
                    } else {
                        icons::CARET_RIGHT
                    };
                    if flat_icon(&mut row_ui, arrow).clicked() {
                        action = Some(TreeAction::ToggleExpand(index));
                    }
                } else {
                    row_ui.add_space(16.0);
                }
                // 祖先被关掉时整棵子树都不渲染 / 不参与逻辑（引擎的
                // `ActiveInHierarchy`，Unity 的 `activeInHierarchy`）。眼睛只写
                // 自身的 `Active`，所以子物体要画成灰的，否则用户点了父级
                // 会以为子物体没反应（#433）。
                let ancestor_active = doc
                    .path_of(index)
                    .iter()
                    .rev()
                    .skip(1)
                    .all(|ancestor| doc.entities.get(*ancestor).is_some_and(|e| e.active()));
                // Unity 式两列：电源 = Active（会停模拟），眼睛 = Scene 可见性
                // （只影响渲染 / 拾取，不改文档、不停模拟，#417）。
                let active_icon = icons::POWER;
                let response = flat_icon_dimmed(
                    &mut row_ui,
                    active_icon,
                    !active || (active && !ancestor_active),
                );
                let response = if active && !ancestor_active {
                    response.on_hover_text("Hidden by an inactive parent")
                } else {
                    response.on_hover_text("Toggle Active")
                };
                if response.clicked() {
                    action = Some(TreeAction::ToggleActive(index));
                }
                let hidden = entity.id.is_some_and(|id| hidden_ids.contains(&id));
                let visibility_icon = if hidden {
                    icons::EYE_CLOSED
                } else {
                    icons::EYE
                };
                let visibility = flat_icon_dimmed(&mut row_ui, visibility_icon, hidden);
                let visibility = if workspace.isolate && hidden {
                    visibility.on_hover_text("Isolated — exit Isolate to show other objects")
                } else if hidden {
                    visibility.on_hover_text("Show in Scene view")
                } else {
                    visibility.on_hover_text("Hide in Scene view")
                };
                if visibility.clicked() && !(workspace.isolate && hidden) {
                    action = Some(TreeAction::ToggleHidden(index));
                }
                if renaming {
                    if let Some((_, buffer)) = state.rename.as_mut() {
                        let response = row_ui.add(
                            egui::TextEdit::singleline(buffer)
                                .desired_width(row_ui.available_width().max(60.0)),
                        );
                        if state.rename_focus {
                            response.request_focus();
                            state.rename_focus = false;
                        }
                        let (enter, escape, clicked) = ui.input(|input| {
                            (
                                input.key_pressed(egui::Key::Enter),
                                input.key_pressed(egui::Key::Escape),
                                input.pointer.any_click(),
                            )
                        });
                        // 点空白 / 点别处 = 提交（引擎的行为），Esc = 放弃。
                        let clicked_outside = clicked
                            && ui
                                .input(|input| input.pointer.interact_pos())
                                .map(|pos| !row_rect.contains(pos))
                                .unwrap_or(false);
                        if response.changed() {
                            action = Some(TreeAction::PreviewRename(index, buffer.clone()));
                        }
                        if escape {
                            action = Some(TreeAction::CancelRename);
                            state.rename_focus = false;
                        } else if enter || clicked_outside || response.lost_focus() {
                            action = Some(TreeAction::CommitRename(index, buffer.clone()));
                        }
                    }
                } else {
                    use prism_editor_core::ThemeToken;
                    let colors = workspace.colors();
                    let text_color = theme::color32(
                        &colors,
                        if selected {
                            ThemeToken::OnAccent
                        } else {
                            ThemeToken::Text
                        },
                    );
                    // 同样用 painter 画，避免 Label 抢走整行点击。
                    let text_pos = row_ui.cursor().min;
                    let font = egui::FontId::proportional(12.0);
                    row_ui.painter().text(
                        egui::pos2(text_pos.x, row_ui.max_rect().center().y),
                        egui::Align2::LEFT_CENTER,
                        &name,
                        font.clone(),
                        text_color,
                    );
                    if broken.contains(&index) {
                        // 名字是 painter 画的、不占布局空间，标记要自己算 x。
                        // 用「圆圈 + !」而不是 `⚠`：默认字体里没有 U+26A0 时它会画成
                        // 豆腐块，靠文字字形不如直接画几何稳。
                        let width = row_ui
                            .painter()
                            .layout_no_wrap(name.clone(), font, text_color)
                            .size()
                            .x;
                        let center =
                            egui::pos2(text_pos.x + width + 9.0, row_ui.max_rect().center().y);
                        let badge = egui::Color32::from_rgb(226, 96, 96);
                        row_ui.painter().circle_filled(center, 5.0, badge);
                        row_ui.painter().text(
                            center,
                            egui::Align2::CENTER_CENTER,
                            "!",
                            egui::FontId::proportional(9.0),
                            egui::Color32::WHITE,
                        );
                    }
                }
                // 行级交互：单击选中、双击重命名（子控件会先吃掉自己的点击）。
                if !renaming {
                    if row.clicked() {
                        let modifiers = ui.input(|i| i.modifiers);
                        // Ctrl/Cmd 单击 = 切换（引擎的多选开关），Shift = 追加。
                        if modifiers.ctrl || modifiers.command {
                            action = Some(TreeAction::Toggle(index));
                        } else {
                            action = Some(TreeAction::Select(index, modifiers.shift));
                        }
                    }
                    if row.double_clicked() {
                        // 双击 = 定位（#472）；重命名改到 F2，与 Unity 一致。
                        action = Some(TreeAction::Frame(index));
                    }
                }

                // ---- 引擎式拖拽：上/下 25% 插入，中间挂成子级 ----
                if row.drag_started_by(egui::PointerButton::Primary) {
                    state.dragging = Some(index);
                }
                if state.dragging.is_some() && state.dragging != Some(index) && row.hovered() {
                    let from = state.dragging.expect("dragging checked above");
                    let invalid = from == index
                        || workspace.document().doc.descendants(from).contains(&index);
                    hovered_drop_target = true;
                    let painter = ui.painter();
                    if invalid {
                        state.drop_target = None;
                        painter.rect_stroke(
                            row.rect,
                            egui::CornerRadius::ZERO,
                            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(220, 90, 90)),
                            egui::StrokeKind::Inside,
                        );
                    } else {
                        let pointer_y = ui
                            .input(|input| input.pointer.interact_pos())
                            .map(|pos| pos.y)
                            .unwrap_or(row.rect.center().y);
                        let ratio = (pointer_y - row.rect.top()) / row.rect.height().max(1.0);
                        let zone = if ratio < 0.25 {
                            DropZone::Before
                        } else if ratio > 0.75 {
                            DropZone::After
                        } else {
                            DropZone::Child
                        };
                        state.drop_target = Some((index, zone));
                        match zone {
                            DropZone::Before => painter.line_segment(
                                [row.rect.left_top(), row.rect.right_top()],
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                            ),
                            DropZone::After => painter.line_segment(
                                [row.rect.left_bottom(), row.rect.right_bottom()],
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                            ),
                            DropZone::Child => painter.rect_stroke(
                                row.rect,
                                egui::CornerRadius::ZERO,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                                egui::StrokeKind::Inside,
                            ),
                        };
                    }
                }

                if row.dnd_hover_payload::<widgets::AssetDrag>().is_some() {
                    ui.painter().rect_filled(
                        row.rect,
                        egui::CornerRadius::ZERO,
                        egui::Color32::from_rgba_unmultiplied(90, 170, 120, 40),
                    );
                }
                if let Some(payload) = row.dnd_release_payload::<widgets::AssetDrag>() {
                    if payload.path.ends_with(".prefab.json") {
                        asset_drop = Some((index, payload.path.clone()));
                    }
                }

                // 右键目标 = 菜单动作 + 检查器的共同目标（#524）。必须**在**打开菜单
                // 之前改选择：`Parent to Selection` 在同一帧就读 `primary_selection()`。
                if row.secondary_clicked() {
                    row_context_open = true;
                    if workspace.select_for_context(index) {
                        sync_hierarchy_lock(workspace, state);
                    }
                }

                row.context_menu(|ui| {
                    let hidden_now = hidden;
                    let visibility_label = if hidden_now {
                        "Show in Scene View"
                    } else {
                        "Hide in Scene View"
                    };
                    if ui.button(visibility_label).clicked() {
                        if workspace.isolate && hidden_now {
                            workspace.show_all_entities();
                        } else {
                            workspace.set_entity_hidden(index, !hidden_now);
                        }
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !workspace.selection.is_empty(),
                            egui::Button::new("Isolate Selection"),
                        )
                        .clicked()
                    {
                        workspace.set_isolate(true);
                        ui.close();
                    }
                    if ui.button("Show All").clicked() {
                        workspace.show_all_entities();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Rename").clicked() {
                        action = Some(TreeAction::BeginRename(index, name.clone()));
                        ui.close();
                    }
                    if ui.button("Duplicate").clicked() {
                        action = Some(TreeAction::Duplicate(index));
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        action = Some(TreeAction::Delete(index));
                        ui.close();
                    }
                    if ui.button("Select Children").clicked() {
                        let children = workspace.document().doc.descendants(index);
                        workspace.select_many(&children, false);
                        sync_hierarchy_lock(workspace, state);
                        ui.close();
                    }
                    if ui.button("Set as First Sibling").clicked() {
                        let _ = workspace.document_mut().doc.move_entity(index, 0);
                        ui.close();
                    }
                    if ui.button("Set as Last Sibling").clicked() {
                        let last = workspace.document().doc.entities.len().saturating_sub(1);
                        let _ = workspace.document_mut().doc.move_entity(index, last);
                        ui.close();
                    }
                    if ui.button("Move to Root").clicked() {
                        workspace.reparent(index, None, None);
                        ui.close();
                    }
                    if let Some(parent) = workspace.primary_selection() {
                        if parent != index && ui.button("Parent to Selection").clicked() {
                            workspace.reparent(index, Some(parent), None);
                            ui.close();
                        }
                    }
                });
            }
            area_rect
        })
        .inner;

    if state.dragging.is_some() && !hovered_drop_target {
        state.drop_target = None;
    }

    // 右键空白处清空选择（Unity 的 `deselectOnUnhandledMouseDown`）。
    if !row_context_open
        && ui.input(|input| input.pointer.secondary_clicked())
        && ui
            .input(|input| input.pointer.interact_pos())
            .is_some_and(|pos| area_rect.contains(pos))
    {
        action = Some(TreeAction::ClearSelection);
    }

    // Hierarchy keeps a lightweight ghost under the pointer while dragging.
    // It makes the operation legible even when the pointer is between rows or over
    // the empty tail of the scroll area.
    if let (Some(from), Some(pointer)) = (
        state.dragging,
        ui.input(|input| input.pointer.interact_pos()),
    ) {
        let label = workspace
            .document()
            .doc
            .entities
            .get(from)
            .map(|entity| entity.display_name())
            .unwrap_or_else(|| "Entity".to_string());
        let ghost = egui::Rect::from_min_size(
            pointer + egui::vec2(12.0, 12.0),
            egui::vec2((label.len() as f32 * 7.0 + 30.0).clamp(96.0, 260.0), 22.0),
        );
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("hierarchy-drag-preview"),
        ));
        painter.rect_filled(
            ghost,
            egui::CornerRadius::same(3),
            egui::Color32::from_rgba_unmultiplied(35, 42, 54, 235),
        );
        painter.rect_stroke(
            ghost,
            egui::CornerRadius::same(3),
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(100, 170, 245)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            ghost.left_center() + egui::vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }

    // 松手 → 应用拖拽结果
    if ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        if let (Some(from), Some((to, zone))) = (state.dragging.take(), state.drop_target.take()) {
            if from != to {
                let parent_of_to = workspace.document().doc.entities[to]
                    .parent
                    .and_then(|id| workspace.document().doc.index_of_id(id));
                // 拖的是多选中的一员时，三种落点都作用于整组（顶层根），保持相对顺序：
                // Child = 改挂到目标下，Before / After = 插到目标前/后（#525）。
                let group = workspace.selection.len() > 1 && workspace.selection.contains(&from);
                if group {
                    let moved = match zone {
                        DropZone::Child => workspace.reparent_selection(Some(to)),
                        DropZone::Before => workspace.reparent_selection_at(to, true),
                        DropZone::After => workspace.reparent_selection_at(to, false),
                    };
                    if moved > 0 {
                        state.status = format!("moved {moved} entities");
                    }
                    state.drop_target = None;
                    return;
                }
                match zone {
                    DropZone::Child => workspace.reparent(from, Some(to), None),
                    DropZone::Before => {
                        let mut position = to;
                        if from < position {
                            position = position.saturating_sub(1);
                        }
                        workspace.reparent(from, parent_of_to, Some(position));
                    }
                    DropZone::After => {
                        let mut position = to.saturating_add(1);
                        if from < position {
                            position = position.saturating_sub(1);
                        }
                        workspace.reparent(from, parent_of_to, Some(position));
                    }
                }
            }
        }
        state.drop_target = None;
    }

    if let Some((parent, prefab)) = asset_drop {
        match workspace.instantiate_prefab_asset(&prefab, Some(parent)) {
            Ok(count) => state.status = format!("instantiated {count} entities from {prefab}"),
            Err(error) => state.status = format!("instantiate failed: {error}"),
        }
    }

    // F2 = 就地重命名（Unity 的绑定；macOS 上 Return 同义）。双击已经改成「定位」，
    // 否则误触双击会直接把实体名变成输入框（#472）。
    let typing = ui.ctx().memory(|memory| memory.focused()).is_some();
    let rename_key = ui.input(|input| {
        input.key_pressed(egui::Key::F2)
            || (cfg!(target_os = "macos") && input.key_pressed(egui::Key::Enter))
    });
    if !typing && rename_key && state.rename.is_none() {
        if let Some(index) = workspace.primary_selection() {
            if let Some(entity) = workspace.document().doc.entities.get(index) {
                let name = entity.display_name();
                action = Some(TreeAction::BeginRename(index, name));
            }
        }
    }

    if let Some(action) = action {
        let selection_before = workspace.selection.clone();
        match action {
            TreeAction::Select(index, extend) => {
                apply_hierarchy_select(workspace, state, index, extend, &visible_rows);
            }
            TreeAction::Toggle(index) => {
                apply_hierarchy_toggle(workspace, state, index);
            }
            TreeAction::ToggleExpand(index) => workspace.toggle_expanded(index),
            TreeAction::ToggleActive(index) => {
                let active = workspace.document().doc.entities[index].active();
                workspace.set_active_entity(index, !active);
            }
            TreeAction::ToggleHidden(index) => {
                let hidden = workspace.is_entity_hidden(index);
                workspace.set_entity_hidden(index, !hidden);
            }
            TreeAction::Delete(index) => {
                workspace.select(index, false);
                if state.confirm_delete {
                    state.pending_entity_delete = Some(vec![index]);
                } else {
                    workspace.delete_selection();
                }
                state.rename = None;
                state.rename_base = None;
            }
            TreeAction::Duplicate(index) => {
                workspace.select(index, false);
                workspace.duplicate_selection();
            }
            TreeAction::BeginRename(index, name) => {
                // 记下「开始编辑前」的文档：提交时补一个撤销步，取消时还原（#473）。
                state.rename_base =
                    Some((workspace.document().doc.clone(), workspace.document().dirty));
                state.rename = Some((index, name));
                state.rename_focus = true;
            }
            TreeAction::PreviewRename(index, name) => {
                workspace.preview_rename(index, &name);
            }
            TreeAction::CancelRename => {
                if let Some((base, was_dirty)) = state.rename_base.take() {
                    workspace.cancel_rename(base, was_dirty);
                }
                state.rename = None;
            }
            TreeAction::Frame(index) => {
                workspace.select(index, false);
                workspace.frame_selection();
            }
            TreeAction::ClearSelection => workspace.select_many(&[], false),
            TreeAction::CommitRename(index, name) => {
                let name = name.trim().to_string();
                match state.rename_base.take() {
                    Some((base, was_dirty)) => {
                        if name.is_empty() {
                            // 空名字 = 放弃这次编辑，把预览还原掉。
                            workspace.cancel_rename(base, was_dirty);
                        } else {
                            workspace.preview_rename(index, &name);
                            workspace.finish_rename(base, was_dirty);
                        }
                    }
                    None => {
                        if !name.is_empty() {
                            workspace.rename_entity(index, &name);
                        }
                    }
                }
                state.rename = None;
            }
        }
        if workspace.selection != selection_before {
            sync_hierarchy_lock(workspace, state);
        }
    }
}

// ---------------------------------------------------------------------------
// 检查器
// ---------------------------------------------------------------------------

/// 检查器实际显示的实体下标（#418）。
///
/// 锁定时用锁住的实体 id 解析下标（失效的自动丢弃）；锁住的对象全没了就清锁、
/// 回落到文档选择。未锁定时就是 `workspace.selection`。
fn inspector_indices(workspace: &Workspace, state: &mut UiState) -> Vec<usize> {
    let Some(ids) = state.inspector_lock.clone() else {
        return workspace.selection.clone();
    };
    let resolved: Vec<usize> = ids
        .iter()
        .filter_map(|id| workspace.document().doc.index_of_id(*id))
        .collect();
    if resolved.is_empty() {
        state.inspector_lock = None;
        return workspace.selection.clone();
    }
    resolved
}

/// 锁定 / 解锁检查器：锁的是当前选择里的实体 id（#418）。
fn toggle_inspector_lock(workspace: &Workspace, state: &mut UiState) {
    if state.inspector_lock.is_some() {
        state.inspector_lock = None;
        return;
    }
    let ids = workspace
        .selection
        .iter()
        .filter_map(|index| workspace.document().doc.id_of_index(*index))
        .collect();
    state.inspector_lock = Some(ids);
}

/// Play 态提示色（#500）：播放中为绿色，暂停为琥珀色，与状态栏的 PLAYING / PAUSED 一致。
///
/// 整窗描边和 Inspector 顶部横幅共用这一个来源，避免两处颜色漂移。
pub fn play_mode_tint(paused: bool) -> egui::Color32 {
    if paused {
        egui::Color32::from_rgb(230, 180, 80)
    } else {
        egui::Color32::from_rgb(96, 200, 120)
    }
}

/// Play 态横幅文案；非播放态返回 `None`（不占横向空间）。
///
/// `playing` 传 `workspace.playing && workspace.has_play_snapshot()`：只有真的
/// 会被 Stop 回滚的那一次播放才提示，避免控制通道里的边界状态误报。
pub fn play_mode_banner(playing: bool) -> Option<&'static str> {
    playing.then_some("Play mode — changes made now are discarded when Play stops.")
}

pub fn inspector(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    // Play 态提示（#500）：运行期的编辑会在 Stop 时被丢弃，检查器顶部要先说清楚。
    if let Some(banner) = play_mode_banner(workspace.playing && workspace.has_play_snapshot()) {
        let tint = play_mode_tint(workspace.paused);
        egui::Frame::new()
            .fill(tint.gamma_multiply(0.16))
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("{} {banner}", icons::INFO))
                        .size(11.0)
                        .color(tint),
                );
            });
        ui.add_space(4.0);
    }

    // Inspector 锁定（#418）：钉住当前对象，去别处点选时不换内容。
    ui.horizontal(|ui| {
        let locked = state.inspector_lock.is_some();
        let icon = if locked {
            icons::LOCK_SIMPLE
        } else {
            icons::LOCK_SIMPLE_OPEN
        };
        let hint = if locked {
            "Unlock the inspector"
        } else {
            "Lock the inspector to the current selection"
        };
        let can_lock = locked || !workspace.selection.is_empty();
        if ui
            .add_enabled(can_lock, egui::Button::new(icon).small())
            .on_hover_text(hint)
            .clicked()
        {
            toggle_inspector_lock(workspace, state);
        }
        if locked {
            ui.label(RichText::new("Locked").size(10.5).weak());
        }
    });

    let indices = inspector_indices(workspace, state);
    if indices.is_empty() {
        // 没有选中实体时，Project 面板选中的资产接管检查器（#514）。
        if !state.asset_selection.is_empty() {
            asset_inspector(ui, workspace, state);
            return;
        }
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Select an entity").weak());
        });
        return;
    }
    if indices.len() > 1 {
        inspector_multi(ui, workspace, state, &indices);
        return;
    }
    inspector_single(ui, workspace, state, indices[0]);
}

#[cfg(test)]
mod asset_inspector_tests {
    use super::*;
    use prism_editor_core::{AssetInfo, Workspace};

    fn render(workspace: &mut Workspace, state: &mut UiState) {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| inspector(ui, workspace, state));
        });
    }

    fn workspace_with_asset(path: &str) -> Workspace {
        let mut workspace = Workspace::sample();
        workspace.assets.push(AssetInfo {
            path: path.to_string(),
            size_bytes: 2048,
            kind: prism_editor_core::AssetKind::Texture,
            importer: "ImageImporter".to_string(),
            guid: None,
        });
        workspace
    }

    /// #514：没有选中实体、但选了资产时，检查器画资产详情（不写文档）。
    #[test]
    fn an_asset_selection_takes_over_the_inspector() {
        let path = "assets/sprites/fish.png";
        let mut workspace = workspace_with_asset(path);
        let mut state = UiState::default();
        state.asset_selection = vec![path.to_string()];

        render(&mut workspace, &mut state);

        assert!(workspace.selection.is_empty());
        assert!(
            workspace.undo_history().is_empty(),
            "只读的资产检查器不该产生撤销步"
        );
        assert!(!workspace.document().dirty);
    }

    /// 实体选择优先：两者同时存在时仍然是实体检查器。
    #[test]
    fn an_entity_selection_keeps_the_entity_inspector() {
        let mut workspace = workspace_with_asset("assets/sprites/fish.png");
        workspace.selection = vec![0];
        let before = workspace.document().doc.clone();
        let mut state = UiState::default();
        state.asset_selection = vec!["assets/sprites/fish.png".to_string()];

        render(&mut workspace, &mut state);

        assert_eq!(workspace.document().doc, before);
        assert_eq!(workspace.selection, vec![0]);
    }

    /// 选中的资产不在索引里时显示占位，不 panic、不写文档。
    #[test]
    fn import_settings_sidecar_round_trips() {
        // #514：编辑器写、cooker 读同一组键；坏文件回落到默认值。
        let dir = std::env::temp_dir().join(format!("prism-import-ui-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets/sprites")).unwrap();
        let sidecar = dir.join(import_settings_path("assets/sprites/boat.png"));
        assert_eq!(read_import_settings(&sidecar), (0, true));

        std::fs::write(&sidecar, import_settings_json(512, false)).unwrap();
        assert_eq!(read_import_settings(&sidecar), (512, false));

        // 缺字段：只覆盖写出来的那一项。
        std::fs::write(&sidecar, r#"{"maxSize": 256}"#).unwrap();
        assert_eq!(read_import_settings(&sidecar), (256, true));

        std::fs::write(&sidecar, "not json").unwrap();
        assert_eq!(read_import_settings(&sidecar), (0, true));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_asset_renders_a_placeholder() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::default();
        state.asset_selection = vec!["assets/missing.png".to_string()];

        render(&mut workspace, &mut state);

        assert!(workspace.undo_history().is_empty());
    }
}

/// 资产检查器（#514）：选中资产时替代实体检查器，显示只读事实。
///
/// 导入参数（sRGB / 压缩格式 / max size …）还没有存放位置 —— 那份数据目前只存在
/// cook profile 里，编辑器没有 per-asset 的读写闭环，所以这里先不做可编辑项，
/// 免得做出一个「改了不生效」的面板。
fn asset_inspector(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    let Some(path) = state.asset_primary().cloned() else {
        return;
    };
    // 详情会读磁盘上的每个文档来数引用，所以按路径缓存 500ms，而不是每帧重算。
    let stale = state
        .asset_details
        .as_ref()
        .map(|(cached, _)| cached != &path)
        .unwrap_or(true);
    let expired = state
        .asset_details_at
        .map(|at| at.elapsed() >= std::time::Duration::from_millis(500))
        .unwrap_or(true);
    if stale || expired {
        let details = workspace.asset_details(&path);
        state.asset_details = details.map(|details| (path.clone(), details));
        state.asset_details_at = Some(std::time::Instant::now());
    }
    let details = state
        .asset_details
        .as_ref()
        .filter(|(cached, _)| cached == &path)
        .map(|(_, details)| details.clone());

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(path.rsplit('/').next().unwrap_or(&path))
                .strong()
                .size(13.0),
        );
        if state.asset_selection.len() > 1 {
            ui.label(
                RichText::new(format!("+{} selected", state.asset_selection.len() - 1))
                    .weak()
                    .size(11.0),
            );
        }
    });
    ui.label(RichText::new(&path).weak().size(10.0));
    ui.separator();

    let Some(details) = details else {
        ui.label(RichText::new("asset not in the project index").weak());
        return;
    };

    let mut row = |label: &str, value: String| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).weak().size(11.0));
            ui.label(RichText::new(value).size(11.0));
        });
    };
    row("Type", details.asset.kind.label().to_string());
    row("Size", format_bytes(details.asset.size_bytes));
    if !details.asset.importer.is_empty() {
        row("Importer", details.asset.importer.clone());
    }
    row("GUID", details.guid.clone());
    row("References", details.references.len().to_string());
    ui.horizontal(|ui| {
        if ui
            .button("Ping In Scene")
            .on_hover_text("Select the first object that references this asset")
            .clicked()
        {
            ping_asset(workspace, state, &path);
        }
    });

    ui.separator();
    ui.label(RichText::new("Preview").strong().size(11.0));
    match &details.preview {
        AssetPreview::Text { text, truncated } => {
            let lines: Vec<&str> = text.lines().take(10).collect();
            ui.add(
                egui::Label::new(
                    RichText::new(lines.join(
                        "
",
                    ))
                    .monospace()
                    .size(10.0),
                )
                .wrap(),
            );
            if *truncated {
                ui.label(RichText::new("… truncated").weak().size(10.0));
            }
        }
        AssetPreview::Image {
            width,
            height,
            bytes,
        } => {
            let size = match (width, height) {
                (Some(width), Some(height)) => format!("{width} × {height}"),
                _ => "unknown size".to_string(),
            };
            ui.label(RichText::new(format!("{size} · {}", format_bytes(*bytes))).size(11.0));
        }
        AssetPreview::Document => {
            ui.label(RichText::new("scene / prefab document").weak().size(11.0));
        }
        AssetPreview::None => {
            ui.label(RichText::new("no preview for this kind").weak().size(11.0));
        }
    }

    if details.asset.kind == AssetKind::Audio {
        ui.add_space(4.0);
        let playing = state.audio_preview_path.as_deref() == Some(path.as_str());
        ui.horizontal(|ui| {
            let label = if playing { "Stop" } else { "Play" };
            if ui
                .button(label)
                .on_hover_text("Preview this clip with the editor audio engine (#508)")
                .clicked()
            {
                state.pending_audio_preview = Some(if playing {
                    AudioPreviewAction::Stop
                } else {
                    AudioPreviewAction::Play(path.clone())
                });
            }
            if playing {
                ui.label(RichText::new("playing…").weak().size(10.0));
            }
        });
    }

    if !details.references.is_empty() {
        ui.separator();
        ui.label(RichText::new("Used by").strong().size(11.0));
        for reference in details.references.iter().take(12) {
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}",
                    reference.document, reference.entity_name, reference.field
                ))
                .weak()
                .size(10.0),
            );
        }
        if details.references.len() > 12 {
            ui.label(
                RichText::new(format!("+{} more", details.references.len() - 12))
                    .weak()
                    .size(10.0),
            );
        }
    }

    if details.asset.kind == AssetKind::Texture {
        texture_import_settings(ui, workspace, state, &path);
    }

    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Copy Path").clicked() {
            ui.ctx().copy_text(path.clone());
            state.status = format!("copied {path}");
        }
        let openable = path.ends_with(".scene.json")
            || path.ends_with(".prefab.json")
            || prism_editor_core::project::is_json_editable_asset(&path);
        if openable && ui.button("Open").clicked() {
            if path.ends_with(".scene.json") || path.ends_with(".prefab.json") {
                match workspace.open_project_file(&path) {
                    Ok(()) => state.status = format!("opened {path}"),
                    Err(error) => state.status = format!("open {path} failed: {error}"),
                }
            } else {
                open_material_asset(workspace, state, &path);
                workspace.show_panel("material");
                state.status = format!("opened {path}");
            }
        }
        if ui.button("Show in Explorer").clicked() {
            state.status = match reveal_asset(workspace, &path) {
                Ok(()) => format!("revealed {path}"),
                Err(error) => error,
            };
        }
    });
}

/// Add Component 菜单：`indices` 是这次要作用的实体集合（多选时是全部选中项）。
fn add_component_menu_for(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
    indices: &[usize],
) {
    ui.menu_button("Add Component", |ui| {
        ui.add(egui::TextEdit::singleline(&mut state.add_component_query).hint_text("search"));
        let query = state.add_component_query.to_lowercase();
        // 只列**至少一个选中项还没挂**的组件：已挂的再点一次会把参数重置成默认值。
        let attachable: std::collections::HashSet<String> = workspace
            .component_types_for(indices)
            .into_iter()
            .map(|component| component.name.clone())
            .collect();
        let categories: Vec<(String, Vec<(String, String)>)> = workspace
            .schema
            .component_menu()
            .into_iter()
            .map(|(category, items)| {
                (
                    category.to_string(),
                    items
                        .into_iter()
                        .filter(|item| attachable.contains(&item.name))
                        .filter(|item| {
                            query.is_empty() || item.name.to_lowercase().contains(&query)
                        })
                        .map(|item| (item.name.clone(), item.display_name().to_string()))
                        .collect(),
                )
            })
            .filter(|(_, items): &(String, Vec<(String, String)>)| !items.is_empty())
            .collect();
        let mut picked = None;
        if categories.is_empty() {
            ui.label(RichText::new("no more components").weak());
        }
        for (category, items) in categories {
            ui.menu_button(category, |ui| {
                for (name, label) in items {
                    if ui.button(label).clicked() {
                        picked = Some(name);
                        ui.close();
                    }
                }
            });
        }
        if let Some(name) = picked {
            workspace.add_component_multi(indices, &name);
        }
    });
}

/// 检查器里一次 Transform 编辑。
enum TransformEdit {
    /// 单个分量（`position` / `scale`，旋转走欧拉角）。
    Axis {
        field: &'static str,
        axis: usize,
        value: f32,
    },
    /// 整个向量（拖 `Position` / `Scale` 行标签时三个轴一起动）。
    Vector {
        field: &'static str,
        values: [f32; 3],
    },
    /// 整个旋转（检查器按 XYZ 欧拉角编辑，换回四元数再写）。
    Rotation([f32; 4]),
}

fn transform_rows(
    ui: &mut Ui,
    state: &mut UiState,
    transform: &prism_editor_core::TransformJson,
    // 多选时每个分量「选中项取值不一致」的掩码：[position, rotation, scale][axis]。
    mixed: Option<&[[bool; 4]; 3]>,
    mut write: impl FnMut(TransformEdit),
) {
    let mixed_at = |row: usize, axis: usize| mixed.map(|mixed| mixed[row][axis]).unwrap_or(false);
    let marker = |ui: &mut Ui, axis: usize, row: usize| {
        if mixed_at(row, axis) {
            ui.label(RichText::new("—").weak())
                .on_hover_text("Selected entities have different values here");
        }
    };
    // 分量名前缀单独画成可拖动的标签（Unity 的 `DragZoneRect`）：拖 X/Y/Z 调一个轴，
    // 拖 `Position` / `Rotation` / `Scale` 三个轴一起动（#479）。
    let axis_labels = ["X", "Y", "Z"];

    let mut position = transform.position;
    let mut position_row: Option<[f32; 3]> = None;
    ui.horizontal(|ui| {
        let mut row_delta = 0.0f32;
        if widgets::scrubbable_label(ui, "Position", &mut row_delta, 0.01) {
            for axis in 0..3 {
                position[axis] += row_delta;
            }
            position_row = Some(position);
        }
        for axis in 0..3 {
            if widgets::scrubbable_label(ui, axis_labels[axis], &mut position[axis], 0.01) {
                write(TransformEdit::Axis {
                    field: "position",
                    axis,
                    value: position[axis],
                });
            }
            let response = ui
                .add(
                    egui::DragValue::new(&mut position[axis])
                        .speed(0.01)
                        .max_decimals(3)
                        .custom_parser(widgets::number_parser),
                )
                .on_hover_text("Type an expression: 90/2, 2*(3+4)");
            transform_row_menu(&response, TransformRow::Position, transform, &mut write);
            if response.changed() {
                write(TransformEdit::Axis {
                    field: "position",
                    axis,
                    value: position[axis],
                });
            }
            marker(ui, axis, 0);
        }
    });
    if let Some(values) = position_row {
        write(TransformEdit::Vector {
            field: "position",
            values,
        });
    }

    // Rotation：三个角度框（`R = Rz Ry Rx`），不再摊成 x/y/z/w 四个裸分量。
    let mut euler = prism_editor_core::viewport_math::quat_to_euler_degrees(transform.rotation);
    let mut rotation_row = false;
    ui.horizontal(|ui| {
        let mut row_delta = 0.0f32;
        if widgets::scrubbable_label(ui, "Rotation", &mut row_delta, 0.5) {
            for value in euler.iter_mut() {
                *value += row_delta;
            }
            rotation_row = true;
        }
        for axis in 0..3 {
            let mut changed =
                widgets::scrubbable_label(ui, axis_labels[axis], &mut euler[axis], 0.5);
            let response = ui
                .add(
                    egui::DragValue::new(&mut euler[axis])
                        .speed(0.5)
                        .max_decimals(2)
                        .suffix("°")
                        .custom_parser(widgets::number_parser),
                )
                .on_hover_text("Type an expression: 90/2, 45+15");
            transform_row_menu(&response, TransformRow::Rotation, transform, &mut write);
            if response.changed() {
                changed = true;
            }
            if changed {
                write(TransformEdit::Rotation(
                    prism_editor_core::viewport_math::euler_degrees_to_quat(euler),
                ));
                break;
            }
            marker(ui, axis, 1);
        }
    });
    if rotation_row {
        write(TransformEdit::Rotation(
            prism_editor_core::viewport_math::euler_degrees_to_quat(euler),
        ));
    }

    // Scale 单独一行，保持 Position / Rotation / Scale 的固定顺序。
    let mut scale = transform.scale;
    let mut scale_row: Option<[f32; 3]> = None;
    ui.horizontal(|ui| {
        let mut row_delta = 0.0f32;
        if widgets::scrubbable_label(ui, "Scale", &mut row_delta, 0.01) {
            for axis in 0..3 {
                scale[axis] += row_delta;
            }
            scale_row = Some(scale);
        }
        for axis in 0..3 {
            if widgets::scrubbable_label(ui, axis_labels[axis], &mut scale[axis], 0.01) {
                write(TransformEdit::Axis {
                    field: "scale",
                    axis,
                    value: scale[axis],
                });
            }
            let response = ui
                .add(
                    egui::DragValue::new(&mut scale[axis])
                        .speed(0.01)
                        .max_decimals(3)
                        .custom_parser(widgets::number_parser),
                )
                .on_hover_text("Type an expression: 90/2, 2*(3+4)");
            transform_row_menu(&response, TransformRow::Scale, transform, &mut write);
            if response.changed() {
                write(TransformEdit::Axis {
                    field: "scale",
                    axis,
                    value: scale[axis],
                });
            }
            marker(ui, axis, 2);
        }
    });
    if let Some(values) = scale_row {
        write(TransformEdit::Vector {
            field: "scale",
            values,
        });
    }
    let _ = state;
}

/// Transform 的三行（Position / Rotation / Scale），右键菜单按行共享剪贴板（#418 第 9 条）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransformRow {
    Position,
    Rotation,
    Scale,
}

impl TransformRow {
    fn label(self) -> &'static str {
        match self {
            Self::Position => "Position",
            Self::Rotation => "Rotation",
            Self::Scale => "Scale",
        }
    }

    /// 该行的重置值（局部坐标；旋转是四元数）。
    fn reset(self) -> Vec<f32> {
        match self {
            Self::Position => vec![0.0, 0.0, 0.0],
            Self::Scale => vec![1.0, 1.0, 1.0],
            Self::Rotation => vec![0.0, 0.0, 0.0, 1.0],
        }
    }

    fn from_transform(self, transform: &prism_editor_core::TransformJson) -> Vec<f32> {
        match self {
            Self::Position => transform.position.to_vec(),
            Self::Scale => transform.scale.to_vec(),
            Self::Rotation => transform.rotation.to_vec(),
        }
    }
}

/// Transform 数值框的右键菜单：Copy / Paste / Reset 整行（比 Edit 菜单近得多）。
fn transform_row_menu(
    response: &egui::Response,
    row: TransformRow,
    transform: &prism_editor_core::TransformJson,
    write: &mut impl FnMut(TransformEdit),
) {
    const CLIPBOARD: &str = "prism-transform-row-clipboard";
    let id = egui::Id::new(CLIPBOARD);
    response.context_menu(|ui| {
        if ui.button(format!("Copy {}", row.label())).clicked() {
            let values = row.from_transform(transform);
            ui.ctx().copy_text(format!("{values:?}"));
            ui.data_mut(|data| data.insert_temp(id, (row, values)));
            ui.close();
        }
        let clipboard = ui.data(|data| data.get_temp::<(TransformRow, Vec<f32>)>(id));
        let compatible = clipboard
            .as_ref()
            .map(|(kind, _)| *kind == row)
            .unwrap_or(false);
        if ui
            .add_enabled(
                compatible,
                egui::Button::new(format!("Paste {}", row.label())),
            )
            .on_hover_text("Rows only accept a value copied from the same row")
            .clicked()
        {
            if let Some((_, values)) = clipboard {
                apply_transform_row(row, &values, &mut *write);
            }
            ui.close();
        }
        if ui.button(format!("Reset {}", row.label())).clicked() {
            apply_transform_row(row, &row.reset(), &mut *write);
            ui.close();
        }
    });
}

/// 把一行 Transform 值写成一次编辑；形状不对时不写（粘贴 / 重置共用）。
fn apply_transform_row(row: TransformRow, values: &[f32], write: &mut impl FnMut(TransformEdit)) {
    match row {
        TransformRow::Position if values.len() == 3 => write(TransformEdit::Vector {
            field: "position",
            values: [values[0], values[1], values[2]],
        }),
        TransformRow::Scale if values.len() == 3 => write(TransformEdit::Vector {
            field: "scale",
            values: [values[0], values[1], values[2]],
        }),
        TransformRow::Rotation if values.len() == 4 => write(TransformEdit::Rotation([
            values[0], values[1], values[2], values[3],
        ])),
        _ => {}
    }
}

/// 瓦片地图组件名（`prism-scene` 的反射名）。
const TILEMAP_COMPONENT: &str = "prism_engine::scene::Tilemap";

/// 左键循环的下一个瓦片索引：`0 → 1 → … → palette → 0`。
fn next_tile(current: u32, palette: u32) -> u32 {
    let palette = palette.max(1);
    if current >= palette {
        0
    } else {
        current + 1
    }
}

fn tilemap_tiles(value: &serde_json::Value) -> Vec<u32> {
    value
        .get("tiles")
        .and_then(|tiles| tiles.as_array())
        .map(|tiles| {
            tiles
                .iter()
                .filter_map(|tile| tile.as_u64())
                .map(|tile| tile as u32)
                .collect()
        })
        .unwrap_or_default()
}

fn tilemap_columns(value: &serde_json::Value) -> usize {
    value
        .get("columns")
        .and_then(|columns| columns.as_u64())
        .unwrap_or(0) as usize
}

fn write_tiles(workspace: &mut Workspace, entity_id: u32, tiles: &[u32]) {
    workspace.set_field(
        entity_id,
        TILEMAP_COMPONENT,
        "tiles",
        serde_json::json!(tiles),
    );
}

/// 瓦片地图网格编辑器（#529）。
///
/// Phase 1 是「单层 + 数值索引」：`Palette` 决定左键循环范围，右键清空单格。
/// 调色板预览 / 刷子 / 矩形填充 / 规则瓦片属于 Phase 2，这里不做。
fn tilemap_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
    entity_id: u32,
    value: &serde_json::Value,
) {
    let mut tiles = tilemap_tiles(value);
    let authored_columns = tilemap_columns(value);
    let columns = if authored_columns > 0 {
        authored_columns
    } else {
        tiles.len().max(1)
    };
    let rows = tiles.len().div_ceil(columns.max(1));

    ui.horizontal(|ui| {
        ui.label(RichText::new("Tilemap").strong().size(12.0));
        ui.label(
            RichText::new(format!("{columns} x {rows}"))
                .size(10.0)
                .weak(),
        );
        if value
            .get("tileset_path")
            .and_then(|path| path.as_str())
            .is_some_and(|path| path.is_empty())
        {
            ui.label(
                RichText::new("no tileset")
                    .size(10.0)
                    .color(Color32::from_rgb(226, 168, 64)),
            )
            .on_hover_text("Set tileset_path before tiles can be rendered");
        }
    });

    ui.horizontal(|ui| {
        ui.label(RichText::new("Columns").size(10.5));
        let mut edited_columns = columns as u32;
        if ui
            .add(egui::DragValue::new(&mut edited_columns).range(1..=256))
            .on_hover_text("Re-flows the same tiles into this many columns")
            .changed()
        {
            workspace.set_field(
                entity_id,
                TILEMAP_COMPONENT,
                "columns",
                serde_json::json!(edited_columns),
            );
        }

        ui.label(RichText::new("Rows").size(10.5));
        let mut edited_rows = rows as u32;
        if ui
            .add(egui::DragValue::new(&mut edited_rows).range(1..=256))
            .on_hover_text("Pads with empty cells or drops the tail")
            .changed()
        {
            tiles.resize(edited_rows as usize * columns.max(1), 0);
            write_tiles(workspace, entity_id, &tiles);
        }

        ui.separator();
        ui.label(RichText::new("Palette").size(10.5))
            .on_hover_text("Left click cycles 0..=Palette, right click clears one cell");
        ui.add(egui::DragValue::new(&mut state.tilemap_palette).range(1..=64));
        if ui.small_button("Clear").clicked() {
            for tile in &mut tiles {
                *tile = 0;
            }
            write_tiles(workspace, entity_id, &tiles);
        }
    });

    let side = 18.0;
    egui::ScrollArea::vertical()
        .id_salt("tilemap-grid")
        .max_height(240.0)
        .show(ui, |ui| {
            for row in 0..rows {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let index = row * columns + column;
                        let Some(current) = tiles.get(index).copied() else {
                            continue;
                        };
                        let label = if current == 0 {
                            "\u{b7}".to_string()
                        } else {
                            current.to_string()
                        };
                        let response = ui.add_sized(
                            [side, side],
                            egui::Button::new(RichText::new(label).size(10.0)),
                        );
                        if response.clicked_by(egui::PointerButton::Secondary) {
                            tiles[index] = 0;
                            write_tiles(workspace, entity_id, &tiles);
                        } else if response.clicked() {
                            tiles[index] = next_tile(current, state.tilemap_palette);
                            write_tiles(workspace, entity_id, &tiles);
                        }
                    }
                });
            }
        });
}

fn inspector_single(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState, index: usize) {
    if index >= workspace.document().doc.entities.len() {
        workspace.clear_selection();
        return;
    }
    let entity_id = workspace.document().doc.id_of_index(index).unwrap_or(0);
    let name = workspace.document().doc.entities[index].display_name();
    let prefab = workspace.document().doc.entities[index]
        .prefab_link()
        .map(|link| link.to_string());

    // Name：**延迟提交**（Enter / 失焦），一次重命名 = 一个撤销步 + 标脏（#473）。
    // 以前逐字符写文档：既没有撤销步，也会把每次按键都当成一次编辑去触发
    // 整场重载（#435）。缓冲留在 `UiState` 里，否则每帧从文档重建会让输入框复位。
    if state.name_edit.as_ref().map(|(target, _)| *target) != Some(index) {
        state.name_edit = Some((index, name.clone()));
    }
    let mut buffer = state
        .name_edit
        .take()
        .map(|(_, buffer)| buffer)
        .unwrap_or_else(|| name.clone());
    ui.horizontal(|ui| {
        ui.label(RichText::new("Name").size(11.0));
        let response = ui.text_edit_singleline(&mut buffer);
        let (enter, escape) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Escape),
            )
        });
        if response.has_focus() && escape {
            buffer = name.clone();
            response.surrender_focus();
        } else if (response.has_focus() && enter) || response.lost_focus() {
            let trimmed = buffer.trim().to_string();
            if !trimmed.is_empty() && trimmed != name {
                workspace.rename_entity(index, &trimmed);
                buffer = trimmed;
            } else {
                buffer = name.clone();
            }
        }
    });
    state.name_edit = Some((index, buffer));

    // ---- 断掉的资产引用（#467） ----
    // 放在最上面、且不依赖组件 schema：有些组件在编辑器里没有 schema
    // （渲染成 "(no schema)"），断引用也不能因此不提示。
    let broken = workspace.missing_references(index);
    if !broken.is_empty() {
        for (field, value) in broken {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Missing")
                        .size(10.5)
                        .strong()
                        .color(Color32::from_rgb(232, 96, 96)),
                );
                ui.label(RichText::new(field).monospace().size(10.0))
                    .on_hover_text("Asset path field");
                ui.label(RichText::new(value).monospace().size(10.0).weak());
            });
        }
        ui.separator();
    }

    // ---- 光源预算（#468） ----
    // 前向渲染的局部光 SSBO 只有 `LIGHT_MAX` 个槽位；超出的按重要性挑，
    // 这里把「你这盏没画」直接写在检查器里，而不是让人对着参数发呆。
    let is_local_light = [
        "prism_engine::scene::Light",
        "prism_engine::scene::PointLight",
        "prism_engine::scene::SpotLight",
    ]
    .iter()
    .any(|name| workspace.document().doc.entities[index].has_component(name));
    if is_local_light {
        if state.dropped_light_ids.contains(&entity_id) {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Not rendered")
                        .size(10.5)
                        .strong()
                        .color(Color32::from_rgb(226, 168, 64)),
                );
                ui.label(
                    RichText::new(format!(
                        "only the {} most important of {} local lights are drawn",
                        state.lights_kept, state.lights_total
                    ))
                    .size(10.5)
                    .weak(),
                );
            });
            ui.separator();
        } else if state.lights_total > state.lights_kept {
            ui.label(
                RichText::new(format!(
                    "{} of {} local lights are drawn (the rest are dropped)",
                    state.lights_kept, state.lights_total
                ))
                .size(10.5)
                .weak(),
            );
            ui.separator();
        }
    }

    // ---- 预制体（引擎的 Prefab 区块） ----
    match &prefab {
        Some(link) => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Prefab").weak().size(10.0));
                ui.label(RichText::new(link).monospace().size(10.0));
            });
            ui.horizontal(|ui| {
                if ui
                    .small_button("Open Prefab")
                    .on_hover_text("Edit the source prefab; the breadcrumb will lead back here")
                    .clicked()
                {
                    let scene_index = workspace.active_index();
                    match workspace.open_prefab_from_instance(scene_index, entity_id) {
                        Ok(()) => state.status = format!("opened {link}"),
                        Err(error) => state.status = format!("open prefab failed: {error}"),
                    }
                }
                if ui.small_button("Revert").clicked() {
                    match workspace.revert_prefab_instance(entity_id) {
                        Ok(_) => state.status = "reverted prefab instance".to_string(),
                        Err(error) => state.status = format!("revert failed: {error}"),
                    }
                }
                if ui
                    .small_button("Sync All")
                    .on_hover_text("Sync every instance of this prefab in the document")
                    .clicked()
                {
                    match workspace.sync_prefab_instances(link) {
                        Ok(count) => state.status = format!("synced {count} instances"),
                        Err(error) => state.status = format!("sync failed: {error}"),
                    }
                }
                if ui
                    .small_button("Apply to Prefab")
                    .on_hover_text("Overwrite the prefab asset with this instance's subtree")
                    .clicked()
                {
                    match apply_to_prefab(workspace, index, link) {
                        Ok(count) => {
                            state.status = format!("applied {count} entities to {link}");
                            let _ = workspace.sync_prefab_instances(link);
                        }
                        Err(error) => state.status = format!("apply failed: {error}"),
                    }
                }
            });
        }
        None => {
            if ui
                .small_button("Save as Prefab…")
                .on_hover_text("Extract this subtree into assets/prefabs")
                .clicked()
            {
                state.prompt = Some(Prompt::new(
                    PromptKind::SavePrefab,
                    "Save as Prefab",
                    "预制体文件名（不含目录）",
                    name.clone(),
                ));
            }
        }
    }
    ui.separator();

    // ---- Transform ----
    let transform = workspace.document().doc.entities[index].transform.clone();
    ui.label(RichText::new("Transform").strong().size(12.0));
    transform_rows(ui, state, &transform, None, |edit| match edit {
        TransformEdit::Axis { field, axis, value } => {
            workspace.set_transform_axis(entity_id, field, axis, value);
        }
        TransformEdit::Vector { field, values } => {
            workspace.set_transform_vector(entity_id, field, values);
        }
        TransformEdit::Rotation(rotation) => {
            workspace.set_transform_rotation(entity_id, rotation);
        }
    });
    ui.separator();

    // ---- UI 布局组（#527）----
    // `Style.layout` 是嵌套 struct，通用字段控件会把它渲染成只读 JSON（#418），
    // 所以这里给布局组一个专门的编辑块。写入仍走 `set_field` 的点路径，
    // 撤销 / 标脏 / 重载语义与其它字段一致。
    if let Some(style) = workspace.document().doc.entities[index]
        .component(STYLE_COMPONENT)
        .cloned()
    {
        if let Some(layout) = style.get("layout") {
            layout_editor(ui, workspace, entity_id, layout);
            ui.separator();
        }
    }

    // ---- 瓦片地图网格（#529）----
    if let Some(tilemap) = workspace.document().doc.entities[index]
        .component(TILEMAP_COMPONENT)
        .cloned()
    {
        tilemap_editor(ui, workspace, state, entity_id, &tilemap);
        ui.separator();
    }

    // ---- 组件 ----
    ui.horizontal(|ui| {
        ui.label(RichText::new("Components").strong().size(12.0));
        ui.add(
            egui::TextEdit::singleline(&mut state.inspector_search)
                .hint_text("filter")
                .desired_width(80.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add_component_menu_for(ui, workspace, state, &[index]);
        });
    });

    let components: Vec<(String, Value)> = workspace.document().doc.entities[index]
        .components
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let filter = state.inspector_search.to_lowercase();
    let asset_paths = workspace.asset_paths();
    let doc_dir = workspace.document_dir();
    let registry = workspace.schema.clone();
    let mut actions: Vec<widgets::CardAction> = Vec::new();
    for (name, value) in components {
        if name == "prism_engine::scene::Active" {
            continue;
        }
        if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
            continue;
        }
        let Some(schema) = workspace.schema.component(&name).cloned() else {
            // 没有 schema 的组件（用户项目模块里自有类型、或引擎新增但还没生成
            // schema）：给一个可编辑的原始 JSON 兜底，而不是只能看「(no schema)」
            // 然后去手改场景文件（#418 的 Debug 视图）。
            component_debug_editor(ui, workspace, entity_id, &name, &value);
            continue;
        };
        let presets = workspace.component_preset_names(&schema.name);
        let mut set = |field: &str, value: Value| {
            workspace.set_field(entity_id, &schema.name, field, value);
        };
        widgets::component_card_with_assets(
            ui,
            &registry,
            &schema,
            &value,
            &mut actions,
            &mut set,
            &asset_paths,
            &doc_dir,
            &presets,
        );
        if name == "prism_engine::scene::LineRenderer" {
            line_renderer_editor(ui, workspace, entity_id, &schema.name, &value);
        } else if name == "prism_engine::scene::SpriteAnimation" {
            sprite_animation_editor(ui, workspace, index, entity_id, &value);
        } else if name == "prism_engine::scene::Animator" {
            animator_editor(ui, workspace, state, entity_id, &schema.name, &value);
        } else if name == "prism_engine::scene::SkeletonRoot" {
            skeleton_root_editor(ui, state, &schema.name, &value);
        } else if name == "prism_engine::scene::KeyframeAnimation" {
            keyframe_animation_editor(ui, workspace, entity_id, &schema.name, &value);
        } else if name == "prism_engine::scene::Terrain" {
            terrain_editor(ui, workspace, &value);
        }
    }
    for action in actions {
        match action {
            widgets::CardAction::Copy(name) => workspace.copy_component_at(index, &name),
            widgets::CardAction::Paste(name) => {
                let applied = workspace.paste_component_values(&[index], &name);
                state.status = if applied == 0 {
                    format!("nothing to paste into {name}")
                } else {
                    format!("pasted values into {name}")
                };
            }
            widgets::CardAction::Remove(name) => workspace.remove_component(&name),
            widgets::CardAction::MoveUp(name) => {
                workspace.move_component_at(index, &name, -1);
            }
            widgets::CardAction::MoveDown(name) => {
                workspace.move_component_at(index, &name, 1);
            }
            widgets::CardAction::SavePreset {
                component,
                name,
                value,
            } => {
                workspace.save_component_preset(&component, &name, value);
                state.status = format!("saved preset {name} for {component}");
            }
            widgets::CardAction::ApplyPreset { component, name } => {
                match workspace.component_preset_value(&component, &name) {
                    Some(value) => {
                        workspace.set_field(entity_id, &component, "", value);
                        state.status = format!("applied preset {name}");
                    }
                    None => state.status = format!("preset {name} not found"),
                }
            }
        }
    }
}

fn line_renderer_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    entity_id: u32,
    component: &str,
    value: &Value,
) {
    let Some(points) = value.get("points").and_then(Value::as_array) else {
        return;
    };
    ui.label(RichText::new("Positions").strong().size(11.0));
    let mut updated = points.clone();
    let mut action = None;
    for (index, point) in points.iter().enumerate() {
        let mut coords = [0.0f64; 3];
        for (axis, value) in coords.iter_mut().enumerate() {
            *value = point.get(axis).and_then(Value::as_f64).unwrap_or_default();
        }
        ui.horizontal(|ui| {
            ui.label(format!("{index}"));
            let mut changed = false;
            for coord in &mut coords {
                changed |= ui.add(egui::DragValue::new(coord).speed(0.05)).changed();
            }
            if changed {
                updated[index] = json!([coords[0], coords[1], coords[2]]);
            }
            if index + 1 < points.len()
                && ui
                    .small_button("+")
                    .on_hover_text("Subdivide after this point")
                    .clicked()
            {
                action = Some(LinePointAction::Subdivide(index));
            }
            if ui.small_button("x").on_hover_text("Remove point").clicked() {
                action = Some(LinePointAction::Remove(index));
            }
        });
    }
    if ui.small_button("Add Position").clicked() {
        action = Some(LinePointAction::Add);
    }
    if let Some(action) = action {
        apply_line_point_action(&mut updated, action);
    }
    if updated.as_slice() != points {
        workspace.set_field(entity_id, component, "points", Value::Array(updated));
    }
}

#[derive(Clone, Copy)]
enum LinePointAction {
    Add,
    Remove(usize),
    Subdivide(usize),
}

fn apply_line_point_action(points: &mut Vec<Value>, action: LinePointAction) {
    match action {
        LinePointAction::Add => {
            let next = points
                .last()
                .and_then(Value::as_array)
                .map(|point| {
                    [0, 1, 2]
                        .map(|axis| point.get(axis).and_then(Value::as_f64).unwrap_or_default())
                })
                .unwrap_or([0.0, 0.0, 0.0]);
            points.push(json!(next));
        }
        LinePointAction::Remove(index) => {
            if index < points.len() {
                points.remove(index);
            }
        }
        LinePointAction::Subdivide(index) => {
            let Some(a) = points.get(index).and_then(Value::as_array) else {
                return;
            };
            let Some(b) = points.get(index + 1).and_then(Value::as_array) else {
                return;
            };
            let midpoint = (0..3)
                .map(|axis| {
                    (a.get(axis).and_then(Value::as_f64).unwrap_or_default()
                        + b.get(axis).and_then(Value::as_f64).unwrap_or_default())
                        * 0.5
                })
                .collect::<Vec<_>>();
            points.insert(index + 1, json!(midpoint));
        }
    }
}

fn sprite_animation_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    index: usize,
    entity_id: u32,
    value: &Value,
) {
    let frames = value
        .get("frames")
        .and_then(Value::as_u64)
        .unwrap_or_default() as u32;
    let playing = value
        .get("playing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let current = workspace
        .document()
        .doc
        .entities
        .get(index)
        .and_then(|entity| entity.components.get("prism_engine::scene::SpriteRenderer"))
        .and_then(|renderer| renderer.get("frame_index"))
        .and_then(Value::as_u64)
        .unwrap_or_default() as u32;
    let max_frame = frames.saturating_sub(1);
    let current = current.min(max_frame);

    ui.label(RichText::new("Sprite Sheet Preview").strong().size(11.0));
    let mut target_frame = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(frames > 0 && !playing, egui::Button::new(icons::SKIP_BACK))
            .on_hover_text("First frame")
            .clicked()
        {
            target_frame = Some(0);
        }
        if ui
            .add_enabled(frames > 0 && !playing, egui::Button::new(icons::CARET_LEFT))
            .on_hover_text("Previous frame")
            .clicked()
        {
            target_frame = Some(current.saturating_sub(1));
        }
        ui.label(if frames == 0 { "No frames" } else { "Frame" });
        let mut selected = current;
        if ui
            .add_enabled(
                frames > 0 && !playing,
                egui::DragValue::new(&mut selected).range(0..=max_frame),
            )
            .changed()
        {
            target_frame = Some(selected);
        }
        ui.label(format!("/ {frames}"));
        if ui
            .add_enabled(
                frames > 0 && !playing,
                egui::Button::new(icons::CARET_RIGHT),
            )
            .on_hover_text("Next frame")
            .clicked()
        {
            target_frame = Some(current.saturating_add(1).min(max_frame));
        }
        if ui
            .add_enabled(
                frames > 0 && !playing,
                egui::Button::new(icons::SKIP_FORWARD),
            )
            .on_hover_text("Last frame")
            .clicked()
        {
            target_frame = Some(max_frame);
        }
    });
    if playing {
        ui.label(
            RichText::new("Pause playback to preview a frame.")
                .weak()
                .size(10.0),
        );
    }
    if let Some(frame) = target_frame {
        set_sprite_frame(workspace, entity_id, frame);
    }
}

fn set_sprite_frame(workspace: &mut Workspace, entity_id: u32, frame: u32) {
    workspace.set_field(
        entity_id,
        "prism_engine::scene::SpriteRenderer",
        "frame_index",
        json!(frame),
    );
}

/// 三条轨道的字段名 / 显示名 / 空轨道时的「单位值」。
///
/// 旋转写欧拉角（度），与组件本身和 `DirectionalLight` 的约定一致。
const CLIP_TRACKS: [(&str, &str, [f32; 3]); 3] = [
    ("position", "Position", [0.0, 0.0, 0.0]),
    ("euler_xyz", "Rotation (deg)", [0.0, 0.0, 0.0]),
    ("scale", "Scale", [1.0, 1.0, 1.0]),
];

/// 「关键帧」编辑区（`KeyframeAnimation`）。
///
/// 通用 reflect 卡片只能把轨道当只读 JSON 显示，这里给出真正能改的表格：
/// 每行「时间 + 三分量」，加 / 删关键帧，写入时按时间排序（采样假设升序）。
/// 播放本身在引擎侧（Play 时才跑），所以这里不画曲线预览。
/// `Animator` 的检查器：播放开关 + 速度 + **时间轴 scrub**（#560 P0-4）。
///
/// 拖动时间轴只改组件的 `time` 字段（一次撤销步），引擎下一帧重新求值姿态；
/// 时长来自已加载的剪辑，取不到时退化成无上限的拖动 + 「未知时长」提示。
/// 关键帧在时间轴上的横向偏移（像素，从轨道左边算起）。
///
/// 纯函数：`duration <= 0`、非有限时间、落在范围外的关键帧都丢掉，`width <= 0`
/// 直接返回空 —— 编辑器每帧都会调它，画一条错误的刻度比不画更误导人。
pub fn key_tick_offsets(times: &[f32], duration: f32, width: f32) -> Vec<f32> {
    if duration <= 0.0 || !duration.is_finite() || width <= 0.0 {
        return Vec::new();
    }
    times
        .iter()
        .filter(|time| time.is_finite() && **time >= 0.0 && **time <= duration)
        .map(|time| time / duration * width)
        .collect()
}

/// 点击位置（相对轨道左边的像素）落在哪一根刻度上，返回该刻度的时间。
///
/// 纯函数，便于单测：`tolerance` 是命中半径（像素），超出范围、非有限值、
/// 零长度轨道一律返回 `None`。取**最近**的一根，所以相邻关键帧挤在一起时点哪
/// 边就跳哪边。
pub fn key_tick_hit(
    times: &[f32],
    duration: f32,
    width: f32,
    x: f32,
    tolerance: f32,
) -> Option<f32> {
    if !x.is_finite() || !tolerance.is_finite() || tolerance < 0.0 {
        return None;
    }
    let mut best: Option<(f32, f32)> = None; // (距离, 时间)
    for time in times {
        // 与 `key_tick_offsets` 同一套过滤，避免两条路径对「有效刻度」理解不同。
        if !time.is_finite() || *time < 0.0 || *time > duration {
            continue;
        }
        let distance = (*time / duration * width - x).abs();
        if distance > tolerance {
            continue;
        }
        match best {
            Some((current, _)) if current <= distance => {}
            _ => best = Some((distance, *time)),
        }
    }
    best.map(|(_, time)| time)
}

/// 命中半径（像素）：刻度带只有 6px 高，太窄会点不中，太宽会抢走空白处的点击。
const KEY_TICK_HIT_TOLERANCE: f32 = 4.0;

/// 在滑杆下方画一条 6px 的刻度带：每个关键帧一根竖线；点中刻度返回该时间。
/// Notify 在时间轴上的标记：`(像素偏移, 名字)`，只保留落在 `0..=duration` 的有限时间。
///
/// 与 [`key_tick_offsets`] 同一套过滤规则，多带一个名字给 tooltip。`duration` 或
/// `width` 不合法时返回空（宁可这条色带不画）。
pub fn notify_markers(notifies: &[(f32, String)], duration: f32, width: f32) -> Vec<(f32, String)> {
    if duration <= 0.0 || !duration.is_finite() || width <= 0.0 {
        return Vec::new();
    }
    notifies
        .iter()
        .filter(|(time, _)| time.is_finite() && *time >= 0.0 && *time <= duration)
        .map(|(time, name)| (time / duration * width, name.clone()))
        .collect()
}

/// 在滑杆下方画 Notify 标记（小三角 + 悬停名字）。放在关键帧刻度带**下面**。
fn draw_notify_markers(
    ui: &mut egui::Ui,
    slider_rect: egui::Rect,
    notifies: &[(f32, String)],
    duration: f32,
) {
    let markers = notify_markers(notifies, duration, slider_rect.width());
    if markers.is_empty() {
        return;
    }
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(slider_rect.width(), 7.0), egui::Sense::hover());
    let painter = ui.painter();
    let mut hovered: Option<&str> = None;
    for (offset, name) in &markers {
        let x = rect.left() + offset;
        let near = response
            .hover_pos()
            .is_some_and(|pos| (pos.x - x).abs() <= KEY_TICK_HIT_TOLERANCE);
        if near {
            hovered = Some(name.as_str());
        }
        // 朝上的三角：和关键帧的竖线区分开，一眼能看出这是事件而不是关键帧。
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(x, rect.top() + 1.0),
                egui::pos2(x - 2.5, rect.bottom() - 1.0),
                egui::pos2(x + 2.5, rect.bottom() - 1.0),
            ],
            egui::Color32::from_rgb(110, 200, 255),
            egui::Stroke::NONE,
        ));
    }
    if let Some(name) = hovered {
        response.on_hover_text(format!("Notify: {name}"));
    }
}

fn draw_key_ticks(
    ui: &mut egui::Ui,
    slider_rect: egui::Rect,
    times: &[f32],
    duration: f32,
) -> Option<f32> {
    let offsets = key_tick_offsets(times, duration, slider_rect.width());
    if offsets.is_empty() {
        return None;
    }
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(slider_rect.width(), 6.0), egui::Sense::click());
    let painter = ui.painter();
    let hovered = response.hover_pos().map(|pos| pos.x - rect.left());
    let mut hovered_tick = None;
    for offset in &offsets {
        let x = rect.left() + offset;
        let near =
            hovered.is_some_and(|x_hover| (x_hover - offset).abs() <= KEY_TICK_HIT_TOLERANCE);
        if near {
            hovered_tick = Some(*offset);
        }
        painter.line_segment(
            [
                egui::pos2(x, rect.top() + 1.0),
                egui::pos2(x, rect.bottom() - 1.0),
            ],
            egui::Stroke::new(
                if near { 2.0 } else { 1.0 },
                egui::Color32::from_rgb(255, 206, 84),
            ),
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let response = response.on_hover_text("Jump to this keyframe");

    let click = response.interact_pointer_pos()?;
    if !response.clicked() {
        return None;
    }
    key_tick_hit(
        times,
        duration,
        rect.width(),
        click.x - rect.left(),
        KEY_TICK_HIT_TOLERANCE,
    )
}

/// 混合空间参数轴的取值范围（最小 / 最大样本位置）。
///
/// 样本位置可能乱序、也可能全都一样（作者填错），所以范围由数据本身决定：
/// 没有任何有限值时返回 `None`，调用方就不用画轴了。
pub fn blend_axis_range(positions: &[f32]) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut any = false;
    for position in positions.iter().copied().filter(|value| value.is_finite()) {
        any = true;
        min = min.min(position);
        max = max.max(position);
    }
    any.then_some((min, max))
}

/// 每个样本在参数轴条带上的横向偏移（像素）。
///
/// 取值范围退化（所有样本一个值）时全部落在**正中**：既不会挤在左端，也不会因为
/// 除零得到 NaN。非有限位置被丢掉。
pub fn blend_axis_offsets(positions: &[f32], width: f32) -> Vec<f32> {
    if width <= 0.0 || !width.is_finite() {
        return Vec::new();
    }
    let Some((min, max)) = blend_axis_range(positions) else {
        return Vec::new();
    };
    let span = max - min;
    positions
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .map(|value| {
            if span <= 1e-6 {
                width * 0.5
            } else {
                (value - min) / span * width
            }
        })
        .collect()
}

/// 条带上的横向位置反算回参数值（点击/拖动时用）。
///
/// `x` 会先夹到 `0..=width`，所以点出边界外也不会把参数推到样本范围之外。
pub fn blend_axis_value_at(x: f32, width: f32, min: f32, max: f32) -> Option<f32> {
    if width <= 0.0 || !width.is_finite() || !x.is_finite() {
        return None;
    }
    if !min.is_finite() || !max.is_finite() {
        return None;
    }
    let t = (x / width).clamp(0.0, 1.0);
    Some(min + (max - min) * t)
}

/// 二维混合空间样本的包围盒 `(min, max)`（只统计有限坐标）。
///
/// 某个轴跨度退化（所有样本同一行 / 同一列）时把该轴扩成 ±1，避免画图与拖动时
/// 除零；没有任何有效样本返回 `None`。
pub fn blend_pad_bounds(positions: &[[f32; 2]]) -> Option<([f32; 2], [f32; 2])> {
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    let mut any = false;
    for position in positions {
        if !position[0].is_finite() || !position[1].is_finite() {
            continue;
        }
        any = true;
        min[0] = min[0].min(position[0]);
        min[1] = min[1].min(position[1]);
        max[0] = max[0].max(position[0]);
        max[1] = max[1].max(position[1]);
    }
    if !any {
        return None;
    }
    for axis in 0..2 {
        if max[axis] - min[axis] <= 1e-6 {
            min[axis] -= 1.0;
            max[axis] += 1.0;
        }
    }
    Some((min, max))
}

/// 把二维混合空间画布上的相对坐标 `(x, y)`（像素，原点在左上）映射回参数平面。
///
/// 屏幕 y 向下，参数 y 向上，所以第二个轴取反；越界夹到包围盒内。零 / 非有限
/// 画布尺寸返回 `None`。
pub fn blend_pad_value_at(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    min: [f32; 2],
    max: [f32; 2],
) -> Option<[f32; 2]> {
    if width <= 0.0
        || height <= 0.0
        || !width.is_finite()
        || !height.is_finite()
        || !x.is_finite()
        || !y.is_finite()
    {
        return None;
    }
    let tx = (x / width).clamp(0.0, 1.0);
    let ty = (1.0 - (y / height).clamp(0.0, 1.0)).clamp(0.0, 1.0);
    Some([
        min[0] + (max[0] - min[0]) * tx,
        min[1] + (max[1] - min[1]) * ty,
    ])
}

/// 画一条混合空间参数轴：底色条 + 每个样本一根竖线 + 当前参数的位置；点一下改参数。
///
/// 返回被点出来的新参数值（`None` = 没动）。拖动时间轴看姿态时，这条轴是唯一
/// 能看出「现在落在哪两个样本之间」的地方。
fn draw_blend_axis(ui: &mut egui::Ui, positions: &[f32], parameter: f32) -> Option<f32> {
    let Some((min, max)) = blend_axis_range(positions) else {
        return None;
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(80.0), 16.0),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(32, 34, 38));

    // 样本刻度。
    for offset in blend_axis_offsets(positions, rect.width()) {
        let x = rect.left() + offset;
        painter.line_segment(
            [
                egui::pos2(x, rect.top() + 3.0),
                egui::pos2(x, rect.bottom() - 3.0),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 124, 132)),
        );
    }

    // 当前参数（夹到样本范围内）。
    let span = max - min;
    let normalized = if span <= 1e-6 {
        0.5
    } else {
        ((parameter.clamp(min, max) - min) / span).clamp(0.0, 1.0)
    };
    let marker = rect.left() + normalized * rect.width();
    painter.circle_filled(
        egui::pos2(marker, rect.center().y),
        4.0,
        egui::Color32::from_rgb(255, 206, 84),
    );

    if response.clicked() || response.dragged() {
        if let Some(pointer) = response.interact_pointer_pos() {
            return blend_axis_value_at(pointer.x - rect.left(), rect.width(), min, max);
        }
    }
    response.on_hover_text("Blend parameter — click or drag to scrub the blend");
    None
}

/// 画二维混合空间：样本点 + 当前参数位置；点 / 拖画布改参数。
///
/// 返回新的 `[x, y]`（`None` = 没动）。样本的包围盒就是可拖范围，凸包外的点
/// 运行时会被夹到边界上。
fn draw_blend_pad(ui: &mut egui::Ui, positions: &[[f32; 2]], point: [f32; 2]) -> Option<[f32; 2]> {
    let Some((min, max)) = blend_pad_bounds(positions) else {
        return None;
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(120.0), 120.0),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(32, 34, 38));

    let to_screen = |value: [f32; 2]| {
        let tx = (value[0] - min[0]) / (max[0] - min[0]);
        let ty = (value[1] - min[1]) / (max[1] - min[1]);
        egui::pos2(
            rect.left() + tx.clamp(0.0, 1.0) * rect.width(),
            rect.bottom() - ty.clamp(0.0, 1.0) * rect.height(),
        )
    };
    for position in positions {
        if position[0].is_finite() && position[1].is_finite() {
            painter.circle_filled(
                to_screen(*position),
                3.0,
                egui::Color32::from_rgb(120, 124, 132),
            );
        }
    }
    painter.circle_filled(to_screen(point), 5.0, egui::Color32::from_rgb(255, 206, 84));

    if response.clicked() || response.dragged() {
        if let Some(pointer) = response.interact_pointer_pos() {
            return blend_pad_value_at(
                pointer.x - rect.left(),
                pointer.y - rect.top(),
                rect.width(),
                rect.height(),
                min,
                max,
            );
        }
    }
    response.on_hover_text("2D blend parameter — click or drag on the plane");
    None
}

fn animator_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &UiState,
    entity_id: u32,
    component: &str,
    value: &Value,
) {
    let clip_path = value
        .get("clip_path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let duration = state
        .clip_durations
        .get(&clip_path)
        .copied()
        .unwrap_or_default();
    let mut time = value.get("time").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let mut speed = value.get("speed").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let mut playing = value
        .get("playing")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut looped = value.get("looped").and_then(Value::as_bool).unwrap_or(true);
    let blend_positions: Vec<f32> = value
        .get("blend_positions")
        .and_then(Value::as_array)
        .map(|positions| {
            positions
                .iter()
                .filter_map(Value::as_f64)
                .map(|value| value as f32)
                .collect()
        })
        .unwrap_or_default();
    let mut blend_parameter = value
        .get("blend_parameter")
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let blend_2d_positions: Vec<[f32; 2]> = value
        .get("blend_2d_positions")
        .and_then(Value::as_array)
        .map(|positions| {
            positions
                .iter()
                .filter_map(|position| {
                    let pair = position.as_array()?;
                    Some([
                        pair.first()?.as_f64()? as f32,
                        pair.get(1)?.as_f64()? as f32,
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let mut blend_parameter_y = value
        .get("blend_parameter_y")
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let mut edited = false;

    ui.horizontal(|ui| {
        let label = if playing { "⏸ Pause" } else { "▶ Play" };
        if ui.button(label).clicked() {
            playing = !playing;
            edited = true;
        }
        if ui.button("⏮").on_hover_text("restart").clicked() {
            time = 0.0;
            edited = true;
        }
        ui.label(RichText::new(clip_label(&clip_path)).size(11.0));
    });

    if duration > 0.0 {
        let response = ui.add(
            egui::Slider::new(&mut time, 0.0..=duration)
                .text("time")
                .suffix("s")
                .max_decimals(2),
        );
        if response.changed() {
            edited = true;
        }
        if let Some(keys) = state.clip_keys.get(&clip_path) {
            if let Some(key_time) = draw_key_ticks(ui, response.rect, keys, duration) {
                time = key_time;
                edited = true;
            }
        }
        if let Some(notifies) = state.clip_notifies.get(&clip_path) {
            draw_notify_markers(ui, response.rect, notifies, duration);
        }
        ui.label(
            RichText::new(format!("duration {duration:.2}s"))
                .size(10.0)
                .weak(),
        );
    } else {
        let response = ui.add(
            egui::DragValue::new(&mut time)
                .speed(0.01)
                .range(0.0..=3600.0)
                .suffix("s"),
        );
        if response.changed() {
            edited = true;
        }
        ui.label(
            RichText::new("duration unknown — the clip is not loaded yet")
                .size(10.0)
                .weak(),
        );
    }

    ui.horizontal(|ui| {
        if ui.checkbox(&mut looped, "Loop").changed() {
            edited = true;
        }
        if ui
            .add(
                egui::DragValue::new(&mut speed)
                    .speed(0.01)
                    .range(0.0..=8.0),
            )
            .changed()
        {
            edited = true;
        }
        ui.label(RichText::new("speed").size(10.0).weak());
    });

    // 二维混合空间：参数平面 + 样本点（#560 P1）。样本够三角化时优先显示；
    // 否则退回一维参数轴。
    if blend_2d_positions.len() >= 3 {
        ui.label(
            RichText::new(format!(
                "blend parameter ({blend_parameter:.2}, {blend_parameter_y:.2})"
            ))
            .size(10.0)
            .weak(),
        );
        if let Some(next) = draw_blend_pad(
            ui,
            &blend_2d_positions,
            [blend_parameter, blend_parameter_y],
        ) {
            blend_parameter = next[0];
            blend_parameter_y = next[1];
            edited = true;
        }
    } else if blend_positions.len() >= 2 {
        ui.label(
            RichText::new(format!("blend parameter {blend_parameter:.2}"))
                .size(10.0)
                .weak(),
        );
        if let Some(next) = draw_blend_axis(ui, &blend_positions, blend_parameter) {
            blend_parameter = next;
            edited = true;
        }
    }

    // 状态机（#560 P1）：显示当前状态与机器路径。状态由引擎每帧写回组件，这里只读；
    // 想换起始状态可以直接改 `state` 字段（Add Component 的通用卡片里）。
    let machine_path = value
        .get("state_machine_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !machine_path.is_empty() {
        let state = value.get("state").and_then(Value::as_str).unwrap_or("");
        ui.horizontal(|ui| {
            ui.label(RichText::new("state").size(10.0).weak());
            ui.label(
                RichText::new(if state.is_empty() { "(first)" } else { state })
                    .size(10.0)
                    .strong(),
            );
            ui.label(RichText::new(clip_label(machine_path)).size(10.0).weak())
                .on_hover_text(machine_path);
        });
    }

    // 动画图（#560 P1）：只读展示路径；图本身的编辑在后续图面板。
    let anim_graph_path = value
        .get("anim_graph_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !anim_graph_path.is_empty() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("graph").size(10.0).weak());
            ui.label(
                RichText::new(clip_label(anim_graph_path))
                    .size(10.0)
                    .strong(),
            )
            .on_hover_text(anim_graph_path);
        });
    }

    // 动画图层（#560 P1）：路径 / 权重 / additive / 骨骼遮罩都可直接编辑，
    // 写回场景 JSON 的 `layers` 数组；空遮罩 = 全身。
    #[derive(Clone)]
    struct LayerRow {
        clip_path: String,
        weight: f32,
        additive: bool,
        bone_mask: String,
    }
    let mut layers: Vec<LayerRow> = value
        .get("layers")
        .and_then(Value::as_array)
        .map(|layers| {
            layers
                .iter()
                .map(|layer| LayerRow {
                    clip_path: layer
                        .get("clip_path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    weight: layer.get("weight").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                    additive: layer
                        .get("additive")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    bone_mask: layer
                        .get("bone_mask")
                        .and_then(Value::as_array)
                        .map(|names| {
                            names
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut layers_edited = false;
    egui::CollapsingHeader::new(format!("Animation Layers ({})", layers.len()))
        .id_salt("animation-layers")
        .show(ui, |ui| {
            let mut remove: Option<usize> = None;
            let mut add = false;
            for (index, layer) in layers.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("#{}", index + 1)).strong());
                        if ui.small_button("remove").clicked() {
                            remove = Some(index);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("clip").size(10.0).weak());
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut layer.clip_path)
                                    .hint_text("clips/aim.ranim")
                                    .desired_width(180.0),
                            )
                            .changed()
                        {
                            layers_edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("weight").size(10.0).weak());
                        if ui
                            .add(egui::Slider::new(&mut layer.weight, 0.0..=1.0))
                            .changed()
                        {
                            layers_edited = true;
                        }
                        if ui.checkbox(&mut layer.additive, "additive").changed() {
                            layers_edited = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("bones").size(10.0).weak());
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut layer.bone_mask)
                                    .hint_text("empty = all bones")
                                    .desired_width(180.0),
                            )
                            .on_hover_text("Comma-separated joint names; empty = whole body.")
                            .changed()
                        {
                            layers_edited = true;
                        }
                    });
                });
            }
            if let Some(index) = remove {
                layers.remove(index);
                layers_edited = true;
            }
            if ui.button("+ Add layer").clicked() {
                add = true;
            }
            if add {
                layers.push(LayerRow {
                    clip_path: String::new(),
                    weight: 1.0,
                    additive: false,
                    bone_mask: String::new(),
                });
                layers_edited = true;
            }
        });
    if layers_edited {
        let encoded: Vec<Value> = layers
            .iter()
            .map(|layer| {
                let bone_mask: Vec<Value> = layer
                    .bone_mask
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(|name| Value::String(name.to_string()))
                    .collect();
                json!({
                    "clip_path": layer.clip_path,
                    "weight": layer.weight as f64,
                    "additive": layer.additive,
                    "bone_mask": bone_mask,
                })
            })
            .collect();
        workspace.set_field(entity_id, component, "layers", Value::Array(encoded));
    }

    if edited {
        workspace.set_field(entity_id, component, "time", Value::from(time as f64));
        workspace.set_field(entity_id, component, "speed", Value::from(speed as f64));
        workspace.set_field(entity_id, component, "playing", Value::Bool(playing));
        workspace.set_field(entity_id, component, "looped", Value::Bool(looped));
        if blend_positions.len() >= 2 || blend_2d_positions.len() >= 3 {
            workspace.set_field(
                entity_id,
                component,
                "blend_parameter",
                Value::from(blend_parameter as f64),
            );
        }
        if blend_2d_positions.len() >= 3 {
            workspace.set_field(
                entity_id,
                component,
                "blend_parameter_y",
                Value::from(blend_parameter_y as f64),
            );
        }
    }
}

/// `SkeletonRoot` 的检查器：骨架路径 + 只读的骨骼树（#560 P0-4）。
fn skeleton_root_editor(ui: &mut Ui, state: &UiState, component: &str, value: &Value) {
    let skeleton_path = value
        .get("skeleton_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ui.label(RichText::new(clip_label(skeleton_path)).size(11.0));
    let joints = state.skeleton_joints.get(skeleton_path);
    match joints {
        Some(joints) if !joints.is_empty() => {
            egui::CollapsingHeader::new(format!("{} bones", joints.len()))
                .id_salt((component, skeleton_path))
                .show(ui, |ui| {
                    for (index, joint) in joints.iter().enumerate() {
                        let depth = joint_depth(joints, index);
                        ui.label(
                            RichText::new(format!(
                                "{:indent$}{}",
                                "",
                                joint.name,
                                indent = depth * 2
                            ))
                            .size(10.0)
                            .monospace(),
                        );
                    }
                });
        }
        _ => {
            ui.label(RichText::new("skeleton not loaded yet").size(10.0).weak());
        }
    }
}

/// 关节在层次里的深度（父级缺失或成环时停在 0，避免无限上溯）。
fn joint_depth(joints: &[SkeletonJointRow], index: usize) -> usize {
    let mut depth = 0usize;
    let mut current = joints.get(index).and_then(|joint| joint.parent);
    let mut guard = 0usize;
    while let Some(parent) = current {
        guard += 1;
        if guard > joints.len() {
            break;
        }
        depth += 1;
        current = joints.get(parent).and_then(|joint| joint.parent);
    }
    depth
}

/// 资产路径只显示最后一段，长的 `assets/...` 前缀在检查器里没信息量。
fn clip_label(path: &str) -> String {
    if path.is_empty() {
        "(none)".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    }
}

/// 高度图片段统计（#517）：检查器直接显示，不再要求用户去日志里找
/// `terrain:` 警告。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainHeightmapStats {
    pub cells: usize,
    pub samples: usize,
    pub min: f32,
    pub max: f32,
}

/// 解析 `.raw`（f32 LE）高度图并校验分辨率，规则与引擎的
/// `load_heightfield` 一致：
///
/// - `resolution == 0` 时样本数必须是完全平方数；
/// - 非 0 时样本数必须正好是 `resolution²`；
/// - 非有限高度按 0 处理（与 `Heightfield::from_raw` 相同）。
pub fn terrain_heightmap_stats(
    bytes: &[u8],
    resolution: u32,
) -> Result<TerrainHeightmapStats, String> {
    if bytes.len() % 4 != 0 {
        return Err(format!(
            "{} bytes is not a multiple of 4 (f32 samples)",
            bytes.len()
        ));
    }
    let samples = bytes.len() / 4;
    let cells = if resolution == 0 {
        let cells = (samples as f64).sqrt() as usize;
        if cells < 2 || cells * cells != samples {
            return Err(format!(
                "expected a square heightmap, got {samples} samples"
            ));
        }
        cells
    } else {
        let cells = resolution.clamp(2, 1024) as usize;
        if samples != cells * cells {
            return Err(format!(
                "expected {} samples for {cells}×{cells} cells, got {samples}",
                cells * cells
            ));
        }
        cells
    };
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for chunk in bytes.chunks_exact(4) {
        let raw = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let value = if raw.is_finite() { raw } else { 0.0 };
        min = min.min(value);
        max = max.max(value);
    }
    Ok(TerrainHeightmapStats {
        cells,
        samples,
        min,
        max,
    })
}

/// `Terrain` 的检查器补充区：高度图文件、分辨率与高度范围（#517）。
///
/// 通用 reflect 卡片已经能改字段；这里只补运行时才会知道的信息，并把
/// 「文件缺失 / 字节数不匹配 / 不是方形」翻译成和 `load_heightfield` 一致的提示。
/// Terrain 笔刷参数（#517）：参数存 egui 临时数据，Scene 视口的 Sculpt 模式读取。
fn terrain_sculpt_controls(ui: &mut Ui) {
    ui.separator();
    ui.label(RichText::new("Sculpt brush").strong().size(11.0));
    ui.label(
        RichText::new("Enable Sculpt in the toolbar, then drag in the Scene view.")
            .weak()
            .size(10.0),
    );
    let mut brush = terrain_brush_settings(ui);
    ui.horizontal(|ui| {
        ui.label("Mode");
        for (mode, label) in [
            (SculptMode::Raise, "Raise"),
            (SculptMode::Lower, "Lower"),
            (SculptMode::Smooth, "Smooth"),
            (SculptMode::Flatten, "Flatten"),
        ] {
            ui.selectable_value(&mut brush.mode, mode, label);
        }
    });
    ui.add(egui::Slider::new(&mut brush.radius, 0.01..=1.0).text("Radius"));
    ui.add(
        egui::Slider::new(&mut brush.strength, 0.01..=100.0)
            .logarithmic(true)
            .text("Strength"),
    );
    set_terrain_brush_settings(ui, brush);
}

fn terrain_editor(ui: &mut Ui, workspace: &Workspace, value: &Value) {
    let heightmap = value
        .get("heightmap")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let resolution = value.get("resolution").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height_scale = value
        .get("height_scale")
        .and_then(Value::as_f64)
        .unwrap_or(1.0) as f32;
    let height_scale = if height_scale.is_finite() {
        height_scale
    } else {
        1.0
    };

    ui.separator();
    ui.label(RichText::new("Heightmap").strong().size(11.0));
    if heightmap.is_empty() {
        ui.label(
            RichText::new("no heightmap — flat terrain at y = 0")
                .weak()
                .size(10.0),
        );
        return;
    }
    ui.label(RichText::new(heightmap).monospace().size(10.0));

    let Some(root) = workspace.project_root.as_ref() else {
        ui.label(
            RichText::new("no project root; cannot read the heightmap")
                .weak()
                .size(10.0),
        );
        return;
    };
    // 引擎的 `heightmap` 是相对 `assets/` 的路径；也接受显式 `assets/...`。
    let relative = heightmap.strip_prefix("assets/").unwrap_or(heightmap);
    let path = root.join("assets").join(relative);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            ui.colored_label(
                Color32::from_rgb(220, 90, 90),
                format!("cannot read {}: {error}", path.display()),
            );
            return;
        }
    };

    match terrain_heightmap_stats(&bytes, resolution) {
        Ok(stats) => {
            ui.label(format!(
                "{} × {} cells ({} samples)",
                stats.cells, stats.cells, stats.samples
            ));
            ui.label(format!(
                "height {:.2} .. {:.2} raw → {:.2} .. {:.2} m",
                stats.min,
                stats.max,
                stats.min * height_scale,
                stats.max * height_scale
            ));
            if let Some(size) = value.get("size").and_then(Value::as_array) {
                if size.len() >= 2 {
                    let x = size[0].as_f64().unwrap_or(0.0);
                    let z = size[1].as_f64().unwrap_or(0.0);
                    ui.label(format!("world {x:.1} × {z:.1} m"));
                }
            }
            terrain_sculpt_controls(ui);
        }
        Err(error) => {
            ui.colored_label(
                Color32::from_rgb(220, 90, 90),
                format!("heightmap: {error}"),
            );
        }
    }
}

fn keyframe_animation_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    entity_id: u32,
    component: &str,
    value: &Value,
) {
    let mut longest = 0.0f32;
    for (field, _, _) in CLIP_TRACKS {
        for key in value
            .get(field)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            longest = longest.max(time_of(key) as f32);
        }
    }
    let duration = value
        .get("duration")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let length_label = if duration > 0.0 {
        format!("{duration:.2}s")
    } else {
        format!("{longest:.2}s (auto)")
    };
    ui.label(
        RichText::new(format!("Keyframes · {length_label}"))
            .strong()
            .size(11.0),
    );

    for (field, label, identity) in CLIP_TRACKS {
        let keys = value
            .get(field)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if let Some(updated) = keyframe_track_editor(ui, label, field, keys, identity) {
            workspace.set_field(entity_id, component, field, Value::Array(updated));
        }
    }
}

/// 单条轨道的表格；返回 `Some` 表示需要写回新数组。
fn keyframe_track_editor(
    ui: &mut Ui,
    label: &str,
    field: &str,
    keys: &[Value],
    identity: [f32; 3],
) -> Option<Vec<Value>> {
    let mut updated = keys.to_vec();
    let mut remove = None;
    let mut dirty = false;
    egui::CollapsingHeader::new(format!("{label} ({})", keys.len()))
        .id_salt(("clip-track", field))
        .default_open(true)
        .show(ui, |ui| {
            for (index, key) in keys.iter().enumerate() {
                let mut time = time_of(key) as f32;
                let mut coords = identity;
                for (axis, coord) in coords.iter_mut().enumerate() {
                    *coord = key
                        .get("value")
                        .and_then(|value| value.get(axis))
                        .and_then(Value::as_f64)
                        .unwrap_or(identity[axis] as f64) as f32;
                }
                let (changed, remove_clicked) = ui
                    .horizontal(|ui| {
                        let mut changed = ui
                            .add(
                                egui::DragValue::new(&mut time)
                                    .speed(0.01)
                                    .range(0.0..=3600.0)
                                    .suffix("s"),
                            )
                            .changed();
                        for (axis, coord) in coords.iter_mut().enumerate() {
                            changed |= ui
                                .add(
                                    egui::DragValue::new(coord)
                                        .speed(0.01)
                                        .prefix(["X ", "Y ", "Z "][axis]),
                                )
                                .changed();
                        }
                        let remove_clicked =
                            ui.small_button("x").on_hover_text("Remove key").clicked();
                        (changed, remove_clicked)
                    })
                    .inner;
                if remove_clicked {
                    remove = Some(index);
                }
                // 同一行的改动合并成一次写回。
                if changed {
                    updated[index] = json!({ "time": time, "value": coords });
                    dirty = true;
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .small_button("+ Key")
                    .on_hover_text("Append a key one second after the last one")
                    .clicked()
                {
                    append_key(&mut updated, identity);
                    dirty = true;
                }
            });
        });
    if let Some(index) = remove {
        if index < updated.len() {
            updated.remove(index);
            dirty = true;
        }
    }
    if !dirty {
        return None;
    }
    // 采样按时间升序（同时间保持插入顺序），所以写回前统一排序。
    sort_key_values(&mut updated);
    Some(updated)
}

/// 在轨道末尾追加一个关键帧：时间 = 上一个 +1 秒，值沿用上一个（空轨道用单位值）。
fn append_key(keys: &mut Vec<Value>, identity: [f32; 3]) {
    let last = keys.last();
    let time = last.map(|key| time_of(key) + 1.0).unwrap_or_default();
    let value = last
        .and_then(|key| key.get("value"))
        .cloned()
        .unwrap_or_else(|| json!(identity));
    keys.push(json!({ "time": time, "value": value }));
}

/// 按时间稳定排序；同一时刻保持插入顺序（采样假设升序）。
fn sort_key_values(keys: &mut [Value]) {
    keys.sort_by(|a, b| time_of(a).total_cmp(&time_of(b)));
}

fn time_of(key: &Value) -> f64 {
    key.get("time").and_then(Value::as_f64).unwrap_or_default()
}

#[cfg(test)]
mod keyframe_editor_tests {
    use super::*;

    #[test]
    fn notify_markers_keep_their_names_and_drop_out_of_range_ones() {
        let notifies = vec![
            (0.0, "Footstep.L".to_string()),
            (0.5, "Footstep.R".to_string()),
            (9.0, "too late".to_string()),
            (f32::NAN, "broken".to_string()),
        ];
        let markers = notify_markers(&notifies, 2.0, 200.0);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0], (0.0, "Footstep.L".to_string()));
        assert_eq!(markers[1], (50.0, "Footstep.R".to_string()));

        assert!(notify_markers(&notifies, 0.0, 200.0).is_empty());
        assert!(notify_markers(&notifies, 2.0, 0.0).is_empty());
        assert!(notify_markers(&[], 2.0, 200.0).is_empty());
    }

    #[test]
    fn blend_pad_bounds_cover_finite_samples_and_expand_degenerate_axes() {
        let (min, max) = blend_pad_bounds(&[[0.0, 0.0], [10.0, 4.0]]).expect("bounds");
        assert_eq!(min, [0.0, 0.0]);
        assert_eq!(max, [10.0, 4.0]);

        // 所有样本同一行：y 轴扩成 ±1，避免除零。
        let (min, max) = blend_pad_bounds(&[[0.0, 2.0], [10.0, 2.0]]).expect("bounds");
        assert_eq!(min[1], 1.0);
        assert_eq!(max[1], 3.0);

        // 非有限坐标被忽略；全坏 / 空 → None。
        let (min, max) = blend_pad_bounds(&[[f32::NAN, 0.0], [1.0, 2.0]]).expect("bounds");
        // 只剩一个点：两个轴都退化，各扩成 ±1。
        assert_eq!(min, [0.0, 1.0]);
        assert_eq!(max, [2.0, 3.0]);
        assert!(blend_pad_bounds(&[]).is_none());
        assert!(blend_pad_bounds(&[[f32::INFINITY, 0.0]]).is_none());
    }

    #[test]
    fn blend_pad_clicks_map_back_to_the_parameter_plane() {
        // 左上 = (min x, max y)，右下 = (max x, min y)。
        let min = [0.0, 0.0];
        let max = [10.0, 20.0];
        assert_eq!(
            blend_pad_value_at(0.0, 0.0, 100.0, 100.0, min, max),
            Some([0.0, 20.0])
        );
        assert_eq!(
            blend_pad_value_at(100.0, 100.0, 100.0, 100.0, min, max),
            Some([10.0, 0.0])
        );
        assert_eq!(
            blend_pad_value_at(50.0, 50.0, 100.0, 100.0, min, max),
            Some([5.0, 10.0])
        );
        // 画布外夹到边界；坏尺寸 / 坏坐标返回 None。
        assert_eq!(
            blend_pad_value_at(-40.0, 400.0, 100.0, 100.0, min, max),
            Some([0.0, 0.0])
        );
        assert_eq!(blend_pad_value_at(0.0, 0.0, 0.0, 100.0, min, max), None);
        assert_eq!(
            blend_pad_value_at(f32::NAN, 0.0, 100.0, 100.0, min, max),
            None
        );
    }

    #[test]
    fn blend_axis_offsets_span_the_sample_range() {
        let offsets = blend_axis_offsets(&[0.0, 0.5, 1.0], 200.0);
        assert_eq!(offsets, vec![0.0, 100.0, 200.0]);
        // 乱序 / 负值同样按数据自身的范围归一化。
        let offsets = blend_axis_offsets(&[2.0, -2.0], 100.0);
        assert_eq!(offsets, vec![100.0, 0.0]);
    }

    #[test]
    fn blend_axis_offsets_handle_degenerate_and_broken_input() {
        // 所有样本一个值：全落在正中，而不是除零。
        assert_eq!(blend_axis_offsets(&[1.0, 1.0], 100.0), vec![50.0, 50.0]);
        // 非有限位置被丢掉，零宽度不画。
        assert_eq!(
            blend_axis_offsets(&[f32::NAN, 0.0, 1.0], 100.0),
            vec![0.0, 100.0]
        );
        assert!(blend_axis_offsets(&[0.0, 1.0], 0.0).is_empty());
        assert!(blend_axis_offsets(&[], 100.0).is_empty());
        assert!(blend_axis_range(&[f32::NAN]).is_none());
        assert_eq!(blend_axis_range(&[3.0, -1.0, 2.0]), Some((-1.0, 3.0)));
    }

    #[test]
    fn clicking_the_blend_axis_maps_back_into_the_parameter_range() {
        assert_eq!(blend_axis_value_at(0.0, 200.0, 0.0, 2.0), Some(0.0));
        assert_eq!(blend_axis_value_at(100.0, 200.0, 0.0, 2.0), Some(1.0));
        assert_eq!(blend_axis_value_at(200.0, 200.0, 0.0, 2.0), Some(2.0));
        // 点出边界外夹到两端。
        assert_eq!(blend_axis_value_at(-50.0, 200.0, 0.0, 2.0), Some(0.0));
        assert_eq!(blend_axis_value_at(999.0, 200.0, 0.0, 2.0), Some(2.0));
        // 坏输入不产生 NaN。
        assert!(blend_axis_value_at(f32::NAN, 200.0, 0.0, 2.0).is_none());
        assert!(blend_axis_value_at(10.0, 0.0, 0.0, 2.0).is_none());
        assert!(blend_axis_value_at(10.0, 200.0, f32::NAN, 2.0).is_none());
    }

    #[test]
    fn key_ticks_map_time_to_the_track_width() {
        let offsets = key_tick_offsets(&[0.0, 0.5, 2.0], 2.0, 200.0);
        assert_eq!(offsets, vec![0.0, 50.0, 200.0]);
    }

    #[test]
    fn clicking_a_tick_snaps_to_that_keyframes_time() {
        // 2 秒宽 200px：0s / 0.5s / 2s → 0 / 50 / 200。
        let times = [0.0, 0.5, 2.0];
        assert_eq!(key_tick_hit(&times, 2.0, 200.0, 50.0, 4.0), Some(0.5));
        assert_eq!(key_tick_hit(&times, 2.0, 200.0, 0.0, 4.0), Some(0.0));
        assert_eq!(key_tick_hit(&times, 2.0, 200.0, 199.0, 4.0), Some(2.0));
        // 空白处不跳：避免拖时间轴时被"吸"到某个关键帧。
        assert_eq!(key_tick_hit(&times, 2.0, 200.0, 120.0, 4.0), None);
    }

    #[test]
    fn the_nearest_tick_wins_when_two_are_close() {
        // 1.00s / 1.02s 在 200px 上是 100 / 102：点在 101.5 时后者更近。
        assert_eq!(
            key_tick_hit(&[1.0, 1.02], 2.0, 200.0, 101.5, 4.0),
            Some(1.02)
        );
        assert_eq!(
            key_tick_hit(&[1.0, 1.02], 2.0, 200.0, 100.4, 4.0),
            Some(1.0)
        );
    }

    #[test]
    fn the_hit_tolerance_is_inclusive_and_broken_inputs_are_ignored() {
        // 刚好等于半径算命中，超出 0.5px 就不算。
        assert_eq!(key_tick_hit(&[0.5], 2.0, 200.0, 54.0, 4.0), Some(0.5));
        assert_eq!(key_tick_hit(&[0.5], 2.0, 200.0, 54.5, 4.0), None);
        // 未知时长 / 零宽 / 非有限点击 / 负半径都不跳。
        assert_eq!(key_tick_hit(&[0.5], 0.0, 200.0, 50.0, 4.0), None);
        assert_eq!(key_tick_hit(&[0.5], 2.0, 0.0, 50.0, 4.0), None);
        assert_eq!(key_tick_hit(&[0.5], 2.0, 200.0, f32::NAN, 4.0), None);
        assert_eq!(key_tick_hit(&[0.5], 2.0, 200.0, 50.0, -1.0), None);
        // 越界 / NaN 关键帧本来就不画，也不会被点中。
        assert_eq!(key_tick_hit(&[3.0, f32::NAN], 2.0, 200.0, 200.0, 4.0), None);
    }

    #[test]
    fn key_ticks_drop_out_of_range_and_broken_values() {
        let offsets = key_tick_offsets(&[-0.5, 1.0, 3.0, f32::NAN], 2.0, 100.0);
        assert_eq!(offsets, vec![50.0], "只留 0..=duration 的有限值");
        assert!(
            key_tick_offsets(&[0.5], 0.0, 100.0).is_empty(),
            "未知时长不画刻度"
        );
        assert!(
            key_tick_offsets(&[0.5], 2.0, 0.0).is_empty(),
            "零宽度不画刻度"
        );
        assert!(key_tick_offsets(&[0.5], f32::NAN, 100.0).is_empty());
    }

    #[test]
    fn the_animator_editor_renders_without_writing_when_nothing_is_touched() {
        let ctx = egui::Context::default();
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        state
            .clip_durations
            .insert("char/walk.ranim".to_string(), 1.5);
        let value = json!({
            "clip_path": "char/walk.ranim",
            "time": 0.25,
            "speed": 1.0,
            "looped": true,
            "playing": true,
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                animator_editor(
                    ui,
                    &mut workspace,
                    &state,
                    1,
                    "prism_engine::scene::Animator",
                    &value,
                );
            });
        });
        // 没有鼠标输入 = 没有编辑：组件字段保持原样。
        let animator = workspace.document().doc.entities[1]
            .components
            .get("prism_engine::scene::Animator");
        assert!(animator.is_none(), "只是画控件不该凭空写出一个组件");
    }

    #[test]
    fn a_scaffolded_bone_tree_indents_children() {
        let joints = vec![
            SkeletonJointRow {
                name: "hip".into(),
                parent: None,
            },
            SkeletonJointRow {
                name: "knee".into(),
                parent: Some(0),
            },
            SkeletonJointRow {
                name: "foot".into(),
                parent: Some(1),
            },
        ];
        assert_eq!(joint_depth(&joints, 0), 0);
        assert_eq!(joint_depth(&joints, 2), 2);
    }

    #[test]
    fn a_cyclic_bone_chain_does_not_hang_the_depth_calculation() {
        // 坏数据（父级成环）不能把检查器卡死。
        let joints = vec![
            SkeletonJointRow {
                name: "a".into(),
                parent: Some(1),
            },
            SkeletonJointRow {
                name: "b".into(),
                parent: Some(0),
            },
        ];
        let depth = joint_depth(&joints, 0);
        assert!(depth <= joints.len(), "depth {depth} must stay bounded");
    }

    #[test]
    fn clip_labels_show_the_file_name_only() {
        assert_eq!(clip_label(""), "(none)");
        assert_eq!(clip_label("assets/char/walk.ranim"), "walk.ranim");
        assert_eq!(clip_label("walk.ranim"), "walk.ranim");
    }

    #[test]
    fn clip_runtime_paths_strip_only_the_assets_prefix() {
        assert_eq!(
            clip_runtime_path("assets/char/walk.ranim"),
            "char/walk.ranim"
        );
        assert_eq!(clip_runtime_path("char/walk.ranim"), "char/walk.ranim");
        assert_eq!(clip_runtime_path("walk.ranim"), "walk.ranim");
    }

    #[test]
    fn appending_a_key_lands_one_second_after_the_last_one() {
        let mut keys = vec![json!({"time": 0.0, "value": [0.0, 0.0, 0.0]})];
        append_key(&mut keys, [1.0, 1.0, 1.0]);
        assert_eq!(keys[1], json!({"time": 1.0, "value": [0.0, 0.0, 0.0]}));
    }

    #[test]
    fn the_first_key_of_an_empty_track_uses_the_identity_value() {
        let mut keys: Vec<Value> = Vec::new();
        append_key(&mut keys, [1.0, 1.0, 1.0]);
        assert_eq!(keys[0], json!({"time": 0.0, "value": [1.0, 1.0, 1.0]}));
    }

    #[test]
    fn the_track_rows_render_without_writing_when_nothing_is_touched() {
        let ctx = egui::Context::default();
        let keys = vec![
            json!({"time": 0.0, "value": [0.0, 0.0, 0.0]}),
            json!({"time": 1.0, "value": [0.0, 2.0, 0.0]}),
        ];
        let mut written = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                written = keyframe_track_editor(ui, "Position", "position", &keys, [0.0; 3]);
            });
        });
        assert!(
            written.is_none(),
            "only user edits may write the track back"
        );
    }

    #[test]
    fn sorting_keyframes_keeps_equal_times_in_insertion_order() {
        let mut keys = vec![
            json!({"time": 2.0, "value": [2.0, 0.0, 0.0]}),
            json!({"time": 0.0, "value": [0.0, 0.0, 0.0]}),
            json!({"time": 2.0, "value": [7.0, 0.0, 0.0]}),
        ];
        sort_key_values(&mut keys);
        let order: Vec<f64> = keys
            .iter()
            .map(|key| key["value"][0].as_f64().unwrap())
            .collect();
        assert_eq!(order, vec![0.0, 2.0, 7.0]);
    }
}

#[cfg(test)]
mod line_renderer_tests {
    use super::*;

    #[test]
    fn subdivide_inserts_midpoint_between_adjacent_positions() {
        let mut points = vec![json!([0.0, 2.0, 4.0]), json!([2.0, 4.0, 8.0])];
        apply_line_point_action(&mut points, LinePointAction::Subdivide(0));
        assert_eq!(
            Value::Array(points),
            json!([[0.0, 2.0, 4.0], [1.0, 3.0, 6.0], [2.0, 4.0, 8.0]])
        );
    }

    #[test]
    fn remove_and_add_positions_preserve_existing_points() {
        let mut points = vec![json!([1.0, 2.0, 3.0]), json!([4.0, 5.0, 6.0])];
        apply_line_point_action(&mut points, LinePointAction::Remove(0));
        apply_line_point_action(&mut points, LinePointAction::Add);
        assert_eq!(
            Value::Array(points),
            json!([[4.0, 5.0, 6.0], [4.0, 5.0, 6.0]])
        );
    }
}

/// 多选时，每个分量是否「选中实体之间取值不同」。
///
/// `[position, rotation, scale][x, y, z, w]`（`rotation` 是四元数，占 4 个分量）。
fn mixed_transform_axes(workspace: &Workspace, indices: &[usize]) -> [[bool; 4]; 3] {
    let mut mixed = [[false; 4]; 3];
    let Some(first) = indices.first().copied() else {
        return mixed;
    };
    let base = workspace.document().doc.entities[first].transform;
    let base_basis = prism_editor_core::viewport_math::basis_from_quat(base.rotation);
    for index in indices.iter().skip(1) {
        let transform = workspace.document().doc.entities[*index].transform;
        for axis in 0..3 {
            if (base.position[axis] - transform.position[axis]).abs() > 1e-5 {
                mixed[0][axis] = true;
            }
            if (base.scale[axis] - transform.scale[axis]).abs() > 1e-5 {
                mixed[2][axis] = true;
            }
        }
        // 四元数 q 与 -q 是同一个旋转，所以比较基向量而不是分量。
        let basis = prism_editor_core::viewport_math::basis_from_quat(transform.rotation);
        let rotated = (0..3).any(|column| {
            (0..3).any(|row| (base_basis[column][row] - basis[column][row]).abs() > 1e-5)
        });
        if rotated {
            for axis in 0..3 {
                mixed[1][axis] = true;
            }
        }
    }
    mixed
}

fn inspector_multi(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState, indices: &[usize]) {
    let ids: Vec<u32> = indices
        .iter()
        .filter_map(|index| workspace.document().doc.id_of_index(*index))
        .collect();
    // #507：活动对象是最后点击的那个，不是文档顺序里的第一个。
    let active = workspace
        .primary_selection()
        .filter(|index| indices.contains(index))
        .unwrap_or(indices[0]);
    let active_name = workspace
        .document()
        .doc
        .entities
        .get(active)
        .and_then(|entity| entity.name.clone())
        .unwrap_or_else(|| format!("entity {active}"));
    ui.label(
        RichText::new(format!(
            "{} GameObjects selected · Active: {}",
            indices.len(),
            active_name
        ))
        .strong()
        .size(12.0),
    );
    ui.label(
        RichText::new("Fields apply to every selected entity in one undo step.")
            .weak()
            .size(10.0),
    );
    ui.separator();

    // 批量 Transform（显示活动对象的值，其余不一致的分量标 "—"）。
    let first = active;
    let transform = workspace.document().doc.entities[first].transform.clone();
    let mixed = mixed_transform_axes(workspace, indices);
    ui.label(RichText::new("Transform").strong().size(12.0));
    transform_rows(ui, state, &transform, Some(&mixed), |edit| match edit {
        TransformEdit::Axis { field, axis, value } => {
            workspace.set_transform_axis_many(&ids, field, axis, value);
        }
        TransformEdit::Vector { field, values } => {
            workspace.set_transform_vector_many(&ids, field, values);
        }
        TransformEdit::Rotation(rotation) => {
            workspace.set_transform_rotation_many(&ids, rotation);
        }
    });
    ui.separator();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Common Components").strong().size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add_component_menu_for(ui, workspace, state, indices);
        });
    });

    let common = workspace.common_components(indices);
    let values: Vec<(String, Value)> = common
        .iter()
        .filter(|component| component.name != "prism_engine::scene::Active")
        .filter_map(|component| {
            let value = workspace
                .document()
                .doc
                .entities
                .get(first)?
                .component(&component.name)?
                .clone();
            Some((component.name.clone(), value))
        })
        .collect();
    let asset_paths = workspace.asset_paths();
    let doc_dir = workspace.document_dir();
    let registry = workspace.schema.clone();
    let mut actions: Vec<widgets::CardAction> = Vec::new();
    for (name, value) in values {
        let Some(schema) = workspace.schema.component(&name).cloned() else {
            continue;
        };
        let presets = workspace.component_preset_names(&schema.name);
        let mut set = |field: &str, value: Value| {
            workspace.set_field_multi(&ids, &schema.name, field, value);
        };
        widgets::component_card_with_assets(
            ui,
            &registry,
            &schema,
            &value,
            &mut actions,
            &mut set,
            &asset_paths,
            &doc_dir,
            &presets,
        );
    }
    for action in actions {
        match action {
            widgets::CardAction::Copy(name) => {
                if let Some(index) = workspace.primary_selection() {
                    workspace.copy_component_at(index, &name);
                }
            }
            widgets::CardAction::Paste(name) => {
                let applied = workspace.paste_component_values(indices, &name);
                state.status = if applied == 0 {
                    format!("nothing to paste into {name}")
                } else {
                    format!("pasted values into {name} ({applied} objects)")
                };
            }
            widgets::CardAction::Remove(name) => {
                workspace.remove_component_multi(indices, &name);
            }
            // 多选时没有唯一的「上一格」：顺序按每个实体自己的组件表解释，
            // 所以各自移动一格（选中集合的顺序另说，见 #507）。
            widgets::CardAction::MoveUp(name) => {
                for index in indices {
                    workspace.move_component_at(*index, &name, -1);
                }
            }
            widgets::CardAction::MoveDown(name) => {
                for index in indices {
                    workspace.move_component_at(*index, &name, 1);
                }
            }
            widgets::CardAction::SavePreset {
                component,
                name,
                value,
            } => {
                workspace.save_component_preset(&component, &name, value);
                state.status = format!("saved preset {name} for {component}");
            }
            widgets::CardAction::ApplyPreset { component, name } => {
                match workspace.component_preset_value(&component, &name) {
                    Some(value) => {
                        let applied = workspace.set_field_multi(&ids, &component, "", value);
                        state.status = format!("applied preset {name} ({applied} objects)");
                    }
                    None => state.status = format!("preset {name} not found"),
                }
            }
        }
    }
}

/// 没有 schema 的组件：可编辑的原始 JSON（#418 的 Debug 兜底视图）。
///
/// 缓冲放在 `egui` 的临时数据里，键是 `(实体, 组件)`；Apply 成功后清掉缓冲，
/// 下一帧重新从文档读，避免显示过期内容。解析失败时按钮禁用并直接显示错误。
fn component_debug_editor(
    ui: &mut Ui,
    workspace: &mut Workspace,
    entity_id: u32,
    name: &str,
    value: &Value,
) {
    let id = egui::Id::new(("component-debug-json", entity_id, name));
    let mut buffer = ui
        .data(|data| data.get_temp::<String>(id))
        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default());

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{name}  (no schema — raw JSON)"))
                .weak()
                .size(11.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("Reload")
                .on_hover_text("Discard the buffer and read the document again")
                .clicked()
            {
                buffer = serde_json::to_string_pretty(value).unwrap_or_default();
            }
        });
    });
    ui.add(
        egui::TextEdit::multiline(&mut buffer)
            .code_editor()
            .desired_rows(5)
            .desired_width(f32::INFINITY),
    );

    let parsed = serde_json::from_str::<Value>(&buffer);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(parsed.is_ok(), egui::Button::new("Apply JSON"))
            .clicked()
        {
            if let Ok(parsed) = &parsed {
                workspace.set_field(entity_id, name, "", parsed.clone());
                ui.data_mut(|data| data.remove::<String>(id));
            }
        }
        match &parsed {
            Ok(_) => {
                ui.label(RichText::new("valid JSON").size(10.0).weak());
            }
            Err(error) => {
                ui.label(
                    RichText::new(error.to_string())
                        .size(10.0)
                        .color(Color32::from_rgb(232, 96, 96)),
                );
            }
        }
    });

    if parsed.is_ok() {
        ui.data_mut(|data| data.insert_temp(id, buffer));
    }
}

/// 实例 → 预制体：用当前子树覆盖预制体文件，再同步同源实例。
fn apply_to_prefab(
    workspace: &mut Workspace,
    index: usize,
    prefab: &str,
) -> std::io::Result<usize> {
    let root = workspace
        .project_root
        .clone()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "没有绑定项目根"))?;
    let doc = workspace.document().doc.extract_subtree(&[index]);
    let count = doc.entities.len();
    prism_editor_core::project::write_document(&root, prefab, &doc)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// 资产
// ---------------------------------------------------------------------------

/// 两列模式下的左侧目录树（#419）。
///
/// 复用 `Workspace::tree`（数据层已经建好的目录树），只显示目录；点击跳转到该目录，
/// 当前目录高亮，父链自动展开（egui 的 `selectable_label` 不需要折叠状态）。
fn asset_folder_column(ui: &mut Ui, workspace: &Workspace, state: &mut UiState) {
    ui.label(RichText::new("Folders").strong().size(11.0));
    egui::ScrollArea::vertical()
        .id_salt("asset-folder-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let selected = state.asset_folder == "assets";
            if ui
                .selectable_label(selected, format!("{} assets", icons::FOLDER))
                .clicked()
            {
                navigate_assets(state, "assets");
            }
            let roots = workspace.tree.roots.clone();
            for node in &roots {
                folder_tree_row(ui, node, state, 1);
            }
        });
}

/// 目录树的一行（递归；只画目录，文件由右侧内容区负责）。
fn folder_tree_row(
    ui: &mut Ui,
    node: &prism_editor_core::assets::AssetNode,
    state: &mut UiState,
    depth: usize,
) {
    if !node.is_dir {
        return;
    }
    let selected = state.asset_folder == node.path;
    let label = format!(
        "{}{} {}",
        "  ".repeat(depth.saturating_sub(1)),
        icons::FOLDER,
        node.name
    );
    if ui.selectable_label(selected, label).clicked() {
        navigate_assets(state, &node.path);
    }
    for child in &node.children {
        folder_tree_row(ui, child, state, depth + 1);
    }
}

/// 网格视图：文件夹在前、文件在后（#419）。
fn asset_grid(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
    folders: &[String],
    files: &[(String, AssetKind, u64)],
    visible: &[String],
    actions: &mut AssetActions,
) {
    let per_row = grid_columns(ui.available_width(), 92.0);
    egui::ScrollArea::vertical()
        .id_salt("asset-grid-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for chunk in folders.chunks(per_row) {
                ui.horizontal(|ui| {
                    for path in chunk {
                        let name = path.rsplit('/').next().unwrap_or(path);
                        let response =
                            asset_grid_cell(ui, icons::FOLDER, name, state.asset_selected(path));
                        if response.double_clicked() {
                            navigate_assets(state, path);
                        } else if response.clicked() {
                            state.select_asset_only(path.clone());
                        }
                        response.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                navigate_assets(state, path);
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                actions.rename = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Copy Path").clicked() {
                                ui.ctx().copy_text(path.clone());
                                state.status = format!("copied {path}");
                                ui.close();
                            }
                            if ui.button("Show in Explorer").clicked() {
                                state.status = match reveal_asset(workspace, path) {
                                    Ok(()) => format!("revealed {path}"),
                                    Err(error) => error,
                                };
                                ui.close();
                            }
                        });
                    }
                });
            }
            for chunk in files.chunks(per_row) {
                ui.horizontal(|ui| {
                    for (path, kind, _size) in chunk {
                        let name = path.rsplit('/').next().unwrap_or(path);
                        let selected = state.asset_selected(path);
                        let drag_id = egui::Id::new(("asset-grid-drag", path.as_str()));
                        let payload = widgets::AssetDrag { path: path.clone() };
                        let response = ui
                            .dnd_drag_source(drag_id, payload, |ui| {
                                asset_grid_cell(ui, asset_kind_icon(*kind), name, selected)
                            })
                            .inner;
                        if response.clicked() {
                            let modifiers = ui.input(|input| input.modifiers);
                            if modifiers.shift {
                                state.select_asset_range(visible, path);
                            } else if modifiers.ctrl || modifiers.command {
                                state.toggle_asset(path);
                            } else {
                                state.select_asset_only(path.clone());
                            }
                            workspace.selection.clear();
                        }
                        if response.double_clicked() {
                            actions.open = Some(path.clone());
                        }
                        let targets: Vec<String> = if selected && state.asset_selection.len() > 1 {
                            state.asset_selection.clone()
                        } else {
                            vec![path.clone()]
                        };
                        response.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                actions.open = Some(path.clone());
                                ui.close();
                            }
                            if path.ends_with(".prefab.json")
                                && ui.button("Instantiate In Scene").clicked()
                            {
                                actions.instantiate = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                actions.rename = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Ping In Scene").clicked() {
                                actions.ping = Some(path.clone());
                                ui.close();
                            }
                            if ui
                                .button(if targets.len() > 1 {
                                    format!("Duplicate {} items", targets.len())
                                } else {
                                    "Duplicate".to_string()
                                })
                                .clicked()
                            {
                                actions.duplicate = Some(targets.clone());
                                ui.close();
                            }
                            if ui.button("Copy Path").clicked() {
                                ui.ctx().copy_text(targets.join("\n"));
                                state.status = format!("copied {path}");
                                ui.close();
                            }
                            if ui.button("Show in Explorer").clicked() {
                                state.status = match reveal_asset(workspace, path) {
                                    Ok(()) => format!("revealed {path}"),
                                    Err(error) => error,
                                };
                                ui.close();
                            }
                            if ui
                                .button(if targets.len() > 1 {
                                    format!("Delete {} items", targets.len())
                                } else {
                                    "Delete".to_string()
                                })
                                .clicked()
                            {
                                actions.delete = Some(targets.clone());
                                ui.close();
                            }
                        });
                    }
                });
            }
            if files.is_empty() && folders.is_empty() {
                ui.label(RichText::new("empty folder").weak().size(11.0));
            }
        });
}

/// Project 面板的内容布局（#419）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AssetView {
    /// 一行一个资产的列表（历史行为）。
    #[default]
    List,
    /// 图标网格（文件夹在前、文件在后）。
    Grid,
}

/// Project 面板的列数（#419）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AssetColumns {
    /// 单列：面包屑 + 列表。
    #[default]
    One,
    /// 两列：左侧目录树、右侧当前目录内容（Unity 的 `ViewMode.TwoColumns`）。
    Two,
}

/// 解析后的资产搜索词（#419）。
///
/// 语法：`t:<类型>` 只留某一类（英文 slug `t:texture` 与中文标签 `t:纹理` 都认），
/// `dir:<子串>` 与普通词一样做路径匹配；多个词之间是 **与**，多个 `t:` 之间是
/// **或**（与 Unity 的 `SearchFilter` 一致）。未知前缀按普通文本处理。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetSearch {
    /// `t:` 指定的类型；空 = 不限。
    pub kinds: Vec<AssetKind>,
    /// 其余关键词（小写）。
    pub needles: Vec<String>,
}

impl AssetSearch {
    pub fn parse(text: &str) -> Self {
        let mut search = AssetSearch::default();
        for term in text.split_whitespace() {
            let lower = term.to_lowercase();
            if let Some(kind_text) = lower.strip_prefix("t:") {
                if let Some(kind) = asset_kind_by_token(kind_text) {
                    if !search.kinds.contains(&kind) {
                        search.kinds.push(kind);
                    }
                }
                continue;
            }
            let needle = lower.strip_prefix("dir:").unwrap_or(&lower);
            if !needle.is_empty() && !search.needles.iter().any(|known| known == needle) {
                search.needles.push(needle.to_string());
            }
        }
        search
    }

    /// 是否有任何条件（没有时就是「列当前目录」）。
    pub fn is_active(&self) -> bool {
        !self.kinds.is_empty() || !self.needles.is_empty()
    }

    pub fn matches(&self, path: &str, kind: AssetKind) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&kind) {
            return false;
        }
        let lower = path.to_lowercase();
        self.needles.iter().all(|needle| lower.contains(needle))
    }
}

/// `t:` 的类型词：英文 slug（`texture`）、`Debug` 名（`Texture`）与中文标签（`纹理`）都认。
fn asset_kind_by_token(token: &str) -> Option<AssetKind> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    AssetKind::all().into_iter().find(|kind| {
        kind.label().eq_ignore_ascii_case(token) || format!("{kind:?}").eq_ignore_ascii_case(token)
    })
}

/// 网格视图每行放几个格子（面板宽度 / 格子宽度，至少 1）。
fn grid_columns(available_width: f32, cell_width: f32) -> usize {
    if !available_width.is_finite() || !cell_width.is_finite() || cell_width <= 0.0 {
        return 1;
    }
    ((available_width / cell_width).floor() as usize).max(1)
}

/// 面板里待处理的资产动作（`assets()` 主体与内容区之间传递）。
#[derive(Default)]
struct AssetActions {
    open: Option<String>,
    delete: Option<Vec<String>>,
    duplicate: Option<Vec<String>>,
    rename: Option<String>,
    instantiate: Option<String>,
    commit_rename: Option<(String, String)>,
    move_to_folder: Option<(String, String)>,
    create_subfolder: Option<String>,
    /// 从资产跳到场景里引用它的实体（#419 的 Ping）。
    ping: Option<String>,
}

/// 在资产与场景之间定位（#419 的 Ping）：
///
/// 找到**第一个**引用该路径的实体（优先当前文档，其次其它打开/磁盘文档），
/// 选中它并在 Scene / Hierarchy 里一起高亮。找不到时只写状态栏，不改选择。
fn ping_asset(workspace: &mut Workspace, state: &mut UiState, path: &str) {
    let active = workspace.active_index();
    let target_guids = workspace.asset_guids(path);
    let mut found: Option<(usize, usize)> = None;
    for offset in 0..workspace.docs.len() {
        let index = (active + offset) % workspace.docs.len().max(1);
        let Some(doc) = workspace.docs.get(index) else {
            continue;
        };
        if let Some((entity, _, _)) = doc
            .doc
            .field_references_with_guids(path, &target_guids)
            .into_iter()
            .next()
        {
            found = Some((index, entity));
            break;
        }
    }
    let Some((doc, entity)) = found else {
        state.status = format!("no object references {path}");
        return;
    };
    if doc != active {
        workspace.set_active(doc);
    }
    // 选中实体 = 退出资产检查器（两者不并存）。
    state.clear_asset_selection();
    workspace.select(entity, false);
    workspace.frame_selection();
    // 资产 Ping 到场景：层级同时展开祖先并滚到这一行（#417）。
    reveal_in_hierarchy(workspace, state, entity);
    let name = workspace
        .document()
        .doc
        .entities
        .get(entity)
        .map(|entity| entity.display_name())
        .unwrap_or_else(|| format!("#{entity}"));
    state.status = format!("pinged {path} → {name}");
}

/// 网格里一个格子（图标 + 文件名），返回点击响应。
fn asset_grid_cell(ui: &mut Ui, icon: &str, name: &str, selected: bool) -> egui::Response {
    let visuals = ui.visuals().clone();
    let cell = egui::vec2(84.0, 84.0);
    let (rect, response) = ui.allocate_exact_size(cell, egui::Sense::click());
    if selected {
        ui.painter().rect_filled(
            rect.shrink(2.0),
            egui::CornerRadius::same(4),
            visuals.selection.bg_fill,
        );
    } else if response.hovered() {
        ui.painter().rect_filled(
            rect.shrink(2.0),
            egui::CornerRadius::same(4),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
        );
    }
    ui.painter().text(
        rect.center_top() + egui::vec2(0.0, 12.0),
        egui::Align2::CENTER_TOP,
        icon,
        egui::FontId::proportional(26.0),
        visuals.text_color(),
    );
    let mut job = egui::text::LayoutJob::simple_singleline(
        truncate_label(name, 14),
        egui::FontId::proportional(10.5),
        visuals.text_color(),
    );
    job.wrap.max_width = rect.width() - 6.0;
    let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(job));
    let size = galley.size();
    ui.painter().galley(
        rect.center_bottom() - egui::vec2(size.x * 0.5, size.y + 8.0),
        galley,
        visuals.text_color(),
    );
    response
}

/// 文件名截断（网格格子很窄，长名字只留前 `max` 个字符）。
fn truncate_label(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max {
        return name.to_string();
    }
    let head: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

pub fn assets(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    // ---- 导航：后退 / 前进 / 面包屑（Project 窗口的路径栏） ----
    ui.horizontal(|ui| {
        let can_back = state.asset_history_pos > 0;
        let can_forward = state.asset_history_pos + 1 < state.asset_history.len();
        if ui
            .add_enabled(can_back, egui::Button::new(icons::CARET_LEFT))
            .clicked()
        {
            state.asset_history_pos -= 1;
            state.asset_folder = state.asset_history[state.asset_history_pos].clone();
        }
        if ui
            .add_enabled(can_forward, egui::Button::new(icons::CARET_RIGHT))
            .clicked()
        {
            state.asset_history_pos += 1;
            state.asset_folder = state.asset_history[state.asset_history_pos].clone();
        }
        let segments: Vec<String> = state
            .asset_folder
            .split('/')
            .map(|s| s.to_string())
            .collect();
        let mut target: Option<String> = None;
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                ui.label("/");
            }
            if ui.small_button(segment).clicked() {
                target = Some(segments[..=index].join("/"));
            }
        }
        if let Some(target) = target {
            navigate_assets(state, &target);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(icons::ARROWS_CLOCKWISE)
                .on_hover_text("Refresh")
                .clicked()
            {
                workspace.refresh_project_files();
            }
        });
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.asset_search)
                .hint_text("Search (t:texture, dir:sprites)")
                .desired_width(170.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let two_columns = state.asset_columns == AssetColumns::Two;
            if ui
                .selectable_label(two_columns, icons::COLUMNS)
                .on_hover_text("Two columns (folder tree + contents)")
                .clicked()
            {
                state.asset_columns = if two_columns {
                    AssetColumns::One
                } else {
                    AssetColumns::Two
                };
            }
            let grid = state.asset_view == AssetView::Grid;
            if ui
                .selectable_label(grid, icons::SQUARES_FOUR)
                .on_hover_text("Grid view")
                .clicked()
            {
                state.asset_view = if grid {
                    AssetView::List
                } else {
                    AssetView::Grid
                };
            }
            if ui
                .selectable_label(!grid, icons::LIST_BULLETS)
                .on_hover_text("List view")
                .clicked()
            {
                state.asset_view = AssetView::List;
            }
        });
    });
    // 类型开关由 `AssetKind::all()` 生成：枚举里加一种，这里自动多一个开关，
    // 不用再记得改这里的清单。
    ui.horizontal_wrapped(|ui| {
        for kind in AssetKind::all() {
            let enabled = workspace.asset_query.kinds.contains(&kind);
            if ui
                .selectable_label(enabled, asset_kind_icon(kind))
                .on_hover_text(kind.label())
                .clicked()
            {
                workspace.toggle_asset_kind(kind);
            }
        }
    });
    // 列头：点一次排序，再点同一列反向（目录永远排在文件前面）。
    ui.horizontal(|ui| {
        for (sort, label) in [
            (AssetSort::Name, "Name"),
            (AssetSort::Kind, "Kind"),
            (AssetSort::Size, "Size"),
        ] {
            let active = state.asset_sort == sort;
            let arrow = if !active {
                ""
            } else if state.asset_sort_desc {
                " ▼"
            } else {
                " ▲"
            };
            if ui
                .selectable_label(active, format!("{label}{arrow}"))
                .clicked()
            {
                if active {
                    state.asset_sort_desc = !state.asset_sort_desc;
                } else {
                    state.asset_sort = sort;
                    state.asset_sort_desc = false;
                }
            }
        }
    });
    ui.separator();

    let query = AssetSearch::parse(&state.asset_search);
    let folder = state.asset_folder.clone();
    let mut actions = AssetActions::default();
    let content = |ui: &mut Ui,
                   workspace: &mut Workspace,
                   state: &mut UiState,
                   actions: &mut AssetActions| {
        let all: Vec<(String, AssetKind, u64)> = workspace
            .filtered_assets()
            .into_iter()
            .map(|asset| (asset.path.clone(), asset.kind, asset.size_bytes))
            .collect();

        // 直接子目录 + 直接子资产（有搜索词时退化成全局扁平搜索）。
        let mut folders: Vec<String> = Vec::new();
        let mut files: Vec<(String, AssetKind, u64)> = Vec::new();
        for (path, kind, size) in all {
            if query.is_active() {
                if query.matches(&path, kind) {
                    files.push((path, kind, size));
                }
                continue;
            }
            let Some(rest) = path.strip_prefix(&format!("{folder}/")) else {
                continue;
            };
            match rest.split_once('/') {
                Some((child, _)) => {
                    let child_path = format!("{folder}/{child}");
                    if !folders.contains(&child_path) {
                        folders.push(child_path);
                    }
                }
                None => files.push((path, kind, size)),
            }
        }
        folders.sort();
        sort_assets(&mut files, state.asset_sort, state.asset_sort_desc);

        // 多选（#486）：区间选择按「面板里看到的顺序」算。
        let visible_paths: Vec<String> = files.iter().map(|(path, _, _)| path.clone()).collect();

        // Ctrl/Cmd+A 选中当前目录里的所有资产；Esc 清空（#486）。
        //
        // `handle_shortcuts` 里 `SelectAll` 会按焦点面板分派，这里再做一次是为了
        // 「面板刚打开、焦点还没落在它上面」时也能用。
        // 有文本框在编辑时（重命名 / 搜索框）不能抢这两个键。
        let typing = ui.ctx().memory(|memory| memory.focused()).is_some();
        let (select_all, escape) = ui.input(|input| {
            (
                !typing
                    && (input.modifiers.ctrl || input.modifiers.command)
                    && input.key_pressed(egui::Key::A),
                !typing && input.key_pressed(egui::Key::Escape),
            )
        });
        if select_all {
            state.asset_selection = visible_paths.clone();
            state.asset_anchor = visible_paths.first().cloned();
        }
        if escape && !state.asset_selection.is_empty() {
            state.clear_asset_selection();
        }

        if state.asset_view == AssetView::Grid {
            asset_grid(
                ui,
                workspace,
                state,
                &folders,
                &files,
                &visible_paths,
                actions,
            );
        } else {
            egui::ScrollArea::vertical()
                .id_salt("asset-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if folder != "assets"
                        && ui
                            .selectable_label(false, format!("{} ..", icons::FOLDER))
                            .clicked()
                    {
                        {
                            let parent = folder.rsplit_once('/').map(|(p, _)| p.to_string());
                            if let Some(parent) = parent {
                                navigate_assets(state, &parent);
                            }
                        }
                    }
                    let colors = workspace.colors();
                    for path in &folders {
                        let selected = state.asset_selected(path);
                        let renaming =
                            matches!(&state.asset_rename, Some((target, _)) if target == path);
                        if renaming {
                            // 目录也走同一个就地重命名输入框（#478）。
                            ui.horizontal(|ui| {
                                if let Some((_, buffer)) = state.asset_rename.as_mut() {
                                    let response = ui.add(
                                        egui::TextEdit::singleline(buffer)
                                            .desired_width(ui.available_width().max(80.0)),
                                    );
                                    if state.asset_rename_focus {
                                        response.request_focus();
                                        state.asset_rename_focus = false;
                                    }
                                    let (enter, escape) = ui.input(|input| {
                                        (
                                            input.key_pressed(egui::Key::Enter),
                                            input.key_pressed(egui::Key::Escape),
                                        )
                                    });
                                    let clicked_outside = response.lost_focus()
                                        && ui.input(|input| input.pointer.any_click());
                                    if escape {
                                        state.asset_rename = None;
                                        state.asset_rename_focus = false;
                                    } else if enter || clicked_outside {
                                        actions.commit_rename = state.asset_rename.take();
                                        state.asset_rename_focus = false;
                                    }
                                }
                            });
                            continue;
                        }
                        let label = format!(
                            "{} {}",
                            icons::FOLDER,
                            path.rsplit('/').next().unwrap_or(path)
                        );
                        let row = list_row(ui, &colors, selected, state.compact, &label);
                        // 拖资产经过目录行：高亮；松手 = 移入该目录（#471）。
                        if row.dnd_hover_payload::<widgets::AssetDrag>().is_some() {
                            ui.painter().rect_filled(
                                row.rect,
                                egui::CornerRadius::ZERO,
                                egui::Color32::from_rgba_unmultiplied(90, 170, 120, 40),
                            );
                        }
                        if let Some(payload) = row.dnd_release_payload::<widgets::AssetDrag>() {
                            actions.move_to_folder = Some((payload.path.clone(), path.clone()));
                        } else if row.double_clicked() {
                            // 单击 = 选中，双击 = 进入（#478）。
                            navigate_assets(state, path);
                        } else if row.clicked() {
                            // 目录不参与区间选择（它们不在文件列表里），修饰键同语义。
                            let modifiers = ui.input(|input| input.modifiers);
                            if modifiers.ctrl || modifiers.command {
                                state.toggle_asset(path);
                            } else {
                                state.select_asset_only(path.clone());
                            }
                        }
                        row.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                navigate_assets(state, path);
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                actions.rename = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("New Subfolder…").clicked() {
                                actions.create_subfolder = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Copy Path").clicked() {
                                ui.ctx().copy_text(path.clone());
                                state.status = format!("copied {path}");
                                ui.close();
                            }
                            if ui.button("Show in Explorer").clicked() {
                                state.status = match reveal_asset(workspace, path) {
                                    Ok(()) => format!("revealed {path}"),
                                    Err(error) => error,
                                };
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Delete").clicked() {
                                actions.delete = Some(vec![path.clone()]);
                                ui.close();
                            }
                        });
                    }
                    for (path, kind, _size) in &files {
                        let selected = state.asset_selected(path);
                        let renaming =
                            matches!(&state.asset_rename, Some((target, _)) if target == path);
                        if renaming {
                            ui.horizontal(|ui| {
                                if let Some((_, buffer)) = state.asset_rename.as_mut() {
                                    let response = ui.add(
                                        egui::TextEdit::singleline(buffer)
                                            .desired_width(ui.available_width().max(80.0)),
                                    );
                                    if state.asset_rename_focus {
                                        response.request_focus();
                                        state.asset_rename_focus = false;
                                    }
                                    let (enter, escape) = ui.input(|input| {
                                        (
                                            input.key_pressed(egui::Key::Enter),
                                            input.key_pressed(egui::Key::Escape),
                                        )
                                    });
                                    // 点别处 = 提交（与层级树 / 引擎一致），Esc = 放弃。
                                    let clicked_outside = response.lost_focus()
                                        && ui.input(|input| input.pointer.any_click());
                                    if escape {
                                        state.asset_rename = None;
                                        state.asset_rename_focus = false;
                                    } else if enter || clicked_outside {
                                        actions.commit_rename = state.asset_rename.take();
                                        state.asset_rename_focus = false;
                                    }
                                }
                            });
                            continue;
                        }
                        let label = format!(
                            "{} {}",
                            asset_kind_icon(*kind),
                            path.rsplit('/').next().unwrap_or(path)
                        );
                        // 右键菜单作用于整个选中集合（点在已选中项上时）。
                        let targets: Vec<String> =
                            if state.asset_selected(path) && state.asset_selection.len() > 1 {
                                state.asset_selection.clone()
                            } else {
                                vec![path.clone()]
                            };
                        let drag_id = egui::Id::new(("asset-drag", path.as_str()));
                        let payload = widgets::AssetDrag { path: path.clone() };
                        let response = ui
                            .dnd_drag_source(drag_id, payload, |ui| {
                                list_row(ui, &workspace.colors(), selected, state.compact, &label)
                            })
                            .inner;
                        if response.clicked() {
                            // Ctrl/Cmd = 切换，Shift = 从锚点区间选择，普通单击 = 替换（#486）。
                            let modifiers = ui.input(|input| input.modifiers);
                            if modifiers.shift {
                                state.select_asset_range(&visible_paths, path);
                            } else if modifiers.ctrl || modifiers.command {
                                state.toggle_asset(path);
                            } else {
                                state.select_asset_only(path.clone());
                            }
                            // 选中资产就切到资产检查器（#514）：清掉实体选择，两者不并存。
                            workspace.selection.clear();
                        }
                        if response.double_clicked() {
                            actions.open = Some(path.clone());
                        }
                        response.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                actions.open = Some(path.clone());
                                ui.close();
                            }
                            if path.ends_with(".prefab.json")
                                && ui.button("Instantiate In Scene").clicked()
                            {
                                actions.instantiate = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                actions.rename = Some(path.clone());
                                ui.close();
                            }
                            if ui
                                .button(if targets.len() > 1 {
                                    format!("Duplicate {} items", targets.len())
                                } else {
                                    "Duplicate".to_string()
                                })
                                .clicked()
                            {
                                actions.duplicate = Some(targets.clone());
                                ui.close();
                            }
                            if ui.button("Copy Path").clicked() {
                                let text = targets.join("\n");
                                ui.ctx().copy_text(text);
                                state.status = if targets.len() > 1 {
                                    format!("copied {} path(s)", targets.len())
                                } else {
                                    format!("copied {path}")
                                };
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Ping In Scene").clicked() {
                                actions.ping = Some(path.clone());
                                ui.close();
                            }
                            if ui.button("Find References In Scene").clicked() {
                                state.pending_find_references = Some((path.clone(), false));
                                ui.close();
                            }
                            if ui.button("Find References In Project").clicked() {
                                state.pending_find_references = Some((path.clone(), true));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Show in Explorer").clicked() {
                                state.status = match reveal_asset(workspace, path) {
                                    Ok(()) => format!("revealed {path}"),
                                    Err(error) => error,
                                };
                                ui.close();
                            }
                            if ui
                                .button(if targets.len() > 1 {
                                    format!("Delete {} items", targets.len())
                                } else {
                                    "Delete".to_string()
                                })
                                .clicked()
                            {
                                actions.delete = Some(targets.clone());
                                ui.close();
                            }
                        });
                        // 右键点到未选中的行时，先把它变成唯一选中项（与 Unity 一致）。
                        if response.secondary_clicked() && !state.asset_selected(path) {
                            state.select_asset_only(path.clone());
                        }
                    }
                    if files.is_empty() && folders.is_empty() {
                        ui.label(RichText::new("empty folder").weak().size(11.0));
                    }
                    // 点列表空白处 = 清空选择（Unity 的 Project 窗口同样行为）。
                    let rest = ui.available_rect_before_wrap();
                    if rest.height() > 0.0 || rest.width() > 0.0 {
                        let blank = ui.interact(
                            rest,
                            egui::Id::new(("asset-blank", folder.as_str())),
                            egui::Sense::click(),
                        );
                        if blank.clicked() {
                            state.clear_asset_selection();
                        }
                    }
                });
        }
    };

    // 两列模式（#419）：左目录树 + 右内容；单列直接铺满。
    if state.asset_columns == AssetColumns::Two {
        let height = ui.available_height().max(120.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(150.0, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| asset_folder_column(ui, workspace, state),
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| content(ui, workspace, state, &mut actions),
            );
        });
    } else {
        content(ui, workspace, state, &mut actions);
    }

    if let Some(path) = actions.ping.take() {
        ping_asset(workspace, state, &path);
    }
    if let Some((from, folder)) = actions.move_to_folder {
        match workspace.move_asset_to_folder(&from, Some(&folder)) {
            // 已经在同一个目录里：不写状态栏，避免「拖了一下什么都没变」的噪音。
            Ok(to) if to == from => {}
            Ok(to) => {
                state.select_asset_only(to.clone());
                state.status = format!("moved → {to}");
            }
            Err(error) => state.status = format!("move failed: {error}"),
        }
    }
    if let Some(path) = actions.open {
        if path.ends_with(".scene.json") || path.ends_with(".prefab.json") {
            if let Err(error) = workspace.open_project_file(&path) {
                state.status = format!("open {path} failed: {error}");
            }
        } else if prism_editor_core::project::is_json_editable_asset(&path) {
            // 材质 / 资产定义：双击进材质面板（#488）。
            open_material_asset(workspace, state, &path);
            workspace.show_panel("material");
            state.status = format!("opened {path}");
        }
    }
    if let Some(path) = actions.instantiate {
        match workspace.instantiate_prefab_asset(&path, None) {
            Ok(count) => state.status = format!("instantiated {count} entities"),
            Err(error) => state.status = format!("instantiate failed: {error}"),
        }
    }
    if let Some(path) = actions.rename {
        state.select_asset_only(path.clone());
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        state.asset_rename = Some((path, name));
        state.asset_rename_focus = true;
    }
    if let Some((from, file_name)) = actions.commit_rename {
        let target = rename_target(&from, &file_name);
        let is_folder = workspace
            .project_root
            .as_ref()
            .map(|root| root.join(&from).is_dir())
            .unwrap_or(false);
        match target {
            Some(to) if to != from => {
                let result = if is_folder {
                    workspace.rename_folder(&from, &to).map(|(count, _)| count)
                } else {
                    workspace.rename_asset(&from, &to).map(|(count, _)| count)
                };
                match result {
                    Ok(count) => {
                        state.select_asset_only(to.clone());
                        state.status = format!("renamed → {to} ({count} references updated)");
                    }
                    Err(error) => state.status = format!("rename failed: {error}"),
                }
            }
            Some(_) => {}
            None => state.status = "invalid file name".to_string(),
        }
    }
    if let Some(folder) = actions.create_subfolder {
        state.prompt = Some(Prompt::new(
            PromptKind::NewFolder,
            "New Folder",
            "assets/ 下的相对目录",
            format!("{folder}/"),
        ));
    }
    if let Some(paths) = actions.delete {
        // 场景 / 预制体走各自的文档确认框（`Workspace::delete_asset` 会拒绝它们），
        // 其余资产合并成一个确认框（#486）。
        let mut assets = Vec::new();
        for path in paths {
            if is_document_path(&path) {
                if state.pending_doc_delete.is_none() {
                    state.pending_doc_delete = Some(path);
                }
            } else {
                assets.push(path);
            }
        }
        if !assets.is_empty() {
            request_delete_assets(workspace, state, assets);
        }
    }
    if let Some(paths) = actions.duplicate {
        let mut created = Vec::new();
        let mut failures = Vec::new();
        for path in &paths {
            match workspace.duplicate_asset(path) {
                Ok(copy) => created.push(copy),
                Err(error) => failures.push(format!("{path}: {error}")),
            }
        }
        state.status = if failures.is_empty() {
            match created.as_slice() {
                [one] => format!("duplicated → {one}"),
                _ => format!("duplicated {} asset(s)", created.len()),
            }
        } else {
            format!("duplicate failed: {}", failures.join("; "))
        };
        if let Some(last) = created.pop() {
            state.select_asset_only(last);
        }
    }
    asset_delete_confirm(ui, workspace, state);
}

/// 删除资产：按首选项决定是否先弹确认框。
pub fn request_delete_asset(workspace: &mut Workspace, state: &mut UiState, path: &str) {
    request_delete_assets(workspace, state, vec![path.to_string()]);
}

/// 批量删除资产（一个确认框、逐项删除）。
pub fn request_delete_assets(workspace: &mut Workspace, state: &mut UiState, paths: Vec<String>) {
    if state.pending_asset_delete.is_some() || paths.is_empty() {
        return;
    }
    state.pending_asset_delete = Some(paths);
    let _ = workspace;
}

/// 真正执行删除（确认框 / 关闭确认后调用）。
pub fn delete_asset_now(workspace: &mut Workspace, state: &mut UiState, path: &str) {
    let is_folder = workspace
        .project_root
        .as_ref()
        .map(|root| root.join(path).is_dir())
        .unwrap_or(false);
    let result = if is_folder {
        workspace.delete_folder(path)
    } else {
        workspace.delete_asset(path)
    };
    match result {
        Ok(()) => {
            state.forget_asset(path);
            state.status = format!("deleted {path}");
        }
        Err(error) => state.status = format!("delete failed: {error}"),
    }
}

// ---------------------------------------------------------------------------
// 动画图 / 状态机只读面板（#560）
// ---------------------------------------------------------------------------

/// 选中 `.animgraph.json` / `.statemachine.json` 时显示结构化视图。
///
/// 图侧可以改连线（增删 / 权重）、增删剪辑节点，并把布局写进 sidecar；状态机侧
/// 可以增删转移，其余字段仍手改 JSON。选中 `*.skinning/notifies.json` 时切换到
/// 事件表编辑器（#560 Notify）。
/// 运行中的图节点一行（Play 模式下由 [`draw_runtime_debug`] 使用）。
fn runtime_node_summary(node: &prism_anim::GraphNodeSnapshot) -> String {
    let detail = if node.state_name.is_empty() {
        format!("{:.2}s", node.time)
    } else {
        format!("{} · {:.2}s", node.state_name, node.time)
    };
    format!(
        "{} [{:?}] w={:.2} {detail}",
        node.name, node.kind, node.weight
    )
}

/// 运行中的 Montage Slot 一行。
fn runtime_montage_summary(montage: &prism_anim::MontageSnapshot) -> String {
    let clip = montage.clip.as_deref().unwrap_or("-");
    let section = montage
        .section
        .as_deref()
        .map(|section| format!(" · {section}"))
        .unwrap_or_default();
    let warp = montage
        .warp_window
        .as_deref()
        .map(|window| {
            montage
                .warp_window_progress
                .map(|progress| format!(" · warp {window} {progress:.0}%"))
                .unwrap_or_else(|| format!(" · warp {window}"))
        })
        .unwrap_or_default();
    let time = montage
        .time
        .map(|time| format!("{time:.2}s"))
        .unwrap_or_else(|| "-".to_string());
    let mut flags = String::new();
    if montage.paused {
        flags.push_str(" paused");
    }
    if montage.finished {
        flags.push_str(" finished");
    }
    if montage.queued > 0 {
        flags.push_str(&format!(" +{} queued", montage.queued));
    }
    format!(
        "slot {} · {clip}{section}{warp} {time} w={:.2}{flags}",
        montage.slot, montage.weight
    )
}

/// Play 模式下动画图面板要读的运行时数据（#560 编辑器-运行时桥）。
#[derive(Clone, Copy)]
pub struct AnimRuntimeView<'a> {
    pub graph: &'a prism_engine::scene::systems::animator::AnimGraphDebug,
    /// 这一帧的 Notify 事件；引擎还没跑过动画系统时是 `None`。
    pub events: Option<&'a prism_engine::scene::systems::animator::AnimEvents>,
}

/// 一条 Notify 事件的显示文本。
fn runtime_event_summary(event: &prism_engine::scene::systems::animator::AnimEvent) -> String {
    format!("• {} @ {:.2}s", event.name, event.time)
}

/// Play 模式下的动画图实时快照（#560 编辑器-运行时桥）。
///
/// 数据由引擎在资源存在时每帧重建；这里只读。没有图实体时给一行提示，
/// 不会因为「还没按 Play」而隐藏整个面板。
fn draw_runtime_debug(ui: &mut Ui, runtime: Option<AnimRuntimeView<'_>>) {
    let Some(runtime) = runtime else {
        return;
    };
    let debug = runtime.graph;
    if debug.entities.is_empty() {
        ui.label(
            RichText::new("runtime: no animated graph entities (press Play)")
                .weak()
                .size(9.0),
        );
        return;
    }
    egui::CollapsingHeader::new(
        RichText::new(format!("Runtime · {} entities", debug.entities.len()))
            .color(Color32::from_rgb(120, 200, 160))
            .size(10.0),
    )
    .id_salt("anim-graph-runtime")
    .default_open(true)
    .show(ui, |ui| {
        let mut entities: Vec<_> = debug.entities.iter().collect();
        entities.sort_by_key(|(entity, _)| format!("{entity:?}"));
        for (entity, nodes) in entities {
            let path = debug.path(*entity).unwrap_or("?");
            ui.label(
                RichText::new(format!("{entity:?} · {path}"))
                    .weak()
                    .size(9.0),
            );
            for node in nodes {
                ui.label(RichText::new(runtime_node_summary(node)).size(9.0));
            }
            if let Some(montages) = debug.montages(*entity) {
                for montage in montages {
                    ui.label(RichText::new(runtime_montage_summary(montage)).size(9.0));
                }
            }
            if let Some(events) = runtime.events {
                for event in events.events.iter().filter(|event| event.entity == *entity) {
                    ui.label(
                        RichText::new(runtime_event_summary(event))
                            .color(Color32::from_rgb(110, 200, 255))
                            .size(9.0),
                    );
                }
            }
        }
    });
}

pub fn anim_graph(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
    runtime: Option<AnimRuntimeView<'_>>,
) {
    let Some(path) = state.asset_primary().cloned() else {
        ui.label(
            RichText::new("select an animation graph / state machine / clip / sidecar asset")
                .weak()
                .size(11.0),
        );
        return;
    };
    let notify_asset = is_notify_sidecar(&path);
    let sync_marker_asset = is_sync_markers_sidecar(&path);
    let montage_asset = is_montages_sidecar(&path);
    let clip_asset = path.ends_with(".ranim");
    let graph_asset = path.ends_with(".animgraph.json");
    let machine_asset = path.ends_with(".statemachine.json");
    if !notify_asset
        && !sync_marker_asset
        && !montage_asset
        && !clip_asset
        && !graph_asset
        && !machine_asset
    {
        ui.label(
            RichText::new("select an animation graph / state machine / clip / sidecar asset")
                .weak()
                .size(11.0),
        );
        return;
    }

    ui.label(
        RichText::new(path.rsplit('/').next().unwrap_or(&path))
            .strong()
            .size(13.0),
    );
    ui.label(RichText::new(&path).weak().size(10.0));
    draw_asset_edit_buttons(ui, workspace, state);
    ui.separator();

    if notify_asset {
        draw_notifies_asset(ui, workspace, state, &path);
        return;
    }
    if sync_marker_asset {
        draw_sync_markers_asset(ui, workspace, state, &path);
        return;
    }
    if montage_asset {
        draw_montages_asset(ui, workspace, state, &path);
        return;
    }
    if clip_asset {
        draw_clip_asset(ui, workspace, &path);
        return;
    }

    draw_runtime_debug(ui, runtime);
    ui.separator();

    let mut value = match workspace.read_asset_json(&path) {
        Ok(value) => value,
        Err(error) => {
            ui.label(
                RichText::new(format!("read {path} failed: {error}"))
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }
    };

    if graph_asset {
        match prism_editor_core::parse_anim_graph_view(&value) {
            Ok(graph) => {
                let validation = prism_editor_core::validate_anim_graph(&value);
                if validation.is_empty() {
                    ui.label(RichText::new("shape check: ok").weak().size(9.0));
                } else {
                    egui::CollapsingHeader::new(
                        RichText::new(format!("Shape issues ({})", validation.len()))
                            .color(Color32::from_rgb(232, 160, 96))
                            .size(10.0),
                    )
                    .id_salt("anim-graph-shape-issues")
                    .default_open(true)
                    .show(ui, |ui| {
                        for issue in &validation {
                            ui.label(
                                RichText::new(issue)
                                    .color(Color32::from_rgb(232, 160, 96))
                                    .size(9.0),
                            );
                        }
                    });
                }
                let stale = state
                    .anim_graph_layout
                    .as_ref()
                    .map(|(cached, _)| cached != &path)
                    .unwrap_or(true);
                if stale {
                    let layout = workspace.read_anim_graph_layout(&path);
                    state.anim_graph_layout = Some((path.clone(), layout));
                }
                let mut changed = false;
                if let Some((_, layout)) = state.anim_graph_layout.as_mut() {
                    changed = draw_anim_graph_layout(ui, &graph, layout);
                }
                if changed {
                    ui.label(RichText::new("layout modified").weak().size(10.0));
                }
                if ui.button("Save Layout").clicked() {
                    if let Some((_, layout)) = state.anim_graph_layout.as_ref() {
                        state.status = match workspace.write_anim_graph_layout(&path, layout) {
                            Ok(()) => format!("saved {}.layout.json", path),
                            Err(error) => format!("save layout failed: {error}"),
                        };
                    }
                }
                ui.separator();
                let before = value.clone();
                let result =
                    draw_anim_graph_view(ui, &graph, &mut value, &mut state.anim_graph_edit);
                match result {
                    Ok(changed_now) => {
                        if changed_now {
                            state.status = match workspace.write_asset_json(&path, &value) {
                                Ok(()) => {
                                    workspace.record_asset_edit(&path, &before, &value);
                                    format!("saved nodes/links in {path}")
                                }
                                Err(error) => format!("save graph failed: {error}"),
                            };
                        }
                        if let Some((old_name, new_name)) =
                            state.anim_graph_edit.graph_parameter_rename.take()
                        {
                            state.status = match workspace
                                .rename_anim_graph_parameter(&path, &old_name, &new_name)
                            {
                                Ok(updated) if updated.is_empty() => {
                                    format!("renamed parameter '{old_name}' to '{new_name}'")
                                }
                                Ok(updated) => format!(
                                    "renamed parameter '{old_name}' to '{new_name}', updated {}",
                                    updated.join(", ")
                                ),
                                Err(error) => error,
                            };
                        }
                    }
                    Err(error) => {
                        state.anim_graph_edit.graph_parameter_rename = None;
                        state.status = format!("animation graph: {error}");
                    }
                }
            }
            Err(error) => {
                ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
            }
        }
    } else {
        match prism_editor_core::parse_state_machine_view(&value) {
            Ok(machine) => {
                let before = value.clone();
                match draw_state_machine_view(
                    ui,
                    &machine,
                    &mut value,
                    &mut state.anim_graph_edit,
                    &mut state.anim_sm_condition,
                ) {
                    Ok(true) => {
                        state.status = match workspace.write_asset_json(&path, &value) {
                            Ok(()) => {
                                workspace.record_asset_edit(&path, &before, &value);
                                format!("saved state machine in {path}")
                            }
                            Err(error) => format!("save transitions failed: {error}"),
                        };
                    }
                    Ok(false) => {}
                    Err(error) => {
                        state.anim_graph_edit.sm_state_rename = None;
                        state.status = format!("state machine: {error}");
                    }
                }
                if let Some((index, new_name)) = state.anim_graph_edit.sm_state_rename.take() {
                    state.status =
                        match workspace.rename_state_machine_state(&path, index, &new_name) {
                            Ok(updated) if updated.is_empty() => {
                                format!("renamed state to '{new_name}'")
                            }
                            Ok(updated) => format!(
                                "renamed state to '{new_name}', updated {}",
                                updated.join(", ")
                            ),
                            Err(error) => format!("state machine: {error}"),
                        };
                }
            }
            Err(error) => {
                ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
            }
        }
    }
}

/// 资产编辑的撤销 / 重做按钮；撤销后本帧继续按磁盘内容重画。
fn draw_asset_edit_buttons(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                workspace.can_undo_asset_edit(),
                egui::Button::new("↶ Undo edit"),
            )
            .on_hover_text("undo the last edit in this asset")
            .clicked()
        {
            state.status = match workspace.undo_asset_edit() {
                Ok(Some(paths)) => format!("undid edit in {}", paths.join(", ")),
                Ok(None) => "nothing to undo".to_string(),
                Err(error) => error,
            };
        }
        if ui
            .add_enabled(
                workspace.can_redo_asset_edit(),
                egui::Button::new("↷ Redo edit"),
            )
            .on_hover_text("redo the last undone edit")
            .clicked()
        {
            state.status = match workspace.redo_asset_edit() {
                Ok(Some(paths)) => format!("redid edit in {}", paths.join(", ")),
                Ok(None) => "nothing to redo".to_string(),
                Err(error) => error,
            };
        }
    });
}

fn is_notify_sidecar(path: &str) -> bool {
    path.to_ascii_lowercase()
        .ends_with(".skinning/notifies.json")
}

fn is_sync_markers_sidecar(path: &str) -> bool {
    path.to_ascii_lowercase()
        .ends_with(".skinning/sync_markers.json")
}

fn is_montages_sidecar(path: &str) -> bool {
    path.to_ascii_lowercase()
        .ends_with(".skinning/montages.json")
}

/// `*.skinning/notifies.json`：按剪辑列出事件，可改时间 / 名字、增删。
fn draw_notifies_asset(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState, path: &str) {
    ui.label(
        RichText::new(path.rsplit('/').next().unwrap_or(path))
            .strong()
            .size(13.0),
    );
    ui.label(RichText::new(path).weak().size(10.0));
    ui.separator();

    let mut value = match workspace.read_asset_json(path) {
        Ok(value) => value,
        Err(error) => {
            ui.label(
                RichText::new(format!("read {path} failed: {error}"))
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }
    };
    let view = match prism_editor_core::parse_notifies_view(&value) {
        Ok(view) => view,
        Err(error) => {
            ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
            return;
        }
    };
    let before = value.clone();
    match draw_notifies_view(ui, &view, &mut value, &mut state.anim_graph_edit) {
        Ok(true) => {
            state.status = match workspace.write_asset_json(path, &value) {
                Ok(()) => {
                    workspace.record_asset_edit(path, &before, &value);
                    format!("saved notifies in {path}")
                }
                Err(error) => format!("save notifies failed: {error}"),
            };
        }
        Ok(false) => {}
        Err(error) => state.status = format!("notify file: {error}"),
    }
}

const ANIMATOR_COMPONENT: &str = "prism_engine::scene::Animator";

/// 资产路径 → `Animator.clip_path` 使用的、相对 `assets/` 的运行时路径。
fn clip_runtime_path(asset_path: &str) -> &str {
    asset_path.strip_prefix("assets/").unwrap_or(asset_path)
}

/// 当前场景选择里第一个带 Animator 的实体下标。
fn selected_animator_index(workspace: &Workspace) -> Option<usize> {
    workspace.selection.iter().rev().copied().find(|index| {
        workspace
            .document()
            .doc
            .entities
            .get(*index)
            .is_some_and(|entity| entity.components.contains_key(ANIMATOR_COMPONENT))
    })
}

fn edit_animator(
    workspace: &mut Workspace,
    index: usize,
    label: &str,
    mutate: impl FnOnce(&mut Value),
) {
    workspace.edit(label, |document| {
        if let Some(animator) = document
            .doc
            .entities
            .get_mut(index)
            .and_then(|entity| entity.components.get_mut(ANIMATOR_COMPONENT))
        {
            mutate(animator);
        }
    });
}

/// `.ranim`：剪辑检查器（时长、轨道、Notify / 同步标记、根运动）。
///
/// 事件 / 标记的编辑走作者侧侧车；这里不写二进制资产。选中带 Animator 的实体时，
/// 还可以把剪辑套到该实体上并做点击 seek / 播放暂停 / 倍速 / loop。
fn draw_clip_asset(ui: &mut Ui, workspace: &mut Workspace, path: &str) {
    let bytes = match workspace.read_asset_bytes(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            ui.label(
                RichText::new(format!("read {path} failed: {error}"))
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }
    };
    let Some(clip) = prism_anim::decode_ranim(&bytes) else {
        ui.label(RichText::new("not a valid .ranim clip").color(Color32::from_rgb(232, 96, 96)));
        return;
    };
    let duration = clip.clip_duration().max(0.0);
    ui.label(RichText::new(&clip.name).strong().size(13.0));
    ui.label(RichText::new(path).weak().size(10.0));
    let flags = match (clip.looped, clip.additive) {
        (true, true) => "looped · additive",
        (true, false) => "looped",
        (false, true) => "additive",
        (false, false) => "one-shot",
    };
    ui.label(
        RichText::new(format!(
            "{duration:.2}s · {:.0} fps · {flags} · {} tracks · {} notifies · {} sync markers",
            clip.fps,
            clip.tracks.len(),
            clip.notifies.len(),
            clip.sync_markers.len()
        ))
        .weak()
        .size(10.0),
    );

    let selected = selected_animator_index(workspace);
    let runtime_path = clip_runtime_path(path).to_string();
    let current_animator = selected.and_then(|index| {
        workspace.document().doc.entities[index]
            .components
            .get(ANIMATOR_COMPONENT)
            .cloned()
    });
    let matches = current_animator
        .as_ref()
        .and_then(|animator| animator.get("clip_path").and_then(Value::as_str))
        == Some(runtime_path.as_str());

    ui.separator();
    let width = ui.available_width().clamp(160.0, 560.0);
    let (track, painter) = ui.allocate_painter(Vec2::new(width, 44.0), egui::Sense::click());
    let rect = track.rect;
    painter.rect_filled(rect, 3.0, Color32::from_rgb(26, 28, 32));
    let inset = 8.0;
    let left = rect.left() + inset;
    let usable = (rect.width() - inset * 2.0).max(1.0);
    let y = rect.center().y;
    painter.line_segment(
        [egui::pos2(left, y), egui::pos2(left + usable, y)],
        egui::Stroke::new(1.0_f32, Color32::from_rgb(58, 63, 72)),
    );
    painter.text(
        egui::pos2(left, rect.bottom() - 1.0),
        egui::Align2::LEFT_BOTTOM,
        "0",
        egui::FontId::monospace(8.0),
        Color32::from_rgb(110, 116, 126),
    );
    painter.text(
        egui::pos2(left + usable, rect.bottom() - 1.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{duration:.2}s"),
        egui::FontId::monospace(8.0),
        Color32::from_rgb(110, 116, 126),
    );
    for notify in &clip.notifies {
        let x = prism_editor_core::timeline_x(notify.time, duration, left, usable);
        painter.circle_filled(egui::pos2(x, y), 3.5, Color32::from_rgb(214, 178, 74));
    }
    for phase in &clip.sync_markers {
        let x = prism_editor_core::timeline_x(phase * duration, duration, left, usable);
        painter.rect_filled(
            egui::Rect::from_center_size(egui::pos2(x, y - 7.0), Vec2::new(5.0, 5.0)),
            1.0,
            Color32::from_rgb(112, 196, 168),
        );
    }
    if let Some(pos) = track.hover_pos() {
        let mut closest: Option<(f32, String)> = None;
        for notify in &clip.notifies {
            let x = prism_editor_core::timeline_x(notify.time, duration, left, usable);
            let distance = (x - pos.x).abs();
            if distance <= 8.0 && closest.as_ref().is_none_or(|(best, _)| distance < *best) {
                closest = Some((distance, format!("{} @ {:.2}s", notify.name, notify.time)));
            }
        }
        for phase in &clip.sync_markers {
            let x = prism_editor_core::timeline_x(phase * duration, duration, left, usable);
            let distance = (x - pos.x).abs();
            if distance <= 8.0 && closest.as_ref().is_none_or(|(best, _)| distance < *best) {
                closest = Some((distance, format!("sync {phase:.3}")));
            }
        }
        if let Some((_, text)) = closest {
            track.clone().on_hover_text(text);
        }
    }
    ui.label(
        RichText::new("gold = notify · green = sync marker · read-only (edit sidecars)")
            .weak()
            .size(9.0),
    );

    if let Some(root) = clip.tracks.iter().min_by_key(|track| track.joint) {
        if let (Some(first), Some(last)) = (
            root.positions
                .iter()
                .min_by(|a, b| a.time.total_cmp(&b.time)),
            root.positions
                .iter()
                .max_by(|a, b| a.time.total_cmp(&b.time)),
        ) {
            let (delta, yaw) = prism_editor_core::root_motion_delta(first.value, last.value);
            ui.label(
                RichText::new(format!(
                    "root joint {} · Δ ({:.2}, {:.2}, {:.2}) m · yaw {:.1}°",
                    root.joint, delta[0], delta[1], delta[2], yaw
                ))
                .monospace()
                .size(10.0),
            );
        }
    }

    egui::CollapsingHeader::new(
        RichText::new(format!("Tracks ({})", clip.tracks.len()))
            .strong()
            .size(11.0),
    )
    .id_salt("clip-tracks")
    .show(ui, |ui| {
        for track in &clip.tracks {
            let first = track
                .positions
                .iter()
                .map(|key| key.time)
                .chain(track.rotations.iter().map(|key| key.time))
                .chain(track.scales.iter().map(|key| key.time))
                .filter(|time| time.is_finite())
                .fold(f32::INFINITY, f32::min);
            let last = track
                .positions
                .iter()
                .map(|key| key.time)
                .chain(track.rotations.iter().map(|key| key.time))
                .chain(track.scales.iter().map(|key| key.time))
                .filter(|time| time.is_finite())
                .fold(f32::NEG_INFINITY, f32::max);
            let range = if first.is_finite() && last.is_finite() {
                format!("{first:.2}..{last:.2}s")
            } else {
                "no keys".to_string()
            };
            ui.label(
                RichText::new(format!(
                    "#{} · pos {} · rot {} · scale {} · {range}",
                    track.joint,
                    track.positions.len(),
                    track.rotations.len(),
                    track.scales.len()
                ))
                .monospace()
                .size(9.0),
            );
        }
    });

    if !clip.notifies.is_empty() {
        egui::CollapsingHeader::new(
            RichText::new(format!("Notifies ({})", clip.notifies.len()))
                .strong()
                .size(11.0),
        )
        .id_salt("clip-notifies")
        .show(ui, |ui| {
            for notify in &clip.notifies {
                ui.label(
                    RichText::new(format!("{:.2}s · {}", notify.time, notify.name))
                        .monospace()
                        .size(9.0),
                );
            }
        });
    }

    ui.separator();
    ui.label(RichText::new("Selected Animator").strong().size(11.0));
    let Some(index) = selected else {
        ui.label(
            RichText::new("select an entity with an Animator in the scene")
                .weak()
                .size(10.0),
        );
        return;
    };
    let Some(animator) = current_animator.as_ref() else {
        return;
    };
    if !matches {
        let current_clip = animator
            .get("clip_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        ui.label(
            RichText::new(format!(
                "current clip: {}",
                if current_clip.is_empty() {
                    "-"
                } else {
                    current_clip
                }
            ))
            .weak()
            .size(10.0),
        );
        if ui
            .button("Use this clip (clears state machine / graph overrides)")
            .clicked()
        {
            edit_animator(workspace, index, "use animation clip", |animator| {
                animator["clip_path"] = json!(runtime_path.as_str());
                animator["time"] = json!(0.0);
                animator["playing"] = json!(true);
                animator["looped"] = json!(clip.looped);
                animator["state_machine_path"] = json!("");
                animator["anim_graph_path"] = json!("");
            });
            ui.label(
                RichText::new("applied; the scene reloads after the edit debounce")
                    .weak()
                    .size(9.0),
            );
        }
        return;
    }

    let mut speed = animator.get("speed").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let playing = animator
        .get("playing")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut looped = animator
        .get("looped")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let time = animator.get("time").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    ui.horizontal(|ui| {
        if ui.button(if playing { "Pause" } else { "Play" }).clicked() {
            let next = !playing;
            edit_animator(workspace, index, "toggle animation playback", |animator| {
                animator["playing"] = json!(next);
            });
        }
        if ui
            .add(
                egui::DragValue::new(&mut speed)
                    .speed(0.01)
                    .range(-4.0..=4.0)
                    .prefix("speed "),
            )
            .changed()
        {
            edit_animator(workspace, index, "set animation speed", |animator| {
                animator["speed"] = json!(speed);
            });
        }
        if ui.checkbox(&mut looped, "loop").changed() {
            edit_animator(workspace, index, "toggle animation loop", |animator| {
                animator["looped"] = json!(looped);
            });
        }
        ui.label(format!("t {time:.2}s"));
    });
    if matches && track.clicked() {
        if let Some(pos) = track.interact_pointer_pos() {
            let time = prism_editor_core::timeline_time_at(pos.x, left, usable, duration);
            edit_animator(workspace, index, "scrub animation", |animator| {
                animator["time"] = json!(time);
            });
        }
    }
    ui.label(
        RichText::new("click the timeline to seek · edits are undoable")
            .weak()
            .size(9.0),
    );
}

/// `*.skinning/montages.json`：按剪辑列出 Montage 段，可改 / 增删。
fn draw_montages_asset(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState, path: &str) {
    let mut value = match workspace.read_asset_json(path) {
        Ok(value) => value,
        Err(error) => {
            ui.label(
                RichText::new(format!("read {path} failed: {error}"))
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }
    };
    let view = match prism_editor_core::parse_montages_view(&value) {
        Ok(view) => view,
        Err(error) => {
            ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
            return;
        }
    };
    let before = value.clone();
    match draw_montages_view(ui, &view, &mut value, &mut state.anim_graph_edit) {
        Ok(true) => {
            state.status = match workspace.write_asset_json(path, &value) {
                Ok(()) => {
                    workspace.record_asset_edit(path, &before, &value);
                    format!("saved montage sections in {path}")
                }
                Err(error) => format!("save montage sections failed: {error}"),
            };
        }
        Ok(false) => {}
        Err(error) => state.status = format!("montage file: {error}"),
    }
}

fn draw_montage_warp_windows(
    ui: &mut Ui,
    clip: &prism_editor_core::ClipMontageView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
    span: f32,
) -> Result<bool, String> {
    let mut changed = false;
    let mut updates: Vec<(usize, String, f32, f32)> = Vec::new();
    let mut removals: Vec<usize> = Vec::new();

    ui.label(
        RichText::new(format!("Warp windows ({})", clip.warp_windows.len()))
            .strong()
            .size(10.0),
    );
    if clip.warp_windows.is_empty() {
        ui.label(
            RichText::new("no warp windows — add one below")
                .weak()
                .size(9.0),
        );
    }
    let width = ui.available_width().clamp(160.0, 560.0);
    let (track, painter) = ui.allocate_painter(Vec2::new(width, 22.0), egui::Sense::hover());
    let rect = track.rect;
    painter.rect_filled(rect, 3.0, Color32::from_rgb(26, 28, 32));
    let inset = 6.0;
    let left = rect.left() + inset;
    let usable = (rect.width() - inset * 2.0).max(1.0);
    for window in &clip.warp_windows {
        let start = prism_editor_core::timeline_x(window.start, span, left, usable);
        let end = prism_editor_core::timeline_x(window.end, span, left, usable);
        let bar = egui::Rect::from_min_max(
            egui::pos2(start, rect.top() + 4.0),
            egui::pos2(end.max(start + 2.0), rect.bottom() - 4.0),
        );
        painter.rect_filled(bar, 2.0, Color32::from_rgb(214, 146, 74));
        if bar.width() > 28.0 {
            painter.text(
                bar.center(),
                egui::Align2::CENTER_CENTER,
                &window.name,
                egui::FontId::monospace(8.0),
                Color32::from_rgb(24, 20, 14),
            );
        }
    }
    painter.text(
        egui::pos2(left + usable, rect.bottom() - 1.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{span:.2}s"),
        egui::FontId::monospace(8.0),
        Color32::from_rgb(110, 116, 126),
    );

    for (index, window) in clip.warp_windows.iter().enumerate() {
        ui.horizontal(|ui| {
            let editing = matches!(
                &edit.montage_warp_name,
                Some((clip_path, edit_index, _))
                    if clip_path == &clip.clip && *edit_index == index
            );
            let mut name = if editing {
                edit.montage_warp_name
                    .as_ref()
                    .map(|(_, _, name)| name.clone())
                    .unwrap_or_else(|| window.name.clone())
            } else {
                window.name.clone()
            };
            let response = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .desired_width(96.0)
                    .hint_text("warp window"),
            );
            if response.has_focus() {
                edit.montage_warp_name = Some((clip.clip.clone(), index, name.clone()));
            }
            let mut start = window.start;
            let mut end = window.end;
            let start_changed = ui
                .add(
                    egui::DragValue::new(&mut start)
                        .speed(0.01)
                        .range(0.0..=span)
                        .prefix("start "),
                )
                .changed();
            let end_changed = ui
                .add(
                    egui::DragValue::new(&mut end)
                        .speed(0.01)
                        .range(start..=span)
                        .prefix("end "),
                )
                .changed();
            let name_changed = response.lost_focus() && name.trim() != window.name;
            if name_changed || start_changed || end_changed {
                let current_name = edit
                    .montage_warp_name
                    .as_ref()
                    .filter(|(clip_path, edit_index, _)| {
                        clip_path == &clip.clip && *edit_index == index
                    })
                    .map(|(_, _, name)| name.as_str())
                    .unwrap_or(&window.name);
                updates.push((index, current_name.to_string(), start, end));
            }
            if ui
                .button("×")
                .on_hover_text("delete this warp window")
                .clicked()
            {
                removals.push(index);
            }
        });
    }

    let had_updates = !updates.is_empty();
    for (index, name, start, end) in updates {
        prism_editor_core::set_montage_warp_window(document, &clip.clip, index, &name, start, end)?;
        changed = true;
    }
    if had_updates {
        edit.montage_warp_name = None;
    }
    for index in removals.into_iter().rev() {
        prism_editor_core::remove_montage_warp_window(document, &clip.clip, index)?;
        changed = true;
    }

    let draft_matches = matches!(
        &edit.montage_new_warp,
        Some((clip_path, _, _, _)) if clip_path == &clip.clip
    );
    if draft_matches {
        let mut add = false;
        let mut cancel = false;
        {
            let (_, name, start, end) =
                edit.montage_new_warp.as_mut().expect("draft checked above");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(name)
                        .desired_width(96.0)
                        .hint_text("new warp window"),
                );
                ui.add(
                    egui::DragValue::new(start)
                        .speed(0.01)
                        .range(0.0..=span)
                        .prefix("start "),
                );
                ui.add(
                    egui::DragValue::new(end)
                        .speed(0.01)
                        .range(*start..=span)
                        .prefix("end "),
                );
                add = ui.button("Add").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        }
        if add {
            let (_, name, start, end) = edit.montage_new_warp.clone().expect("draft checked above");
            prism_editor_core::add_montage_warp_window(document, &clip.clip, &name, start, end)?;
            edit.montage_new_warp = None;
            changed = true;
        } else if cancel {
            edit.montage_new_warp = None;
        }
    } else if ui.button("+ Warp").clicked() {
        edit.montage_new_warp = Some((clip.clip.clone(), String::new(), 0.0, span.min(0.5)));
    }

    Ok(changed)
}

fn draw_montages_view(
    ui: &mut Ui,
    view: &prism_editor_core::MontagesView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
) -> Result<bool, String> {
    let mut changed = false;
    let mut error: Option<String> = None;
    ui.label(
        RichText::new(format!(
            "Montage ranges ({} sections · {} warp windows)",
            view.section_count(),
            view.warp_window_count()
        ))
        .strong()
        .size(11.0),
    );
    if view.is_empty() {
        ui.label(
            RichText::new("no montage ranges yet — add one below")
                .weak()
                .size(10.0),
        );
    }
    if edit.montage_new_section.is_none() && ui.button("+ New clip section").clicked() {
        edit.montage_new_section = Some((String::new(), String::new(), 0.0, 0.5));
    }

    for clip in &view.clips {
        ui.label(
            RichText::new(format!("{} ({})", clip.clip, clip.sections.len()))
                .strong()
                .size(10.0),
        );
        let span = clip
            .sections
            .iter()
            .map(|section| section.end)
            .chain(clip.warp_windows.iter().map(|window| window.end))
            .filter(|end| end.is_finite() && *end > 0.0)
            .fold(1.0_f32, f32::max)
            * 1.1;
        let width = ui.available_width().clamp(160.0, 560.0);
        let (track, painter) = ui.allocate_painter(Vec2::new(width, 28.0), egui::Sense::hover());
        let rect = track.rect;
        painter.rect_filled(rect, 3.0, Color32::from_rgb(26, 28, 32));
        let inset = 6.0;
        let left = rect.left() + inset;
        let usable = (rect.width() - inset * 2.0).max(1.0);
        for (index, section) in clip.sections.iter().enumerate() {
            let start = prism_editor_core::timeline_x(section.start, span, left, usable);
            let end = prism_editor_core::timeline_x(section.end, span, left, usable);
            let bar = egui::Rect::from_min_max(
                egui::pos2(start, rect.top() + 5.0),
                egui::pos2(end.max(start + 2.0), rect.bottom() - 5.0),
            );
            let color = if index % 2 == 0 {
                Color32::from_rgb(86, 142, 196)
            } else {
                Color32::from_rgb(112, 168, 124)
            };
            painter.rect_filled(bar, 2.0, color);
            if bar.width() > 28.0 {
                painter.text(
                    bar.center(),
                    egui::Align2::CENTER_CENTER,
                    &section.name,
                    egui::FontId::monospace(8.0),
                    Color32::from_rgb(18, 20, 24),
                );
            }
        }
        painter.text(
            egui::pos2(left + usable, rect.bottom() - 1.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{span:.2}s"),
            egui::FontId::monospace(8.0),
            Color32::from_rgb(110, 116, 126),
        );

        for (index, section) in clip.sections.iter().enumerate() {
            ui.horizontal(|ui| {
                let editing = matches!(
                    &edit.montage_section_name,
                    Some((clip_path, edit_index, _))
                        if clip_path == &clip.clip && *edit_index == index
                );
                let mut name = if editing {
                    edit.montage_section_name
                        .as_ref()
                        .map(|(_, _, name)| name.clone())
                        .unwrap_or_else(|| section.name.clone())
                } else {
                    section.name.clone()
                };
                let response = ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .desired_width(96.0)
                        .hint_text("section name"),
                );
                if response.has_focus() {
                    edit.montage_section_name = Some((clip.clip.clone(), index, name.clone()));
                }
                if response.lost_focus() && name.trim() != section.name {
                    if let Err(err) = prism_editor_core::set_montage_section(
                        document,
                        &clip.clip,
                        index,
                        &name,
                        section.start,
                        section.end,
                    ) {
                        error = Some(err);
                    } else {
                        edit.montage_section_name = None;
                        changed = true;
                    }
                }

                let mut start = section.start;
                let mut end = section.end;
                let start_changed = ui
                    .add(
                        egui::DragValue::new(&mut start)
                            .speed(0.01)
                            .range(0.0..=span)
                            .prefix("start "),
                    )
                    .changed();
                let end_changed = ui
                    .add(
                        egui::DragValue::new(&mut end)
                            .speed(0.01)
                            .range(start..=span)
                            .prefix("end "),
                    )
                    .changed();
                if start_changed || end_changed {
                    let current_name = edit
                        .montage_section_name
                        .as_ref()
                        .filter(|(clip_path, edit_index, _)| {
                            clip_path == &clip.clip && *edit_index == index
                        })
                        .map(|(_, _, name)| name.as_str())
                        .unwrap_or(&section.name);
                    if let Err(err) = prism_editor_core::set_montage_section(
                        document,
                        &clip.clip,
                        index,
                        current_name,
                        start,
                        end,
                    ) {
                        error = Some(err);
                    } else {
                        changed = true;
                    }
                }
                if ui
                    .button("×")
                    .on_hover_text("delete this montage section")
                    .clicked()
                {
                    if let Err(err) =
                        prism_editor_core::remove_montage_section(document, &clip.clip, index)
                    {
                        error = Some(err);
                    } else {
                        edit.montage_section_name = None;
                        changed = true;
                    }
                }
            });
            if let Some(err) = error.take() {
                return Err(err);
            }
        }

        let draft_matches = matches!(
            &edit.montage_new_section,
            Some((clip_path, _, _, _)) if clip_path == &clip.clip
        );
        if draft_matches {
            let mut add = false;
            let mut cancel = false;
            {
                let (_, name, start, end) = edit
                    .montage_new_section
                    .as_mut()
                    .expect("draft checked above");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(96.0)
                            .hint_text("new section"),
                    );
                    ui.add(
                        egui::DragValue::new(start)
                            .speed(0.01)
                            .range(0.0..=span)
                            .prefix("start "),
                    );
                    ui.add(
                        egui::DragValue::new(end)
                            .speed(0.01)
                            .range(*start..=span)
                            .prefix("end "),
                    );
                    add = ui.button("Add").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            }
            if add {
                let (_, name, start, end) = edit
                    .montage_new_section
                    .clone()
                    .expect("draft checked above");
                prism_editor_core::add_montage_section(document, &clip.clip, &name, start, end)?;
                edit.montage_new_section = None;
                changed = true;
            } else if cancel {
                edit.montage_new_section = None;
            }
        } else if ui.button("+ Section").clicked() {
            edit.montage_new_section = Some((clip.clip.clone(), String::new(), 0.0, span.min(0.5)));
        }

        changed |= draw_montage_warp_windows(ui, clip, document, edit, span)?;
    }

    // 新剪辑的段：顶部按钮打开草稿后，这里允许填剪辑名（glTF 动画原名）。
    if let Some((clip, _, _, _)) = edit.montage_new_section.as_ref() {
        if !view.clips.iter().any(|entry| &entry.clip == clip) {
            ui.separator();
            let mut add = false;
            let mut cancel = false;
            {
                let (clip_name, name, start, end) = edit
                    .montage_new_section
                    .as_mut()
                    .expect("draft checked above");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(clip_name)
                            .desired_width(110.0)
                            .hint_text("glTF clip name"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(96.0)
                            .hint_text("section name"),
                    );
                    ui.add(
                        egui::DragValue::new(start)
                            .speed(0.01)
                            .range(0.0..=f32::MAX)
                            .prefix("start "),
                    );
                    ui.add(
                        egui::DragValue::new(end)
                            .speed(0.01)
                            .range(*start..=f32::MAX)
                            .prefix("end "),
                    );
                    add = ui.button("Add").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            }
            if add {
                let (clip_name, name, start, end) = edit
                    .montage_new_section
                    .clone()
                    .expect("draft checked above");
                prism_editor_core::add_montage_section(document, &clip_name, &name, start, end)?;
                edit.montage_new_section = None;
                changed = true;
            } else if cancel {
                edit.montage_new_section = None;
            }
        }
    }

    ui.label(
        RichText::new(
            "cook merges sections + warp windows into .ranim v5 · active_warp_window() queries them",
        )
        .weak()
        .size(9.0),
    );
    Ok(changed)
}

/// `*.skinning/sync_markers.json`：按剪辑列出归一化相位，可改 / 增删。
fn draw_sync_markers_asset(
    ui: &mut Ui,
    workspace: &mut Workspace,
    state: &mut UiState,
    path: &str,
) {
    let mut value = match workspace.read_asset_json(path) {
        Ok(value) => value,
        Err(error) => {
            ui.label(
                RichText::new(format!("read {path} failed: {error}"))
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            return;
        }
    };
    let view = match prism_editor_core::parse_sync_markers_view(&value) {
        Ok(view) => view,
        Err(error) => {
            ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
            return;
        }
    };
    let before = value.clone();
    match draw_sync_markers_view(ui, &view, &mut value, &mut state.anim_graph_edit) {
        Ok(true) => {
            state.status = match workspace.write_asset_json(path, &value) {
                Ok(()) => {
                    workspace.record_asset_edit(path, &before, &value);
                    format!("saved sync markers in {path}")
                }
                Err(error) => format!("save sync markers failed: {error}"),
            };
        }
        Ok(false) => {}
        Err(error) => state.status = format!("sync markers: {error}"),
    }
}

fn draw_sync_markers_view(
    ui: &mut Ui,
    view: &prism_editor_core::SyncMarkersView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
) -> Result<bool, String> {
    let mut changed = false;
    ui.label(
        RichText::new(format!("Sync markers ({})", view.len()))
            .strong()
            .size(11.0),
    );
    if view.is_empty() {
        ui.label(
            RichText::new("no sync markers yet — add one below")
                .weak()
                .size(10.0),
        );
    }

    let mut phase_edits = Vec::new();
    let mut removals = Vec::new();
    for clip in &view.clips {
        ui.label(
            RichText::new(format!("{} ({})", clip.clip, clip.phases.len()))
                .strong()
                .size(10.0),
        );
        let width = ui.available_width().clamp(140.0, 520.0);
        let (track, painter) =
            ui.allocate_painter(Vec2::new(width, 20.0), egui::Sense::click_and_drag());
        let rect = track.rect;
        painter.rect_filled(rect, 3.0, Color32::from_rgb(26, 28, 32));
        let inset = 5.0;
        let left = rect.left() + inset;
        let usable = (rect.width() - inset * 2.0).max(1.0);
        let y = rect.center().y;
        painter.line_segment(
            [egui::pos2(left, y), egui::pos2(left + usable, y)],
            egui::Stroke::new(1.0_f32, Color32::from_rgb(58, 63, 72)),
        );
        for tick in [0.25, 0.5, 0.75] {
            let x = left + tick * usable;
            painter.line_segment(
                [egui::pos2(x, y - 3.0), egui::pos2(x, y + 3.0)],
                egui::Stroke::new(1.0_f32, Color32::from_rgb(48, 52, 60)),
            );
        }
        painter.text(
            egui::pos2(left, rect.top() + 1.0),
            egui::Align2::LEFT_TOP,
            "0",
            egui::FontId::monospace(8.0),
            Color32::from_rgb(110, 116, 126),
        );
        painter.text(
            egui::pos2(left + usable, rect.top() + 1.0),
            egui::Align2::RIGHT_TOP,
            "1",
            egui::FontId::monospace(8.0),
            Color32::from_rgb(110, 116, 126),
        );
        let x_of = |phase: f32| left + phase.clamp(0.0, 1.0) * usable;
        if track.drag_started() {
            edit.sync_marker_drag = track
                .interact_pointer_pos()
                .and_then(|pos| {
                    prism_editor_core::nearest_sync_marker_index(
                        &clip.phases,
                        pos.x,
                        left,
                        usable,
                        8.0,
                    )
                })
                .map(|index| (clip.clip.clone(), index));
        }
        if track.dragged() {
            if let (Some(pos), Some((owner, index))) =
                (track.interact_pointer_pos(), edit.sync_marker_drag.as_ref())
            {
                if owner == &clip.clip && *index < clip.phases.len() {
                    phase_edits.push((
                        clip.clip.clone(),
                        *index,
                        prism_editor_core::sync_marker_phase_at(pos.x, left, usable),
                    ));
                }
            }
        }
        if track.drag_stopped() {
            edit.sync_marker_drag = None;
        }
        if track.clicked() {
            if let Some(pos) = track.interact_pointer_pos() {
                let hit = prism_editor_core::nearest_sync_marker_index(
                    &clip.phases,
                    pos.x,
                    left,
                    usable,
                    8.0,
                );
                if hit.is_none() {
                    edit.sync_marker_draft = Some((
                        clip.clip.clone(),
                        prism_editor_core::sync_marker_phase_at(pos.x, left, usable),
                    ));
                }
            }
        }
        let hovered = track.hover_pos().and_then(|pos| {
            prism_editor_core::nearest_sync_marker_index(&clip.phases, pos.x, left, usable, 8.0)
        });
        for (index, phase) in clip.phases.iter().enumerate() {
            let active = edit
                .sync_marker_drag
                .as_ref()
                .is_some_and(|(owner, slot)| owner == &clip.clip && *slot == index);
            let radius = if active || hovered == Some(index) {
                4.5
            } else {
                3.5
            };
            painter.circle_filled(
                egui::pos2(x_of(*phase), y),
                radius,
                Color32::from_rgb(112, 196, 168),
            );
        }
        if let Some(phase) = hovered.and_then(|index| clip.phases.get(index)) {
            track
                .clone()
                .on_hover_text(format!("phase {phase:.3} — drag to move"));
        } else if track.hovered() {
            track
                .clone()
                .on_hover_text("click to add a sync marker at this phase");
        }
        for (index, phase) in clip.phases.iter().enumerate() {
            ui.horizontal(|ui| {
                let mut phase = *phase;
                if ui
                    .add(
                        egui::DragValue::new(&mut phase)
                            .speed(0.001)
                            .range(0.0..=1.0)
                            .max_decimals(3)
                            .prefix("phase "),
                    )
                    .changed()
                {
                    phase_edits.push((clip.clip.clone(), index, phase));
                }
                if ui.small_button("×").clicked() {
                    removals.push((clip.clip.clone(), index));
                }
            });
        }
    }

    ui.separator();
    let mut add = None;
    let mut cancel = false;
    if let Some(draft) = edit.sync_marker_draft.as_mut() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.0)
                    .hint_text("clip (glTF name)")
                    .desired_width(120.0),
            );
            ui.add(
                egui::DragValue::new(&mut draft.1)
                    .speed(0.001)
                    .range(0.0..=1.0)
                    .max_decimals(3)
                    .prefix("phase "),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Add marker").clicked() {
                add = Some(draft.clone());
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    } else if ui.button("+ Marker").clicked() {
        edit.sync_marker_draft = Some((String::new(), 0.0));
    }

    for (clip, index, phase) in phase_edits {
        prism_editor_core::set_sync_marker(document, &clip, index, phase)?;
        changed = true;
    }
    for (clip, index) in removals {
        prism_editor_core::remove_sync_marker(document, &clip, index)?;
        changed = true;
    }
    if let Some((clip, phase)) = add {
        prism_editor_core::add_sync_marker(document, &clip, phase)?;
        edit.sync_marker_draft = None;
        changed = true;
    }
    if cancel {
        edit.sync_marker_draft = None;
    }
    Ok(changed)
}

fn draw_notifies_view(
    ui: &mut Ui,
    view: &prism_editor_core::NotifiesView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
) -> Result<bool, String> {
    let mut changed = false;
    ui.label(
        RichText::new(format!("Notifies ({})", view.len()))
            .strong()
            .size(11.0),
    );
    if view.is_empty() {
        ui.label(
            RichText::new("no notifies yet — add one below")
                .weak()
                .size(10.0),
        );
    }

    let mut time_edits = Vec::new();
    let mut name_edits = Vec::new();
    let mut removals = Vec::new();
    for clip in &view.clips {
        ui.label(
            RichText::new(format!("{} ({})", clip.clip, clip.entries.len()))
                .strong()
                .size(10.0),
        );
        let span = prism_editor_core::notify_timeline_span(&clip.entries);
        let width = ui.available_width().clamp(140.0, 520.0);
        let (track, painter) =
            ui.allocate_painter(Vec2::new(width, 20.0), egui::Sense::click_and_drag());
        let rect = track.rect;
        painter.rect_filled(rect, 3.0, Color32::from_rgb(26, 28, 32));
        let inset = 5.0;
        let left = rect.left() + inset;
        let usable = (rect.width() - inset * 2.0).max(1.0);
        let y = rect.center().y;
        painter.line_segment(
            [egui::pos2(left, y), egui::pos2(left + usable, y)],
            egui::Stroke::new(1.0_f32, Color32::from_rgb(58, 63, 72)),
        );
        painter.text(
            egui::pos2(left + usable, rect.top() + 1.0),
            egui::Align2::RIGHT_TOP,
            format!("{span:.1}s"),
            egui::FontId::monospace(8.0),
            Color32::from_rgb(110, 116, 126),
        );
        let x_of = |time: f32| left + (time / span).clamp(0.0, 1.0) * usable;
        if track.drag_started() {
            edit.notify_drag = track
                .interact_pointer_pos()
                .and_then(|pos| {
                    prism_editor_core::nearest_notify_index(
                        &clip.entries,
                        pos.x,
                        left,
                        usable,
                        span,
                        8.0,
                    )
                })
                .map(|index| (clip.clip.clone(), index));
        }
        if track.dragged() {
            if let (Some(pos), Some((owner, index))) =
                (track.interact_pointer_pos(), edit.notify_drag.as_ref())
            {
                if owner == &clip.clip {
                    if let Some(entry) = clip.entries.get(*index) {
                        let time = prism_editor_core::notify_time_at(pos.x, left, usable, span);
                        time_edits.push((clip.clip.clone(), *index, time, entry.name.clone()));
                    }
                }
            }
        }
        if track.drag_stopped() {
            edit.notify_drag = None;
        }
        if track.clicked() {
            if let Some(pos) = track.interact_pointer_pos() {
                let hit = prism_editor_core::nearest_notify_index(
                    &clip.entries,
                    pos.x,
                    left,
                    usable,
                    span,
                    8.0,
                );
                if hit.is_none() {
                    // 空白处点一下 = 在那个时间打开新增草稿，名字留空等作者填。
                    edit.notify_draft = Some((
                        clip.clip.clone(),
                        prism_editor_core::notify_time_at(pos.x, left, usable, span),
                        String::new(),
                    ));
                }
            }
        }
        let hovered = track.hover_pos().and_then(|pos| {
            prism_editor_core::nearest_notify_index(&clip.entries, pos.x, left, usable, span, 8.0)
        });
        for (index, entry) in clip.entries.iter().enumerate() {
            let active = edit
                .notify_drag
                .as_ref()
                .is_some_and(|(owner, slot)| owner == &clip.clip && *slot == index);
            let radius = if active || hovered == Some(index) {
                4.5
            } else {
                3.5
            };
            painter.circle_filled(
                egui::pos2(x_of(entry.time), y),
                radius,
                Color32::from_rgb(214, 178, 74),
            );
        }
        if let Some(entry) = hovered.and_then(|index| clip.entries.get(index)) {
            track.clone().on_hover_text(format!(
                "{} @ {:.2}s — drag to retime",
                entry.name, entry.time
            ));
        } else if track.hovered() {
            track
                .clone()
                .on_hover_text("click to add a notify at this time");
        }
        for (index, entry) in clip.entries.iter().enumerate() {
            ui.horizontal(|ui| {
                let mut time = entry.time;
                if ui
                    .add(
                        egui::DragValue::new(&mut time)
                            .speed(0.01)
                            .range(0.0..=60.0)
                            .prefix("t "),
                    )
                    .changed()
                {
                    time_edits.push((clip.clip.clone(), index, time, entry.name.clone()));
                }
                let editing = edit
                    .notify_name
                    .as_ref()
                    .filter(|(owner, slot, _)| owner == &clip.clip && *slot == index);
                let mut text = editing
                    .map(|(_, _, text)| text.clone())
                    .unwrap_or_else(|| entry.name.clone());
                let response = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .hint_text("Footstep.L")
                        .desired_width(150.0),
                );
                if response.changed() {
                    edit.notify_name = Some((clip.clip.clone(), index, text));
                } else if response.lost_focus() {
                    if let Some((owner, slot, text)) = edit.notify_name.take() {
                        if owner == clip.clip && slot == index {
                            name_edits.push((owner, slot, text, entry.time));
                        }
                    }
                }
                if ui.small_button("×").clicked() {
                    removals.push((clip.clip.clone(), index));
                }
            });
        }
    }

    ui.separator();
    let mut add = None;
    let mut cancel = false;
    if let Some(draft) = edit.notify_draft.as_mut() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.0)
                    .hint_text("clip (glTF name)")
                    .desired_width(120.0),
            );
            ui.add(
                egui::DragValue::new(&mut draft.1)
                    .speed(0.01)
                    .range(0.0..=60.0)
                    .prefix("t "),
            );
            ui.add(
                egui::TextEdit::singleline(&mut draft.2)
                    .hint_text("Footstep.L")
                    .desired_width(140.0),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Add notify").clicked() {
                add = Some(draft.clone());
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    } else if ui.button("+ Notify").clicked() {
        edit.notify_draft = Some((String::new(), 0.0, String::new()));
    }

    for (clip, index, time, name) in time_edits {
        prism_editor_core::set_notify(document, &clip, index, time, &name)?;
        changed = true;
    }
    for (clip, index, name, time) in name_edits {
        prism_editor_core::set_notify(document, &clip, index, time, &name)?;
        changed = true;
    }
    for (clip, index) in removals {
        prism_editor_core::remove_notify(document, &clip, index)?;
        changed = true;
    }
    if let Some((clip, time, name)) = add {
        prism_editor_core::add_notify(document, &clip, time, &name)?;
        edit.notify_draft = None;
        changed = true;
    }
    if cancel {
        edit.notify_draft = None;
    }
    Ok(changed)
}

/// 画布上的节点拖拽；返回这一帧是否有节点移动。
fn draw_anim_graph_layout(
    ui: &mut Ui,
    graph: &prism_editor_core::AnimGraphView,
    layout: &mut prism_editor_core::AnimGraphLayout,
) -> bool {
    let width = ui.available_width().clamp(240.0, 560.0);
    let height = 300.0;
    let (response, painter) =
        ui.allocate_painter(Vec2::new(width, height), Sense::click_and_drag());
    painter.rect_filled(response.rect, 4.0, Color32::from_rgb(28, 30, 34));

    // 先补齐缺省位置：新节点排成网格。
    for (index, node) in graph.nodes.iter().enumerate() {
        if layout.position(&node.name).is_none() {
            let position = [
                40.0 + (index % 5) as f32 * 120.0,
                30.0 + (index / 5) as f32 * 70.0,
            ];
            layout.set_position(node.name.clone(), position);
        }
    }

    let origin = response.rect.min;
    let to_screen = |position: [f32; 2]| egui::pos2(origin.x + position[0], origin.y + position[1]);

    // 连线先画，节点盖在上面。
    for node in &graph.nodes {
        let Some(target) = layout.position(&node.name) else {
            continue;
        };
        for input in &node.inputs {
            let Some(source) = layout.position(&input.node) else {
                continue;
            };
            painter.line_segment(
                [to_screen(source), to_screen(target)],
                egui::Stroke::new(1.0, Color32::from_rgb(88, 96, 108)),
            );
        }
    }

    let mut changed = false;
    for node in &graph.nodes {
        let Some(position) = layout.position(&node.name) else {
            continue;
        };
        let center = to_screen(position);
        let rect = egui::Rect::from_center_size(center, Vec2::new(112.0, 34.0));
        let response = ui.interact(
            rect,
            ui.id().with(("anim-graph-layout", &node.name)),
            Sense::drag(),
        );
        if response.dragged() {
            let delta = response.drag_delta();
            let next = [
                (position[0] + delta.x).clamp(12.0, width - 12.0),
                (position[1] + delta.y).clamp(12.0, height - 12.0),
            ];
            layout.set_position(node.name.clone(), next);
            changed = true;
        }
        let fill = if node.is_output() {
            Color32::from_rgb(52, 74, 52)
        } else {
            Color32::from_rgb(48, 54, 64)
        };
        painter.rect_filled(rect, 4.0, fill);
        painter.rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0, Color32::from_rgb(120, 132, 150)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            &node.name,
            egui::FontId::monospace(11.0),
            Color32::from_rgb(230, 234, 240),
        );
    }
    changed
}

/// Animation Graph 面板的输入缓冲（跨帧保留正在编辑的文本）。
#[derive(Debug, Default)]
pub struct AnimGraphEditState {
    /// 「新增节点」：(节点名, 剪辑 / 状态机路径或可选上游节点)。
    pub new_node: (String, String),
    /// 「新增节点」类型；空串按 `clip_player` 处理。
    pub new_node_kind: String,
    /// 正在编辑的连线骨骼掩码：(节点名, 连线下标, 文本)。
    ///
    /// 失焦时才写回：边打字边规范化会把还没打完的逗号吃掉。
    pub mask: Option<(String, usize, String)>,
    /// 正在编辑的状态名 / 剪辑：(状态下标, 文本)，同样失焦才写回。
    pub sm_state_name: Option<(usize, String)>,
    pub sm_state_clip: Option<(usize, String)>,
    /// 「+ State」草稿：(名字, 剪辑路径)。
    pub sm_new_state: Option<(String, String)>,
    /// 待执行的状态改名：(状态下标, 新名字)，由工作区同步场景 / 预制体引用。
    pub sm_state_rename: Option<(usize, String)>,
    /// 正在编辑的条件参数名：(转移下标, 条件下标, 文本)，失焦才写回。
    pub sm_condition_name: Option<(usize, usize, String)>,
    /// 正在编辑的图节点剪辑：(节点名, 文本)。
    pub graph_node_clip: Option<(String, String)>,
    /// 正在编辑的图节点名字：(旧名字, 文本)。
    pub graph_node_name: Option<(String, String)>,
    /// 正在编辑的混合空间参数名：(节点名, 文本)；`parameter_y` 单独一份。
    pub graph_node_parameter: Option<(String, String)>,
    pub graph_node_parameter_y: Option<(String, String)>,
    /// 「+ Sample」草稿：(节点名, 剪辑, x, y)。
    pub graph_sample: Option<(String, String, f32, f32)>,
    /// 「+ Parameter」草稿：(名字, 类型, 默认值)。
    pub graph_parameter: Option<(String, String, f32)>,
    /// 参数改名缓冲：(原名, 新文本)，失焦后写进 `graph_parameter_rename`。
    pub graph_parameter_name: Option<(String, String)>,
    /// 待执行的应用级参数改名：(原名, 新名)，面板在画完之后调用工作区。
    pub graph_parameter_rename: Option<(String, String)>,
    /// Notify 编辑：(剪辑, 下标, 文本)，失焦写回。
    pub notify_name: Option<(String, usize, String)>,
    /// 新增 Notify 草稿：(剪辑, time, 名字)。
    pub notify_draft: Option<(String, f32, String)>,
    /// 正在拖拽的 Notify 标记：(剪辑, 下标)。
    pub notify_drag: Option<(String, usize)>,
    /// 正在拖拽的同步标记：(剪辑, 下标)。
    pub sync_marker_drag: Option<(String, usize)>,
    /// 新增同步标记草稿：(剪辑, 相位)。
    pub sync_marker_draft: Option<(String, f32)>,
    /// 正在编辑的 Montage 段名：(剪辑, 下标, 文本)，失焦才写回。
    pub montage_section_name: Option<(String, usize, String)>,
    /// 新增 Montage 段草稿：(剪辑, 名字, 起点, 终点)。
    pub montage_new_section: Option<(String, String, f32, f32)>,
    /// 正在编辑的 warp window 名字：(剪辑, 下标, 文本)，失焦才写回。
    pub montage_warp_name: Option<(String, usize, String)>,
    /// 新增 warp window 草稿：(剪辑, 名字, 起点, 终点)。
    pub montage_new_warp: Option<(String, String, f32, f32)>,
    /// 正在拖拽的二维混合空间样本：(节点, 下标)。
    pub graph_sample_drag: Option<(String, usize)>,
}

/// 「Add node」下拉框里能用缺省字段构成合法形状的节点类型。
const GRAPH_NODE_KINDS: [(&str, &str); 6] = [
    ("clip_player", "clip"),
    ("blend_stack", "blend stack"),
    ("slot", "slot"),
    ("layered_blend", "layered blend"),
    ("cached_pose", "cached pose"),
    ("state_machine", "state machine"),
];

fn graph_node_kind_label(kind: &str) -> &str {
    GRAPH_NODE_KINDS
        .iter()
        .find(|(value, _)| *value == kind)
        .map(|(_, label)| *label)
        .unwrap_or(kind)
}

fn is_clip_player(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "clip" | "clip_player" | "clipplayer"
    )
}

fn is_blend_space(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "blend_space" | "blend_space_1d" | "blendspace1d" | "blend_space_2d" | "blendspace2d"
    )
}

fn is_blend_space_2d(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "blend_space_2d" | "blendspace2d"
    )
}

fn draw_anim_graph_view(
    ui: &mut Ui,
    graph: &prism_editor_core::AnimGraphView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
) -> Result<bool, String> {
    let mut changed = false;
    ui.label(
        RichText::new(format!("Parameters ({})", graph.parameters.len()))
            .strong()
            .size(11.0),
    );
    if graph.parameters.is_empty() {
        ui.label(
            RichText::new("no parameters — add one below")
                .weak()
                .size(10.0),
        );
    }
    let mut parameter_default_edits = Vec::new();
    let mut parameter_removals = Vec::new();
    for parameter in &graph.parameters {
        ui.horizontal(|ui| {
            let editing_name = edit
                .graph_parameter_name
                .as_ref()
                .filter(|(owner, _)| owner == &parameter.name);
            let mut name_text = editing_name
                .map(|(_, text)| text.clone())
                .unwrap_or_else(|| parameter.name.clone());
            let name_response = ui.add(
                egui::TextEdit::singleline(&mut name_text)
                    .desired_width(110.0)
                    .font(egui::TextStyle::Monospace),
            );
            if name_response.changed() {
                edit.graph_parameter_name = Some((parameter.name.clone(), name_text));
            } else if name_response.lost_focus() {
                if let Some((owner, text)) = edit.graph_parameter_name.take() {
                    if owner == parameter.name {
                        let new_name = text.trim().to_string();
                        if new_name != parameter.name {
                            edit.graph_parameter_rename = Some((parameter.name.clone(), new_name));
                        }
                    }
                }
            }
            ui.label(
                RichText::new(format!("· {}", parameter.kind))
                    .monospace()
                    .size(10.0),
            );
            let bool_like = !parameter.kind.eq_ignore_ascii_case("float");
            let mut value = parameter.default;
            let drag = egui::DragValue::new(&mut value).speed(0.01);
            let drag = if bool_like {
                drag.range(0.0..=1.0).max_decimals(0)
            } else {
                drag.range(-1000.0..=1000.0)
            };
            if ui.add(drag).changed() {
                parameter_default_edits.push((parameter.name.clone(), value));
            }
            if ui
                .small_button("×")
                .on_hover_text("remove parameter")
                .clicked()
            {
                parameter_removals.push(parameter.name.clone());
            }
        });
    }
    let mut add_parameter = None;
    let mut cancel_parameter = false;
    if let Some(draft) = edit.graph_parameter.as_mut() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.0)
                    .hint_text("name")
                    .desired_width(110.0),
            );
            egui::ComboBox::from_id_salt("anim-graph-param-kind")
                .selected_text(&draft.1)
                .show_ui(ui, |ui| {
                    for kind in ["float", "bool", "trigger"] {
                        if ui.selectable_label(draft.1 == kind, kind).clicked() {
                            draft.1 = kind.to_string();
                        }
                    }
                });
            let bool_like = draft.1 != "float";
            let drag = egui::DragValue::new(&mut draft.2).speed(0.01);
            let drag = if bool_like {
                drag.range(0.0..=1.0).max_decimals(0)
            } else {
                drag.range(-1000.0..=1000.0)
            };
            ui.add(drag);
        });
        ui.horizontal(|ui| {
            if ui.button("Add parameter").clicked() {
                add_parameter = Some(draft.clone());
            }
            if ui.button("Cancel").clicked() {
                cancel_parameter = true;
            }
        });
    } else if ui.button("+ Parameter").clicked() {
        edit.graph_parameter = Some((String::new(), "float".to_string(), 0.0));
    }
    for (name, value) in parameter_default_edits {
        prism_editor_core::set_parameter_default(document, &name, value)?;
        changed = true;
    }
    for name in parameter_removals {
        prism_editor_core::remove_parameter(document, &name)?;
        changed = true;
    }
    if let Some((name, kind, default)) = add_parameter {
        prism_editor_core::add_parameter(document, &name, &kind, default)?;
        edit.graph_parameter = None;
        changed = true;
    }
    if cancel_parameter {
        edit.graph_parameter = None;
    }
    ui.separator();

    ui.label(
        RichText::new(format!("Nodes ({})", graph.nodes.len()))
            .strong()
            .size(11.0),
    );
    let mut remove_node = None;
    let mut duplicate_node = None;
    let mut add_link = None;
    let mut remove_link = None;
    let mut weight_edits = Vec::new();
    let mut mask_edits = Vec::new();
    let mut node_clip_edits = Vec::new();
    let mut node_speed_edits = Vec::new();
    let mut node_weight_edits = Vec::new();
    let mut node_loop_edits = Vec::new();
    let mut node_name_edits = Vec::new();
    let mut parameter_edits = Vec::new();
    let mut position_edits = Vec::new();
    let mut sample_speed_edits = Vec::new();
    let mut sample_removals = Vec::new();
    let mut sample_adds = Vec::new();
    let mut sample_draft_cancel = false;
    for node in &graph.nodes {
        let title = if node.is_output() {
            format!("{} · {} (output)", node.name, node.kind)
        } else {
            format!("{} · {}", node.name, node.kind)
        };
        egui::CollapsingHeader::new(RichText::new(title).monospace().size(10.0))
            .id_salt(("anim-graph-node", &node.name))
            .show(ui, |ui| {
                if !node.is_output() {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("⧉ Duplicate")
                            .on_hover_text("copy this node with a unique name")
                            .clicked()
                        {
                            duplicate_node = Some(node.name.clone());
                        }
                        if ui.small_button("× Remove node").clicked() {
                            remove_node = Some(node.name.clone());
                        }
                    });
                }
                let editing_name = edit
                    .graph_node_name
                    .as_ref()
                    .filter(|(owner, _)| owner == &node.name);
                let mut name_text = editing_name
                    .map(|(_, text)| text.clone())
                    .unwrap_or_else(|| node.name.clone());
                let name_response = ui.add(
                    egui::TextEdit::singleline(&mut name_text)
                        .hint_text("node name")
                        .desired_width(140.0),
                );
                if name_response.changed() {
                    edit.graph_node_name = Some((node.name.clone(), name_text));
                } else if name_response.lost_focus() {
                    if let Some((owner, text)) = edit.graph_node_name.take() {
                        if owner == node.name {
                            node_name_edits.push((owner, text));
                        }
                    }
                }
                if !node.parameter.is_empty() {
                    ui.label(
                        RichText::new(format!("parameter: {}", node.parameter))
                            .weak()
                            .size(10.0),
                    );
                }
                if node.state_machine.is_empty() && node.kind.eq_ignore_ascii_case("state_machine")
                {
                    ui.label(
                        RichText::new("state machine path is empty")
                            .weak()
                            .size(10.0),
                    );
                }
                if is_clip_player(&node.kind) {
                    let editing = edit
                        .graph_node_clip
                        .as_ref()
                        .filter(|(owner, _)| owner == &node.name);
                    let mut text = editing
                        .map(|(_, text)| text.clone())
                        .unwrap_or_else(|| node.clips.first().cloned().unwrap_or_default());
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .hint_text("clip.ranim")
                            .desired_width(180.0),
                    );
                    if response.changed() {
                        edit.graph_node_clip = Some((node.name.clone(), text));
                    } else if response.lost_focus() {
                        if let Some((owner, text)) = edit.graph_node_clip.take() {
                            if owner == node.name {
                                node_clip_edits.push((node.name.clone(), text));
                            }
                        }
                    }
                } else if !node.clips.is_empty() {
                    ui.label(
                        RichText::new(format!("clips: {}", node.clips.join(", ")))
                            .weak()
                            .size(10.0),
                    );
                }
                ui.horizontal(|ui| {
                    let mut speed = node.speed;
                    if ui
                        .add(
                            egui::DragValue::new(&mut speed)
                                .speed(0.01)
                                .range(0.0..=5.0)
                                .prefix("speed "),
                        )
                        .changed()
                    {
                        node_speed_edits.push((node.name.clone(), speed));
                    }
                    let mut weight = node.weight;
                    if ui
                        .add(
                            egui::DragValue::new(&mut weight)
                                .speed(0.01)
                                .range(0.0..=1.0)
                                .prefix("w "),
                        )
                        .changed()
                    {
                        node_weight_edits.push((node.name.clone(), weight));
                    }
                    let mut looped = node.looped;
                    if ui.checkbox(&mut looped, "loop").changed() {
                        node_loop_edits.push((node.name.clone(), looped));
                    }
                });
                if is_blend_space(&node.kind) {
                    let two_d = is_blend_space_2d(&node.kind);
                    ui.horizontal(|ui| {
                        let editing_x = edit
                            .graph_node_parameter
                            .as_ref()
                            .filter(|(owner, _)| owner == &node.name);
                        let mut x = editing_x
                            .map(|(_, text)| text.clone())
                            .unwrap_or_else(|| node.parameter.clone());
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut x)
                                .hint_text("parameter")
                                .desired_width(100.0),
                        );
                        if response.changed() {
                            edit.graph_node_parameter = Some((node.name.clone(), x));
                        } else if response.lost_focus() {
                            if let Some((owner, text)) = edit.graph_node_parameter.take() {
                                if owner == node.name {
                                    parameter_edits.push((owner, text, node.parameter_y.clone()));
                                }
                            }
                        }
                        if two_d {
                            let editing_y = edit
                                .graph_node_parameter_y
                                .as_ref()
                                .filter(|(owner, _)| owner == &node.name);
                            let mut y = editing_y
                                .map(|(_, text)| text.clone())
                                .unwrap_or_else(|| node.parameter_y.clone());
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut y)
                                    .hint_text("parameter y")
                                    .desired_width(100.0),
                            );
                            if response.changed() {
                                edit.graph_node_parameter_y = Some((node.name.clone(), y));
                            } else if response.lost_focus() {
                                if let Some((owner, text)) = edit.graph_node_parameter_y.take() {
                                    if owner == node.name {
                                        parameter_edits.push((owner, node.parameter.clone(), text));
                                    }
                                }
                            }
                        }
                    });
                    for (index, clip) in node.clips.iter().enumerate() {
                        let position = node.positions.get(index).copied().unwrap_or([0.0, 0.0]);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("#{index} {clip}"))
                                    .monospace()
                                    .size(10.0),
                            );
                            let mut x = position[0];
                            if ui
                                .add(egui::DragValue::new(&mut x).speed(0.01).prefix("x "))
                                .changed()
                            {
                                position_edits.push((node.name.clone(), index, [x, position[1]]));
                            }
                            if two_d {
                                let mut y = position[1];
                                if ui
                                    .add(egui::DragValue::new(&mut y).speed(0.01).prefix("y "))
                                    .changed()
                                {
                                    position_edits.push((
                                        node.name.clone(),
                                        index,
                                        [position[0], y],
                                    ));
                                }
                            }
                            let mut speed = node.speeds.get(index).copied().unwrap_or(1.0);
                            if ui
                                .add(
                                    egui::DragValue::new(&mut speed)
                                        .speed(0.01)
                                        .range(0.0..=10.0)
                                        .prefix("speed "),
                                )
                                .changed()
                            {
                                sample_speed_edits.push((node.name.clone(), index, speed));
                            }
                            if ui.small_button("×").clicked() {
                                sample_removals.push((node.name.clone(), index));
                            }
                        });
                    }
                    if two_d
                        && node.positions.len() >= 2
                        && node.positions.iter().all(|position| {
                            position[0].is_finite() && position[1].is_finite()
                        })
                    {
                        let positions = &node.positions;
                        let mut min_x = positions.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min);
                        let mut max_x = positions
                            .iter()
                            .map(|p| p[0])
                            .fold(f32::NEG_INFINITY, f32::max);
                        let mut min_y = positions.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
                        let mut max_y = positions
                            .iter()
                            .map(|p| p[1])
                            .fold(f32::NEG_INFINITY, f32::max);
                        if (max_x - min_x).abs() < 1e-3 {
                            min_x -= 0.5;
                            max_x += 0.5;
                        }
                        if (max_y - min_y).abs() < 1e-3 {
                            min_y -= 0.5;
                            max_y += 0.5;
                        }
                        let pad_x = (max_x - min_x) * 0.15;
                        let pad_y = (max_y - min_y) * 0.15;
                        min_x -= pad_x;
                        max_x += pad_x;
                        min_y -= pad_y;
                        max_y += pad_y;

                        let (canvas, painter) = ui.allocate_painter(
                            Vec2::new(180.0, 120.0),
                            egui::Sense::click_and_drag(),
                        );
                        let rect = canvas.rect;
                        painter.rect_filled(rect, 3.0, Color32::from_rgb(28, 30, 34));
                        let left = rect.left() + 8.0;
                        let width = rect.width() - 16.0;
                        let bottom = rect.bottom() - 8.0;
                        let height = rect.height() - 16.0;
                        let sx = |x: f32| {
                            left + ((x - min_x) / (max_x - min_x)).clamp(0.0, 1.0) * width
                        };
                        let sy = |y: f32| {
                            bottom - ((y - min_y) / (max_y - min_y)).clamp(0.0, 1.0) * height
                        };
                        let to_x = |px: f32| {
                            min_x + ((px - left) / width).clamp(0.0, 1.0) * (max_x - min_x)
                        };
                        let to_y = |py: f32| {
                            min_y + ((bottom - py) / height).clamp(0.0, 1.0) * (max_y - min_y)
                        };

                        if canvas.drag_started() {
                            edit.graph_sample_drag = canvas
                                .interact_pointer_pos()
                                .and_then(|pos| {
                                    positions
                                        .iter()
                                        .enumerate()
                                        .map(|(index, position)| {
                                            (
                                                index,
                                                (sx(position[0]) - pos.x)
                                                    .hypot(sy(position[1]) - pos.y),
                                            )
                                        })
                                        .filter(|(_, distance)| *distance <= 10.0)
                                        .min_by(|a, b| a.1.total_cmp(&b.1))
                                        .map(|(index, _)| (node.name.clone(), index))
                                });
                        }
                        if canvas.dragged() {
                            if let (Some(pos), Some((owner, index))) = (
                                canvas.interact_pointer_pos(),
                                edit.graph_sample_drag.as_ref(),
                            ) {
                                if owner == &node.name && *index < positions.len() {
                                    position_edits.push((
                                        node.name.clone(),
                                        *index,
                                        [to_x(pos.x), to_y(pos.y)],
                                    ));
                                }
                            }
                        }
                        if canvas.drag_stopped() {
                            edit.graph_sample_drag = None;
                        }
                        for (index, position) in positions.iter().enumerate() {
                            let active = edit.graph_sample_drag.as_ref().is_some_and(
                                |(owner, slot)| owner == &node.name && *slot == index,
                            );
                            painter.circle_filled(
                                egui::pos2(sx(position[0]), sy(position[1])),
                                if active { 5.0 } else { 3.5 },
                                Color32::from_rgb(120, 170, 230),
                            );
                        }
                        if prism_editor_core::blend_space_2d_degenerate(positions) {
                            ui.label(
                                RichText::new(
                                    "degenerate: samples are collinear — runtime falls back to 1D/clip",
                                )
                                .color(Color32::from_rgb(232, 160, 96))
                                .size(9.0),
                            );
                        }
                        ui.label(
                            RichText::new(format!(
                                "x {min_x:.2}..{max_x:.2} · y {min_y:.2}..{max_y:.2} — drag a point"
                            ))
                            .weak()
                            .size(9.0),
                        );
                    }
                    let draft_here = edit
                        .graph_sample
                        .as_ref()
                        .is_some_and(|(owner, ..)| owner == &node.name);
                    if draft_here {
                        if let Some(draft) = edit.graph_sample.as_mut() {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.1)
                                        .hint_text("clip.ranim")
                                        .desired_width(120.0),
                                );
                                ui.add(egui::DragValue::new(&mut draft.2).speed(0.01).prefix("x "));
                                if two_d {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.3).speed(0.01).prefix("y "),
                                    );
                                }
                                if ui.small_button("Add").clicked() {
                                    sample_adds.push(draft.clone());
                                }
                                if ui.small_button("Cancel").clicked() {
                                    sample_draft_cancel = true;
                                }
                            });
                        }
                    } else if ui.small_button("+ Sample").clicked() {
                        edit.graph_sample = Some((node.name.clone(), String::new(), 0.0, 0.0));
                    }
                    if node.clips.len() != node.positions.len() {
                        ui.label(
                            RichText::new(format!(
                                "{} clips vs {} positions — cooker rejects this",
                                node.clips.len(),
                                node.positions.len()
                            ))
                            .color(Color32::from_rgb(232, 160, 96))
                            .size(10.0),
                        );
                    }
                    if !node.speeds.is_empty() && node.speeds.len() != node.clips.len() {
                        ui.label(
                            RichText::new(format!(
                                "{} clips vs {} sample speeds — cooker rejects this",
                                node.clips.len(),
                                node.speeds.len()
                            ))
                            .color(Color32::from_rgb(232, 160, 96))
                            .size(10.0),
                        );
                    }
                }
                if !node.state_machine.is_empty() {
                    ui.label(
                        RichText::new(format!("state machine: {}", node.state_machine))
                            .weak()
                            .size(10.0),
                    );
                }
                if !node.parameter.is_empty() {
                    ui.label(
                        RichText::new(format!("parameter: {}", node.parameter))
                            .weak()
                            .size(10.0),
                    );
                }
                for input in &node.inputs {
                    ui.label(
                        RichText::new(format!("← {}  × {}", input.node, input.weight))
                            .monospace()
                            .size(10.0),
                    );
                }

                // 连线编辑：只对使用 `inputs` 数组的节点开放；链式单输入节点提示手改 JSON。
                let raw = document
                    .get_mut("nodes")
                    .and_then(Value::as_array_mut)
                    .and_then(|nodes| {
                        nodes.iter_mut().find(|value| {
                            value.get("name").and_then(Value::as_str) == Some(&node.name)
                        })
                    });
                if let Some(raw) = raw {
                    if let Some(inputs) = raw.get("inputs").and_then(Value::as_array) {
                        let mut remove = None;
                        for (index, input) in inputs.iter().enumerate() {
                            let (name, mut weight, mask) = match input {
                                Value::String(name) => (name.clone(), 1.0, Vec::new()),
                                Value::Object(map) => (
                                    map.get("node")
                                        .and_then(Value::as_str)
                                        .unwrap_or("?")
                                        .to_string(),
                                    map.get("weight").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                                    map.get("bone_mask")
                                        .and_then(Value::as_array)
                                        .map(Vec::as_slice)
                                        .unwrap_or_default()
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect::<Vec<_>>(),
                                ),
                                _ => continue,
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("link #{index}: {name}")).size(10.0),
                                );
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut weight)
                                            .speed(0.01)
                                            .range(0.0..=1.0)
                                            .prefix("w "),
                                    )
                                    .changed()
                                {
                                    weight_edits.push((node.name.clone(), index, weight));
                                }
                                if ui.small_button("×").clicked() {
                                    remove = Some(index);
                                }
                            });

                            // 骨骼掩码：逗号分隔的关节名；空串清空 = 整条姿态。
                            let editing_mask =
                                edit.mask.as_ref().is_some_and(|(owner, slot, _)| {
                                    owner == &node.name && *slot == index
                                });
                            let mut text = if editing_mask {
                                edit.mask
                                    .as_ref()
                                    .map(|(_, _, text)| text.clone())
                                    .unwrap_or_default()
                            } else {
                                mask.join(", ")
                            };
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut text)
                                    .hint_text("bone mask: spine_02, arm_l")
                                    .desired_width(220.0),
                            );
                            if response.changed() {
                                edit.mask = Some((node.name.clone(), index, text));
                            } else if response.lost_focus() {
                                if let Some((_, _, text)) = edit.mask.take() {
                                    let joints = text
                                        .split(',')
                                        .map(|joint| joint.trim().to_string())
                                        .filter(|joint| !joint.is_empty())
                                        .collect::<Vec<_>>();
                                    mask_edits.push((node.name.clone(), index, joints));
                                }
                            }
                        }
                        if let Some(index) = remove {
                            remove_link = Some((node.name.clone(), index));
                        }
                        ui.menu_button("+ Link", |ui| {
                            for other in &graph.nodes {
                                if other.name == node.name
                                    || node.inputs.iter().any(|input| input.node == other.name)
                                {
                                    continue;
                                }
                                if ui.button(&other.name).clicked() {
                                    add_link = Some((node.name.clone(), other.name.clone()));
                                    ui.close();
                                }
                            }
                        });
                    } else if raw.get("input").is_some() {
                        ui.label(
                            RichText::new("single input — edit JSON to rewire")
                                .weak()
                                .size(10.0),
                        );
                    }
                }
            });
    }
    if let Some(node) = duplicate_node {
        prism_editor_core::duplicate_graph_node(document, &node)?;
        changed = true;
    }
    if let Some(node) = remove_node {
        prism_editor_core::remove_graph_node(document, &node)?;
        changed = true;
    }
    for (old, new) in node_name_edits {
        prism_editor_core::rename_graph_node(document, &old, &new)?;
        changed = true;
    }
    for (node, x, y) in parameter_edits {
        prism_editor_core::set_blend_space_parameter(document, &node, &x, &y)?;
        changed = true;
    }
    for (node, index, position) in position_edits {
        prism_editor_core::set_blend_space_position(document, &node, index, position)?;
        changed = true;
    }
    for (node, index, speed) in sample_speed_edits {
        prism_editor_core::set_blend_space_sample_speed(document, &node, index, speed)?;
        changed = true;
    }
    for (node, index) in sample_removals {
        prism_editor_core::remove_blend_space_sample(document, &node, index)?;
        changed = true;
    }
    for (node, clip, x, y) in sample_adds {
        prism_editor_core::add_blend_space_sample(document, &node, &clip, [x, y])?;
        edit.graph_sample = None;
        changed = true;
    }
    if sample_draft_cancel {
        edit.graph_sample = None;
    }
    for (node, clip) in node_clip_edits {
        prism_editor_core::set_graph_node_clip(document, &node, &clip)?;
        changed = true;
    }
    for (node, speed) in node_speed_edits {
        prism_editor_core::set_graph_node_speed(document, &node, speed)?;
        changed = true;
    }
    for (node, weight) in node_weight_edits {
        prism_editor_core::set_graph_node_weight(document, &node, weight)?;
        changed = true;
    }
    for (node, looped) in node_loop_edits {
        prism_editor_core::set_graph_node_looped(document, &node, looped)?;
        changed = true;
    }
    if let Some((target, source)) = add_link {
        prism_editor_core::add_graph_input(document, &target, &source)?;
        changed = true;
    }
    for (target, index, weight) in weight_edits {
        prism_editor_core::set_graph_input_weight(document, &target, index, weight)?;
        changed = true;
    }
    for (target, index, joints) in mask_edits {
        prism_editor_core::set_graph_input_bone_mask(document, &target, index, &joints)?;
        changed = true;
    }
    if let Some((target, index)) = remove_link {
        prism_editor_core::remove_graph_input(document, &target, index)?;
        changed = true;
    }

    ui.separator();
    ui.label(RichText::new("Add node").strong().size(11.0));
    if edit.new_node_kind.is_empty() {
        edit.new_node_kind = "clip_player".to_string();
    }
    let source_hint = match edit.new_node_kind.as_str() {
        "clip_player" => "clip.ranim",
        "state_machine" => "anim/hero.rasm",
        _ => "input node",
    };
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut edit.new_node.0)
                .hint_text("node name")
                .desired_width(96.0),
        );
        egui::ComboBox::from_id_salt("anim-graph-new-node-kind")
            .selected_text(graph_node_kind_label(&edit.new_node_kind))
            .show_ui(ui, |ui| {
                for (kind, label) in GRAPH_NODE_KINDS {
                    ui.selectable_value(&mut edit.new_node_kind, kind.to_string(), label);
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut edit.new_node.1)
                .hint_text(source_hint)
                .desired_width(150.0),
        );
    });
    if ui.button("+ Node").clicked() {
        prism_editor_core::add_graph_node(
            document,
            &edit.new_node.0,
            &edit.new_node_kind,
            &edit.new_node.1,
        )?;
        edit.new_node.0.clear();
        edit.new_node.1.clear();
        changed = true;
    }

    if let Some(output) = &graph.output {
        ui.separator();
        ui.label(
            RichText::new(format!("Output: {output}"))
                .strong()
                .size(11.0),
        );
    }
    Ok(changed)
}

/// 状态机条件草稿：`+ Condition` 先选类型、填参数，点 Add 才写文档。
#[derive(Debug, Clone)]
pub struct StateConditionDraft {
    pub transition: usize,
    pub kind: String,
    pub name: String,
    pub threshold: f32,
    pub value: bool,
}

/// 面板里可添加的条件类型（与 cooker / 运行时保持一致）。
const STATE_CONDITION_KINDS: [(&str, &str); 4] = [
    ("float_greater", "float >"),
    ("float_less", "float <"),
    ("bool_is", "bool =="),
    ("trigger", "trigger"),
];

fn condition_kind_label(kind: &str) -> &str {
    STATE_CONDITION_KINDS
        .iter()
        .find(|(value, _)| *value == kind)
        .map(|(_, label)| *label)
        .unwrap_or(kind)
}

fn draw_state_machine_view(
    ui: &mut Ui,
    machine: &prism_editor_core::AnimStateMachineView,
    document: &mut Value,
    edit: &mut AnimGraphEditState,
    draft_slot: &mut Option<StateConditionDraft>,
) -> Result<bool, String> {
    let mut changed = false;
    ui.label(
        RichText::new(format!("States ({})", machine.states.len()))
            .strong()
            .size(11.0),
    );
    ui.label(
        RichText::new("first state is the initial state — ↑/↓ reorder")
            .weak()
            .size(9.0),
    );
    let mut move_state = None;
    let mut state_rename = None;
    let mut clip_edits = Vec::new();
    let mut speed_edits = Vec::new();
    let mut loop_edits = Vec::new();
    let mut duplicate_states = Vec::new();
    let mut remove_states = Vec::new();
    let mut add_state_draft = None;
    let mut cancel_new_state = false;
    for (index, state) in machine.states.iter().enumerate() {
        ui.horizontal(|ui| {
            let mut name_text = edit
                .sm_state_name
                .as_ref()
                .filter(|(slot, _)| *slot == index)
                .map(|(_, text)| text.clone())
                .unwrap_or_else(|| state.name.clone());
            let name_response = ui.add(
                egui::TextEdit::singleline(&mut name_text)
                    .hint_text("state")
                    .desired_width(80.0),
            );
            if name_response.changed() {
                edit.sm_state_name = Some((index, name_text));
            } else if name_response.lost_focus() {
                if let Some((slot, text)) = edit.sm_state_name.take() {
                    if slot == index {
                        state_rename = Some((index, text));
                    }
                }
            }

            let mut clip_text = edit
                .sm_state_clip
                .as_ref()
                .filter(|(slot, _)| *slot == index)
                .map(|(_, text)| text.clone())
                .unwrap_or_else(|| state.clip.clone());
            let clip_response = ui.add(
                egui::TextEdit::singleline(&mut clip_text)
                    .hint_text("clip.ranim")
                    .desired_width(130.0),
            );
            if clip_response.changed() {
                edit.sm_state_clip = Some((index, clip_text));
            } else if clip_response.lost_focus() {
                if let Some((slot, text)) = edit.sm_state_clip.take() {
                    if slot == index {
                        clip_edits.push((index, text));
                    }
                }
            }

            let mut speed = state.speed;
            if ui
                .add(
                    egui::DragValue::new(&mut speed)
                        .speed(0.01)
                        .range(0.0..=5.0)
                        .prefix("speed "),
                )
                .changed()
            {
                speed_edits.push((index, speed));
            }
            let mut looped = state.looped;
            if ui.checkbox(&mut looped, "loop").changed() {
                loop_edits.push((index, looped));
            }
            let last = machine.states.len().saturating_sub(1);
            ui.add_enabled_ui(index > 0, |ui| {
                if ui
                    .small_button("↑")
                    .on_hover_text("move earlier (closer to the initial state)")
                    .clicked()
                {
                    move_state = Some((index, -1));
                }
            });
            ui.add_enabled_ui(index < last, |ui| {
                if ui.small_button("↓").on_hover_text("move later").clicked() {
                    move_state = Some((index, 1));
                }
            });
            if ui
                .small_button("⧉")
                .on_hover_text("duplicate state")
                .clicked()
            {
                duplicate_states.push(state.name.clone());
            }
            ui.add_enabled_ui(machine.states.len() > 1, |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text("remove state and its transitions")
                    .clicked()
                {
                    remove_states.push(state.name.clone());
                }
            });
        });
    }
    if let Some((index, offset)) = move_state {
        prism_editor_core::move_state(document, index, offset)?;
        changed = true;
    }
    if state_rename.is_some() {
        edit.sm_state_rename = state_rename;
    }
    for (index, clip) in clip_edits {
        prism_editor_core::set_state_clip(document, index, &clip)?;
        changed = true;
    }
    for (index, speed) in speed_edits {
        prism_editor_core::set_state_speed(document, index, speed)?;
        changed = true;
    }
    for (index, looped) in loop_edits {
        prism_editor_core::set_state_looped(document, index, looped)?;
        changed = true;
    }
    for name in duplicate_states {
        prism_editor_core::duplicate_state(document, &name)?;
        changed = true;
    }
    for name in remove_states {
        prism_editor_core::remove_state(document, &name)?;
        changed = true;
    }

    if let Some(draft) = edit.sm_new_state.as_mut() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.0)
                    .hint_text("state")
                    .desired_width(80.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut draft.1)
                    .hint_text("clip.ranim")
                    .desired_width(130.0),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Add state").clicked() {
                add_state_draft = Some(draft.clone());
            }
            if ui.button("Cancel").clicked() {
                cancel_new_state = true;
            }
        });
    } else if ui.button("+ State").clicked() {
        edit.sm_new_state = Some((String::new(), String::new()));
    }
    if let Some((name, clip)) = add_state_draft {
        prism_editor_core::add_state(document, &name, &clip)?;
        edit.sm_new_state = None;
        changed = true;
    }
    if cancel_new_state {
        edit.sm_new_state = None;
    }

    ui.separator();
    ui.label(
        RichText::new(format!("Transitions ({})", machine.transitions.len()))
            .strong()
            .size(11.0),
    );
    ui.label(
        RichText::new("first match wins — ↑/↓ change priority")
            .weak()
            .size(9.0),
    );
    let mut move_transition = None;
    let mut duplicate_transition = None;
    let mut remove = None;
    let mut timing_edits = Vec::new();
    let mut kind_edits = Vec::new();
    let mut condition_name_edits = Vec::new();
    let mut threshold_edits = Vec::new();
    let mut value_edits = Vec::new();
    let mut remove_condition = None;
    let mut add_condition = None;
    let mut cancel_draft = false;
    let mut add_transition = None;
    let mut retargets = Vec::new();
    for (index, transition) in machine.transitions.iter().enumerate() {
        let from = transition.from.as_deref().unwrap_or("*");
        let title = format!(
            "{} → {}  ({}s, min {:.2})",
            from, transition.to, transition.duration, transition.min_state_fraction
        );
        egui::CollapsingHeader::new(RichText::new(title).monospace().size(10.0))
            .id_salt(("anim-sm-transition", from, &transition.to))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let last = machine.transitions.len().saturating_sub(1);
                    ui.add_enabled_ui(index > 0, |ui| {
                        if ui
                            .small_button("↑")
                            .on_hover_text("move earlier (higher priority)")
                            .clicked()
                        {
                            move_transition = Some((index, -1));
                        }
                    });
                    ui.add_enabled_ui(index < last, |ui| {
                        if ui
                            .small_button("↓")
                            .on_hover_text("move later (lower priority)")
                            .clicked()
                        {
                            move_transition = Some((index, 1));
                        }
                    });
                    if ui
                        .small_button("⧉ Duplicate")
                        .on_hover_text("copy this transition and its conditions")
                        .clicked()
                    {
                        duplicate_transition = Some(index);
                    }
                    if ui.small_button("× Remove transition").clicked() {
                        remove = Some(index);
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("from");
                    egui::ComboBox::from_id_salt(("anim-sm-from", index))
                        .selected_text(from)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(transition.from.is_none(), "any")
                                .clicked()
                            {
                                retargets.push((index, None, transition.to.clone()));
                                ui.close();
                            }
                            for state in &machine.states {
                                let selected =
                                    transition.from.as_deref() == Some(state.name.as_str());
                                if ui.selectable_label(selected, &state.name).clicked() {
                                    retargets.push((
                                        index,
                                        Some(state.name.clone()),
                                        transition.to.clone(),
                                    ));
                                    ui.close();
                                }
                            }
                        });
                    ui.label("to");
                    egui::ComboBox::from_id_salt(("anim-sm-to", index))
                        .selected_text(&transition.to)
                        .show_ui(ui, |ui| {
                            for state in &machine.states {
                                let selected = transition.to == state.name;
                                if ui.selectable_label(selected, &state.name).clicked() {
                                    retargets.push((
                                        index,
                                        transition.from.clone(),
                                        state.name.clone(),
                                    ));
                                    ui.close();
                                }
                            }
                        });
                });

                let mut duration = transition.duration;
                let mut fraction = transition.min_state_fraction;
                let mut timing_changed = ui
                    .add(
                        egui::DragValue::new(&mut duration)
                            .speed(0.01)
                            .range(0.0..=5.0)
                            .prefix("blend "),
                    )
                    .changed();
                timing_changed |= ui
                    .add(
                        egui::DragValue::new(&mut fraction)
                            .speed(0.01)
                            .range(0.0..=1.0)
                            .prefix("min "),
                    )
                    .changed();
                if timing_changed {
                    timing_edits.push((index, duration, fraction));
                }

                if transition.conditions.is_empty() {
                    ui.label(RichText::new("no conditions").weak().size(10.0));
                }
                for (condition_index, condition) in transition.conditions.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut kind = condition.kind.clone();
                        egui::ComboBox::from_id_salt((
                            "anim-sm-condition-kind-edit",
                            index,
                            condition_index,
                        ))
                        .selected_text(condition_kind_label(&condition.kind))
                        .show_ui(ui, |ui| {
                            for (value, label) in STATE_CONDITION_KINDS {
                                if ui
                                    .selectable_label(condition.kind.as_str() == value, label)
                                    .clicked()
                                {
                                    kind = value.to_string();
                                    ui.close();
                                }
                            }
                        });
                        if kind != condition.kind {
                            kind_edits.push((index, condition_index, kind));
                        }
                        let mut name_text = edit
                            .sm_condition_name
                            .as_ref()
                            .filter(|(transition, slot, _)| {
                                *transition == index && *slot == condition_index
                            })
                            .map(|(_, _, text)| text.clone())
                            .unwrap_or_else(|| condition.name.clone());
                        let name_response = ui.add(
                            egui::TextEdit::singleline(&mut name_text)
                                .hint_text("parameter")
                                .desired_width(90.0),
                        );
                        if name_response.changed() {
                            edit.sm_condition_name = Some((index, condition_index, name_text));
                        } else if name_response.lost_focus() {
                            if let Some((transition, slot, text)) = edit.sm_condition_name.take() {
                                if transition == index && slot == condition_index {
                                    condition_name_edits.push((index, condition_index, text));
                                }
                            }
                        }
                        match condition.kind.as_str() {
                            "float_greater" | "float_less" => {
                                let mut threshold = condition.threshold;
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut threshold)
                                            .speed(0.01)
                                            .range(-1000.0..=1000.0),
                                    )
                                    .changed()
                                {
                                    threshold_edits.push((index, condition_index, threshold));
                                }
                            }
                            "bool_is" => {
                                let mut value = condition.value;
                                if ui.checkbox(&mut value, "").changed() {
                                    value_edits.push((index, condition_index, value));
                                }
                            }
                            _ => {}
                        }
                        if ui.small_button("×").clicked() {
                            remove_condition = Some((index, condition_index));
                        }
                    });
                }

                if ui.small_button("+ Condition").clicked() {
                    *draft_slot = Some(StateConditionDraft {
                        transition: index,
                        kind: "float_greater".to_string(),
                        name: String::new(),
                        threshold: 0.0,
                        value: false,
                    });
                }
                if let Some(draft) = draft_slot
                    .as_mut()
                    .filter(|draft| draft.transition == index)
                {
                    ui.separator();
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt(("anim-sm-condition-kind", index))
                            .selected_text(condition_kind_label(&draft.kind))
                            .show_ui(ui, |ui| {
                                for (kind, label) in STATE_CONDITION_KINDS {
                                    ui.selectable_value(&mut draft.kind, kind.to_string(), label);
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.name)
                                .hint_text("parameter")
                                .desired_width(110.0),
                        );
                        if draft.kind.starts_with("float") {
                            ui.add(
                                egui::DragValue::new(&mut draft.threshold)
                                    .speed(0.01)
                                    .range(-1000.0..=1000.0)
                                    .prefix("threshold "),
                            );
                        } else if draft.kind == "bool_is" {
                            ui.checkbox(&mut draft.value, "value");
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Add condition").clicked() {
                            add_condition = Some(draft.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_draft = true;
                        }
                    });
                }
            });
    }
    if let Some((index, offset)) = move_transition {
        prism_editor_core::move_state_transition(document, index, offset)?;
        changed = true;
    }
    if let Some(index) = duplicate_transition {
        prism_editor_core::duplicate_state_transition(document, index)?;
        changed = true;
    }
    for (index, from, to) in retargets {
        prism_editor_core::set_transition_states(document, index, from.as_deref(), &to)?;
        changed = true;
    }
    if let Some(index) = remove {
        prism_editor_core::remove_state_transition(document, index)?;
        changed = true;
    }
    for (index, duration, fraction) in timing_edits {
        prism_editor_core::set_transition_timing(document, index, duration, fraction)?;
        changed = true;
    }
    for (index, condition, kind) in kind_edits {
        prism_editor_core::set_condition_kind(document, index, condition, &kind)?;
        changed = true;
    }
    for (index, condition, name) in condition_name_edits {
        prism_editor_core::set_condition_name(document, index, condition, &name)?;
        changed = true;
    }
    for (index, condition, threshold) in threshold_edits {
        prism_editor_core::set_condition_threshold(document, index, condition, threshold)?;
        changed = true;
    }
    for (index, condition, value) in value_edits {
        prism_editor_core::set_condition_value(document, index, condition, value)?;
        changed = true;
    }
    if let Some((index, condition)) = remove_condition {
        prism_editor_core::remove_state_condition(document, index, condition)?;
        changed = true;
    }
    if let Some(draft) = add_condition {
        prism_editor_core::add_state_condition(
            document,
            draft.transition,
            &draft.kind,
            &draft.name,
            draft.threshold,
            draft.value,
        )?;
        *draft_slot = None;
        changed = true;
    }
    if cancel_draft {
        *draft_slot = None;
    }

    ui.menu_button("+ Transition", |ui| {
        for from in
            std::iter::once(None).chain(machine.states.iter().map(|state| Some(&state.name)))
        {
            let label = from.map(String::as_str).unwrap_or("any");
            ui.menu_button(label, |ui| {
                for to in &machine.states {
                    if from.is_some_and(|from| from == &to.name) {
                        continue;
                    }
                    if ui.button(&to.name).clicked() {
                        add_transition = Some((from.map(String::as_str), to.name.clone()));
                        ui.close();
                    }
                }
            });
        }
    });
    if let Some((from, to)) = add_transition {
        prism_editor_core::add_state_transition(document, from.as_deref(), &to)?;
        changed = true;
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------
// 材质 / JSON 资产
// ---------------------------------------------------------------------------

/// 打开一个 JSON 资产到材质面板（`.mat.json` / `.asset.json`，#488）。
pub fn open_material_asset(workspace: &Workspace, state: &mut UiState, path: &str) {
    match workspace.read_asset_json(path) {
        Ok(value) => {
            state.material_asset = Some(path.to_string());
            state.material_saved = Some(value.clone());
            state.material_value = Some(value);
            state.material_error = None;
        }
        Err(error) => {
            state.material_asset = Some(path.to_string());
            state.material_value = None;
            state.material_saved = None;
            state.material_error = Some(format!("读取 {path} 失败：{error}"));
        }
    }
}

/// 材质面板：把 `.mat.json` 当作普通 JSON 值编辑。
///
/// 与 Unity 的 `MaterialEditor` 相比这是最小闭环——没有材质球预览、没有按
/// shader 分组，但 `base_color` / `metallic` / `roughness` / 贴图槽这些字段
/// 都能在这里改完存回磁盘，不用离开编辑器手写 JSON（#488）。
pub fn material(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    // 选中一个材质资产时自动载入（面板打开着、用户换选中项也跟手）。
    // 有未保存改动时不动，免得把编辑中的值冲掉。
    let dirty = state.material_saved.as_ref() != state.material_value.as_ref();
    if !dirty {
        if let Some(path) = state.asset_primary().cloned() {
            if prism_editor_core::project::is_json_editable_asset(&path)
                && state.material_asset.as_deref() != Some(path.as_str())
            {
                open_material_asset(workspace, state, &path);
            }
        }
    }
    let Some(value) = state.material_value.clone() else {
        ui.label(
            RichText::new("double-click a .mat.json / .asset.json in the assets panel")
                .weak()
                .size(11.0),
        );
        if let Some(error) = state.material_error.clone() {
            ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
        }
        return;
    };
    let path = state.material_asset.clone().unwrap_or_default();
    let dirty = state.material_saved.as_ref() != Some(&value);

    ui.horizontal(|ui| {
        ui.label(RichText::new(&path).size(11.0));
        if dirty {
            ui.label(
                RichText::new("● unsaved")
                    .size(11.0)
                    .color(Color32::from_rgb(226, 178, 84)),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(dirty, egui::Button::new("Save"))
                .on_hover_text("Write the asset back to disk")
                .clicked()
            {
                match workspace.write_asset_json(&path, &value) {
                    Ok(()) => {
                        state.material_saved = Some(value.clone());
                        state.material_error = None;
                        state.status = format!("saved {path}");
                    }
                    Err(error) => state.material_error = Some(format!("保存失败：{error}")),
                }
            }
            if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                state.material_value = state.material_saved.clone();
            }
            if ui.button("Reload").clicked() {
                open_material_asset(workspace, state, &path);
            }
        });
    });
    if let Some(error) = state.material_error.clone() {
        ui.label(RichText::new(error).color(Color32::from_rgb(232, 96, 96)));
    }
    ui.separator();

    let Some(mut value) = state.material_value.clone() else {
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt("material-fields")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(map) = value.as_object_mut() {
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                egui::Grid::new("material-grid")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        for key in keys {
                            ui.label(RichText::new(&key).size(11.0));
                            let slot = map.get_mut(&key).expect("key came from the map");
                            material_field(ui, &key, slot);
                            ui.end_row();
                        }
                    });
            } else {
                ui.label(RichText::new("top-level value is not an object").weak());
                ui.label(RichText::new(value.to_string()).monospace().size(10.0));
            }
        });
    state.material_value = Some(value);
}

/// 一个材质字段的控件（按 JSON 值的类型决定）。
fn material_field(ui: &mut Ui, key: &str, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Bool(flag) => {
            ui.checkbox(flag, "");
        }
        serde_json::Value::Number(number) => {
            let mut item = number.as_f64().unwrap_or(0.0);
            let speed = if key.contains("rough") || key.contains("metal") {
                0.005
            } else {
                0.01
            };
            if ui
                .add(
                    egui::DragValue::new(&mut item)
                        .speed(speed)
                        .max_decimals(4)
                        .custom_parser(crate::widgets::number_parser),
                )
                .changed()
            {
                *value = serde_json::json!(item);
            }
        }
        serde_json::Value::String(text) => {
            ui.add(egui::TextEdit::singleline(text).desired_width(220.0));
        }
        serde_json::Value::Array(items) => {
            let numbers: Option<Vec<f64>> = items.iter().map(|item| item.as_f64()).collect();
            match numbers {
                // 颜色（4 分量）用取色器，其余数值数组用一排 DragValue。
                Some(channels) if channels.len() == 4 && key.contains("color") => {
                    let mut rgba = [
                        channels[0] as f32,
                        channels[1] as f32,
                        channels[2] as f32,
                        channels[3] as f32,
                    ];
                    if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                        *value = serde_json::json!([rgba[0], rgba[1], rgba[2], rgba[3]]);
                    }
                }
                Some(channels) => {
                    let mut channels = channels;
                    let mut changed = false;
                    ui.horizontal(|ui| {
                        for channel in channels.iter_mut() {
                            if ui
                                .add(
                                    egui::DragValue::new(channel)
                                        .speed(0.01)
                                        .max_decimals(4)
                                        .custom_parser(crate::widgets::number_parser),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                        }
                    });
                    if changed {
                        *value = serde_json::json!(channels);
                    }
                }
                None => {
                    ui.label(
                        RichText::new(value.to_string())
                            .monospace()
                            .size(10.0)
                            .weak(),
                    );
                }
            }
        }
        serde_json::Value::Object(_) => {
            ui.label(
                RichText::new(value.to_string())
                    .monospace()
                    .size(10.0)
                    .weak(),
            );
        }
        serde_json::Value::Null => {
            ui.label(RichText::new("null").weak().size(11.0));
        }
    }
}

/// 资产删除确认框（绘制在资产面板内，使用全局 `ctx`）。
pub fn asset_delete_confirm(ui: &Ui, workspace: &mut Workspace, state: &mut UiState) {
    let Some(paths) = state.pending_asset_delete.clone() else {
        return;
    };
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Window::new("Delete Asset")
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            match paths.as_slice() {
                [one] => {
                    ui.label(format!("删除 {one}？此操作不可撤销。"));
                }
                many => {
                    ui.label(format!("删除 {} 项资产？此操作不可撤销。", many.len()));
                    for path in many.iter().take(10) {
                        ui.label(RichText::new(path).monospace().size(11.0));
                    }
                    if many.len() > 10 {
                        ui.label(RichText::new(format!("… 还有 {} 项", many.len() - 10)).weak());
                    }
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
    if confirmed {
        apply_pending_asset_delete(workspace, state);
    } else if cancelled {
        state.pending_asset_delete = None;
    }
}

/// 执行 `pending_asset_delete` 里的批量删除（确认框 / 控制通道共用，#618）。
pub fn apply_pending_asset_delete(workspace: &mut Workspace, state: &mut UiState) {
    let Some(paths) = state.pending_asset_delete.take() else {
        return;
    };
    let mut deleted = 0usize;
    let mut failures = Vec::new();
    for path in &paths {
        let before = state.status.clone();
        delete_asset_now(workspace, state, path);
        if state.status.starts_with("deleted ") {
            deleted += 1;
        } else if state.status != before {
            failures.push(state.status.clone());
        }
    }
    state.status = if failures.is_empty() {
        format!("deleted {deleted} asset(s)")
    } else {
        format!("deleted {deleted}, failed: {}", failures.join("; "))
    };
}

fn is_document_path(path: &str) -> bool {
    path.ends_with(".scene.json") || path.ends_with(".prefab.json")
}

/// 删除场景 / 预制体前的确认框：删磁盘文件 + 关标签 + 清 `scenes.json`。
pub fn doc_delete_confirm(ctx: &egui::Context, workspace: &mut Workspace, state: &mut UiState) {
    let Some(path) = state.pending_doc_delete.clone() else {
        return;
    };
    let mut decision = None;
    egui::Window::new("Delete Document")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(format!("Delete {path}?"));
            ui.label(
                RichText::new(
                    "The file, its open tab and its Build Settings entry are removed. This cannot be undone.",
                )
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
    match decision {
        Some(true) => apply_pending_doc_delete(workspace, state),
        Some(false) => state.pending_doc_delete = None,
        None => {}
    }
}

/// 执行 `pending_doc_delete`（确认框 / 控制通道共用，#618）。
pub fn apply_pending_doc_delete(workspace: &mut Workspace, state: &mut UiState) {
    let Some(path) = state.pending_doc_delete.take() else {
        return;
    };
    match workspace.delete_document(&path) {
        Ok(()) => {
            state.forget_asset(&path);
            state.status = format!("deleted {path}");
        }
        Err(error) => state.status = format!("delete failed: {error}"),
    }
}

fn navigate_assets(state: &mut UiState, folder: &str) {
    if state.asset_folder == folder {
        return;
    }
    state.asset_folder = folder.to_string();
    state.asset_history.truncate(state.asset_history_pos + 1);
    state.asset_history.push(folder.to_string());
    state.asset_history_pos = state.asset_history.len() - 1;
}

// ---------------------------------------------------------------------------
// 文档标签
// ---------------------------------------------------------------------------

pub fn docs(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    let mut activate: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut save: Option<usize> = None;
    let mut delete_doc: Option<String> = None;
    let count = workspace.docs.len();
    for index in 0..count {
        let title = workspace.docs[index].title_with_dirty();
        let dirty = title.ends_with(" *");
        let active = workspace.active_index() == index;
        let colors = workspace.colors();
        // 整行可点（桌面 / Web 的文档列表都是整行选中），行内按钮用无框图标。
        let (rect, row) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), row_height(state.compact)),
            egui::Sense::click(),
        );
        if ui.is_rect_visible(rect) {
            let bg = if active {
                Some(theme::color32(
                    &colors,
                    prism_editor_core::ThemeToken::Selection,
                ))
            } else if row.hovered() {
                Some(theme::color32(
                    &colors,
                    prism_editor_core::ThemeToken::RowHover,
                ))
            } else {
                None
            };
            if let Some(bg) = bg {
                ui.painter().rect_filled(rect, 0.0, bg);
            }
        }
        let text_color = theme::color32(
            &colors,
            if active {
                prism_editor_core::ThemeToken::OnAccent
            } else {
                prism_editor_core::ThemeToken::Text
            },
        );
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        row_ui.add_space(6.0);
        row_ui.label(RichText::new(title).size(12.0).color(text_color));
        if workspace.docs[index].externally_modified {
            row_ui
                .label(
                    RichText::new("⚠")
                        .size(11.0)
                        .color(egui::Color32::from_rgb(220, 170, 60)),
                )
                .on_hover_text("磁盘上的版本已被外部修改，保存前会先问你");
        }
        row_ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if flat_icon(ui, icons::X).clicked() {
                close = Some(index);
            }
            if dirty && flat_icon(ui, icons::FLOPPY_DISK).clicked() {
                save = Some(index);
            }
        });
        if row.clicked() {
            activate = Some(index);
        }
        // 文档行右键：删除磁盘文件（资产面板拒绝直接删文档，入口收在这里）。
        if let Some(path) = workspace.docs[index].path.clone() {
            row.context_menu(|ui| {
                if ui.button("Delete…").clicked() {
                    delete_doc = Some(path);
                    ui.close();
                }
            });
        }
    }
    if let Some(index) = activate {
        workspace.set_active(index);
    }
    if let Some(index) = save {
        request_save_document(workspace, state, index);
    }
    if let Some(index) = close {
        request_close_document(workspace, state, index);
    }
    if let Some(path) = delete_doc {
        state.pending_doc_delete = Some(path);
    }
}

/// 保存一个文档；磁盘上的版本被外部改过就挂起，交给冲突对话框（#452）。
pub fn request_save_document(workspace: &mut Workspace, state: &mut UiState, index: usize) {
    match workspace.save_document(index) {
        Ok(path) => state.status = format!("saved {path}"),
        Err(_) if workspace.save_conflict(index).is_some() => {
            state.pending_save_conflict = Some(index);
            state.status = "文件在磁盘上已被修改".to_string();
        }
        Err(error) => state.status = format!("save failed: {error}"),
    }
}

/// 关闭文档标签：有未保存改动时挂起，交给 `close_confirm_dialog` 询问；
/// 干净的文档直接关闭（引擎的行为）。
pub fn request_close_document(workspace: &mut Workspace, state: &mut UiState, index: usize) {
    let dirty = workspace
        .docs
        .get(index)
        .map(|document| document.dirty)
        .unwrap_or(false);
    if dirty {
        state.pending_close = Some(CloseTarget::Document(index));
    } else {
        workspace.close_document(index);
    }
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

pub fn log(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    let mut filter = workspace.log.filter().clone();
    filter.levels = state.log_levels.clone();
    filter.sources = state.log_sources.clone();
    filter.search = state.log_search.clone();
    if filter != *workspace.log.filter() {
        workspace.log.set_filter(filter);
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.log_search)
                .hint_text("Filter")
                .desired_width(140.0),
        );
        ui.menu_button("Levels", |ui| {
            for level in [
                LogLevel::Error,
                LogLevel::Warn,
                LogLevel::Info,
                LogLevel::Debug,
                LogLevel::Trace,
                LogLevel::Panic,
            ] {
                let mut on = state.log_levels.contains(&level);
                if ui.checkbox(&mut on, level.label()).changed() {
                    if on {
                        state.log_levels.push(level);
                    } else {
                        state.log_levels.retain(|item| *item != level);
                    }
                }
            }
        });
        ui.menu_button("Sources", |ui| {
            for source in [
                LogSource::Editor,
                LogSource::Engine,
                LogSource::Game,
                LogSource::System,
            ] {
                let mut on = state.log_sources.contains(&source);
                if ui.checkbox(&mut on, source.label()).changed() {
                    if on {
                        state.log_sources.push(source);
                    } else {
                        state.log_sources.retain(|item| *item != source);
                    }
                }
            }
        });
        ui.checkbox(&mut state.log_autoscroll, "Follow");
        ui.checkbox(&mut state.log_collapse, "Collapse")
            .on_hover_text("Merge identical messages into one row with a count");
        ui.checkbox(&mut state.log_clear_on_play, "Clear on Play")
            .on_hover_text("Clear the Console when Play starts");
        ui.checkbox(&mut state.log_error_pause, "Error Pause")
            .on_hover_text("Pause Play when an error is logged");
        if ui.button("Clear").clicked() {
            workspace.log.clear();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{} lines", workspace.log.len())).weak());
        });
    });
    ui.separator();

    let lines = workspace.log.visible_lines();
    let rows = log_rows(&lines, state.log_collapse);
    let row_height = 16.0;
    let mut jump: Option<(std::path::PathBuf, Option<usize>, Option<usize>)> = None;
    egui::ScrollArea::vertical()
        .id_salt("log-scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(state.log_autoscroll)
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            for (index, count) in &rows[range] {
                let line = &lines[*index];
                let color = match line.level {
                    LogLevel::Error | LogLevel::Panic => egui::Color32::from_rgb(232, 96, 96),
                    LogLevel::Warn => egui::Color32::from_rgb(224, 176, 80),
                    LogLevel::Info => egui::Color32::from_gray(200),
                    LogLevel::Debug | LogLevel::Trace => egui::Color32::from_gray(140),
                };
                let text = if *count > 1 {
                    format!("[{}] {}  (×{count})", line.source.label(), line.text)
                } else {
                    format!("[{}] {}", line.source.label(), line.text)
                };
                let response = ui.label(RichText::new(text).monospace().size(11.0).color(color));
                // 双击跳转（#421）：日志里带 `path:line[:col]` 时交给默认编辑器。
                if log_location(&line.text).is_some() {
                    let response = response.interact(Sense::click());
                    if response.double_clicked() {
                        jump = log_location(&line.text).map(|(path, line, column)| {
                            (std::path::PathBuf::from(path), line, column)
                        });
                    }
                }
            }
        });
    if let Some((path, line, column)) = jump {
        state.status = match open_source_location(&path, line, column) {
            Ok(()) => format!("opened {}", path.display()),
            Err(error) => error,
        };
    }
}

/// Console 的显示行：`(原始行下标, 重复次数)`。
///
/// `collapse` 关闭时每行一份；打开后**相同的 `(级别, 来源, 文本)`** 合并到第一次
/// 出现的位置并计数（Unity Collapse 的语义），因此顺序仍然是按时间。
fn log_rows(lines: &[prism_editor_core::LogLine], collapse: bool) -> Vec<(usize, usize)> {
    if !collapse {
        return (0..lines.len()).map(|index| (index, 1)).collect();
    }
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut seen: std::collections::HashMap<(LogLevel, LogSource, &str), usize> =
        std::collections::HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        match seen.get(&(line.level, line.source, line.text.as_str())) {
            Some(&row) => rows[row].1 += 1,
            None => {
                seen.insert((line.level, line.source, line.text.as_str()), rows.len());
                rows.push((index, 1));
            }
        }
    }
    rows
}

/// 从日志文本里找第一个 `path:line[:col]`，用于双击跳转（#421）。
///
/// 只认真实存在的文件，避免把 `foo.rs` 这种「举例文本」也当路径；行号 / 列号
/// 可省略。返回的路径保持日志里写的相对形式（`open_source_location` 自己
/// 按当前目录解析）。
fn log_location(text: &str) -> Option<(String, Option<usize>, Option<usize>)> {
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\'' | '`')
        });
        // 从尾部剥掉最多两段 `:数字`（先剥列号，再剥行号）。
        let mut path = token;
        let mut numbers: Vec<usize> = Vec::new();
        while numbers.len() < 2 {
            let Some((head, tail)) = path.rsplit_once(':') else {
                break;
            };
            let Ok(number) = tail.parse::<usize>() else {
                break;
            };
            numbers.push(number);
            path = head;
        }
        if !path.contains('.') || !std::path::Path::new(path).is_file() {
            continue;
        }
        numbers.reverse();
        return Some((
            path.to_string(),
            numbers.first().copied(),
            numbers.get(1).copied(),
        ));
    }
    None
}

/// Problems 面板：编译的 error / warning，点一行跳到源码位置。
///
/// 两个来源并列：Game View 的项目模块（preview-module）和
/// `File → Build Player` 的 `cargo build`。诊断都由 `cargo --message-format=json` 提供。
pub fn problems(ui: &mut Ui, module: &[Diagnostic], build: &[Diagnostic], state: &mut UiState) {
    let errors = module
        .iter()
        .chain(build)
        .filter(|diagnostic| diagnostic.level == LogLevel::Error)
        .count();
    let warnings = module.len() + build.len() - errors;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{errors} errors · {warnings} warnings")).size(11.0));
        ui.label(
            RichText::new("module build · player build")
                .weak()
                .size(10.5),
        );
    });
    ui.separator();
    if module.is_empty() && build.is_empty() {
        ui.label(
            RichText::new("No problems. Compiler errors and warnings appear here.")
                .weak()
                .size(11.0),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("problems-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            problem_section(ui, "Project module (Ctrl+R recompiles)", module, state);
            problem_section(ui, "Player build", build, state);
        });
}

fn problem_section(ui: &mut Ui, title: &str, diagnostics: &[Diagnostic], state: &mut UiState) {
    if diagnostics.is_empty() {
        return;
    }
    ui.label(RichText::new(title).size(10.5).weak());
    for diagnostic in diagnostics {
        let (icon, color) = match diagnostic.level {
            LogLevel::Error => (icons::X_CIRCLE, Color32::from_rgb(232, 96, 96)),
            _ => (icons::WARNING, Color32::from_rgb(224, 176, 80)),
        };
        let location = diagnostic.location();
        let mut response = ui
            .horizontal(|ui| {
                ui.label(RichText::new(icon).size(12.0).color(color));
                ui.label(RichText::new(&diagnostic.message).size(11.0));
                if let Some(location) = &location {
                    ui.label(RichText::new(location).weak().size(10.0));
                }
            })
            .response;
        if location.is_some() {
            response = response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Open this location in the default editor");
            if response.clicked() {
                if let Some(file) = diagnostic.file.as_deref() {
                    state.status =
                        match open_source_location(file, diagnostic.line, diagnostic.column) {
                            Ok(()) => format!("opened {}", file.display()),
                            Err(error) => error,
                        };
                }
            }
        }
    }
    ui.add_space(4.0);
}

/// 在默认编辑器里打开源码位置（best effort）：先试 VS Code 的 `-g file:line:col`，
/// 没有 `code` 就退回系统默认处理程序（只能到文件，不能到行）。
pub fn open_source_location(
    path: &std::path::Path,
    line: Option<usize>,
    column: Option<usize>,
) -> Result<(), String> {
    let file = path.display().to_string();
    let target = match line {
        Some(line) => match column {
            Some(column) => format!("{file}:{line}:{column}"),
            None => format!("{file}:{line}"),
        },
        None => file.clone(),
    };
    // Windows 上 `code` 是 `code.cmd`，`Command::new("code")` 不会按 PATHEXT 找它，
    // 所以先确认它在 PATH 里，再经 `cmd /C` 启动。
    #[cfg(target_os = "windows")]
    let code_ready = on_path("code.cmd");
    #[cfg(not(target_os = "windows"))]
    let code_ready = on_path("code");
    if code_ready {
        let opened = if cfg!(target_os = "windows") {
            spawn_detached("cmd", &["/C", "code", "-g", &target])
        } else {
            spawn_detached("code", &["-g", &target])
        };
        if opened {
            return Ok(());
        }
    }
    let fallback = if cfg!(target_os = "windows") {
        // `start` 的第一个参数是窗口标题，必须给空串，否则路径会被当成标题。
        spawn_detached("cmd", &["/C", "start", "", &file])
    } else if cfg!(target_os = "macos") {
        spawn_detached("open", &[&file])
    } else {
        spawn_detached("xdg-open", &[&file])
    };
    if fallback {
        Ok(())
    } else {
        Err(format!("cannot open {file}"))
    }
}

/// `name` 是否是 PATH 上的可执行文件（不启动进程，避免“找不到 code”时静默吞掉）。
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(name).is_file())
    })
}

/// 起一个**不等待**的进程（打开编辑器不该卡住 UI 线程）。
fn spawn_detached(program: &str, args: &[&str]) -> bool {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.spawn().is_ok()
}

// ---------------------------------------------------------------------------
// 撤销历史
// ---------------------------------------------------------------------------

pub fn history(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    let entries = workspace.history_entries();
    let cursor = workspace.history_cursor();
    ui.horizontal(|ui| {
        ui.checkbox(&mut state.history_latest_first, "Latest first");
        ui.label(RichText::new(format!("{cursor}/{}", entries.len())).weak());
    });
    ui.separator();

    let mut jump: Option<usize> = None;
    let order: Vec<usize> = if state.history_latest_first {
        (0..entries.len()).rev().collect()
    } else {
        (0..entries.len()).collect()
    };
    egui::ScrollArea::vertical()
        .id_salt("history-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !state.history_latest_first {
                history_row(ui, "Initial state", cursor == 0, cursor != 0, 0, &mut jump);
            }
            for index in &order {
                let (label, undone) = &entries[*index];
                // 行 = 「执行完这一步之后的状态」，所以下标要 +1。
                let target = index + 1;
                let current = target == cursor;
                history_row(ui, label, current, *undone, target, &mut jump);
            }
            if state.history_latest_first {
                history_row(ui, "Initial state", cursor == 0, cursor != 0, 0, &mut jump);
            }
        });
    if let Some(target) = jump {
        workspace.jump_history(target);
    }
}

/// 历史面板的一行：`weak` 表示这步当前处于「已撤销」状态。
fn history_row(
    ui: &mut Ui,
    label: &str,
    current: bool,
    weak: bool,
    target: usize,
    jump: &mut Option<usize>,
) {
    let mut text = if weak {
        RichText::new(format!("(undone) {label}")).weak()
    } else {
        RichText::new(label)
    };
    if current {
        text = text.strong();
    }
    if ui.selectable_label(current, text.size(11.0)).clicked() {
        *jump = Some(target);
    }
}

// ---------------------------------------------------------------------------
// 渲染图
// ---------------------------------------------------------------------------

/// Scene View 的调试渲染模式（对应引擎的 `SceneRenderModeWindow` 第一组）。
///
/// 映射引擎已有的 `debug_flags` / `debug_rt`，以及前向管线的
/// `POLYGON_MODE_LINE`（Wireframe，#431）。PBR 分量隔离用
/// `PBR_FLAG_NAMES` 里的位，深度 / 视图空间法线用 PostPass 的调试 RT。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneRenderMode {
    Shaded,
    Wireframe,
    ShadedWireframe,
    Direct,
    Specular,
    Metallic,
    Roughness,
    AmbientOcclusion,
    Emissive,
    Depth,
    ViewNormals,
    SkinWeights,
}

impl SceneRenderMode {
    pub const ALL: [SceneRenderMode; 12] = [
        SceneRenderMode::Shaded,
        SceneRenderMode::Wireframe,
        SceneRenderMode::ShadedWireframe,
        SceneRenderMode::Direct,
        SceneRenderMode::Specular,
        SceneRenderMode::Metallic,
        SceneRenderMode::Roughness,
        SceneRenderMode::AmbientOcclusion,
        SceneRenderMode::Emissive,
        SceneRenderMode::Depth,
        SceneRenderMode::ViewNormals,
        SceneRenderMode::SkinWeights,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SceneRenderMode::Shaded => "Shaded",
            SceneRenderMode::Wireframe => "Wireframe",
            SceneRenderMode::ShadedWireframe => "Shaded Wireframe",
            SceneRenderMode::Direct => "Direct",
            SceneRenderMode::Specular => "Specular",
            SceneRenderMode::Metallic => "Metallic",
            SceneRenderMode::Roughness => "Roughness",
            SceneRenderMode::AmbientOcclusion => "AO",
            SceneRenderMode::Emissive => "Emissive",
            SceneRenderMode::Depth => "Depth",
            SceneRenderMode::ViewNormals => "View Normals",
            SceneRenderMode::SkinWeights => "Skin Weights",
        }
    }

    /// `PBR_FLAG_*` 位（见 `PBR_FLAG_NAMES`）；`0` = 正常着色。
    fn debug_flags(self) -> u32 {
        match self {
            SceneRenderMode::Direct => 1 << 0,
            SceneRenderMode::Specular => 1 << 2,
            SceneRenderMode::Metallic => 1 << 3,
            SceneRenderMode::Roughness => 1 << 4,
            SceneRenderMode::AmbientOcclusion => 1 << 8,
            SceneRenderMode::Emissive => 1 << 9,
            // 第 16 位不是 PBR 分量，而是蒙皮权重热图（#560）。
            SceneRenderMode::SkinWeights => 1 << 15,
            _ => 0,
        }
    }

    /// 前向管线是否用 `POLYGON_MODE_LINE`（#431）。
    fn wireframe(self) -> bool {
        matches!(self, SceneRenderMode::Wireframe)
    }

    /// 是否在正常填充上叠加线框（#431）。
    fn shaded_wireframe(self) -> bool {
        matches!(self, SceneRenderMode::ShadedWireframe)
    }

    /// PostPass 调试 RT：`0` 正常、`1` 线性深度、`2` 视图空间法线。
    fn debug_rt(self) -> u32 {
        match self {
            SceneRenderMode::Depth => 1,
            SceneRenderMode::ViewNormals => 2,
            _ => 0,
        }
    }

    /// 从当前设置**反推**模式；引擎设置被其它入口改成组合值时返回 `None`。
    pub fn from_settings(settings: &prism_engine::render_settings::RenderSettings) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| {
            mode.debug_flags() == settings.debug_flags
                && mode.debug_rt() == settings.debug_rt
                && mode.wireframe() == settings.wireframe
                && mode.shaded_wireframe() == settings.shaded_wireframe
        })
    }

    pub fn apply(self, settings: &mut prism_engine::render_settings::RenderSettings) {
        settings.debug_flags = self.debug_flags();
        settings.debug_rt = self.debug_rt();
        settings.wireframe = self.wireframe();
        settings.shaded_wireframe = self.shaded_wireframe();
    }
}

/// 工具栏 / 渲染设置面板共用的「渲染模式」下拉框。返回是否改动。
pub fn scene_render_mode_combo(
    ui: &mut Ui,
    settings: &mut prism_engine::render_settings::RenderSettings,
) -> bool {
    let current = SceneRenderMode::from_settings(settings);
    let selected = current.map(|mode| mode.label()).unwrap_or("Custom");
    let mut chosen = current;
    egui::ComboBox::from_id_salt("scene-render-mode")
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for mode in SceneRenderMode::ALL {
                if ui
                    .selectable_label(current == Some(mode), mode.label())
                    .clicked()
                {
                    chosen = Some(mode);
                }
            }
        });
    match chosen {
        Some(mode) if current != Some(mode) => {
            mode.apply(settings);
            true
        }
        _ => false,
    }
}

pub fn render_graph(
    ui: &mut Ui,
    report: Option<&str>,
    settings: &mut prism_engine::render_settings::RenderSettings,
    viewport_scale: &mut f32,
    paused: &mut bool,
    selected_call: &mut Option<usize>,
    frame_budget_ms: &mut Option<f32>,
) -> bool {
    let mut changed = false;
    ui.heading("Render Settings");
    egui::Grid::new("render-settings")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Mode");
            changed |= scene_render_mode_combo(ui, settings);
            ui.end_row();

            ui.label("Tonemap");
            egui::ComboBox::from_id_salt("tonemap")
                .selected_text(match settings.tonemap_mode {
                    0 => "Reinhard",
                    1 => "ACES",
                    _ => "Passthrough",
                })
                .show_ui(ui, |ui| {
                    for (mode, label) in [(0u32, "Reinhard"), (1, "ACES"), (2, "Passthrough")] {
                        if ui
                            .selectable_label(settings.tonemap_mode == mode, label)
                            .clicked()
                        {
                            settings.tonemap_mode = mode;
                            changed = true;
                        }
                    }
                });
            ui.end_row();

            ui.label("Contrast");
            changed |= ui
                .add(egui::Slider::new(&mut settings.contrast, 0.0..=3.0))
                .changed();
            ui.end_row();

            ui.label("Saturation");
            changed |= ui
                .add(egui::Slider::new(&mut settings.saturation, 0.0..=3.0))
                .changed();
            ui.end_row();

            ui.label("Vignette");
            changed |= ui
                .add(egui::Slider::new(&mut settings.vignette, 0.0..=1.0))
                .changed();
            ui.end_row();

            ui.label("Chromatic Aberration");
            changed |= ui
                .add(egui::Slider::new(
                    &mut settings.chromatic_aberration,
                    0.0..=1.0,
                ))
                .on_hover_text("Radial red/blue split toward the frame edges.")
                .changed();
            ui.end_row();

            ui.label("Film Grain");
            changed |= ui
                .add(egui::Slider::new(&mut settings.film_grain, 0.0..=1.0))
                .on_hover_text("Monochrome noise over the final image.")
                .changed();
            ui.end_row();

            ui.label("Temperature");
            changed |= ui
                .add(egui::Slider::new(&mut settings.temperature, -1.0..=1.0))
                .on_hover_text("White balance: negative is cooler (blue), positive is warmer (orange).")
                .changed();
            ui.end_row();

            ui.label("Tint");
            changed |= ui
                .add(egui::Slider::new(&mut settings.tint, -1.0..=1.0))
                .on_hover_text("White balance: negative is green, positive is magenta.")
                .changed();
            ui.end_row();

            ui.label("Depth of Field");
            changed |= ui
                .add(egui::Slider::new(&mut settings.dof_aperture, 0.0..=1.0))
                .on_hover_text("Depth-of-field strength; 0 disables it.")
                .changed();
            ui.end_row();

            ui.label("Focus Distance");
            changed |= ui
                .add(egui::Slider::new(
                    &mut settings.dof_focus_distance,
                    0.1..=100.0,
                ))
                .on_hover_text("Distance from the camera that stays sharp, in metres.")
                .changed();
            ui.end_row();

            ui.label("Max Blur");
            changed |= ui
                .add(egui::Slider::new(&mut settings.dof_max_blur, 0.0..=32.0))
                .on_hover_text("Largest depth-of-field blur radius in pixels.")
                .changed();
            ui.end_row();

            ui.label("LUT");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut settings.lut)
                        .hint_text("luts/grade.cube")
                        .desired_width(180.0),
                )
                .on_hover_text("3D LUT path relative to assets/. Empty disables it.")
                .changed();
            ui.end_row();

            ui.label("LUT Strength");
            changed |= ui
                .add_enabled(
                    !settings.lut.trim().is_empty(),
                    egui::Slider::new(&mut settings.lut_strength, 0.0..=1.0),
                )
                .on_hover_text("Blend toward the LUT; 0 disables it, 1 uses it fully.")
                .changed();
            ui.end_row();

            ui.label("Motion Blur");
            changed |= ui
                .add(egui::Slider::new(&mut settings.motion_blur, 0.0..=1.0))
                .on_hover_text("Camera motion-blur strength; 0 disables it.")
                .changed();
            ui.end_row();

            ui.label("Motion Trail");
            changed |= ui
                .add(egui::Slider::new(
                    &mut settings.motion_blur_max_pixels,
                    0.0..=64.0,
                ))
                .on_hover_text("Largest motion-blur trail in pixels.")
                .changed();
            ui.end_row();

            ui.label("TAA");
            changed |= ui.checkbox(&mut settings.taa, "Enable").changed();
            ui.end_row();

            ui.label("TAA Strength");
            changed |= ui
                .add_enabled(
                    settings.taa,
                    egui::Slider::new(&mut settings.taa_strength, 0.0..=0.95),
                )
                .on_hover_text("History blend weight; 0 is current frame only.")
                .changed();
            ui.end_row();

            ui.label("Bloom");
            changed |= ui.checkbox(&mut settings.bloom, "Enable").changed();
            ui.end_row();

            ui.label("Bloom Intensity");
            changed |= ui
                .add_enabled(
                    settings.bloom,
                    egui::Slider::new(&mut settings.bloom_intensity, 0.0..=1.0),
                )
                .changed();
            ui.end_row();

            ui.label("Bloom Threshold");
            changed |= ui
                .add_enabled(
                    settings.bloom,
                    egui::Slider::new(&mut settings.bloom_threshold, 0.0..=8.0),
                )
                .on_hover_text("HDR luminance above which pixels start to bloom.")
                .changed();
            ui.end_row();

            ui.label("Shadow Distance");
            changed |= ui
                .add(
                    egui::Slider::new(&mut settings.shadow_distance, 10.0..=300.0)
                        .suffix(" m")
                        .fixed_decimals(1),
                )
                .on_hover_text(
                    "Directional shadow coverage. Larger sees further, each texel covers more                      world, so contact shadows get blurrier.",
                )
                .changed();
            ui.end_row();

            ui.label("Shadow Cascades");
            egui::ComboBox::from_id_salt("shadow-cascades")
                .selected_text(match settings.shadow_cascade_count {
                    2 => "2 (atlas)",
                    _ => "1",
                })
                .show_ui(ui, |ui| {
                    for count in [1u32, 2] {
                        let label = if count == 1 { "1" } else { "2 (atlas)" };
                        if ui
                            .selectable_label(settings.shadow_cascade_count == count, label)
                            .clicked()
                        {
                            settings.shadow_cascade_count = count;
                            changed = true;
                        }
                    }
                })
                .response
                .on_hover_text(
                    "1 = single shadow map; 2 = near/far cascades in a 2×1 atlas. Requires recreating the renderer.",
                );
            ui.end_row();

            ui.label("Anti-aliasing");
            egui::ComboBox::from_id_salt("aa-mode")
                .selected_text(match settings.aa_mode {
                    1 => "FXAA",
                    _ => "Off",
                })
                .show_ui(ui, |ui| {
                    for (mode, label) in [(0u32, "Off"), (1, "FXAA")] {
                        if ui
                            .selectable_label(settings.aa_mode == mode, label)
                            .clicked()
                        {
                            settings.aa_mode = mode;
                            changed = true;
                        }
                    }
                });
            ui.end_row();

            ui.label("Path trace");
            let mut path_trace = matches!(
                settings.render_mode,
                prism_engine::render_settings::RenderMode::PathTrace
            );
            if ui.checkbox(&mut path_trace, "Path traced").changed() {
                settings.render_mode = if path_trace {
                    prism_engine::render_settings::RenderMode::PathTrace
                } else {
                    prism_engine::render_settings::RenderMode::Raster
                };
                changed = true;
            }
            ui.end_row();

            if path_trace {
                ui.label("Max bounces");
                changed |= ui
                    .add(egui::Slider::new(&mut settings.pt_max_bounces, 1..=16))
                    .changed();
                ui.end_row();
                ui.label("Samples");
                changed |= ui
                    .add(egui::Slider::new(&mut settings.pt_max_iterations, 0..=4096))
                    .changed();
                ui.end_row();
            }

            ui.label("Grid");
            changed |= ui
                .checkbox(&mut settings.show_grid, "Show ground grid")
                .changed();
            ui.end_row();

            ui.label("Viewport res");
            changed |= ui
                .add(egui::Slider::new(viewport_scale, 0.5..=1.5).suffix("×"))
                .changed();
            ui.end_row();
        });

    ui.separator();
    ui.heading("Render Graph");
    let mut budget = frame_budget_ms.unwrap_or(FRAME_BUDGET_MS);
    ui.horizontal(|ui| {
        ui.label("Frame budget");
        if ui
            .add(
                egui::DragValue::new(&mut budget)
                    .range(1.0..=50.0)
                    .speed(0.1)
                    .suffix(" ms"),
            )
            .changed()
        {
            *frame_budget_ms = Some(budget);
        }
        if frame_budget_ms.is_some() && ui.small_button("Auto").clicked() {
            *frame_budget_ms = None;
        }
        ui.label(
            RichText::new(if frame_budget_ms.is_some() {
                "custom"
            } else {
                "auto 16.6 ms"
            })
            .weak()
            .size(10.0),
        );
    });
    ui.horizontal(|ui| {
        let label = if *paused {
            "Resume live capture"
        } else {
            "Pause capture"
        };
        if ui.button(label).clicked() {
            *paused = !*paused;
        }
        if *paused {
            ui.label(
                RichText::new("showing the frame captured when paused")
                    .weak()
                    .size(10.0),
            );
        }
    });
    ui.label(
        RichText::new(
            "Shadow → Scene → GTAO → PathTrace → Sprite → Sky → Post run on one queue; \
             the Scene/Game viewports sample the final offscreen image directly.",
        )
        .weak()
        .size(11.0),
    );
    match report {
        Some(json) => {
            ui.label(
                RichText::new("target/prism-render-graph.json")
                    .weak()
                    .size(10.0),
            );
            let parsed = prism_editor_core::render_graph::RenderGraphReport::parse(json);
            if let Some(parsed) = &parsed {
                render_graph_timings(ui, parsed, budget);
                render_graph_history(ui, parsed, budget);
                render_graph_stats(ui, parsed);
                render_graph_draw_calls(ui, parsed, paused, selected_call);
            }
            ui.collapsing("Raw JSON", |ui| {
                egui::ScrollArea::both()
                    .id_salt("graph-scroll")
                    .auto_shrink([false, false])
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new(json).monospace().size(10.0));
                    });
            });
        }
        None => {
            ui.label(
                RichText::new(
                    "No report yet — the Scene view writes one to                      target/prism-render-graph.json after its first frame.",
                )
                .weak(),
            );
        }
    }
    changed
}

/// Frame Debugger 的数据段（#518 Phase 2）：本帧的 draw call 列表。
///
/// 只读展示（不含回放）；被剔除的网格用弱化颜色标出，便于解释“为什么没画”。
fn render_graph_draw_calls(
    ui: &mut Ui,
    report: &prism_editor_core::render_graph::RenderGraphReport,
    paused: &mut bool,
    selected: &mut Option<usize>,
) {
    let count = report.draw_calls.len();
    if count == 0 {
        return;
    }
    if selected.is_some_and(|index| index >= count) {
        *selected = Some(count - 1);
    }

    ui.add_space(4.0);
    let mut step = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Draw calls").strong());
        ui.label(RichText::new(format!("{count} recorded")).weak().size(10.0));
        if ui.button("◀ Prev").clicked() {
            step = Some(-1);
        }
        if ui.button("Next ▶").clicked() {
            step = Some(1);
        }
        match *selected {
            Some(index) => ui.label(
                RichText::new(format!("call {}/{}", index + 1, count))
                    .weak()
                    .size(10.0),
            ),
            None => ui.label(RichText::new("no call selected").weak().size(10.0)),
        };
    });
    if let Some(delta) = step {
        *selected = stepped_call(*selected, count, delta);
        // 单步检查的语义就是「把这一帧固定住」：还没暂停时自动暂停。
        *paused = true;
    }

    egui::ScrollArea::vertical()
        .id_salt("graph-draw-calls")
        .max_height(160.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("graph-draw-call-grid")
                .num_columns(4)
                .spacing([12.0, 3.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Call").weak().size(10.0));
                    ui.label(RichText::new("Inst.").weak().size(10.0));
                    ui.label(RichText::new("Tris").weak().size(10.0));
                    ui.label(RichText::new("State").weak().size(10.0));
                    ui.end_row();
                    for (index, call) in report.draw_calls.iter().enumerate() {
                        let mut text = RichText::new(call.summary()).size(11.0);
                        if !call.visible {
                            text = text.weak().italics();
                        }
                        if ui
                            .selectable_label(*selected == Some(index), text)
                            .clicked()
                        {
                            *selected = Some(index);
                            *paused = true;
                        }
                        ui.label(RichText::new(call.instances.to_string()).size(11.0));
                        ui.label(RichText::new(call.triangles.to_string()).size(11.0));
                        let mut state = RichText::new(call.state.as_deref().unwrap_or("—"))
                            .weak()
                            .size(10.0);
                        if !call.visible {
                            state = state.italics();
                        }
                        ui.label(state);
                        ui.end_row();
                    }
                });
        });

    let Some(index) = *selected else {
        return;
    };
    let Some(call) = report.draw_calls.get(index) else {
        return;
    };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(
            RichText::new(format!("Call {}/{} · {}", index + 1, count, call.summary()))
                .strong()
                .size(11.0),
        );
        ui.label(
            RichText::new(format!(
                "instances {} · triangles {} · visible {}",
                call.instances,
                call.triangles,
                if call.visible { "yes" } else { "no (culled)" }
            ))
            .size(10.0),
        );
        ui.label(
            RichText::new(format!(
                "pipeline: {}",
                call.pipeline.as_deref().unwrap_or("not recorded")
            ))
            .size(10.0),
        );
        let descriptor_sets = if call.descriptor_sets.is_empty() {
            "none".to_string()
        } else {
            call.descriptor_sets.join(", ")
        };
        ui.label(RichText::new(format!("descriptor sets: {descriptor_sets}")).size(10.0));
        ui.label(
            RichText::new(format!(
                "vertices: {}",
                call.vertices
                    .map(|vertices| vertices.to_string())
                    .unwrap_or_else(|| "not recorded".to_string())
            ))
            .size(10.0),
        );
        ui.label(
            RichText::new(format!(
                "state: {}",
                call.state.as_deref().unwrap_or("not recorded")
            ))
            .size(10.0),
        );
    });
}

/// Frame Debugger 的 Prev / Next 下标推进：不越界、不环绕。
fn stepped_call(current: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let start = match current {
        Some(index) => index.min(count - 1) as isize,
        // 没有选择时：Next 选中第一条，Prev 选中最后一条。
        None if delta < 0 => count as isize,
        None => -1,
    };
    let next = (start + delta).clamp(0, count as isize - 1);
    Some(next as usize)
}

/// 资源统计（#518 Phase 2）：纹理 / 网格 / 材质的驻留量与估算字节。
///
/// 数据由渲染器的管理器实时给出；旧报告没有 `stats` 时整段隐藏（不是 0 值冒充）。
fn render_graph_stats(ui: &mut Ui, report: &prism_editor_core::render_graph::RenderGraphReport) {
    let Some(stats) = report.stats else {
        return;
    };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Resources").strong());
        ui.label(
            RichText::new(format!("~{} resident", format_bytes(stats.total_bytes())))
                .weak()
                .size(10.0),
        );
    });
    egui::Grid::new("graph-resource-stats")
        .num_columns(2)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            for (label, count, bytes) in stats.rows() {
                ui.label(RichText::new(label).weak().size(11.0));
                ui.label(RichText::new(format!("{count}  ·  {}", format_bytes(bytes))).size(11.0));
                ui.end_row();
            }
        });
}

/// 每帧预算（16.6 ms ≈ 60 FPS），超过的 pass 标红。
const FRAME_BUDGET_MS: f32 = 16.6;

/// 每个有 GPU 时间的 pass 分到的预算份额（#518）。
///
/// 没有逐 pass 的用户配置时，最直观的「分层预算」是把整帧预算均分给当前
/// 实际计时的 pass；某个 pass 独占超过自己的份额就标红。pass 数为 0 时返回
/// 0，调用方不会拿它做除法。
fn per_pass_budget_ms(pass_count: usize) -> f32 {
    per_pass_budget_ms_with_total(pass_count, FRAME_BUDGET_MS)
}

/// 用户配置的整帧预算按实际计时 pass 均分（#518）。
fn per_pass_budget_ms_with_total(pass_count: usize, frame_budget_ms: f32) -> f32 {
    if pass_count == 0 || !frame_budget_ms.is_finite() || frame_budget_ms <= 0.0 {
        0.0
    } else {
        frame_budget_ms / pass_count as f32
    }
}

/// per-pass GPU 时间表（#518）：总时间 + 按成本从高到低排的 pass。
///
/// 数据来自渲染器的时间戳查询（延迟一帧）。设备不支持时整段隐藏 —— 面板只显示
/// 「没有 GPU 计时」，不会拿 CPU 墙钟冒充 GPU 时间。
fn render_graph_timings(
    ui: &mut Ui,
    report: &prism_editor_core::render_graph::RenderGraphReport,
    frame_budget_ms: f32,
) {
    let Some(total) = report.gpu_total_ms() else {
        ui.label(
            RichText::new("No GPU timings (device does not report timestamps).")
                .weak()
                .size(11.0),
        );
        return;
    };

    let passes = report.passes_by_cost();
    let measured = passes.iter().filter(|pass| pass.gpu_ms.is_some()).count();
    let budget = per_pass_budget_ms_with_total(measured, frame_budget_ms);

    ui.horizontal(|ui| {
        ui.label(RichText::new("GPU").strong());
        let color = if total > frame_budget_ms {
            Color32::from_rgb(232, 96, 96)
        } else {
            Color32::from_rgb(120, 200, 140)
        };
        ui.label(RichText::new(format!("{total:.2} ms")).color(color));
        ui.label(
            RichText::new(format!(
                "budget {frame_budget_ms:.1} ms · {budget:.2} ms/pass"
            ))
            .weak()
            .size(10.0),
        );
    });

    egui::Grid::new("graph-pass-timings")
        .num_columns(4)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Pass").weak().size(10.0));
            ui.label(RichText::new("Kind").weak().size(10.0));
            ui.label(RichText::new("GPU ms").weak().size(10.0));
            ui.label(RichText::new("Budget").weak().size(10.0));
            ui.end_row();
            for pass in &passes {
                ui.label(RichText::new(&pass.name).size(11.0));
                ui.label(RichText::new(&pass.kind).weak().size(11.0));
                match pass.gpu_ms {
                    Some(ms) => {
                        let color = if budget > 0.0 && ms > budget {
                            Color32::from_rgb(232, 96, 96)
                        } else if budget > 0.0 && ms > budget * 0.8 {
                            Color32::from_rgb(226, 178, 84)
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(RichText::new(format!("{ms:.2}")).size(11.0).color(color));
                        ui.label(RichText::new(format!("{budget:.2}")).weak().size(10.0));
                    }
                    None => {
                        ui.label(RichText::new("—").weak().size(11.0));
                        ui.label(RichText::new("—").weak().size(10.0));
                    }
                }
                ui.end_row();
            }
        });
}

/// 最近 N 帧 GPU 总时间的 sparkline（#518 Phase 2 历史曲线）。
///
/// 横轴等距（报告只存每帧采样，不存时间戳）；虚线位置是当前 GPU 预算。
fn render_graph_history(
    ui: &mut Ui,
    report: &prism_editor_core::render_graph::RenderGraphReport,
    frame_budget_ms: f32,
) {
    let samples = &report.gpu_history_ms;
    if samples.len() < 2 {
        return;
    }
    ui.add_space(4.0);
    let average = samples.iter().sum::<f32>() / samples.len() as f32;
    ui.horizontal(|ui| {
        ui.label(RichText::new("GPU history").strong());
        ui.label(
            RichText::new(format!("{average:.2} ms avg · {} frames", samples.len()))
                .weak()
                .size(10.0),
        );
    });

    let desired = egui::vec2(ui.available_width().clamp(120.0, 360.0), 44.0);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);

    let max = samples
        .iter()
        .copied()
        .fold(frame_budget_ms * 1.25, f32::max)
        .max(1.0);
    let y_for = |ms: f32| rect.bottom() - (ms / max).clamp(0.0, 1.0) * rect.height();

    let budget_y = y_for(frame_budget_ms);
    painter.line_segment(
        [
            egui::pos2(rect.left(), budget_y),
            egui::pos2(rect.right(), budget_y),
        ],
        egui::Stroke::new(1.0, Color32::from_rgb(226, 178, 84)),
    );

    let last = (samples.len() - 1) as f32;
    let points: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(index, ms)| egui::pos2(rect.left() + rect.width() * index as f32 / last, y_for(*ms)))
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, Color32::from_rgb(120, 200, 140)),
    ));
}

// ---------------------------------------------------------------------------
// 设置类面板
// ---------------------------------------------------------------------------

/// 项目设置：`assets/settings.ini` 的 13 个键全部可编辑。
///
/// 读盘走 [`SettingsCache`]（首次 / mtime 变化才解析），写盘是显式的 Apply，
/// 所以敲 `output_dir` 不会把中间态逐字符写进文件（#428）。
pub fn project_settings(ui: &mut Ui, root: Option<&std::path::Path>, state: &mut UiState) {
    let Some(root) = root else {
        ui.label(RichText::new("Open a project directory first.").weak());
        return;
    };
    let same_root = state
        .settings_cache
        .as_ref()
        .is_some_and(|cache| cache.root() == root);
    let mut cache = if same_root {
        state.settings_cache.take().unwrap()
    } else {
        SettingsCache::load(root)
    };
    cache.refresh();
    let mut query = std::mem::take(&mut state.settings_search);

    let mut apply = false;
    let mut revert = false;
    let mut reset_all = false;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut query)
                .hint_text("Search settings…")
                .desired_width(170.0),
        );
        if cache.is_dirty() {
            ui.label(
                RichText::new("● unsaved")
                    .size(11.0)
                    .color(Color32::from_rgb(226, 178, 84)),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            apply = ui
                .add_enabled(cache.is_dirty(), egui::Button::new("Apply"))
                .on_hover_text("Write assets/settings.ini")
                .clicked();
            revert = ui
                .add_enabled(cache.is_dirty(), egui::Button::new("Revert"))
                .clicked();
            reset_all = ui
                .button("Reset all")
                .on_hover_text("Fill every field with the built-in default")
                .clicked();
        });
    });
    if cache.external_change() {
        ui.label(
            RichText::new(
                "settings.ini changed on disk while you were editing — Revert reloads it.",
            )
            .size(11.0)
            .color(Color32::from_rgb(226, 178, 84)),
        );
    }
    if let Some(error) = cache.error.clone() {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(error)
                    .size(11.0)
                    .color(Color32::from_rgb(232, 96, 96)),
            );
            if ui.small_button("Reset to defaults").clicked() {
                reset_all = true;
            }
            if ui.small_button("Reload").clicked() {
                cache.reload_from_disk();
            }
        });
    }
    if apply {
        match cache.apply() {
            Ok(()) => state.status = "assets/settings.ini saved".to_string(),
            Err(error) => state.status = format!("settings save failed: {error}"),
        }
    }
    if revert {
        cache.revert();
        state.status = "settings reverted".to_string();
    }
    if reset_all {
        cache.reset_all();
    }
    ui.separator();

    let fields = matching_fields(&query);
    if fields.is_empty() {
        ui.label(RichText::new("no matching setting").weak());
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for section in prism_editor_core::settings::SETTINGS_SECTIONS {
                let rows: Vec<_> = fields
                    .iter()
                    .filter(|field| field.section == *section)
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(*section).strong().size(12.0));
                    if ui
                        .small_button("Reset")
                        .on_hover_text(format!("Restore [{section}] to its defaults"))
                        .clicked()
                    {
                        cache.reset_section(section);
                    }
                });
                egui::Grid::new(format!("settings-{section}"))
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        for field in rows {
                            ui.label(field.label);
                            settings_field(ui, &mut cache.settings, field);
                            ui.end_row();
                        }
                    });
                ui.separator();
            }
        });

    state.settings_search = query;
    state.settings_cache = Some(cache);
}

/// 一个字段的控件（类型由 `(section, key)` 决定，见 [`SettingsField`]）。
fn settings_field(
    ui: &mut Ui,
    settings: &mut ProjectSettings,
    field: &prism_editor_core::settings::SettingsField,
) {
    match (field.section, field.key) {
        ("app", "company") => {
            ui.text_edit_singleline(&mut settings.app.company);
        }
        ("app", "name") => {
            ui.text_edit_singleline(&mut settings.app.name);
        }
        ("app", "version") => {
            ui.text_edit_singleline(&mut settings.app.version);
        }
        ("window", "title") => {
            ui.text_edit_singleline(&mut settings.window.title);
        }
        ("window", "width") => {
            ui.add(
                egui::DragValue::new(&mut settings.window.width)
                    .speed(1.0)
                    .range(320..=16_384),
            );
        }
        ("window", "height") => {
            ui.add(
                egui::DragValue::new(&mut settings.window.height)
                    .speed(1.0)
                    .range(240..=16_384),
            );
        }
        ("window", "resizable") => {
            ui.checkbox(&mut settings.window.resizable, "");
        }
        ("window", "fullscreen") => {
            ui.checkbox(&mut settings.window.fullscreen, "");
        }
        ("window", "vsync") => {
            ui.checkbox(&mut settings.window.vsync, "");
        }
        ("build", "target") => {
            settings_choice(ui, field.key, &mut settings.build.target, TARGET_CHOICES);
        }
        ("build", "profile") => {
            settings_choice(ui, field.key, &mut settings.build.profile, PROFILE_CHOICES);
        }
        ("build", "output_dir") => {
            ui.text_edit_singleline(&mut settings.build.output_dir);
        }
        ("build", "run_after") => {
            ui.checkbox(&mut settings.build.run_after, "");
        }
        _ => {
            ui.label(RichText::new("?").weak());
        }
    }
}

/// 下拉选择：常用取值 +「当前值不在列表里时」把它自己也列出来。
fn settings_choice(ui: &mut Ui, id: &str, value: &mut String, choices: &[&str]) {
    egui::ComboBox::from_id_salt(("settings-choice", id))
        .selected_text(value.clone())
        .width(140.0)
        .show_ui(ui, |ui| {
            for choice in choices {
                if ui.selectable_label(value == choice, *choice).clicked() {
                    *value = (*choice).to_string();
                }
            }
            if !choices.contains(&value.as_str()) {
                ui.label(RichText::new(value.clone()).weak());
            }
        });
}

pub fn preferences(
    ui: &mut Ui,
    preferences: &mut Preferences,
    workspace: &mut Workspace,
    state: &mut UiState,
    control_cli_disabled: bool,
) {
    ui.checkbox(&mut preferences.autosave, "Auto save layout");
    ui.checkbox(&mut preferences.confirm_delete, "Confirm before delete");
    ui.checkbox(&mut preferences.compact, "Compact layout");
    ui.add_enabled(
        !control_cli_disabled,
        egui::Checkbox::new(
            &mut preferences.control_channel,
            "Enable local control channel (MCP / automation)",
        ),
    )
    .on_hover_text(
        "Listen on 127.0.0.1 and write target/editor-egui/control.port so the MCP \
         server can drive the editor; turning it off stops the listener immediately \
         (#1008). --no-control disables it for the whole process.",
    );
    if control_cli_disabled {
        ui.label(
            RichText::new("Control channel disabled by --no-control")
                .weak()
                .small(),
        );
    }
    ui.checkbox(
        &mut preferences.hot_reload_assets,
        "Rebuild scene pack when loose assets change",
    )
    .on_hover_text(
        "Watch assets/ (textures, meshes, HDR, fonts…) and rerun Build Scene Pack \
         after a change, so Play picks it up without a restart (#495)",
    );
    ui.horizontal(|ui| {
        ui.label("Max log lines");
        let response =
            ui.add(egui::DragValue::new(&mut preferences.max_log_lines).range(500.0..=20_000.0));
        if response.changed() {
            workspace
                .log
                .set_capacity(preferences.max_log_lines as usize);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Language");
        let locale = workspace.i18n.locale();
        if ui
            .selectable_label(locale == Locale::EnUs, "English")
            .clicked()
        {
            workspace.set_locale(Locale::EnUs);
            state.status = "locale: en-US".to_string();
        }
        if ui
            .selectable_label(locale == Locale::ZhCn, "简体中文")
            .clicked()
        {
            workspace.set_locale(Locale::ZhCn);
            state.status = "locale: zh-CN".to_string();
        }
    });
}

/// Preferences 的「Confirm before delete」：删除实体前弹一次确认。
pub fn entity_delete_confirm(ctx: &egui::Context, workspace: &mut Workspace, state: &mut UiState) {
    let Some(indices) = state.pending_entity_delete.clone() else {
        return;
    };
    let mut decision: Option<bool> = None;
    egui::Window::new("Delete")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            let count = indices.len();
            if count == 1 {
                ui.label("Delete the selected entity and its children?");
            } else {
                ui.label(format!(
                    "Delete {count} selected entities and their children?"
                ));
            }
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
    match decision {
        Some(true) => apply_pending_entity_delete(workspace, state),
        Some(false) => state.pending_entity_delete = None,
        None => {}
    }
}

/// 执行 `pending_entity_delete`（确认框 / 控制通道共用，#618）。
pub fn apply_pending_entity_delete(workspace: &mut Workspace, state: &mut UiState) {
    let Some(indices) = state.pending_entity_delete.take() else {
        return;
    };
    workspace.select_many(&indices, false);
    workspace.delete_selection();
}

pub fn theme(ui: &mut Ui, workspace: &mut Workspace) {
    let current = workspace.theme.preset.clone();
    for preset in THEME_PRESETS {
        let selected = preset.id == current;
        if ui.selectable_label(selected, preset.label).clicked() {
            workspace.set_theme_preset(preset.id);
        }
    }
}

pub fn build(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    ui.heading("Scenes in build");
    let entries: Vec<(String, bool)> = workspace
        .scene_manifest
        .scenes
        .iter()
        .map(|entry| (entry.path.clone(), entry.enabled))
        .collect();
    let mut changed = false;
    for (index, (path, enabled)) in entries.iter().enumerate() {
        ui.horizontal(|ui| {
            let mut on = *enabled;
            if ui.checkbox(&mut on, "").changed() {
                workspace.set_scene_enabled(index, on);
                changed = true;
            }
            ui.label(RichText::new(path).size(11.0));
            if ui.small_button(icons::ARROW_UP).clicked() {
                workspace.move_scene_entry(index, -1);
                changed = true;
            }
            if ui.small_button(icons::ARROW_DOWN).clicked() {
                workspace.move_scene_entry(index, 1);
                changed = true;
            }
            if ui.small_button(icons::X).clicked() {
                workspace.remove_scene_entry(index);
                changed = true;
            }
        });
    }
    if changed {
        if let Some(root) = workspace.project_root.clone() {
            if let Err(error) = workspace.scene_manifest.write(&root) {
                state.status = format!("scenes.json save failed: {error}");
            }
        }
    }

    // ---- Add scenes that exist on disk but are missing from the manifest ----
    let available = workspace.available_scene_documents();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Add").size(11.0));
        ui.add_enabled_ui(!available.is_empty() && !state.build_running, |ui| {
            egui::ComboBox::from_id_salt("build-add-scene")
                .selected_text(if available.is_empty() {
                    "all scenes in build"
                } else {
                    "scene…"
                })
                .show_ui(ui, |ui| {
                    for path in &available {
                        if ui.selectable_label(false, path).clicked() {
                            if workspace.add_scene_document(path).is_some() {
                                state.status = format!("added {path} to build");
                            }
                        }
                    }
                });
        });
    });

    // ---- The active document may not be part of the build yet ----
    if let Some(path) = workspace.document().path.clone() {
        let relative = path.trim_start_matches("assets/").to_string();
        if relative.ends_with(".scene.json")
            && !workspace.scene_manifest.contains_document(&relative)
        {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("This scene is not in the build")
                        .weak()
                        .size(11.0),
                );
                if ui
                    .small_button("+ Add to build")
                    .on_hover_text("Append this document to assets/scenes.toml")
                    .clicked()
                    && workspace.add_scene_document(&relative).is_some()
                {
                    state.status = format!("added {relative} to build");
                }
            });
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!state.build_running, |ui| {
            if ui.button("Build").clicked() {
                state.pending_build = Some(crate::build::BuildKind::ScenePack);
            }
            if ui.button("Build And Run").clicked() {
                state.pending_build = Some(crate::build::BuildKind::PlayerRun);
            }
        });
        if state.build_running {
            ui.label(RichText::new("building…").weak().size(11.0));
        }
    });
    ui.label(
        RichText::new(
            "Building runs `cargo build` + `prism-build-pipeline build-scenes` on the host; \
             the editor never links the cooker.",
        )
        .weak()
        .size(11.0),
    );
}

/// 快捷键面板（#421）：点键位 → 按新键改绑；冲突 / Esc / Cancel 都给出明确反馈。
pub fn shortcuts(ui: &mut Ui, preferences: &mut Preferences, state: &mut UiState) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("Click a key to rebind it. Esc cancels; Reset restores the default.")
                .weak()
                .size(11.0),
        );
        if ui.button("Reset all").clicked() {
            preferences.shortcut_overrides.clear();
            state.shortcut_capture = None;
            state.status = "shortcuts reset to defaults".to_string();
        }
    });

    if let Some(command) = state.shortcut_capture {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Press a new key for {command:?}"))
                    .strong()
                    .color(Color32::from_rgb(255, 180, 80)),
            );
            if ui.small_button("Cancel").clicked() {
                state.shortcut_capture = None;
            }
        });
        if let Some(captured) = captured_shortcut(ui) {
            match captured {
                CapturedShortcut::Cancel => state.shortcut_capture = None,
                CapturedShortcut::Bound(keys) => match prism_editor_core::KeyChord::parse(&keys) {
                    Some(chord) => {
                        match prism_editor_core::conflicting_command(
                            &preferences.shortcut_overrides,
                            command,
                            &chord,
                        ) {
                            Some(other) => {
                                state.status = format!("{keys} is already bound to {other:?}");
                            }
                            None => {
                                preferences
                                    .shortcut_overrides
                                    .insert(command.id().to_string(), keys.clone());
                                state.status = format!("bound {command:?} to {keys}");
                                state.shortcut_capture = None;
                            }
                        }
                    }
                    None => state.status = format!("cannot bind {keys}"),
                },
            }
        }
        ui.separator();
    }

    egui::ScrollArea::vertical()
        .id_salt("shortcuts-scroll")
        .show(ui, |ui| {
            egui::Grid::new("shortcuts-grid")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for entry in prism_editor_core::SHORTCUTS {
                        let capturing = state.shortcut_capture == Some(entry.command);
                        let overridden = preferences
                            .shortcut_overrides
                            .contains_key(entry.command.id());
                        let keys = prism_editor_core::effective_keys(
                            entry.command,
                            &preferences.shortcut_overrides,
                        )
                        .unwrap_or_else(|| entry.keys.to_string());
                        let text = if capturing {
                            "Press…".to_string()
                        } else {
                            keys
                        };
                        let mut rich = RichText::new(text);
                        if overridden || capturing {
                            rich = rich.strong();
                        }
                        if ui
                            .add(egui::Button::new(rich))
                            .on_hover_text("Click, then press the new key")
                            .clicked()
                        {
                            state.shortcut_capture = Some(entry.command);
                        }
                        ui.label(format!("{:?}", entry.command));
                        if overridden {
                            if ui.small_button("Reset").clicked() {
                                preferences.shortcut_overrides.remove(entry.command.id());
                                if state.shortcut_capture == Some(entry.command) {
                                    state.shortcut_capture = None;
                                }
                            }
                        } else {
                            ui.label("");
                        }
                        ui.end_row();
                    }
                });
        });
}

/// 改键捕获到的一次按键。
enum CapturedShortcut {
    Cancel,
    Bound(String),
}

/// 从这一帧的输入里取第一个按下的键（忽略按住重复）。
fn captured_shortcut(ui: &Ui) -> Option<CapturedShortcut> {
    ui.input(|input| {
        input.events.iter().find_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } => {
                if *key == egui::Key::Escape {
                    Some(CapturedShortcut::Cancel)
                } else {
                    let chord = prism_editor_core::KeyChord::from_event(
                        modifiers.ctrl || modifiers.command,
                        modifiers.shift,
                        modifiers.alt,
                        key.name(),
                    );
                    Some(CapturedShortcut::Bound(chord.display()))
                }
            }
            _ => None,
        })
    })
}

pub fn about(ui: &mut Ui) {
    ui.heading("PrismaRev Editor");
    ui.label("egui + ash Vulkan editor with zero-copy Scene / Game viewports.");
    ui.separator();
    ui.label(
        RichText::new(
            "Scene View renders the edited document with an editor camera; \
             Game View uses the document camera and runs the simulation on Play.",
        )
        .weak()
        .size(11.0),
    );
}

/// Offline Tools：把 `prism-build-pipeline` 的离线能力搬进面板（#477）。
///
/// 编辑器不链接 cooker / bake（AGENTS.md 的硬规则），所以这里和 Build 面板一样
/// 只**拼命令行**、交给 `BuildKind` / `BuildJob` 起子进程；输出流式进 Console，
/// 失败时带退出码。参数默认值抄自 `prism-build-pipeline --help`。
pub fn tools(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    ui.heading("Offline tools");
    ui.label(
        RichText::new(
            "Runs `cargo run -p prism-build-pipeline -- …` on the host. \
             Output streams into the Console; the editor never links the cooker.",
        )
        .weak()
        .size(11.0),
    );
    if state.build_running {
        ui.label(
            RichText::new("a task is already running…")
                .weak()
                .size(11.0),
        );
    }
    ui.separator();

    egui::CollapsingHeader::new("GI Bake")
        .default_open(true)
        .show(ui, |ui| {
            ui.label(
                RichText::new("Offline probe-volume bake (GPU ray query, multi-bounce).")
                    .weak()
                    .size(10.5),
            );
            ui.horizontal(|ui| {
                ui.label("rays");
                ui.add(
                    egui::DragValue::new(&mut state.offline.gi_rays)
                        .speed(4.0)
                        .range(1..=4096),
                )
                .on_hover_text("Rays per probe");
                ui.label("bounces");
                ui.add(
                    egui::DragValue::new(&mut state.offline.gi_bounces)
                        .speed(0.2)
                        .range(1..=16),
                )
                .on_hover_text("Max bounces");
            });
            ui.horizontal(|ui| {
                ui.label("output");
                ui.add(
                    egui::TextEdit::singleline(&mut state.offline.gi_output)
                        .desired_width(ui.available_width().max(120.0)),
                );
            });
            let (output, rays, bounces) = (
                state.offline.gi_output.trim().to_string(),
                state.offline.gi_rays,
                state.offline.gi_bounces,
            );
            ui.add_enabled_ui(!state.build_running, |ui| {
                if ui.button("Bake GI").clicked() {
                    state.pending_build = Some(crate::build::BuildKind::BakeGi {
                        output,
                        rays,
                        bounces,
                    });
                }
            });
        });

    egui::CollapsingHeader::new("Heightmap")
        .default_open(true)
        .show(ui, |ui| {
            ui.label(
                RichText::new("Procedural fbm terrain + hydraulic / thermal erosion (raw f32).")
                    .weak()
                    .size(10.5),
            );
            ui.horizontal(|ui| {
                ui.label("size");
                ui.add(
                    egui::DragValue::new(&mut state.offline.heightmap_width)
                        .speed(8.0)
                        .range(16..=8192),
                );
                ui.label("×");
                ui.add(
                    egui::DragValue::new(&mut state.offline.heightmap_height)
                        .speed(8.0)
                        .range(16..=8192),
                );
            });
            ui.horizontal(|ui| {
                ui.label("seed");
                ui.add(
                    egui::DragValue::new(&mut state.offline.heightmap_seed)
                        .speed(1.0)
                        .range(0..=u64::MAX),
                );
                ui.label("erosion");
                ui.add(
                    egui::DragValue::new(&mut state.offline.heightmap_iterations)
                        .speed(4.0)
                        .range(0..=10_000),
                )
                .on_hover_text("Erosion iterations");
            });
            ui.horizontal(|ui| {
                ui.label("sea level");
                ui.add(
                    egui::DragValue::new(&mut state.offline.heightmap_sea_level)
                        .speed(0.01)
                        .range(0.0..=1.0)
                        .max_decimals(2),
                )
                .on_hover_text("0 = all land, 1 = all ocean (0.7 ≈ Earth)");
            });
            ui.horizontal(|ui| {
                ui.label("output");
                ui.add(
                    egui::TextEdit::singleline(&mut state.offline.heightmap_output)
                        .desired_width(ui.available_width().max(120.0)),
                );
            });
            let (output, width, height, seed, iterations, sea_level) = (
                state.offline.heightmap_output.trim().to_string(),
                state.offline.heightmap_width,
                state.offline.heightmap_height,
                state.offline.heightmap_seed,
                state.offline.heightmap_iterations,
                state.offline.heightmap_sea_level,
            );
            ui.add_enabled_ui(!state.build_running, |ui| {
                if ui.button("Generate Heightmap").clicked() {
                    state.pending_build = Some(crate::build::BuildKind::Heightmap {
                        output,
                        width,
                        height,
                        seed,
                        iterations,
                        sea_level,
                    });
                }
            });
        });

    egui::CollapsingHeader::new("Cook / Build Scenes")
        .default_open(true)
        .show(ui, |ui| {
            ui.label(
                RichText::new("Cook every scene in `assets/scenes.json` into the runtime `.pak`.")
                    .weak()
                    .size(10.5),
            );
            ui.horizontal(|ui| {
                ui.label("output");
                ui.add(
                    egui::TextEdit::singleline(&mut state.offline.pack_output)
                        .desired_width(ui.available_width().max(120.0)),
                );
            });
            ui.horizontal(|ui| {
                ui.label("compression");
                ui.add(
                    egui::DragValue::new(&mut state.offline.pack_compression)
                        .speed(0.2)
                        .range(0..=22),
                )
                .on_hover_text("zstd level (0 = store)");
            });
            let (output, compression) = (
                state.offline.pack_output.trim().to_string(),
                state.offline.pack_compression,
            );
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!state.build_running, |ui| {
                    if ui.button("Build Scenes").clicked() {
                        state.pending_build = Some(crate::build::BuildKind::ScenePackCustom {
                            output,
                            compression,
                        });
                    }
                    // 「Cook 当前场景」用文档路径；面板拿不到 App 的 root，所以自己
                    // 用项目根拼绝对路径（与 Build 面板的 Cook 一致）。
                    let cook_path = workspace
                        .project_root
                        .as_ref()
                        .zip(workspace.document().path.as_ref())
                        .map(|(root, path)| root.join(path));
                    if ui
                        .add_enabled(cook_path.is_some(), egui::Button::new("Cook active scene"))
                        .on_hover_text("Single scene → RSCN (assets/cooked)")
                        .clicked()
                    {
                        if let Some(path) = cook_path {
                            state.pending_build = Some(crate::build::BuildKind::Cook(path));
                        }
                    }
                });
            });
        });
}

/// 关闭所有 `True` 的布尔窗口（`About` / `Shortcuts` 用）。
pub fn boolean_window(
    ctx: &egui::Context,
    open: &mut bool,
    title: &str,
    body: impl FnOnce(&mut Ui),
) {
    if !*open {
        return;
    }
    let mut visible = true;
    egui::Window::new(title)
        .open(&mut visible)
        .resizable(true)
        .default_width(420.0)
        .show(ctx, |ui| body(ui));
    if !visible {
        *open = false;
    }
}

#[cfg(test)]
mod hierarchy_search_tests {
    use super::*;

    #[test]
    fn search_keeps_ancestors_of_matches() {
        let mut doc = SceneDoc {
            version: 3,
            entities: Vec::new(),
            ..Default::default()
        };
        let parent = doc.add_named_child(None, "Boat");
        let child = doc.add_named_child(Some(parent), "Rod");
        doc.add_named_child(None, "Water");

        let keep = search_matches(&doc, "rod", HierarchySearchMode::Name);
        assert!(keep.contains(&child));
        assert!(keep.contains(&parent), "collapsed parent must stay visible");
        assert_eq!(keep.len(), 2);

        assert!(search_matches(&doc, "", HierarchySearchMode::Name).is_empty());
        assert!(search_matches(&doc, "missing", HierarchySearchMode::Name).is_empty());
    }

    #[test]
    fn component_search_matches_the_short_type_name_and_keeps_ancestors() {
        let mut doc = SceneDoc::default();
        let parent = doc.add_named_child(None, "Lamp");
        let _child = doc.add_named_child(Some(parent), "Bulb");
        doc.entities[parent].components.insert(
            "prism_engine::scene::PointLight".to_string(),
            serde_json::json!({ "intensity": 2.0 }),
        );

        let keep = search_matches(&doc, "pointlight", HierarchySearchMode::Component);
        assert!(keep.contains(&parent));
        assert_eq!(keep.len(), 1, "祖先链只补命中项的祖先，不扩子孙");

        // 名字搜索不把组件名当命中；类型搜索也不按实体名匹配。
        assert!(search_matches(&doc, "pointlight", HierarchySearchMode::Name).is_empty());
        assert!(search_matches(&doc, "lamp", HierarchySearchMode::Component).is_empty());
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::test_harness::EguiHarness;
    use std::cell::RefCell;

    struct ToolbarScene {
        workspace: Workspace,
        state: UiState,
        toolbar: Option<HierarchyToolbar>,
    }

    impl ToolbarScene {
        fn new() -> Self {
            let mut workspace = Workspace::sample();
            workspace.select(1, false);
            Self {
                workspace,
                state: UiState::new(),
                toolbar: None,
            }
        }

        fn draw(&mut self, ctx: &egui::Context) {
            let mut toolbar = None;
            egui::CentralPanel::default().show(ctx, |ui| {
                toolbar = Some(hierarchy_toolbar(ui, &mut self.workspace, &mut self.state));
            });
            self.toolbar = toolbar;
        }

        fn toolbar(&self) -> &HierarchyToolbar {
            self.toolbar.as_ref().expect("draw a frame first")
        }
    }

    #[test]
    fn hierarchy_toolbar_search_isolate_lock_and_find_are_interactive() {
        let mut harness = EguiHarness::with_size(1000.0, 220.0);
        let scene = RefCell::new(ToolbarScene::new());
        let mut draw = |ctx: &egui::Context| scene.borrow_mut().draw(ctx);

        harness.frame(&mut draw);
        let search = scene.borrow().toolbar().search.rect;
        harness.click(search.center(), &mut draw);
        harness.text("cube", &mut draw);
        assert_eq!(scene.borrow().state.hierarchy_search, "cube");
        assert!(search_matches(
            &scene.borrow().workspace.document().doc,
            "cube",
            scene.borrow().state.hierarchy_search_mode
        )
        .contains(&1));

        // Isolate appears before the lock/find buttons, so re-read their rects
        // after every state-changing click.
        let isolate = scene.borrow().toolbar().isolate.rect;
        harness.click(isolate.center(), &mut draw);
        assert!(
            scene.borrow().workspace.isolate,
            "isolate button toggles the workspace"
        );
        // The new Show All button shifts the following controls; lay out one
        // more frame before reading their rects.
        harness.frame(&mut draw);

        let lock = scene.borrow().toolbar().lock.rect;
        harness.click(lock.center(), &mut draw);
        let cube_id = scene
            .borrow()
            .workspace
            .document()
            .doc
            .id_of_index(1)
            .expect("cube id");
        assert_eq!(scene.borrow().state.hierarchy_lock, Some(vec![cube_id]));

        let find = scene.borrow().toolbar().find.rect;
        harness.click(find.center(), &mut draw);
        assert!(
            scene.borrow().state.hierarchy_search.is_empty(),
            "find must clear the search that filters the target out"
        );
        assert!(
            scene.borrow().state.hierarchy_lock.is_none(),
            "find must release the old locked selection"
        );
        assert_eq!(scene.borrow().state.hierarchy_reveal, Some(cube_id));
    }

    #[test]
    fn hierarchy_search_mode_combo_switches_to_type() {
        let mut harness = EguiHarness::with_size(1000.0, 220.0);
        let scene = RefCell::new(ToolbarScene::new());
        let mut draw = |ctx: &egui::Context| scene.borrow_mut().draw(ctx);

        harness.frame(&mut draw);
        let mode = scene.borrow().toolbar().mode.rect;
        harness.click(mode.center(), &mut draw);
        // The popup opens below the combo: Name first, Type second.
        harness.click(egui::pos2(mode.center().x, mode.bottom() + 33.0), &mut draw);
        assert_eq!(
            scene.borrow().state.hierarchy_search_mode,
            HierarchySearchMode::Component
        );
    }

    #[test]
    fn shift_click_uses_visible_rows_and_lock_keeps_the_snapshot() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        // Tree order is camera, cube, cube/Detail, light.
        let visible = vec![0, 1, 3, 2];

        apply_hierarchy_select(&mut workspace, &mut state, 1, false, &visible);
        assert_eq!(workspace.selection, vec![1]);
        apply_hierarchy_select(&mut workspace, &mut state, 2, true, &visible);
        assert_eq!(
            workspace.selection,
            vec![1, 2, 3],
            "Shift range walks visible rows and includes the nested Detail"
        );

        let locked_ids: Vec<u32> = workspace
            .selection
            .iter()
            .filter_map(|index| workspace.document().doc.id_of_index(*index))
            .collect();
        toggle_hierarchy_lock(&workspace, &mut state);
        assert_eq!(state.hierarchy_lock, Some(locked_ids));

        // A Scene/Project selection change must not move the locked highlight.
        workspace.select(0, false);
        assert_eq!(hierarchy_indices(&workspace, &mut state), vec![1, 2, 3]);

        // Clicking inside the hierarchy re-syncs the lock to the new selection.
        apply_hierarchy_select(&mut workspace, &mut state, 0, false, &visible);
        sync_hierarchy_lock(&workspace, &mut state);
        let camera_id = workspace.document().doc.id_of_index(0).expect("camera id");
        assert_eq!(state.hierarchy_lock, Some(vec![camera_id]));
    }
}

#[cfg(test)]
mod close_guard_tests {
    use super::*;

    #[test]
    fn dirty_document_close_is_deferred_until_confirmed() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.add_entity("");
        let active = workspace.active_index();
        assert!(workspace.document().dirty);

        let count = workspace.docs.len();
        request_close_document(&mut workspace, &mut state, active);

        assert_eq!(workspace.docs.len(), count, "dirty document must stay open");
        assert_eq!(state.pending_close, Some(CloseTarget::Document(active)));
    }

    #[test]
    fn clean_document_close_is_not_deferred() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();

        request_close_document(&mut workspace, &mut state, 0);

        assert!(state.pending_close.is_none());
    }
}

#[cfg(test)]
mod render_mode_tests {
    use super::*;
    use prism_engine::render_settings::RenderSettings;

    #[test]
    fn every_scene_mode_round_trips_through_render_settings() {
        for mode in SceneRenderMode::ALL {
            let mut settings = RenderSettings::default();
            mode.apply(&mut settings);
            assert_eq!(
                SceneRenderMode::from_settings(&settings),
                Some(mode),
                "{} did not round-trip",
                mode.label()
            );
        }
    }

    #[test]
    fn shaded_clears_flags_and_unknown_combinations_report_none() {
        let mut settings = RenderSettings::default();
        SceneRenderMode::Metallic.apply(&mut settings);
        assert_ne!(settings.debug_flags, 0);
        SceneRenderMode::Shaded.apply(&mut settings);
        assert_eq!((settings.debug_flags, settings.debug_rt), (0, 0));
        assert!(!settings.wireframe);
        assert!(!settings.shaded_wireframe);

        SceneRenderMode::Wireframe.apply(&mut settings);
        assert!(settings.wireframe);
        assert!(!settings.shaded_wireframe);
        assert_eq!((settings.debug_flags, settings.debug_rt), (0, 0));

        SceneRenderMode::ShadedWireframe.apply(&mut settings);
        assert!(!settings.wireframe);
        assert!(settings.shaded_wireframe);
        assert_eq!((settings.debug_flags, settings.debug_rt), (0, 0));

        settings.debug_flags = (1 << 0) | (1 << 3);
        assert_eq!(SceneRenderMode::from_settings(&settings), None);
    }
}

#[cfg(test)]
mod window_smoke_tests {
    use super::*;

    fn run_frame(mut draw: impl FnMut(&mut egui::Ui, &mut Workspace, &mut UiState)) {
        run_frame_with(
            Workspace::sample(),
            UiState::new(),
            |ui, workspace, state| draw(ui, workspace, state),
        );
    }

    fn run_frame_with(
        mut workspace: Workspace,
        mut state: UiState,
        mut draw: impl FnMut(&mut egui::Ui, &mut Workspace, &mut UiState),
    ) {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw(ui, &mut workspace, &mut state);
            });
        });
    }

    #[test]
    fn collapse_merges_identical_log_lines_and_keeps_time_order() {
        let line = |seq: u64, level: LogLevel, text: &str| prism_editor_core::LogLine {
            source: LogSource::Engine,
            level,
            text: text.to_string(),
            seq,
        };
        let lines = vec![
            line(0, LogLevel::Info, "a"),
            line(1, LogLevel::Warn, "b"),
            line(2, LogLevel::Info, "a"),
        ];
        assert_eq!(log_rows(&lines, false), vec![(0, 1), (1, 1), (2, 1)]);
        assert_eq!(log_rows(&lines, true), vec![(0, 2), (1, 1)]);
        // 不同级别 / 来源的同样文本不合并。
        let mut other = line(3, LogLevel::Info, "a");
        other.source = LogSource::Game;
        let mut lines = lines;
        lines.push(other);
        assert_eq!(log_rows(&lines, true), vec![(0, 2), (1, 1), (3, 1)]);
    }

    #[test]
    fn log_location_finds_an_existing_file_with_line_and_column() {
        let temp = std::env::temp_dir().join("prism-log-location-test.rs");
        std::fs::write(&temp, "// x").unwrap();
        let text = format!("thread panicked at {}:12:3", temp.display());
        let found = log_location(&text).expect("location");
        assert_eq!(found.0, temp.display().to_string());
        assert_eq!(found.1, Some(12));
        assert_eq!(found.2, Some(3));
        // 行号可省略；不存在的文件不算位置。
        let only_file = format!("see {}", temp.display());
        assert_eq!(log_location(&only_file).map(|found| found.1), Some(None));
        assert!(log_location("no path here").is_none());
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn tilemap_cells_cycle_through_the_palette() {
        // #529：左键 0 → 1 → … → palette → 0，右键清零由 UI 直接写 0。
        assert_eq!(next_tile(0, 4), 1);
        assert_eq!(next_tile(3, 4), 4);
        assert_eq!(next_tile(4, 4), 0);
        assert_eq!(next_tile(9, 4), 0, "越界的旧值也回到空");
        assert_eq!(next_tile(0, 0), 1, "palette 至少为 1");

        let value = serde_json::json!({ "columns": 3, "tiles": [0, 2] });
        assert_eq!(tilemap_columns(&value), 3);
        assert_eq!(tilemap_tiles(&value), vec![0, 2]);
        assert!(tilemap_tiles(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn a_tilemap_entity_renders_the_grid_editor() {
        let mut workspace = Workspace::sample();
        let index = workspace.document_mut().doc.add_named_child(None, "Ground");
        let id = workspace.document().doc.id_of_index(index).unwrap();
        workspace.document_mut().doc.entities[index]
            .components
            .insert(
                TILEMAP_COMPONENT.to_string(),
                serde_json::json!({ "columns": 2, "tiles": [1, 0, 2, 0] }),
            );
        workspace.selection = vec![index];
        assert_eq!(workspace.document().doc.id_of_index(index), Some(id));

        run_frame_with(workspace, UiState::new(), |ui, workspace, state| {
            inspector(ui, workspace, state)
        });
    }

    #[test]
    fn asset_search_supports_type_and_path_filters() {
        // #419：`t:` 认英文 slug / Debug 名 / 中文标签，其余词按 AND 匹配路径。
        let query = AssetSearch::parse("t:texture sprites");
        assert_eq!(query.kinds, vec![AssetKind::Texture]);
        assert_eq!(query.needles, vec!["sprites".to_string()]);
        assert!(query.matches("assets/sprites/boat.png", AssetKind::Texture));
        assert!(!query.matches("assets/sprites/boat.png", AssetKind::Scene));
        assert!(!query.matches("assets/models/boat.gltf", AssetKind::Texture));

        assert_eq!(
            AssetSearch::parse("t:材质").kinds,
            vec![AssetKind::Material],
            "中文标签也认"
        );
        assert_eq!(
            AssetSearch::parse("t:Material").kinds,
            vec![AssetKind::Material]
        );
        // 普通词之间是 AND；未知前缀按文本处理。
        let both = AssetSearch::parse("sprites boat");
        assert!(both.matches("assets/sprites/boat.png", AssetKind::Texture));
        assert!(!both.matches("assets/sprites/water.png", AssetKind::Texture));
        assert!(AssetSearch::parse("zz:foo").matches("assets/zz:foo/x.png", AssetKind::Other));
        // 空查询 = 不过滤（列当前目录）。
        assert!(!AssetSearch::parse("   ").is_active());
    }

    #[test]
    fn the_grid_wraps_cells_by_the_available_width() {
        assert_eq!(grid_columns(400.0, 92.0), 4);
        assert_eq!(grid_columns(90.0, 92.0), 1, "窄面板至少一列");
        assert_eq!(grid_columns(0.0, 92.0), 1);
        assert_eq!(grid_columns(f32::NAN, 92.0), 1);
        assert_eq!(truncate_label("short.png", 14), "short.png");
        assert_eq!(truncate_label("a-very-long-asset-name.png", 6), "a-ver…");
    }

    #[test]
    fn grid_and_two_column_modes_render() {
        for (view, columns) in [
            (AssetView::Grid, AssetColumns::One),
            (AssetView::Grid, AssetColumns::Two),
            (AssetView::List, AssetColumns::Two),
        ] {
            let mut workspace = Workspace::sample();
            let mut state = UiState::new();
            state.asset_view = view;
            state.asset_columns = columns;
            run_frame_with(workspace, state, |ui, workspace, state| {
                assets(ui, workspace, state);
            });
        }
    }

    #[test]
    fn ping_selects_the_object_that_references_the_asset() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.document_mut().doc.entities[0].components.insert(
            "prism_engine::scene::MeshRenderer".to_string(),
            serde_json::json!({ "mesh_path": "assets/models/probe.gltf" }),
        );
        state.asset_selection = vec!["assets/models/probe.gltf".to_string()];

        ping_asset(&mut workspace, &mut state, "assets/models/probe.gltf");

        assert_eq!(workspace.selection, vec![0], "ping 要选中引用它的实体");
        assert!(state.asset_selection.is_empty(), "选中实体后退出资产检查器");
        assert!(state.status.contains("pinged"), "status: {}", state.status);

        // 没有引用时只写状态栏，不动选择。
        workspace.clear_selection();
        ping_asset(&mut workspace, &mut state, "assets/models/missing.gltf");
        assert!(workspace.selection.is_empty());
        assert!(state.status.contains("no object references"));
    }

    #[test]
    fn transform_row_actions_emit_the_expected_edits() {
        // #418：Transform 行的 Copy / Paste / Reset 都收敛到同一个写入口。
        let mut edits = Vec::new();
        apply_transform_row(TransformRow::Position, &[1.0, 2.0, 3.0], &mut |edit| {
            edits.push(edit);
        });
        apply_transform_row(TransformRow::Rotation, &[0.0, 0.0, 0.0, 1.0], &mut |edit| {
            edits.push(edit);
        });
        apply_transform_row(TransformRow::Scale, &[2.0, 2.0, 2.0], &mut |edit| {
            edits.push(edit);
        });
        // 形状不对（少分量）不产生编辑。
        apply_transform_row(TransformRow::Rotation, &[1.0, 0.0], &mut |edit| {
            edits.push(edit);
        });
        assert_eq!(edits.len(), 3);
        assert!(matches!(
            &edits[0],
            TransformEdit::Vector { field: "position", values } if *values == [1.0, 2.0, 3.0]
        ));
        assert!(matches!(&edits[1], TransformEdit::Rotation(_)));
        assert!(matches!(
            &edits[2],
            TransformEdit::Vector { field: "scale", .. }
        ));

        // 重置值：位置 0、旋转单位四元数、缩放 1。
        assert_eq!(TransformRow::Position.reset(), vec![0.0, 0.0, 0.0]);
        assert_eq!(TransformRow::Rotation.reset(), vec![0.0, 0.0, 0.0, 1.0]);
        assert_eq!(TransformRow::Scale.reset(), vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn a_component_without_schema_gets_an_editable_json_fallback() {
        // #418：没有 schema 的组件不再只显示「(no schema)」，而是可编辑 JSON。
        let mut workspace = Workspace::sample();
        workspace.document_mut().doc.entities[0].components.insert(
            "test::Custom".to_string(),
            serde_json::json!({"speed": 3.0}),
        );
        workspace.select(0, false);
        let mut state = UiState::new();
        let before = workspace.document().doc.to_json_string();

        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                inspector(ui, &mut workspace, &mut state);
            });
        });

        // 只是渲染，不点 Apply 就不写文档、不产生撤销步。
        assert_eq!(workspace.document().doc.to_json_string(), before);
        assert!(workspace.undo_history().is_empty());
    }

    #[test]
    fn locking_the_inspector_pins_entity_ids() {
        // #418：锁定存实体 id，去别处点选时检查器内容不换。
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.select(1, false);
        toggle_inspector_lock(&workspace, &mut state);
        assert!(state.inspector_lock.is_some());
        assert_eq!(inspector_indices(&workspace, &mut state), vec![1]);

        workspace.select(0, false);
        assert_eq!(
            inspector_indices(&workspace, &mut state),
            vec![1],
            "锁定时忽略新的选择"
        );

        toggle_inspector_lock(&workspace, &mut state);
        assert!(state.inspector_lock.is_none());
        assert_eq!(inspector_indices(&workspace, &mut state), vec![0]);
    }

    #[test]
    fn locking_the_hierarchy_pins_the_highlighted_rows() {
        // #417：锁定时 Scene / Project 的新选择不改层级高亮；层级内部点选会同步。
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.select(1, false);
        toggle_hierarchy_lock(&workspace, &mut state);
        assert!(state.hierarchy_lock.is_some());
        assert_eq!(hierarchy_indices(&workspace, &mut state), vec![1]);

        workspace.select(0, false);
        assert_eq!(
            hierarchy_indices(&workspace, &mut state),
            vec![1],
            "外部选择不改变锁定的层级高亮"
        );

        sync_hierarchy_lock(&workspace, &mut state);
        assert_eq!(
            hierarchy_indices(&workspace, &mut state),
            vec![0],
            "层级内部点选会更新锁定快照"
        );

        toggle_hierarchy_lock(&workspace, &mut state);
        assert!(state.hierarchy_lock.is_none());
        assert_eq!(hierarchy_indices(&workspace, &mut state), vec![0]);
    }

    #[test]
    fn deleting_the_hierarchy_lock_target_releases_the_lock() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.select(1, false);
        toggle_hierarchy_lock(&workspace, &mut state);
        workspace.delete_selection();
        let indices = hierarchy_indices(&workspace, &mut state);
        assert!(
            state.hierarchy_lock.is_none(),
            "锁住的对象被删掉后要自动解锁"
        );
        assert_eq!(indices, workspace.selection);
    }

    #[test]
    fn reveal_expands_ancestors_and_clears_filters() {
        // #417：Find in Hierarchy 要清掉会过滤掉目标的搜索、展开祖先并请求滚动。
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        let detail = 3;
        let detail_id = workspace.document().doc.id_of_index(detail).unwrap();
        let cube_id = workspace.document().doc.id_of_index(1).unwrap();
        state.hierarchy_search = "not-a-match".to_string();
        state.hierarchy_lock = Some(vec![detail_id]);
        workspace.expanded.clear();

        reveal_in_hierarchy(&mut workspace, &mut state, detail);

        assert!(state.hierarchy_search.is_empty());
        assert!(state.hierarchy_lock.is_none());
        assert!(workspace.expanded.contains(&cube_id), "父级要展开");
        assert!(workspace.expanded.contains(&detail_id), "目标自身也可展开");
        assert_eq!(state.hierarchy_reveal, Some(detail_id));
    }

    #[test]
    fn deleting_the_locked_entity_releases_the_lock() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        workspace.select(1, false);
        toggle_inspector_lock(&workspace, &mut state);
        let locked_id = workspace.document().doc.id_of_index(1).expect("entity id");

        workspace.document_mut().doc.entities.remove(1);
        assert!(workspace.document().doc.index_of_id(locked_id).is_none());
        let indices = inspector_indices(&workspace, &mut state);
        assert!(
            state.inspector_lock.is_none(),
            "锁住的对象被删掉后要自动解锁"
        );
        assert_eq!(indices, workspace.selection.clone(), "回落到文档选择");
    }

    #[test]
    fn play_mode_banner_only_appears_while_playing() {
        // #500：非播放态不占空间，播放态给出一句「Stop 会丢弃改动」。
        assert_eq!(play_mode_banner(false), None);
        let banner = play_mode_banner(true).expect("playing banner");
        assert!(banner.contains("discarded"), "{banner}");
        // 播放 / 暂停用不同颜色，和状态栏的 PLAYING / PAUSED 对应。
        assert_ne!(play_mode_tint(false), play_mode_tint(true));
    }

    #[test]
    fn the_inspector_shows_the_play_banner_without_editing_the_document() {
        let mut workspace = Workspace::sample();
        workspace.play();
        let mut state = UiState::new();
        let before = workspace.document().doc.to_json_string();

        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                inspector(ui, &mut workspace, &mut state);
            });
        });

        assert_eq!(
            workspace.document().doc.to_json_string(),
            before,
            "只是显示提示，不能碰文档"
        );
        assert!(workspace.undo_history().is_empty(), "也不能产生撤销步");

        // Stop 之后横幅消失（文档已恢复）。
        workspace.stop();
        assert_eq!(
            play_mode_banner(workspace.playing && workspace.has_play_snapshot()),
            None
        );
    }

    #[test]
    fn runtime_debug_lines_name_states_and_montage_slots() {
        let node = prism_anim::GraphNodeSnapshot {
            name: "sm".into(),
            kind: prism_anim::GraphNodeKind::StateMachine,
            time: 0.5,
            state: 1,
            blending: false,
            blend_weight: 1.0,
            cached: false,
            weight: 1.0,
            state_name: "walk".into(),
            state_clip: "walk.ranim".into(),
        };
        let line = runtime_node_summary(&node);
        assert!(line.contains("sm"), "{line}");
        assert!(line.contains("StateMachine"), "{line}");
        assert!(line.contains("walk"), "{line}");
        assert!(line.contains("0.50s"), "{line}");

        let montage = prism_anim::MontageSnapshot {
            slot: "hit".into(),
            clip: Some("hit.anim".into()),
            section: Some("Impact".into()),
            warp_window: Some("VaultWarp".into()),
            warp_window_progress: Some(0.5),
            time: Some(0.25),
            weight: 0.5,
            playing: true,
            paused: true,
            finished: false,
            queued: 2,
        };
        let line = runtime_montage_summary(&montage);
        assert!(line.contains("hit"), "{line}");
        assert!(line.contains("0.25s"), "{line}");
        assert!(line.contains("paused"), "{line}");
        assert!(line.contains("+2 queued"), "{line}");
    }

    #[test]
    fn runtime_event_lines_show_name_and_time() {
        let event = prism_engine::scene::systems::animator::AnimEvent {
            entity: prism_engine::ecs::Entity::from_raw(7, 0),
            name: "footstep".into(),
            time: 0.3,
        };
        let line = runtime_event_summary(&event);
        assert!(line.contains("footstep"), "{line}");
        assert!(line.contains("0.30s"), "{line}");
    }

    #[test]
    fn per_pass_budget_splits_the_frame_budget_and_handles_zero() {
        assert_eq!(per_pass_budget_ms(0), 0.0);
        assert!((per_pass_budget_ms(1) - FRAME_BUDGET_MS).abs() < 1e-6);
        assert!((per_pass_budget_ms(4) - FRAME_BUDGET_MS / 4.0).abs() < 1e-6);
        assert!(per_pass_budget_ms(8) < per_pass_budget_ms(4));

        // 用户自定义整帧预算（#518）：按当前计时 pass 数均分。
        assert_eq!(per_pass_budget_ms_with_total(0, 8.0), 0.0);
        assert!((per_pass_budget_ms_with_total(4, 8.0) - 2.0).abs() < 1e-6);
        assert_eq!(
            per_pass_budget_ms_with_total(4, 0.0),
            0.0,
            "非法预算不能产生 NaN/Inf 份额"
        );
        assert_eq!(per_pass_budget_ms_with_total(4, f32::NAN), 0.0);
    }

    #[test]
    fn stepped_call_moves_between_draw_calls_without_wrapping() {
        assert_eq!(stepped_call(None, 0, 1), None);
        assert_eq!(stepped_call(Some(0), 0, 1), None);
        // 第一次 Next 从 0 开始，第一次 Prev 从最后一条开始。
        assert_eq!(stepped_call(None, 3, 1), Some(0));
        assert_eq!(stepped_call(None, 3, -1), Some(2));
        assert_eq!(stepped_call(Some(0), 3, -1), Some(0));
        assert_eq!(stepped_call(Some(2), 3, 1), Some(2));
        assert_eq!(stepped_call(Some(1), 3, 1), Some(2));
        // 冻结帧换了一份更短的报告时，旧下标被夹回范围内。
        assert_eq!(stepped_call(Some(9), 2, 0), Some(1));
    }

    #[test]
    fn every_editor_panel_builds_a_frame() {
        run_frame(|ui, workspace, state| hierarchy(ui, workspace, state));
        run_frame(|ui, workspace, state| anim_graph(ui, workspace, state, None));
        run_frame(|ui, workspace, state| inspector(ui, workspace, state));
        run_frame(|ui, workspace, state| assets(ui, workspace, state));
        run_frame(|ui, workspace, state| docs(ui, workspace, state));
        run_frame(|ui, workspace, state| log(ui, workspace, state));
        run_frame(|ui, workspace, state| history(ui, workspace, state));
        run_frame(|ui, _workspace, _state| {
            let mut settings = prism_engine::render_settings::RenderSettings::default();
            let mut viewport_scale = 1.0;
            let _ = render_graph(
                ui,
                None,
                &mut settings,
                &mut viewport_scale,
                &mut false,
                &mut None,
                &mut None,
            );
        });
        let report = r#"{"generatedAtMs":1,"frame":2,"passes":[],"resources":[],
            "stats":{"textures":3,"textureBytes":1024,"meshes":2,"meshBytes":2048,
                     "materials":4,"materialBytes":384},
            "drawCalls":[
              {"pass":"forward","label":"mesh #0 · material 0","instances":1,
               "triangles":12,"visible":true,
               "state":"queue 2000 · cull Back · z-write true · One/Zero Add"},
              {"pass":"sprite","label":"SrcAlpha / OneMinusSrcAlpha","instances":4,
               "triangles":8,"visible":false}
            ]}"#;
        run_frame(|ui, _workspace, _state| {
            let mut settings = prism_engine::render_settings::RenderSettings::default();
            let mut viewport_scale = 1.0;
            let _ = render_graph(
                ui,
                Some(report),
                &mut settings,
                &mut viewport_scale,
                &mut true,
                &mut Some(1),
                &mut None,
            );
        });
        run_frame(|ui, _workspace, state| project_settings(ui, None, state));
        run_frame(|ui, workspace, state| {
            let mut prefs = Preferences::default();
            preferences(ui, &mut prefs, workspace, state, false);
        });
        run_frame(|ui, workspace, _state| theme(ui, workspace));
        run_frame(|ui, workspace, state| build(ui, workspace, state));
        run_frame(|ui, _workspace, state| {
            let mut prefs = Preferences::default();
            shortcuts(ui, &mut prefs, state);
        });
        run_frame(|ui, _workspace, _state| about(ui));
        run_frame(|ui, workspace, state| tools(ui, workspace, state));
    }

    #[test]
    fn boolean_window_builds_and_can_be_closed() {
        let ctx = egui::Context::default();
        let mut open = true;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            boolean_window(ctx, &mut open, "Test", |ui| {
                ui.label("content");
            });
        });
        assert!(open);
    }

    #[test]
    fn confirmation_actions_can_run_without_the_dialog_widget() {
        let mut workspace = Workspace::sample();
        let mut state = UiState::new();
        let before = workspace.document().doc.entities.len();
        state.pending_entity_delete = Some(vec![0]);
        apply_pending_entity_delete(&mut workspace, &mut state);
        assert!(workspace.document().doc.entities.len() < before);
        assert!(state.pending_entity_delete.is_none());
    }

    #[test]
    fn the_layout_group_editor_renders_without_a_panic() {
        // #527：`Style.layout` 是嵌套 struct，通用控件只读；这个编辑块必须
        // 对 Row / Column / None 三种状态都能画出来。
        for direction in ["None", "Row", "Column"] {
            let layout = serde_json::json!({
                "direction": direction,
                "spacing": 8,
                "padding": {"left": 4, "right": 4, "top": 2, "bottom": 2},
                "child_align": "Stretch",
                "fit_content": true
            });
            let ctx = egui::Context::default();
            let mut workspace = Workspace::sample();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    layout_editor(ui, &mut workspace, 1, &layout);
                });
            });
        }
    }

    #[test]
    fn the_layout_editor_writes_the_same_json_paths_the_system_reads() {
        let mut workspace = Workspace::sample();
        let index = workspace.document_mut().doc.add_named_child(None, "Panel");
        let id = workspace.document().doc.id_of_index(index).unwrap();
        workspace.document_mut().doc.entities[index]
            .components
            .insert(
                STYLE_COMPONENT.to_string(),
                serde_json::json!({
                    "layout": {
                        "direction": "None",
                        "spacing": 0,
                        "padding": {"left": 0, "right": 0, "top": 0, "bottom": 0},
                        "child_align": "Start",
                        "fit_content": false
                    }
                }),
            );
        workspace.set_field(
            id,
            STYLE_COMPONENT,
            "layout.direction",
            serde_json::json!("Column"),
        );
        workspace.set_field(
            id,
            STYLE_COMPONENT,
            "layout.padding.left",
            serde_json::json!(12),
        );
        let layout =
            &workspace.document().doc.entities[index].components[STYLE_COMPONENT]["layout"];
        assert_eq!(layout["direction"], serde_json::json!("Column"));
        assert_eq!(layout["padding"]["left"].as_f64(), Some(12.0));
    }

    #[test]
    fn panel_row_metrics_follow_compact_preference() {
        assert_eq!(tree_row_height(true), 15.0);
        assert_eq!(tree_row_height(false), 18.0);
        assert_eq!(row_height(true), 16.0);
        assert_eq!(row_height(false), 20.0);
    }

    #[test]
    fn mixed_transform_axes_flags_only_the_differing_components() {
        let mut workspace = Workspace::sample();
        workspace.clear_selection();
        workspace.document_mut().doc.entities[0].transform.position = [1.0, 2.0, 3.0];
        workspace.document_mut().doc.entities[1].transform.position = [1.0, 5.0, 3.0];
        let mixed = mixed_transform_axes(&workspace, &[0, 1]);
        assert!(!mixed[0][0]);
        assert!(mixed[0][1], "只有 Y 不同");
        assert!(!mixed[0][2]);
        assert!(mixed[2].iter().all(|flag| !flag), "scale 相同时不标记");

        // 只选一个实体时永远没有 mixed。
        let single = mixed_transform_axes(&workspace, &[0]);
        assert!(single.iter().all(|row| row.iter().all(|flag| !flag)));
    }

    #[test]
    fn mixed_transform_axes_ignores_the_quaternion_double_cover() {
        let mut workspace = Workspace::sample();
        workspace.document_mut().doc.entities[0].transform.rotation =
            prism_editor_core::viewport_math::euler_degrees_to_quat([0.0, 45.0, 0.0]);
        // q 与 -q 表示同一个旋转：不该被当成「值不一致」。
        workspace.document_mut().doc.entities[1].transform.rotation =
            prism_editor_core::viewport_math::euler_degrees_to_quat([0.0, 45.0, 0.0])
                .map(|value| -value);
        let mixed = mixed_transform_axes(&workspace, &[0, 1]);
        assert!(mixed[1].iter().all(|flag| !flag), "{mixed:?}");
    }
}

#[cfg(test)]
mod asset_browser_tests {
    use super::*;

    fn files() -> Vec<(String, AssetKind, u64)> {
        vec![
            ("assets/b.png".to_string(), AssetKind::Texture, 300),
            ("assets/a.json".to_string(), AssetKind::Data, 100),
            ("assets/c.png".to_string(), AssetKind::Texture, 200),
        ]
    }

    #[test]
    fn sorting_by_name_ignores_the_scan_order() {
        let mut list = files();
        sort_assets(&mut list, AssetSort::Name, false);
        let names: Vec<&str> = list.iter().map(|entry| entry.0.as_str()).collect();
        assert_eq!(
            names,
            ["assets/a.json", "assets/b.png", "assets/c.png"],
            "按文件名升序，与 scan_assets 的顺序无关"
        );
        sort_assets(&mut list, AssetSort::Name, true);
        let names: Vec<&str> = list.iter().map(|entry| entry.0.as_str()).collect();
        assert_eq!(names, ["assets/c.png", "assets/b.png", "assets/a.json"]);
    }

    #[test]
    fn sorting_by_size_and_kind_breaks_ties_by_name() {
        let mut list = files();
        sort_assets(&mut list, AssetSort::Size, false);
        let order: Vec<u64> = list.iter().map(|entry| entry.2).collect();
        assert_eq!(order, [100, 200, 300]);

        // 两个纹理（同 kind）之间按名字排，不会因为排序不稳定而互换。
        sort_assets(&mut list, AssetSort::Kind, false);
        let names: Vec<&str> = list.iter().map(|entry| entry.0.as_str()).collect();
        assert_eq!(
            names,
            ["assets/b.png", "assets/c.png", "assets/a.json"],
            "Texture(1) 在 Data(7) 之前，同 kind 内按文件名"
        );
    }

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn clicking_without_modifiers_replaces_the_selection() {
        let mut state = UiState::new();
        state.select_asset_only("assets/a.png");
        state.select_asset_only("assets/b.png");
        assert_eq!(state.asset_selection, paths(&["assets/b.png"]));
        assert_eq!(
            state.asset_primary().map(String::as_str),
            Some("assets/b.png")
        );
        assert_eq!(state.asset_anchor.as_deref(), Some("assets/b.png"));
    }

    #[test]
    fn ctrl_click_toggles_a_single_item() {
        let mut state = UiState::new();
        state.select_asset_only("assets/a.png");
        state.toggle_asset("assets/b.png");
        assert_eq!(
            state.asset_selection,
            paths(&["assets/a.png", "assets/b.png"])
        );
        state.toggle_asset("assets/a.png");
        assert_eq!(state.asset_selection, paths(&["assets/b.png"]));
        assert!(state.asset_selected("assets/b.png"));
    }

    #[test]
    fn shift_click_selects_the_range_from_the_anchor() {
        let visible = paths(&["a", "b", "c", "d", "e"]);
        let mut state = UiState::new();
        state.select_asset_only("b");
        state.select_asset_range(&visible, "d");
        assert_eq!(state.asset_selection, paths(&["b", "c", "d"]));
        // 反向拖拽也是同一段。
        state.select_asset_range(&visible, "a");
        assert_eq!(state.asset_selection, paths(&["a", "b"]));

        // 锚点不在当前列表里（切过目录）：退化成普通单选。
        let mut state = UiState::new();
        state.select_asset_only("elsewhere/x");
        state.select_asset_range(&visible, "c");
        assert_eq!(state.asset_selection, paths(&["c"]));
    }

    #[test]
    fn forgetting_a_deleted_asset_keeps_the_rest_selected() {
        let mut state = UiState::new();
        state.asset_selection = paths(&["a", "b", "c"]);
        state.asset_anchor = Some("b".to_string());
        state.forget_asset("b");
        assert_eq!(state.asset_selection, paths(&["a", "c"]));
        assert_eq!(state.asset_anchor, None);
        state.clear_asset_selection();
        assert!(state.asset_selection.is_empty());
        assert!(state.asset_anchor.is_none());
    }

    #[test]
    fn rename_target_rejects_paths_and_keeps_the_folder() {
        assert_eq!(
            rename_target("assets/sprites/boat.png", " canoe.png "),
            Some("assets/sprites/canoe.png".to_string())
        );
        // 只改文件名：不允许顺手换目录，也不允许空名字。
        assert_eq!(rename_target("assets/boat.png", "sub/boat.png"), None);
        assert_eq!(rename_target("assets/boat.png", "   "), None);
        assert_eq!(rename_target("assets/boat.png", "a:b.png"), None);
    }
}

// ---------------------------------------------------------------------------
// 预制体重写
// ---------------------------------------------------------------------------

/// 预制体实例相对源预制体改了哪些字段（Unity 的 Prefab Overrides 窗口）。
///
/// 数据来自 `Workspace::prefab_overrides`：按 `PrefabInstance` 指向的文件实时
/// 比对，文档里不存快照，所以打开场景时不需要迁移格式。
pub fn prefab_overrides(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    let Some(root) = workspace.primary_selection() else {
        ui.label(
            RichText::new("select a prefab instance root in the hierarchy")
                .weak()
                .size(11.0),
        );
        return;
    };
    let linked = workspace
        .document()
        .doc
        .entities
        .get(root)
        .and_then(|entity| entity.prefab_link())
        .map(|path| path.to_string());
    let Some(prefab_path) = linked else {
        ui.label(
            RichText::new("the selected entity is not a prefab instance")
                .weak()
                .size(11.0),
        );
        return;
    };

    let overrides = match workspace.prefab_overrides(root) {
        Ok(list) => list,
        Err(error) => {
            ui.label(RichText::new(format!("cannot read {prefab_path}: {error}")).weak());
            return;
        }
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{prefab_path}")).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("Unpack")
                .on_hover_text("Remove the prefab link, keep the objects")
                .clicked()
            {
                let removed = workspace.unpack_prefab(root);
                state.status = format!("unpacked ({removed} link(s) removed)");
            }
        });
    });
    ui.separator();

    if overrides.is_empty() {
        ui.label(RichText::new("no overrides").weak().size(11.0));
        return;
    }
    ui.label(
        RichText::new(format!("{} override(s)", overrides.len()))
            .size(11.0)
            .weak(),
    );

    let mut revert: Option<prism_editor_core::workspace::PrefabOverride> = None;
    let mut apply: Option<prism_editor_core::workspace::PrefabOverride> = None;
    egui::ScrollArea::vertical()
        .id_salt("prefab-overrides")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for entry in &overrides {
                ui.horizontal(|ui| {
                    let field = if entry.field.is_empty() {
                        entry.component.clone()
                    } else {
                        format!("{}.{}", entry.component, entry.field)
                    };
                    let label = format!("{} · {field}", entry.entity_name);
                    ui.label(RichText::new(label).size(11.0))
                        .on_hover_text(format!(
                            "instance: {}\nprefab: {}",
                            preview_value(&entry.instance_value),
                            preview_value(&entry.prefab_value)
                        ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("Apply")
                            .on_hover_text("Write to the prefab")
                            .clicked()
                        {
                            apply = Some(entry.clone());
                        }
                        if ui
                            .small_button("Revert")
                            .on_hover_text("Copy the prefab value back")
                            .clicked()
                        {
                            revert = Some(entry.clone());
                        }
                    });
                });
            }
        });

    if let Some(entry) = revert {
        if workspace.revert_prefab_override(&entry) {
            state.status = format!(
                "reverted {} · {}",
                entry.entity_name,
                if entry.field.is_empty() {
                    entry.component.clone()
                } else {
                    format!("{}.{}", entry.component, entry.field)
                }
            );
        }
    }
    if let Some(entry) = apply {
        match workspace.apply_prefab_override(root, &entry) {
            Ok(()) => {
                state.status = format!(
                    "applied {} · {}",
                    entry.entity_name,
                    if entry.field.is_empty() {
                        entry.component.clone()
                    } else {
                        format!("{}.{}", entry.component, entry.field)
                    }
                );
            }
            Err(error) => state.status = format!("apply failed: {error}"),
        }
    }
}

/// 覆盖值的一行预览（太长就截断，完整值在 hover 里）。
fn preview_value(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 80 {
        text
    } else {
        format!("{}…", text.chars().take(79).collect::<String>())
    }
}

/// `Style` 组件的名字（布局组编辑块用）。
const STYLE_COMPONENT: &str = "prism_engine::ui::Style";

/// UI 布局组的编辑块（#527）。
///
/// 只写 `layout.*` 这几个字段：`direction` / `spacing` / `padding` /
/// `child_align` / `fit_content`。`direction = None` 时其余控件隐藏，
/// 元素继续按锚点定位（历史行为）。
fn layout_editor(ui: &mut Ui, workspace: &mut Workspace, entity_id: u32, layout: &Value) {
    let direction = layout
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("None")
        .to_string();
    ui.label(RichText::new("Layout Group").strong().size(12.0));
    egui::Grid::new(format!("layout-group-{entity_id}"))
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Direction").size(11.0));
            egui::ComboBox::from_id_salt(format!("layout-direction-{entity_id}"))
                .selected_text(direction.as_str())
                .show_ui(ui, |ui| {
                    for option in ["None", "Row", "Column"] {
                        if ui.selectable_label(direction == option, option).clicked() {
                            workspace.set_field(
                                entity_id,
                                STYLE_COMPONENT,
                                "layout.direction",
                                json!(option),
                            );
                        }
                    }
                });
            ui.end_row();

            if direction != "None" {
                ui.label(RichText::new("Spacing").size(11.0));
                let mut spacing =
                    layout.get("spacing").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                if ui
                    .add(
                        egui::DragValue::new(&mut spacing)
                            .speed(0.5)
                            .range(0.0..=512.0),
                    )
                    .changed()
                {
                    workspace.set_field(
                        entity_id,
                        STYLE_COMPONENT,
                        "layout.spacing",
                        json!(spacing),
                    );
                }
                ui.end_row();

                ui.label(RichText::new("Padding").size(11.0));
                ui.horizontal(|ui| {
                    for (key, label) in
                        [("left", "L"), ("right", "R"), ("top", "T"), ("bottom", "B")]
                    {
                        let mut value = layout
                            .get("padding")
                            .and_then(|padding| padding.get(key))
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0) as f32;
                        ui.label(RichText::new(label).size(10.0).weak());
                        if ui
                            .add(
                                egui::DragValue::new(&mut value)
                                    .speed(0.5)
                                    .range(0.0..=1024.0),
                            )
                            .changed()
                        {
                            workspace.set_field(
                                entity_id,
                                STYLE_COMPONENT,
                                &format!("layout.padding.{key}"),
                                json!(value),
                            );
                        }
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Child align").size(11.0));
                let align = layout
                    .get("child_align")
                    .and_then(Value::as_str)
                    .unwrap_or("Start")
                    .to_string();
                egui::ComboBox::from_id_salt(format!("layout-align-{entity_id}"))
                    .selected_text(align.as_str())
                    .show_ui(ui, |ui| {
                        for option in ["Start", "Center", "End", "Stretch"] {
                            if ui.selectable_label(align == option, option).clicked() {
                                workspace.set_field(
                                    entity_id,
                                    STYLE_COMPONENT,
                                    "layout.child_align",
                                    json!(option),
                                );
                            }
                        }
                    });
                ui.end_row();

                ui.label(RichText::new("Fit content").size(11.0));
                let mut fit = layout
                    .get("fit_content")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ui.checkbox(&mut fit, "").changed() {
                    workspace.set_field(
                        entity_id,
                        STYLE_COMPONENT,
                        "layout.fit_content",
                        json!(fit),
                    );
                }
                ui.end_row();
            }
        });
}

/// 纹理导入设置的旁挂路径（与 cooker 的 `ImportSettings::sidecar_path` 同约定）。
fn import_settings_path(asset: &str) -> String {
    format!("{asset}.import.json")
}

/// 读旁挂 JSON 的 `maxSize` / `generateMips`；缺失或坏文件回落到 `(0, true)`。
fn read_import_settings(path: &std::path::Path) -> (u32, bool) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, true);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (0, true);
    };
    let max_size = value.get("maxSize").and_then(Value::as_u64).unwrap_or(0) as u32;
    let mips = value
        .get("generateMips")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    (max_size, mips)
}

/// 旁挂 JSON 的写回形状（cooker 的 `ImportSettings` 读同一组键）。
fn import_settings_json(max_size: u32, generate_mips: bool) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "maxSize": max_size,
        "generateMips": generate_mips,
    }))
    .unwrap_or_default()
}

/// 纹理导入设置（#514）：`Max Size` / `Mips`，Apply 写旁挂、Revert 重读、Reimport 重打场景包。
fn texture_import_settings(ui: &mut Ui, workspace: &Workspace, state: &mut UiState, asset: &str) {
    let Some(root) = workspace.project_root.clone() else {
        return;
    };
    let sidecar = root.join(import_settings_path(asset));
    let id = egui::Id::new(("import-settings", asset));
    let mut buffer = ui
        .data(|data| data.get_temp::<(u32, bool)>(id))
        .unwrap_or_else(|| read_import_settings(&sidecar));

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Import Settings").strong().size(11.0));
        ui.label(
            RichText::new("max size / mips take effect after Reimport")
                .weak()
                .size(10.0),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Max Size");
        ui.add(
            egui::DragValue::new(&mut buffer.0)
                .range(0..=16384)
                .speed(16.0),
        )
        .on_hover_text("Longest edge in pixels; 0 = unlimited");
        ui.checkbox(&mut buffer.1, "Mips");
    });
    ui.horizontal(|ui| {
        if ui.button("Apply").clicked() {
            state.status = match std::fs::write(&sidecar, import_settings_json(buffer.0, buffer.1))
            {
                Ok(()) => format!("saved {}", sidecar.display()),
                Err(error) => format!("save import settings failed: {error}"),
            };
            ui.data_mut(|data| data.insert_temp(id, buffer));
        }
        if ui.button("Revert").clicked() {
            buffer = read_import_settings(&sidecar);
            ui.data_mut(|data| data.insert_temp(id, buffer));
            state.status = format!("reloaded import settings for {asset}");
        }
        if ui.button("Reimport").clicked() {
            state.pending_build = Some(crate::build::BuildKind::ScenePack);
            state.status = "rebuilding the scene pack (Reimport)".to_string();
        }
    });
    if !sidecar.is_file() {
        ui.label(
            RichText::new("no sidecar yet — Apply creates it")
                .weak()
                .size(10.0),
        );
    }
}

#[cfg(test)]
mod terrain_stats_tests {
    use super::*;

    fn raw(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn infers_a_square_heightmap_and_reports_the_range() {
        let stats =
            terrain_heightmap_stats(&raw(&[-1.5, 2.0, 0.0, 3.0]), 0).expect("2×2 heightmap");
        assert_eq!(stats.cells, 2);
        assert_eq!(stats.samples, 4);
        assert_eq!(stats.min, -1.5);
        assert_eq!(stats.max, 3.0);
    }

    #[test]
    fn explicit_resolution_must_match_the_sample_count() {
        let error = terrain_heightmap_stats(&raw(&[0.0; 4]), 3).expect_err("3×3 needs 9 samples");
        assert!(error.contains("expected 9 samples"), "{error}");
    }

    #[test]
    fn rejects_unaligned_non_square_and_empty_files() {
        assert!(terrain_heightmap_stats(&[0, 1, 2], 0).is_err());
        assert!(terrain_heightmap_stats(&raw(&[0.0; 5]), 0).is_err());
        assert!(terrain_heightmap_stats(&[], 0).is_err());
    }

    #[test]
    fn non_finite_heights_are_treated_as_zero() {
        let stats = terrain_heightmap_stats(&raw(&[f32::NAN, 1.0, -2.0, f32::INFINITY]), 0)
            .expect("2×2 heightmap");
        assert_eq!(stats.min, -2.0);
        assert_eq!(stats.max, 1.0);
    }
}
