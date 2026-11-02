use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_root(tag: &str) -> std::path::PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "prism-asset-meta-{tag}-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("assets/textures")).unwrap();
    root
}

#[test]
fn the_sidecar_path_appends_the_suffix_to_the_full_name() {
    assert_eq!(
        meta_relative_path("assets/textures/wood.png"),
        "assets/textures/wood.png.rmeta"
    );
}

#[test]
fn generated_guids_are_32_hex_digits_and_unique() {
    let first = new_guid("assets/a.png");
    let second = new_guid("assets/a.png");
    assert!(valid_guid(&first), "{first}");
    assert!(valid_guid(&second), "{second}");
    assert_ne!(first, second, "同一路径连续生成也要不同");
    assert!(!valid_guid(""), "空串不合法");
    assert!(!valid_guid("xyz"), "非十六进制不合法");
    assert!(!valid_guid(&"a".repeat(31)), "长度不够不合法");
}

#[test]
fn ensure_creates_a_sidecar_and_keeps_it_stable() {
    let root = temp_root("ensure");
    let asset = "assets/textures/wood.png";
    let first = AssetMeta::ensure(&root, asset, "TextureImporter").unwrap();
    assert!(first.guid.len() == 32);
    assert_eq!(first.importer, "TextureImporter");

    // 第二次即使换了 importer 也不重写已有 GUID。
    let second = AssetMeta::ensure(&root, asset, "OtherImporter").unwrap();
    assert_eq!(first.guid, second.guid);
    assert_eq!(second.importer, "TextureImporter");
    assert_eq!(
        AssetMeta::read(&root, asset),
        Some(first),
        "读回来的内容与第一次一致"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_corrupt_sidecar_is_regenerated() {
    let root = temp_root("corrupt");
    let asset = "assets/textures/wood.png";
    let path = root.join(meta_relative_path(asset));
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(AssetMeta::read(&root, asset), None);

    let meta = AssetMeta::ensure(&root, asset, "TextureImporter").unwrap();
    assert!(valid_guid(&meta.guid));
    assert_eq!(AssetMeta::read(&root, asset), Some(meta));

    // 形状不对的 GUID 同样视为损坏。
    std::fs::write(&path, r#"{"guid":"nope","importer":""}"#).unwrap();
    assert_eq!(AssetMeta::read(&root, asset), None);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn read_returns_none_for_a_missing_sidecar() {
    let root = temp_root("missing");
    assert_eq!(AssetMeta::read(&root, "assets/textures/none.png"), None);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn write_round_trips_through_disk() {
    let root = temp_root("roundtrip");
    let meta = AssetMeta {
        guid: "0123456789abcdef0123456789abcdef".into(),
        importer: "ModelImporter".into(),
    };
    meta.write(&root, "assets/textures/wood.png").unwrap();
    assert_eq!(
        AssetMeta::read(&root, "assets/textures/wood.png"),
        Some(meta)
    );
    std::fs::remove_dir_all(root).ok();
}
