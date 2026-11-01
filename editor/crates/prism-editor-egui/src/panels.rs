//! 面板主体：层级 / 检查器 / 资产 / 文档 / 日志 / 撤销历史 / 设置类窗口。
//!
//! 数据与命令全部走 `prism-editor-core`（与 Web 编辑器、GPUI 编辑器同一份
//! 文档模型 / schema / 撤销栈），这里只负责画 egui 控件。

use egui::{Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use prism_editor_core::settings::{
    matching_fields, SettingsCache, PROFILE_CHOICES, TARGET_CHOICES,
};
use prism_editor_core::{
    format_bytes, AssetKind, AssetPreview, Locale, LogLevel, LogSource, Preferences,
    ProjectSettings, SceneDoc, Workspace, THEME_PRESETS,
};
use serde_json::{json, Value};

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

/// egui 侧的临时 UI 状态（不进文档、不进撤销栈）。
#[derive(Default)]
pub struct UiState {
    pub inspector_search: String,
    /// 检查器锁定：按**实体 id** 钉住内容（Unity 的 Inspector Lock，#418）。
    ///
    /// 存 id 而不是下标：删除 / 重排实体后仍然指向同一个对象；对象全被删掉时
    /// 下一帧自动解锁、回落到文档选择。
    pub inspector_lock: Option<Vec<u32>>,
    pub hierarchy_search: String,
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
fn search_matches(doc: &SceneDoc, filter: &str) -> std::collections::HashSet<usize> {
    let mut keep = std::collections::HashSet::new();
    if filter.is_empty() {
        return keep;
    }
    for (index, entity) in doc.entities.iter().enumerate() {
        if entity.display_name().to_lowercase().contains(filter) {
            keep.extend(doc.path_of(index));
        }
    }
    keep
}

pub fn hierarchy(ui: &mut Ui, workspace: &mut Workspace, state: &mut UiState) {
    // ---- 顶部工具条：搜索 + 新建实体（引擎的 Create 下拉） ----
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.hierarchy_search)
                .hint_text("Search")
                .desired_width(110.0),
        );
        let presets: Vec<(String, String)> = workspace
            .presets
            .iter()
            .map(|preset| (preset.id.clone(), preset.label.clone()))
            .collect();
        if presets.is_empty() {
            if ui.button("Create").clicked() {
                workspace.add_entity("");
            }
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
            });
        }
    });
    ui.separator();

    let filter = state.hierarchy_search.to_lowercase();
    let searching = !filter.is_empty();
    // 命中项 + 祖先：折叠父级下的对象也要能被搜到（会展开祖先并显示路径）。
    let search = search_matches(&workspace.document().doc, &filter);
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
    // 引用断掉的实体（文件被删 / 改名 / 拼错）：行尾标一个警告图标。
    // 每帧只算一次——`resolve_reference` 要扫资产表，放在行循环里是 O(实体 × 资产)（#467）。
    let broken: std::collections::HashSet<usize> = {
        let dir = workspace.document_dir();
        let paths = workspace.asset_paths();
        workspace
            .document()
            .doc
            .reference_values()
            .into_iter()
            .filter(|(_, _, value)| {
                prism_editor_core::assets::resolve_reference(&paths, &dir, value).is_none()
            })
            .map(|(entity, _, _)| entity)
            .collect()
    };
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
            for (index, _id) in rows {
                let doc = &workspace.document().doc;
                let Some(entity) = doc.entities.get(index) else {
                    continue;
                };
                let name = entity.display_name();
                if searching && !search.contains(&index) {
                    continue;
                }
                let depth = doc.path_of(index).len().saturating_sub(1);
                let selected = workspace.selection.contains(&index);
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
                let eye = if active { icons::EYE } else { icons::EYE_SLASH };
                let response = flat_icon_dimmed(&mut row_ui, eye, active && !ancestor_active);
                let response = if active && !ancestor_active {
                    response.on_hover_text("Hidden by an inactive parent")
                } else {
                    response.on_hover_text("Toggle Active")
                };
                if response.clicked() {
                    action = Some(TreeAction::ToggleActive(index));
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
                    workspace.select_for_context(index);
                }

                row.context_menu(|ui| {
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
        match action {
            TreeAction::Select(index, extend) => workspace.select(index, extend),
            TreeAction::Toggle(index) => {
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
            TreeAction::ToggleExpand(index) => workspace.toggle_expanded(index),
            TreeAction::ToggleActive(index) => {
                let active = workspace.document().doc.entities[index].active();
                workspace.set_active_entity(index, !active);
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
        } else if name == "prism_engine::scene::KeyframeAnimation" {
            keyframe_animation_editor(ui, workspace, entity_id, &schema.name, &value);
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
    let mut found: Option<(usize, usize)> = None;
    for offset in 0..workspace.docs.len() {
        let index = (active + offset) % workspace.docs.len().max(1);
        let Some(doc) = workspace.docs.get(index) else {
            continue;
        };
        if let Some((entity, _, _)) = doc.doc.field_references(path).into_iter().next() {
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
/// 只映射引擎**已有**的 `debug_flags` / `debug_rt`：PBR 分量隔离用
/// `PBR_FLAG_NAMES` 里的位，深度 / 视图空间法线用 PostPass 的调试 RT。
/// 线框（Wireframe / Shaded Wireframe）需要渲染管线支持 `POLYGON_MODE_LINE`，
/// 不在这里假装实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneRenderMode {
    Shaded,
    Direct,
    Specular,
    Metallic,
    Roughness,
    AmbientOcclusion,
    Emissive,
    Depth,
    ViewNormals,
}

impl SceneRenderMode {
    pub const ALL: [SceneRenderMode; 9] = [
        SceneRenderMode::Shaded,
        SceneRenderMode::Direct,
        SceneRenderMode::Specular,
        SceneRenderMode::Metallic,
        SceneRenderMode::Roughness,
        SceneRenderMode::AmbientOcclusion,
        SceneRenderMode::Emissive,
        SceneRenderMode::Depth,
        SceneRenderMode::ViewNormals,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SceneRenderMode::Shaded => "Shaded",
            SceneRenderMode::Direct => "Direct",
            SceneRenderMode::Specular => "Specular",
            SceneRenderMode::Metallic => "Metallic",
            SceneRenderMode::Roughness => "Roughness",
            SceneRenderMode::AmbientOcclusion => "AO",
            SceneRenderMode::Emissive => "Emissive",
            SceneRenderMode::Depth => "Depth",
            SceneRenderMode::ViewNormals => "View Normals",
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
            _ => 0,
        }
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
            mode.debug_flags() == settings.debug_flags && mode.debug_rt() == settings.debug_rt
        })
    }

    pub fn apply(self, settings: &mut prism_engine::render_settings::RenderSettings) {
        settings.debug_flags = self.debug_flags();
        settings.debug_rt = self.debug_rt();
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
                render_graph_timings(ui, parsed);
                render_graph_stats(ui, parsed);
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

/// per-pass GPU 时间表（#518）：总时间 + 按成本从高到低排的 pass。
///
/// 数据来自渲染器的时间戳查询（延迟一帧）。设备不支持时整段隐藏 —— 面板只显示
/// 「没有 GPU 计时」，不会拿 CPU 墙钟冒充 GPU 时间。
fn render_graph_timings(ui: &mut Ui, report: &prism_editor_core::render_graph::RenderGraphReport) {
    let Some(total) = report.gpu_total_ms() else {
        ui.label(
            RichText::new("No GPU timings (device does not report timestamps).")
                .weak()
                .size(11.0),
        );
        return;
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new("GPU").strong());
        let color = if total > FRAME_BUDGET_MS {
            Color32::from_rgb(232, 96, 96)
        } else {
            Color32::from_rgb(120, 200, 140)
        };
        ui.label(RichText::new(format!("{total:.2} ms")).color(color));
        ui.label(
            RichText::new(format!("budget {FRAME_BUDGET_MS:.1} ms"))
                .weak()
                .size(10.0),
        );
    });

    egui::Grid::new("graph-pass-timings")
        .num_columns(3)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Pass").weak().size(10.0));
            ui.label(RichText::new("Kind").weak().size(10.0));
            ui.label(RichText::new("GPU ms").weak().size(10.0));
            ui.end_row();
            for pass in report.passes_by_cost() {
                ui.label(RichText::new(&pass.name).size(11.0));
                ui.label(RichText::new(&pass.kind).weak().size(11.0));
                match pass.gpu_ms {
                    Some(ms) => {
                        let color = if ms > FRAME_BUDGET_MS * 0.5 {
                            Color32::from_rgb(226, 178, 84)
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(RichText::new(format!("{ms:.2}")).size(11.0).color(color));
                    }
                    None => {
                        ui.label(RichText::new("—").weak().size(11.0));
                    }
                }
                ui.end_row();
            }
        });
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
) {
    ui.checkbox(&mut preferences.autosave, "Auto save layout");
    ui.checkbox(&mut preferences.confirm_delete, "Confirm before delete");
    ui.checkbox(&mut preferences.compact, "Compact layout");
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

        let keep = search_matches(&doc, "rod");
        assert!(keep.contains(&child));
        assert!(keep.contains(&parent), "collapsed parent must stay visible");
        assert_eq!(keep.len(), 2);

        assert!(search_matches(&doc, "").is_empty());
        assert!(search_matches(&doc, "missing").is_empty());
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
    fn every_editor_panel_builds_a_frame() {
        run_frame(|ui, workspace, state| hierarchy(ui, workspace, state));
        run_frame(|ui, workspace, state| inspector(ui, workspace, state));
        run_frame(|ui, workspace, state| assets(ui, workspace, state));
        run_frame(|ui, workspace, state| docs(ui, workspace, state));
        run_frame(|ui, workspace, state| log(ui, workspace, state));
        run_frame(|ui, workspace, state| history(ui, workspace, state));
        run_frame(|ui, _workspace, _state| {
            let mut settings = prism_engine::render_settings::RenderSettings::default();
            let mut viewport_scale = 1.0;
            let _ = render_graph(ui, None, &mut settings, &mut viewport_scale);
        });
        let report = r#"{"generatedAtMs":1,"frame":2,"passes":[],"resources":[],
            "stats":{"textures":3,"textureBytes":1024,"meshes":2,"meshBytes":2048,
                     "materials":4,"materialBytes":384}}"#;
        run_frame(|ui, _workspace, _state| {
            let mut settings = prism_engine::render_settings::RenderSettings::default();
            let mut viewport_scale = 1.0;
            let _ = render_graph(ui, Some(report), &mut settings, &mut viewport_scale);
        });
        run_frame(|ui, _workspace, state| project_settings(ui, None, state));
        run_frame(|ui, workspace, state| {
            let mut prefs = Preferences::default();
            preferences(ui, &mut prefs, workspace, state);
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
