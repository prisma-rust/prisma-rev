// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("prism-user-data-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(FILE_NAME)
}

#[test]
fn round_trips_through_the_file() {
    let path = temp_path("round-trip");
    {
        let mut store = UserStore::open(&path);
        assert!(store.get("caught").is_none());
        store.set("caught", "3");
        store.set("coins", "120");
        assert!(store.is_dirty());
        assert!(store.flush().unwrap(), "第一次写盘应真的落盘");
        assert!(!store.is_dirty());
    }

    let reopened = UserStore::open(&path);
    assert_eq!(reopened.get("caught"), Some("3"));
    assert_eq!(reopened.get("coins"), Some("120"));
    assert_eq!(reopened.data().version, USER_DATA_VERSION);
}

#[test]
fn a_missing_file_is_an_empty_save_not_an_error() {
    let path = temp_path("missing").join("nested").join(FILE_NAME);
    let mut store = UserStore::open(&path);
    assert!(store.get("anything").is_none());
    // 空存档也允许写：父目录会被创建出来。
    store.set("caught", "1");
    assert!(store.flush().unwrap());
    assert!(path.is_file());
}

#[test]
fn a_newer_save_file_is_never_overwritten() {
    let path = temp_path("newer");
    let newer = format!(
        "{{\"version\": {}, \"entries\": {{\"caught\": \"9\"}}}}",
        USER_DATA_VERSION + 1
    );
    std::fs::write(&path, &newer).unwrap();

    let mut store = UserStore::open(&path);
    // 新版存档的键读不到（解析失败），但**文件必须原样保留**。
    store.set("caught", "0");
    assert!(!store.flush().unwrap(), "只读存档不该写盘");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

#[test]
fn a_corrupt_file_is_kept_and_the_store_stays_read_only() {
    let path = temp_path("corrupt");
    std::fs::write(&path, "{ not json").unwrap();
    let mut store = UserStore::open(&path);
    store.set("caught", "1");
    assert!(!store.flush().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
}

#[test]
fn flush_without_changes_does_not_touch_the_disk() {
    let path = temp_path("clean");
    let mut store = UserStore::open(&path);
    store.set("caught", "2");
    store.flush().unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    let mut reopened = UserStore::open(&path);
    assert!(!reopened.flush().unwrap(), "没有改动不该写盘");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn in_memory_stores_never_touch_the_disk() {
    let mut store = UserStore::in_memory();
    store.set("caught", "5");
    assert_eq!(store.get("caught"), Some("5"));
    assert!(!store.flush().unwrap());
    assert!(store.path().is_none());
}

#[test]
fn global_flags_use_the_engine_namespace_and_round_trip() {
    let path = temp_path("flags");
    {
        let mut store = UserStore::open(&path);
        assert!(!store.get_flag("gate.open"));
        store.set_flag("gate.open", true);
        assert!(store.get_flag("gate.open"));
        assert_eq!(
            store.get("__prism.flag.gate.open"),
            Some("1"),
            "flags are stored in the reserved engine namespace"
        );
        assert!(store.flush().unwrap());
    }
    let mut store = UserStore::open(&path);
    assert!(store.get_flag("gate.open"));
    store.set_flag("gate.open", false);
    assert!(store.flush().unwrap());
    assert!(!UserStore::open(&path).get_flag("gate.open"));
}

#[test]
fn save_paths_follow_the_platform_and_the_override() {
    // 覆盖变量优先级最高，其他分支不参与（测试不能依赖本机 APPDATA）。
    let previous = std::env::var_os(ENV_OVERRIDE);
    std::env::set_var(ENV_OVERRIDE, "D:/tmp/prism-user-data-test");
    let path = save_path("my game/v2");
    match previous {
        Some(value) => std::env::set_var(ENV_OVERRIDE, value),
        None => std::env::remove_var(ENV_OVERRIDE),
    }
    assert!(path.ends_with(FILE_NAME));
    let project_dir = path.parent().unwrap();
    assert_eq!(project_dir.file_name().unwrap(), "my game_v2");
    assert!(project_dir.starts_with("D:/tmp/prism-user-data-test"));
}

#[test]
fn version_defaults_to_one_for_hand_written_files() {
    let data = UserData::parse("{\"entries\": {\"caught\": \"4\"}}").unwrap();
    assert_eq!(data.version, 1);
    assert_eq!(data.get("caught"), Some("4"));
}
