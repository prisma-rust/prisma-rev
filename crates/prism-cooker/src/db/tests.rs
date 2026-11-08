// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

fn make_db() -> AssetDatabase {
    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    let record = AssetRecord::new(
        id,
        "meshes/cube.gltf".into(),
        AssetType::Mesh,
        "gltf-importer",
    );
    db.insert(record).unwrap();
    db
}

#[test]
fn insert_and_find() {
    let db = make_db();
    let r = db.get_by_path("meshes/cube.gltf").unwrap();
    assert_eq!(r.asset_type, AssetType::Mesh);
}

#[test]
fn find_by_id() {
    let db = make_db();
    let r = db.get_by_path("meshes/cube.gltf").unwrap();
    let found = db.get(r.id).unwrap();
    assert_eq!(found.path, "meshes/cube.gltf");
}

#[test]
fn duplicate_path_errors() {
    let mut db = AssetDatabase::new();
    let id1 = db.generate_id();
    let id2 = db.generate_id();
    let r1 = AssetRecord::new(id1, "same/path.png".into(), AssetType::Texture, "img");
    db.insert(r1).unwrap();
    let r2 = AssetRecord::new(id2, "same/path.png".into(), AssetType::Texture, "img");
    let err = db.insert(r2).unwrap_err();
    assert!(matches!(err, DatabaseError::DuplicatePath(_)));
}

#[test]
fn remove_marks_tombstone() {
    let mut db = make_db();
    let r = db.get_by_path("meshes/cube.gltf").unwrap();
    let id = r.id;
    db.remove(id);
    assert!(db.get(id).is_none());
    assert!(db.get_by_path("meshes/cube.gltf").is_none());
    assert_eq!(db.len(), 0);
    assert!(db.records().next().is_none());
}

#[test]
fn tombstone_survives_json_roundtrip() {
    let mut db = make_db();
    let id = db.get_by_path("meshes/cube.gltf").unwrap().id;
    db.remove(id);

    let json = serde_json::to_string_pretty(&db).unwrap();
    assert!(json.contains("Deleted"), "tombstone must be serialized");

    let mut loaded: AssetDatabase = serde_json::from_str(&json).unwrap();
    loaded.rebuild_index();
    assert!(loaded.get(id).is_none());
    assert!(loaded.get_by_path("meshes/cube.gltf").is_none());
    assert!(loaded
        .records
        .iter()
        .any(|r| r.id == id && r.state == AssetState::Deleted));
    assert_eq!(loaded.len(), 0);
}

#[test]
fn roundtrip_json() {
    let mut db = make_db();
    // Add a 秒 record so we have multiple
    let id2 = db.generate_id();
    let r2 = AssetRecord::new(id2, "tex/albedo.png".into(), AssetType::Texture, "img");
    db.insert(r2).unwrap();

    let json = serde_json::to_string_pretty(&db).unwrap();
    let mut parsed: AssetDatabase = serde_json::from_str(&json).unwrap();
    parsed.rebuild_index();

    let r = parsed.get_by_path("meshes/cube.gltf").unwrap();
    assert_eq!(r.asset_type, AssetType::Mesh);
    let r2 = parsed.get_by_path("tex/albedo.png").unwrap();
    assert_eq!(r2.asset_type, AssetType::Texture);
}

#[test]
fn import_cache_basic() {
    let mut cache = ImportCache::new();
    assert!(cache.is_empty());

    cache.record("tex/albedo.png", 0xDEAD, 0xBEEF, AssetId::generate(), 1);
    assert_eq!(cache.len(), 1);

    assert!(cache.is_up_to_date("tex/albedo.png", 0xDEAD, 0xBEEF, 1));
    assert!(!cache.is_up_to_date("tex/albedo.png", 0xDEAD, 0xBEEF, 2));
    assert!(!cache.is_up_to_date("tex/albedo.png", 0xFFFF, 0xBEEF, 1));
}

#[test]
fn normalize_handles_variants() {
    assert_eq!(normalize_path("foo/bar"), "foo/bar");
    assert_eq!(normalize_path("./foo/bar"), "foo/bar");
    assert_eq!(normalize_path("foo\\bar"), "foo/bar");
    assert_eq!(normalize_path("/foo/bar/"), "foo/bar");
    assert_eq!(normalize_path("./"), ".");
}

#[test]
fn id_generator_works() {
    let mut gen = prism_asset::core::id::AssetIdGenerator::new(1, 0);
    let a = gen.next();
    let b = gen.next();
    assert!(b > a);
    assert_eq!(gen.current_serial(), 3);
}

#[test]
fn empty_db_serialize() {
    let db = AssetDatabase::new();
    let json = serde_json::to_string(&db).unwrap();
    assert!(!json.is_empty());
}

#[test]
fn file_roundtrip() {
    let dir = std::env::temp_dir();
    let path = dir.join("test_asset_db.json");
    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    db.insert(AssetRecord::new(
        id,
        "test.bin".into(),
        AssetType::Binary,
        "raw",
    ))
    .unwrap();
    db.save(&path).unwrap();

    let loaded = AssetDatabase::load(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    let r = loaded.get(id).unwrap();
    assert_eq!(r.path, "test.bin");

    std::fs::remove_file(&path).ok();
}

#[test]
fn save_is_atomic_and_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Library/AssetDatabase.json");
    let mut db = AssetDatabase::new();
    let id = db.generate_id();
    db.insert(AssetRecord::new(
        id,
        "test.bin".into(),
        AssetType::Binary,
        "raw",
    ))
    .unwrap();

    db.save(&path).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        serde_json::to_string_pretty(&db).unwrap().as_bytes()
    );

    let loaded = AssetDatabase::load(&path).unwrap();
    assert!(loaded.get(id).is_some());

    // A second save replaces the file in place and leaves no staging file.
    db.save(&path).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "leftover staging files: {leftovers:?}"
    );
}

#[test]
fn corrupt_database_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("AssetDatabase.json");
    std::fs::write(&path, b"{\"records\":[").unwrap();

    let error = AssetDatabase::load(&path).unwrap_err();
    let DatabaseError::Corrupt { backup, .. } = &error else {
        panic!("expected Corrupt, got {error:?}");
    };
    assert!(!path.exists(), "corrupt file must be moved away");
    assert_eq!(std::fs::read(backup).unwrap(), b"{\"records\":[");

    // The next command can rebuild and save to the same path.
    AssetDatabase::new().save(&path).unwrap();
    assert_eq!(AssetDatabase::load(&path).unwrap().len(), 0);
}
