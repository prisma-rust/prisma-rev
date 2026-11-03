//! # prism-asset::core
//!
//! PrismaRev 资源管道的基础类型
//!
//! 提供管道中所有其他 crate 共享的原始构建块：
//! [`AssetId`]、[`AssetGuid`]、[`AssetType`]、[`Handle<T>`]、
//! [`AssetRef`]，以及 ScriptableObject 风格的 [`AssetData`](asset_data::AssetData)
//! trait 和 [`AssetHandle<T>`](asset_data::AssetHandle)。

#[cfg(feature = "asset-data")]
pub mod asset_data;
pub mod asset_type;
pub mod guid;
pub mod handle;
pub mod id;
pub mod meta;

#[cfg(feature = "asset-data")]
pub use asset_data::{AssetData, AssetHandle, LoadedAsset};
pub use asset_type::AssetRef;
pub use asset_type::AssetType;
pub use guid::AssetGuid;
pub use handle::{AnyAsset, AnyHandle, Handle};
pub use id::AssetId;
pub use meta::{AssetMeta, GuidIndex, META_SUFFIX};
