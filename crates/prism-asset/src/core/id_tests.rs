// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

#[test]
fn generate_produces_unique_ids() {
    let a = AssetId::generate();
    let b = AssetId::generate();
    assert_ne!(a, b);
    assert_eq!(a.generation(), 0);
    assert_eq!(b.generation(), 0);
    assert_eq!(b.serial(), a.serial() + 1);
}

#[test]
fn generate_does_not_collide_with_database_generation() {
    let database_id = AssetIdGenerator::new(1, 0).next();
    let runtime_id = AssetId::generate();
    assert_ne!(runtime_id, database_id);
    assert_eq!(runtime_id.generation(), 0);
    assert_eq!(database_id.generation(), 1);
}

#[test]
#[should_panic(expected = "serial space exhausted")]
fn generator_rejects_serial_overflow() {
    let mut gen = AssetIdGenerator::new(1, u64::from(u32::MAX));
    let _ = gen.next();
}

#[test]
fn tombstone_is_recognised() {
    let t = AssetId::tombstone(42);
    assert!(t.is_tombstone());
    assert_eq!(t.serial(), 42);
    assert_eq!(t.generation(), u32::MAX);
}

#[test]
fn normal_id_is_not_tombstone() {
    let id = AssetId::generate();
    assert!(!id.is_tombstone());
}

#[test]
fn ordering_tombstone_after_live() {
    let live = AssetId::generate();
    let dead = AssetId::tombstone(live.serial().into());
    assert!(dead > live);
}

#[test]
fn roundtrip_serde_json() {
    let id = AssetId::generate();
    let json = serde_json::to_string(&id).unwrap();
    let back: AssetId = serde_json::from_str(&json).unwrap();
    assert_eq!(id, back);
}

#[test]
fn roundtrip_bincode() {
    let id = AssetId::generate();
    let bytes = bincode::serde::encode_to_vec(id, bincode::config::standard()).unwrap();
    let (back, _): (AssetId, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    assert_eq!(id, back);
}

#[test]
fn generator_monotonic() {
    let mut gen = AssetIdGenerator::new(1, 100);
    let a = gen.next();
    let b = gen.next();
    assert_eq!(a.serial(), 101);
    assert_eq!(b.serial(), 102);
    assert_eq!(gen.current_serial(), 103);
}

#[test]
fn display_and_debug_dont_panic() {
    let id = AssetId::generate();
    let _ = format!("{id}");
    let _ = format!("{id:?}");
}

#[test]
fn path_ids_are_stable_and_order_independent() {
    // #530：打包器按路径派生 ID，材质里烘的贴图 ID 才不会因为多一个文件而错位。
    let a = AssetId::from_path("textures/marble.png");
    assert_eq!(a, AssetId::from_path("textures/marble.png"));
    assert_ne!(a, AssetId::from_path("textures/brick.png"));
    // Windows 风格的输入归一化成同一 ID。
    assert_eq!(a, AssetId::from_path("textures\\marble.png"));
    assert_eq!(a.generation(), 1, "构建期 ID 用 generation 1");
    assert_ne!(a.serial(), 0, "serial 0 保留给「未分配」");
}

#[test]
fn path_ids_do_not_collide_for_the_common_extensions() {
    // 同一张图的不同 cook 形态是不同路径，必须拿到不同 ID。
    let paths = [
        "textures/a.png",
        "textures/a.rtex",
        "models/a.gltf",
        "materials/a.mat.json",
        "scenes/a.rscn",
        "fonts/a.ttf",
        "sprites/a.png",
    ];
    let mut ids: Vec<u64> = paths
        .iter()
        .map(|p| AssetId::from_path(p).into_raw())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), paths.len(), "不同路径不应撞 ID");
}
