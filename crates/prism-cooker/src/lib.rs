//! Build-time asset pipeline：importer → cooker → database。
//!
//! 这个 crate 只在构建机/编辑器里用，**不进入** shipping runtime。
//! 运行时格式（`AssetId`/`Handle`/`.pak`/`ResourceManager`）在 `prism-asset`。
//!
//! 分层：
//! - `importer`：源文件 → 中间数据（glTF、scene JSON、图片…）
//! - `cooker`：中间数据 → 运行时 blob（mesh/texture/material/…）
//! - `db`：资产记录与导入缓存（构建期数据库）
//! - `pack`：已 cook 目录 → `.pak` + 路径清单（底层打包工具）

pub mod cooker;
pub mod db;
pub mod import_settings;
pub mod importer;
pub mod pack;

pub use cooker::{
    default_cooker_registry, CookContext, CookError, CookPipeline, CookResult, Cooker,
    CookerRegistry,
};
pub use db::{AssetDatabase, AssetRecord, ImportCache};
pub use import_settings::ImportSettings;
pub use importer::{default_importer_registry, ImportError, ImportPipeline, Importer};
pub use pack::{pack_directory, pack_directory_with_options, PackOptions, PackSummary};
