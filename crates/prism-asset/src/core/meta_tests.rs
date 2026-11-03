use super::*;
use std::fs;

fn write_meta(root: &Path, asset: &str, guid: &str, importer: &str) {
    let path = root.join(meta_relative_path(asset));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(
        path,
        serde_json::json!({ "guid": guid, "importer": importer }).to_string(),
    )
    .unwrap();
}

#[test]
fn meta_round_trips_and_ensure_is_stable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("textures")).unwrap();
    fs::write(root.join("textures/wood.png"), b"pixels").unwrap();

    let first = AssetMeta::ensure(root, "textures/wood.png", "TextureImporter").unwrap();
    assert!(valid_guid(&first.guid));
    assert_eq!(first.importer, serde_json::json!("TextureImporter"));

    let second = AssetMeta::ensure(root, "textures/wood.png", "OtherImporter").unwrap();
    assert_eq!(first, second, "an existing sidecar must never be rewritten");

    let moved = AssetMeta::read(root, "textures/wood.png").unwrap();
    assert_eq!(moved.guid, first.guid);
}

#[test]
fn invalid_guid_is_treated_as_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("textures")).unwrap();
    fs::write(root.join("textures/wood.png"), b"pixels").unwrap();
    write_meta(root, "textures/wood.png", "not-a-guid", "TextureImporter");

    assert_eq!(AssetMeta::read(root, "textures/wood.png"), None);
}

#[test]
fn guid_index_scans_sidecars_and_ignores_deleted_assets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("models")).unwrap();
    fs::create_dir_all(root.join("textures")).unwrap();
    fs::write(root.join("models/box.gltf"), b"mesh").unwrap();
    fs::write(root.join("textures/wood.png"), b"pixels").unwrap();

    let box_guid = "11111111111111111111111111111111";
    let wood_guid = "22222222222222222222222222222222";
    write_meta(root, "models/box.gltf", box_guid, "GltfImporter");
    write_meta(root, "textures/wood.png", wood_guid, "TextureImporter");
    // Sidecar without the asset file: deleted asset, must not resolve.
    write_meta(
        root,
        "models/gone.gltf",
        "33333333333333333333333333333333",
        "GltfImporter",
    );

    let index = GuidIndex::scan(root);
    assert_eq!(index.len(), 2);
    assert_eq!(index.path_for_guid(box_guid), Some("models/box.gltf"));
    assert_eq!(
        index.path_for_guid(&box_guid.to_ascii_uppercase()),
        Some("models/box.gltf")
    );
    assert_eq!(index.guid_for_path("models/box.gltf"), Some(box_guid));
    assert_eq!(index.guid_for_path("textures/wood.png"), Some(wood_guid));
    assert_eq!(
        index.path_for_guid("33333333333333333333333333333333"),
        None
    );
}

#[test]
fn guid_index_is_empty_for_missing_root() {
    let temp = tempfile::tempdir().unwrap();
    let index = GuidIndex::scan(&temp.path().join("missing"));
    assert!(index.is_empty());
}
