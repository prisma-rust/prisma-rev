use crate::{impl_asset_data, AssetGuid};
use serde::{Deserialize, Serialize};

/// Cubemap texture source asset.
///
/// References an equirectangular HDR file that gets cooked into a GPU
/// cubemap for IBL / skybox use. No rendering parameters (intensity, tint,
/// rotation) live here — those belong to the scene's lighting configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CubeDef {
    /// Stable GUID.
    pub guid: AssetGuid,
    /// Human-readable label.
    pub label: String,
    /// Path to equirectangular HDR source file (relative to asset library).
    pub hdr_source: Option<String>,
}

impl_asset_data!(CubeDef, "cube", "Cubemap Texture");
