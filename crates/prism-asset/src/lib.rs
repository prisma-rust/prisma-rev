#[cfg(feature = "core")]
pub mod core;
#[cfg(feature = "core")]
pub mod formats;
pub mod formats_skinned;
#[cfg(feature = "package")]
pub mod package;
#[cfg(feature = "runtime")]
pub mod runtime;
#[cfg(feature = "types")]
pub mod types;

// Re-exports so macros using `$crate::` resolve correctly
#[cfg(feature = "asset-data")]
pub use core::asset_data::{AssetData, AssetHandle, LoadedAsset};
#[cfg(feature = "core")]
pub use core::asset_type::{AssetRef, AssetType};
#[cfg(feature = "core")]
pub use core::guid::AssetGuid;
#[cfg(feature = "core")]
pub use core::handle::{AnyAsset, AnyHandle, Handle};
#[cfg(feature = "core")]
pub use core::id::AssetId;
