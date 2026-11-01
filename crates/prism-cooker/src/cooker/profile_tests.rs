// ===========================================================================
// Tests
// ===========================================================================

use super::*;

#[test]
fn settings_hash_is_stable() {
    let s1 = CookSettings::default();
    let s2 = CookSettings::default();
    assert_eq!(s1.settings_hash(), s2.settings_hash());
}

#[test]
fn settings_hash_changes_on_modification() {
    let mut s1 = CookSettings::default();
    let h1 = s1.settings_hash();
    s1.texture.max_size = 2048;
    let h2 = s1.settings_hash();
    assert_ne!(h1, h2, "hash must change when settings change");
}

#[test]
fn settings_hash_ignores_custom_map_insertion_order() {
    let entries = [
        ("alpha", serde_json::json!(1)),
        ("beta", serde_json::json!("two")),
        ("gamma", serde_json::json!({"nested": [1, 2, 3]})),
        ("delta", serde_json::json!(null)),
        ("epsilon", serde_json::json!([true, false])),
        ("zeta", serde_json::json!(3.5)),
        ("eta", serde_json::json!({"a": 1})),
        ("theta", serde_json::json!("x")),
    ];

    let mut forward = CookSettings::default();
    for (key, value) in entries.clone() {
        forward.custom.insert(key.to_owned(), value);
    }

    // 同样的键值、相反的插入顺序 + 不同的 HashMap 容量：容量会改变桶布局，
    // 因此迭代顺序一定与 forward 不同，能稳定暴露「直接序列化 HashMap」的问题。
    let mut reverse = CookSettings {
        custom: HashMap::with_capacity(64),
        ..Default::default()
    };
    for (key, value) in entries.into_iter().rev() {
        reverse.custom.insert(key.to_owned(), value);
    }

    assert_eq!(
        forward.settings_hash(),
        reverse.settings_hash(),
        "custom map iteration order must not affect the settings hash"
    );
}

#[test]
fn settings_hash_covers_custom_entries() {
    let mut one = CookSettings::default();
    one.custom.insert("alpha".to_owned(), serde_json::json!(1));
    let mut two = CookSettings::default();
    two.custom.insert("alpha".to_owned(), serde_json::json!(2));

    assert_ne!(one.settings_hash(), two.settings_hash());
    assert_ne!(
        CookSettings::default().settings_hash(),
        one.settings_hash(),
        "custom entries must still contribute to the hash"
    );
}

#[test]
fn builtin_profile_loading() {
    let dir = std::env::temp_dir().join("cook_profiles_test");
    let mut mgr = ProfileManager::new(&dir);
    let profile = mgr.load_profile("desktop").unwrap();
    assert_eq!(profile.platform.as_deref(), Some("desktop"));
    assert_eq!(profile.base.as_deref(), Some("base"));
}

#[test]
fn resolve_desktop_profile() {
    let dir = std::env::temp_dir().join("cook_profiles_test");
    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("desktop").unwrap();

    assert_eq!(settings.platform, "desktop");
    assert_eq!(settings.texture.compression, TextureCompression::Bc7);
    assert!(settings.texture.generate_mips);
    assert_eq!(settings.texture.quality, 90);
    assert_eq!(settings.texture.max_size, 4096);
    assert!(settings.mesh.generate_tangents);
    assert!(!settings.streaming);
}

#[test]
fn resolve_android_profile() {
    let dir = std::env::temp_dir().join("cook_profiles_test");
    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("android").unwrap();

    assert_eq!(settings.platform, "android");
    assert_eq!(settings.texture.compression, TextureCompression::Astc8x8);
    assert_eq!(settings.texture.max_size, 2048);
    assert!(settings.mesh.vertex_compression);
    assert!(settings.streaming);
    assert_eq!(settings.chunk_size, 32 * 1024);
}

#[test]
fn resolve_embedded_profile() {
    let dir = std::env::temp_dir().join("cook_profiles_test");
    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("embedded").unwrap();

    assert_eq!(settings.platform, "embedded");
    assert!(!settings.texture.generate_mips);
    assert_eq!(settings.texture.compression, TextureCompression::Etc2Rgba);
    assert_eq!(settings.texture.max_size, 1024);
    assert_eq!(settings.texture.quality, 50);
    assert_eq!(settings.chunk_size, 16 * 1024);
    assert_eq!(settings.compression.level, 5);
}

#[test]
fn cycle_detection() {
    let unique = format!("cycle_test_{}", std::process::id());
    let cycle_dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&cycle_dir).ok();
    let cycle_json = serde_json::json!({
        "base": "cycle_self"
    });
    std::fs::write(cycle_dir.join("cycle_self.json"), cycle_json.to_string()).ok();

    let mut mgr = ProfileManager::new(&cycle_dir);
    let result = mgr.resolve("cycle_self");
    assert!(result.is_err(), "cycle must be detected");
    if let Err(ProfileError::Cycle(chain)) = result {
        assert!(
            chain.contains("cycle_self"),
            "chain should include the cycle name"
        );
    } else {
        panic!("expected Cycle error");
    }

    std::fs::remove_file(cycle_dir.join("cycle_self.json")).ok();
    std::fs::remove_dir(&cycle_dir).ok();
}

#[test]
fn profile_not_found_error() {
    let dir = std::env::temp_dir().join("nonexistent_profiles");
    let mut mgr = ProfileManager::new(&dir);
    let result = mgr.resolve("does_not_exist");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ProfileError::NotFound(_)));
}

#[test]
fn cli_overrides_apply_correctly() {
    let mut settings = CookSettings::default();

    let overrides = CliOverrides {
        texture_compression: Some(TextureCompression::Bc7),
        no_mipmaps: true,
        streaming: Some(true),
        streaming_threshold_bytes: Some(2048),
        compression_level: Some(9),
        ..Default::default()
    };

    ProfileManager::apply_cli_overrides(&mut settings, &overrides).unwrap();
    assert_eq!(settings.texture.compression, TextureCompression::Bc7);
    assert!(!settings.texture.generate_mips);
    assert!(settings.streaming);
    assert_eq!(settings.streaming_threshold_bytes, 2048);
    assert_eq!(settings.compression.level, 9);
}

#[test]
fn streaming_threshold_only_applies_when_streaming_is_enabled() {
    let disabled = CookSettings {
        streaming_threshold_bytes: 1024,
        ..Default::default()
    };

    // Desktop/default profiles keep the old all-compressed layout.
    assert!(!disabled.streaming);
    assert!(!disabled.should_stream(usize::MAX));

    let enabled = CookSettings {
        streaming: true,
        streaming_threshold_bytes: 1024,
        ..Default::default()
    };
    assert!(!enabled.should_stream(1023));
    assert!(enabled.should_stream(1024), "cutoff is inclusive");
    assert!(enabled.should_stream(usize::MAX));
}

#[test]
fn profile_can_override_the_streaming_threshold() {
    let dir = std::env::temp_dir().join(format!("prism-stream-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mobile.json"),
        serde_json::json!({
            "base": "android",
            "streaming": true,
            "streaming_threshold_bytes": 4096
        })
        .to_string(),
    )
    .unwrap();

    let mut manager = ProfileManager::new(&dir);
    let settings = manager.resolve("mobile").unwrap();
    assert!(settings.streaming);
    assert_eq!(settings.streaming_threshold_bytes, 4096);
    assert!(settings.should_stream(4096));
    assert!(!settings.should_stream(4095));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_overrides_custom_extend() {
    let mut settings = CookSettings::default();
    let mut custom = HashMap::new();
    custom.insert("foo".into(), serde_json::json!("bar"));
    let overrides = CliOverrides {
        custom,
        ..Default::default()
    };
    ProfileManager::apply_cli_overrides(&mut settings, &overrides).unwrap();
    assert_eq!(
        settings.custom.get("foo").and_then(|v| v.as_str()),
        Some("bar")
    );
}

#[test]
fn list_builtin_profiles() {
    let dir = std::env::temp_dir().join("cook_profiles_list_test");
    let mgr = ProfileManager::new(&dir);
    let names = mgr.list_profiles();
    assert!(names.contains(&"desktop".to_owned()));
    assert!(names.contains(&"android".to_owned()));
    assert!(names.contains(&"ios".to_owned()));
    assert!(names.contains(&"embedded".to_owned()));
    assert!(names.contains(&"base".to_owned()));
}

#[test]
fn user_profile_overrides_builtin() {
    let unique = format!("user_profile_test_{}", std::process::id());
    let dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&dir).ok();

    // 写入 a 自定义 配置 that inherits from built-in "desktop".
    let user_profile = serde_json::json!({
        "base": "desktop",
        "texture": {
            "quality": 100,
            "max_size": 8192
        }
    });
    std::fs::write(dir.join("high_quality.json"), user_profile.to_string()).ok();

    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("high_quality").unwrap();
    assert_eq!(settings.texture.quality, 100);
    assert_eq!(settings.texture.max_size, 8192);
    // Should still inherit desktop base features.
    assert!(settings.mesh.generate_tangents);

    // Best-effort cleanup.
    let _ = std::fs::remove_file(dir.join("high_quality.json"));
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn profile_priority_chain() {
    // CLI overrides > resolved 配置
    let dir = std::env::temp_dir().join("priority_test");
    let mut mgr = ProfileManager::new(&dir);
    let mut settings = mgr.resolve("android").unwrap();

    assert_eq!(settings.texture.compression, TextureCompression::Astc8x8);

    let cli = CliOverrides {
        texture_compression: Some(TextureCompression::Bc7),
        ..Default::default()
    };
    ProfileManager::apply_cli_overrides(&mut settings, &cli).unwrap();

    assert_eq!(settings.texture.compression, TextureCompression::Bc7);
    // Other Android settings preserved.
    assert!(settings.streaming);
    assert_eq!(settings.chunk_size, 32 * 1024);
}

#[test]
fn hash_depends_on_profile() {
    let dir = std::env::temp_dir().join("hash_profile_test");
    let mut mgr = ProfileManager::new(&dir);

    let desktop = mgr.resolve("desktop").unwrap();
    let android = mgr.resolve("android").unwrap();

    assert_ne!(desktop.settings_hash(), android.settings_hash());
}

#[test]
fn partial_texture_override_keeps_parent_fields() {
    let unique = format!("partial_override_test_{}", std::process::id());
    let dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(
        dir.join("wide.json"),
        serde_json::json!({
            "base": "base",
            "texture": { "compression": "bc7", "max_size": 4096, "quality": 90 }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("child.json"),
        serde_json::json!({
            "base": "wide",
            "texture": { "quality": 95 }
        })
        .to_string(),
    )
    .unwrap();

    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("child").unwrap();

    assert_eq!(settings.texture.quality, 95, "explicit field must win");
    assert_eq!(
        settings.texture.max_size, 4096,
        "a field the child omits must keep the parent value, not reset to 0"
    );
    assert_eq!(settings.texture.compression, TextureCompression::Bc7);
    assert!(settings.texture.generate_mips);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_default_values_are_honoured() {
    let unique = format!("explicit_default_test_{}", std::process::id());
    let dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&dir).unwrap();

    // Every value here equals the built-in default; they must still override
    // the parent profile instead of being treated as "not specified".
    std::fs::write(
        dir.join("plain.json"),
        serde_json::json!({
            "base": "desktop",
            "texture": { "compression": "rgba8", "max_size": 0, "generate_mips": false }
        })
        .to_string(),
    )
    .unwrap();

    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("plain").unwrap();

    assert_eq!(settings.texture.compression, TextureCompression::Rgba8);
    assert_eq!(settings.texture.max_size, 0);
    assert!(!settings.texture.generate_mips);
    // Sections the child does not mention still inherit from `desktop`.
    assert_eq!(settings.texture.quality, 90);
    assert!(settings.mesh.generate_tangents);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_false_flag_is_applied() {
    let unique = format!("explicit_false_test_{}", std::process::id());
    let dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&dir).unwrap();

    // `optimize` defaults to true, so `false` used to be indistinguishable
    // from an absent field.
    std::fs::write(
        dir.join("unoptimized.json"),
        serde_json::json!({
            "base": "desktop",
            "mesh": { "optimize": false, "generate_tangents": false }
        })
        .to_string(),
    )
    .unwrap();

    let mut mgr = ProfileManager::new(&dir);
    let settings = mgr.resolve("unoptimized").unwrap();

    assert!(!settings.mesh.optimize);
    assert!(!settings.mesh.generate_tangents);
    assert!(!settings.mesh.vertex_compression);
    assert_eq!(settings.texture.quality, 90, "untouched sections inherit");

    let _ = std::fs::remove_dir_all(&dir);
}
