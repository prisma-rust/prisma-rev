//! GPUI 编辑器的数据层（UI 框架无关）。
//!
//! 与 Web 编辑器（`editor/`）共享同一套概念与持久化形状：
//!
//! | 模块 | 对应前端 |
//! |------|----------|
//! | [`dock`] | `editor/src/dock.ts`（split-tree docking + 浮动窗口） |
//! | [`doc`] | `.scene.json` / `.prefab.json` 文档模型 |
//! | [`schema`] | `prism-editor-api::build_schema` 的反射投影 |
//! | [`undo`] | 按字段合并的撤销栈 |
//! | [`theme`] | `editor/src/theme.ts`（token + 预设） |
//! | [`i18n`] | `editor/src/i18n.ts` + `locales/*` |
//! | [`logbuf`] | 窗口化日志缓冲 |
//! | [`assets`] | `AssetBrowser.vue` 的目录树 / 检索 |
//! | [`keys`] | 菜单命令 + 快捷键 |
//! | [`workspace`] | 上面所有东西的宿主状态 |

pub mod anim_graph_view;
pub mod asset_edit;
pub mod asset_meta;
pub mod assets;
pub mod build;
pub mod components;
pub mod doc;
pub mod dock;
pub mod expr;
pub mod i18n;
pub mod ini;
pub mod keys;
pub mod logbuf;
pub mod manifest;
pub mod notify_view;
pub mod palette;
pub mod project;
pub mod recovery;
pub mod render_graph;
pub mod schema;
pub mod settings;
pub mod state;
pub mod textinput;
pub mod theme;
pub mod undo;
pub mod viewport_math;
pub mod workspace;

pub use anim_graph_view::{
    add_blend_space_sample, add_clip_node, add_graph_input, add_parameter, add_state_condition,
    add_state_transition, move_state_transition, parse_anim_graph_view, parse_state_machine_view,
    remove_blend_space_sample, remove_graph_input, remove_graph_node, remove_parameter,
    remove_state_condition, remove_state_transition, rename_graph_node, rename_state,
    set_blend_space_parameter, set_blend_space_position, set_blend_space_sample_clip,
    set_condition_threshold, set_condition_value, set_graph_input_bone_mask,
    set_graph_input_weight, set_graph_node_clip, set_graph_node_looped, set_graph_node_speed,
    set_graph_node_weight, set_parameter_default, set_state_clip, set_state_looped,
    set_state_speed, set_transition_states, set_transition_timing, AnimConditionView,
    AnimGraphLayout, AnimGraphView, AnimInputView, AnimNodeView, AnimParameterView,
    AnimStateMachineView, AnimStateView, AnimTransitionView,
};
pub use asset_edit::{AssetEdit, AssetEditHistory};
pub use asset_meta::{AssetMeta, META_SUFFIX};
pub use assets::{
    format_bytes, AssetDetails, AssetInfo, AssetKind, AssetPreview, AssetQuery, AssetTree,
};
pub use build::{copy_artifacts, plan_build, ArtifactCopy, BuildPlan, BuildStep};
pub use components::ComponentMap;
pub use doc::{EntityJson, SceneDoc, TransformJson};
pub use dock::{
    DockDirection, DockEdge, DockLayout, DockNode, DockZone, FloatPanel, LAYOUT_PRESETS,
};
pub use i18n::{I18n, Locale};
pub use keys::{
    command_for, conflicting_command, default_keys, effective_bindings, effective_keys, Command,
    KeyChord, SHORTCUTS, SHORTCUT_ALIASES,
};
pub use logbuf::{LogBuffer, LogFilter, LogLevel, LogLine, LogSource, LOG_ROW_HEIGHT};
pub use manifest::{doc_relative_path, SceneEntry, SceneManifest};
pub use notify_view::{
    add_notify, parse_notifies_view, remove_notify, set_notify, ClipNotifiesView, NotifiesView,
    NotifyEntryView,
};
pub use project::{find_project_root, scan_documents, ProjectFile};
pub use recovery::{RecoveredDocument, RECOVERY_DIR, RECOVERY_FILE};
pub use schema::{
    component_value_with_overrides, get_at, set_at_path, EntityPreset, FieldSchema, FieldWidget,
    PropertyAttribute, PropertyDrawer, Schema, TypeSchema,
};
pub use settings::ProjectSettings;
pub use state::{EditorState, Preferences};
pub use textinput::TextInput;
pub use theme::{Rgba, ThemeColors, ThemePreset, ThemeState, ThemeToken, THEME_PRESETS};
pub use undo::UndoStack;
pub use viewport_math::GizmoSpace;
pub use workspace::{
    DocKind, Document, OrbitCamera, PanelInfo, PrefabOrigin, SelectMode, Workspace,
};
