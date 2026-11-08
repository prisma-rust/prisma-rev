//! Editor data layer (UI-framework-free).
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`dock`] | split-tree docking + floating windows |
//! | [`doc`] | `.scene.json` / `.prefab.json` document model |
//! | [`schema`] | UI-side component schema (data comes from `prism-editor-api`) |
//! | [`undo`] | field-merged undo stack |
//! | [`theme`] | theme tokens + presets |
//! | [`i18n`] | UI language + `locales/*` |
//! | [`logbuf`] | windowed log buffer |
//! | [`assets`] | asset tree / search |
//! | [`keys`] | menu commands + shortcuts |
//! | [`workspace`] | host state tying all of the above together |

pub mod anim_graph_view;
pub mod asset_edit;
pub mod asset_meta;
pub mod assets;
pub mod build;
pub mod clip_view;
pub mod components;
pub mod doc;
pub mod dock;
pub mod expr;
pub mod graph_validation;
pub mod i18n;
pub mod ini;
pub mod keys;
pub mod logbuf;
pub mod manifest;
pub mod montage_view;
pub mod notify_view;
pub mod palette;
pub mod project;
pub mod recovery;
pub mod render_graph;
pub mod schema;
pub mod settings;
pub mod state;
pub mod sync_marker_view;
pub mod textinput;
pub mod theme;
pub mod undo;
pub mod viewport_math;
pub mod workspace;

pub use anim_graph_view::{
    add_blend_space_sample, add_clip_node, add_graph_input, add_graph_node, add_parameter,
    add_state, add_state_condition, add_state_transition, blend_space_2d_area,
    blend_space_2d_degenerate, duplicate_graph_node, duplicate_state, duplicate_state_transition,
    move_state, move_state_transition, parse_anim_graph_view, parse_state_machine_view,
    remove_blend_space_sample, remove_graph_input, remove_graph_node, remove_parameter,
    remove_state, remove_state_condition, remove_state_transition, rename_graph_node, rename_state,
    set_blend_space_parameter, set_blend_space_position, set_blend_space_sample_clip,
    set_blend_space_sample_speed, set_condition_kind, set_condition_name, set_condition_threshold,
    set_condition_value, set_graph_input_bone_mask, set_graph_input_weight, set_graph_node_clip,
    set_graph_node_looped, set_graph_node_speed, set_graph_node_weight, set_parameter_default,
    set_state_clip, set_state_looped, set_state_speed, set_transition_states,
    set_transition_timing, AnimConditionView, AnimGraphLayout, AnimGraphView, AnimInputView,
    AnimNodeView, AnimParameterView, AnimStateMachineView, AnimStateView, AnimTransitionView,
};
pub use asset_edit::{AssetEdit, AssetEditHistory};
pub use asset_meta::{AssetMeta, META_SUFFIX};
pub use assets::{
    format_bytes, AssetDetails, AssetInfo, AssetKind, AssetPreview, AssetQuery, AssetTree,
};
pub use build::{copy_artifacts, plan_build, ArtifactCopy, BuildPlan, BuildStep};
pub use clip_view::{root_motion_delta, timeline_time_at, timeline_x};
pub use components::ComponentMap;
pub use doc::{EntityJson, SceneDoc, TransformJson};
pub use dock::{
    DockDirection, DockEdge, DockLayout, DockNode, DockZone, FloatPanel, LAYOUT_PRESETS,
};
pub use graph_validation::validate_anim_graph;
pub use i18n::{I18n, Locale};
pub use keys::{
    command_for, conflicting_command, default_keys, effective_bindings, effective_keys, Command,
    KeyChord, SHORTCUTS, SHORTCUT_ALIASES,
};
pub use logbuf::{LogBuffer, LogFilter, LogLevel, LogLine, LogSource, LOG_ROW_HEIGHT};
pub use manifest::{doc_relative_path, SceneEntry, SceneManifest};
pub use montage_view::{
    add_montage_section, add_montage_warp_window, montage_section_at, montage_warp_window_at,
    parse_montages_view, remove_montage_section, remove_montage_warp_window, set_montage_section,
    set_montage_warp_window, ClipMontageView, MontageSectionView, MontageWarpWindowView,
    MontagesView,
};
pub use notify_view::{
    add_notify, nearest_notify_index, notify_time_at, notify_timeline_span, parse_notifies_view,
    remove_notify, set_notify, ClipNotifiesView, NotifiesView, NotifyEntryView,
};
pub use project::{find_project_root, scan_documents, ProjectFile};
pub use recovery::{RecoveredDocument, RECOVERY_DIR, RECOVERY_FILE};
pub use schema::{
    component_value_with_overrides, get_at, set_at_path, EntityPreset, FieldSchema, FieldWidget,
    PropertyAttribute, PropertyDrawer, Schema, TypeSchema,
};
pub use settings::ProjectSettings;
pub use state::{EditorState, Preferences};
pub use sync_marker_view::{
    add_sync_marker, nearest_sync_marker_index, parse_sync_markers_view, remove_sync_marker,
    set_sync_marker, sync_marker_phase_at, SyncMarkerClipView, SyncMarkersView,
};
pub use textinput::TextInput;
pub use theme::{Rgba, ThemeColors, ThemePreset, ThemeState, ThemeToken, THEME_PRESETS};
pub use undo::UndoStack;
pub use viewport_math::GizmoSpace;
pub use workspace::{
    DocKind, Document, OrbitCamera, PanelInfo, PrefabOrigin, SelectMode, Workspace,
};
