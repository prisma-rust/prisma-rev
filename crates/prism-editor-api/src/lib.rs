//! PrismaRev 编辑器 / MCP 的**后端数据层**。
//!
//! 这一层刻意保持纯净：只依赖 `prism-scene` / `prism-reflect`，
//! **不依赖 egui、不依赖 Vulkan / 引擎 / 渲染**。它把「场景 schema」和
//! 「`.scene.json` 文档 IO」暴露成可序列化的数据结构，供 MCP 工具
//! （见 `crates/prism-editor-mcp`）直接转成 JSON。
//!
//! 职责划分：
//! - [`schema`]：把 `prism-scene` 的组件注册表投影成前端可消费的属性面板
//!   schema（字段名 / 类型 / 只读 / 嵌套结构 / 默认值）。
//! - [`scene`]：定位项目根、枚举/读写/校验 `.scene.json`、场景模板与文件操作。
//! - [`presets`]：快速添加实体的预设（相机 / 光源 / UI / 网格 / 空）。
//! - [`assets`]：`assets/` 目录枚举（含大小/分类）与 `.scene.json` 引用分析。
//! - [`reflect_edit`]：**基于反射**的字段级写入（只读拒绝、类型校验、作者 JSON 往返）。
//!
//! 编辑器门面层（供 `editor/` 与自动化工具消费）：
//! - [`asset_database`]：资产数据库（枚举 / 查找 / 标识符 / 增删改 / 导入）。
//! - [`asset_inspect`]：已 cook 资源（RSKL / RSKN / RANI / RMES …）的检查与校验。
//! - [`asset_importer`]：按扩展名投影导入器类别 / 主资产类型。
//! - [`prefab_utility`]：预制体操作（提取 / 实例化 / 还原 / 应用 / 解包）。
//! - [`selection`]：实体选中集合 + 活动项 + 过滤。
//! - [`undo`]：快照式撤销栈 + 标签 / 合并 / 上限。
//! - [`editor_application`]：编辑器运行时（播放模式 / 路径 / 延迟调用）。
//! - [`scene_management`]：场景打开 / 保存 / 加载状态。
//! - [`editor_window`]：面板注册表（id / 标题 / 菜单路径 / 命令 id），
//!   驱动前端 `PANELS` 与「窗口 / 帮助」菜单。
//! - [`build_profile`]：构建目标组、构建选项、场景构建顺序。
//! - [`build_report`]：构建阶段、产物大小、结果。
//! - [`property_drawer`]：字段渲染声明（范围 / 多行文本 / 颜色用法 / 自定义绘制）。
//!
//! 真正的编辑发生在 web UI 里：前端直接编辑 `.scene.json` 的 JSON 文档，
//! schema 只作为控件类型与只读判断的提示。

pub mod asset_database;
pub mod asset_importer;
pub mod asset_inspect;
pub mod asset_ops;
pub mod assets;
pub mod build_profile;
pub mod build_report;
pub mod editor_application;
pub mod editor_window;
pub mod ini;
pub mod play;
pub mod prefab_utility;
pub mod presets;
pub mod property_drawer;
pub mod reflect_edit;
pub mod scene;
pub mod scene_management;
pub mod scene_manifest;
pub mod schema;
pub mod selection;
pub mod settings;
pub mod undo;

pub use asset_importer::{
    get_at_path as asset_importer_at_path, list_importers, AssetImporter, ImporterClass,
};
pub use asset_ops::{
    asset_reference_documents, create_asset_dir, delete_asset, move_asset, normalize_asset_path,
    rename_asset, AssetMutation,
};
pub use assets::{
    asset_kind, asset_usage, assets_dir, list_asset_dirs, list_assets, list_assets_info,
    read_asset_bytes, read_asset_text, AssetInfo, AssetRef, AssetTextPreview, MAX_ASSETS,
    MAX_PREVIEW_BYTES, MAX_TEXT_PREVIEW_BYTES,
};
// 形状门面（`delete_asset` / `move_asset` / `rename_asset` /
// `instantiate_prefab` / `revert_prefab_instance` / `sync_prefab_instances` 等
// 与既有导出同名，保留模块路径访问，不在根上重复导出）。
pub use asset_database::{
    asset_absolute_path, asset_path_to_guid, contains, copy_asset, create_folder, find_assets,
    get_all_asset_paths, get_asset_dependency_hash, get_main_asset_type_at_path,
    guid_to_asset_path, import_asset, is_valid_folder, is_valid_guid, load_asset_at_path, refresh,
    save_assets, ASSETS_FOLDER,
};
pub use build_profile::{read_build_profile, target_group, BuildOptions, BuildProfile, BuildScene};
pub use build_report::{
    file_size, BuildFile, BuildFileRole, BuildReport, BuildResult, BuildStep, BuildSummary,
};
pub use editor_application::{
    application_path, data_path, engine_version, project_path, streaming_assets_path,
    EditorApplicationState, PlayModeState,
};
pub use editor_window::{build_window_registry, EditorWindowDescriptor, EditorWindowRegistry};
pub use play::{
    list_playable_scenes, manifest_path, read_scene_manifest, scene_package_status,
    write_scene_manifest, PlayableScene, SceneListEntry, ScenePackageStatus,
};
pub use prefab_utility::{
    apply_prefab_instance, get_corresponding_object_from_source, get_nearest_prefab_instance_root,
    get_prefab_asset_type, get_prefab_instance_roots, get_prefab_instance_status,
    instantiate_prefab as instantiate_prefab_asset, is_part_of_prefab_instance,
    prefab_instance_infos, read_prefab, save_as_prefab_asset, save_as_prefab_asset_and_connect,
    unpack_prefab_instance, PrefabAssetType, PrefabConnect, PrefabInstanceInfo,
    PrefabInstanceStatus, PrefabUnpackMode, DEFAULT_PREFAB_FOLDER,
};
pub use presets::{entity_presets, EntityPreset};
pub use property_drawer::{
    PropertyAttribute, PropertyDrawer, PropertyDrawerRegistry, PropertyRenderer,
};
pub use reflect_edit::{normalize_component, set_component_field};
pub use scene::{
    // 通用（场景 / 预制体）
    delete_doc,
    delete_entities,
    // 场景别名（旧调用）
    delete_scene,
    doc_path,
    docs_dir,
    empty_scene_doc,
    extract_subtree,
    find_project_root,
    instantiate_prefab,
    list_docs,
    list_scenes,
    move_entity,
    prefab_instances,
    prefab_template_doc,
    read_doc,
    read_scene_doc,
    rename_doc,
    rename_scene,
    reparent_doc,
    revert_prefab_instance,
    scene_path,
    scenes_dir,
    sync_prefab_instances,
    template_doc,
    template_scene_doc,
    templates,
    templates_for,
    unlink_prefab_instance,
    validate_doc,
    validate_scene_doc,
    write_doc,
    write_doc_as,
    write_doc_in_dir,
    write_scene_as,
    write_scene_doc,
    DocKind,
    InstantiateResult,
    MoveResult,
    PrefabLink,
    SceneEntry,
    SceneTemplate,
    PREFAB_LINK_COMPONENT,
};
pub use scene_management::{
    get_loaded_scenes, get_scene_at, new_scene, open_scene, open_scene_at_path, resolve_scene,
    save_open_scenes, save_scene, save_scene_as, scene_count, scene_from_doc, LoadSceneMode, Scene,
    SceneManagerState,
};
pub use schema::{
    build_schema, build_schema_with, builtin_registry, FieldSchema, Schema, TypeSchema,
};
pub use selection::{SelectionFilter, SelectionMode, SelectionState};
pub use settings::{
    read_project_settings, read_render_settings, settings_path, write_project_settings,
    write_project_settings_diff, write_render_settings, write_render_settings_diff,
    AppSettingsData, BuildSettingsData, ProjectSettingsData, RenderSettingsData,
    WindowSettingsData,
};
pub use undo::{DocUndoStack, UndoRecord, UndoStack, DEFAULT_UNDO_LIMIT};
