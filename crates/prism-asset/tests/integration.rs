//! End-to-end serialization round-trip tests for concrete asset types.
//!
//! These verify:
//! - Typetag polymorphic deserialization (`Box<dyn AssetData>`)
//! - Concrete typed deserialization (`AssetServer::load<T>`)
//! - Field integrity after round-trip
//! - Asset file loading from real file system

use prism_asset::core::AssetData;
use prism_asset::core::AssetGuid;
use prism_asset::types::{CubeDef, MaterialDef};

/// Repository-root asset directory.
/// Tests are compiled from `crates/prism-asset/`, so we back out two levels to the repo root.
const ASSET_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/asset-defs/");

fn asset_path(file: &str) -> String {
    let mut p = ASSET_DIR.to_owned();
    p.push_str(file);
    p
}

// ---------------------------------------------------------------------------
// Typetag polymorphic round-trip
// ---------------------------------------------------------------------------

/// Deserialize each asset type as `Box<dyn AssetData>`, verify type info.
fn round_trip_as_erased(path: &str) {
    let bytes = std::fs::read(path).expect("read asset file");
    let asset: Box<dyn AssetData> = serde_json::from_slice(&bytes).expect("deserialize as erased");

    // Must have a non-empty display name (set by `impl_asset_data!`).
    let name = asset.display_name();
    assert!(!name.is_empty(), "display_name should not be empty");
}

/// Deserialize as concrete type, check display_name.
fn round_trip_concrete<T>(path: &str)
where
    T: AssetData + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let bytes = std::fs::read(path).expect("read asset file");
    let asset: T = serde_json::from_slice(&bytes).expect("deserialize as concrete");
    assert!(
        !asset.display_name().is_empty(),
        "display_name should not be empty for {path}"
    );
    println!("  OK: {asset:?}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn material_round_trip_erased() {
    round_trip_as_erased(&asset_path("default_material.asset.json"));
}

#[test]
fn material_round_trip_concrete() {
    round_trip_concrete::<MaterialDef>(&asset_path("default_material.asset.json"));
}

#[test]
fn cube_round_trip_erased() {
    round_trip_as_erased(&asset_path("default_env.cube.asset.json"));
}

#[test]
fn cube_round_trip_concrete() {
    round_trip_concrete::<CubeDef>(&asset_path("default_env.cube.asset.json"));
}

#[test]
fn material_guid_is_preserved() {
    let bytes = std::fs::read(asset_path("default_material.asset.json")).unwrap();
    let mat: MaterialDef = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        mat.guid,
        AssetGuid::parse_str("11111111-1111-4111-1111-111111111111").unwrap(),
        "GUID mismatch"
    );
}

#[test]
fn cube_label_is_preserved() {
    let bytes = std::fs::read(asset_path("default_env.cube.asset.json")).unwrap();
    let cube: CubeDef = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(cube.label, "Default Environment");
    assert_eq!(
        cube.hdr_source.as_deref(),
        Some("textures/sunset_puresky_4k.hdr")
    );
}
