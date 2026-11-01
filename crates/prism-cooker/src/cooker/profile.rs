//! # 烹饪配置系统
//!
//! 支持平台的烹饪配置，包含基于文件的配置文件、继承、
//! 深度合并、优先级覆盖和稳定哈希计算。
//!
//! ## 优先级（从高到低）
//!
//! 1. CLI 覆盖（命令行参数）
//! 2. 资源级覆盖（逐记录设置）
//! 3. 激活的项目配置（CLI `--profile`；没有持久化的 active 文件，缺省即平台默认）
//! 4. 平台默认配置（从 `--platform` 派生）
//! 5. `base.json`（可选，最低优先级）

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use thiserror::Error;

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error in {path}: {detail}")]
    ParseError { path: String, detail: String },

    #[error("Cycle detected in profile inheritance: {0}")]
    Cycle(String),

    #[error("Profile not found: {0}")]
    NotFound(String),

    #[error("Unsupported value: {0}")]
    Unsupported(String),
}

// ===========================================================================
// Core Enums
// ===========================================================================

/// 纹理 压缩 格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum TextureCompression {
    None,
    /// Uncompressed RGBA8 默认
    #[default]
    Rgba8,
    /// BC1-5 (DXT1/3/5) — desktop D3D.
    Bc1,
    Bc3,
    Bc5,
    /// BC6H / BC7 — desktop 高动态范围 / high quality.
    Bc6H,
    Bc7,
    /// ASTC — mobile (Android/iOS).
    Astc4x4,
    Astc6x6,
    Astc8x8,
    Astc12x12,
    /// ETC2 — Android 回退
    Etc2Rgba,
}

/// 目标 platform identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Desktop,
    Android,
    Ios,
    Embedded,
    Custom(String),
}

impl Platform {
    pub fn as_str(&self) -> &str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Android => "android",
            Platform::Ios => "ios",
            Platform::Embedded => "embedded",
            Platform::Custom(s) => s.as_str(),
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "desktop" => Platform::Desktop,
            "android" => Platform::Android,
            "ios" => Platform::Ios,
            "embedded" => Platform::Embedded,
            other => Platform::Custom(other.to_owned()),
        }
    }
}

// ===========================================================================
// Settings Structs (final merged 配置
// ===========================================================================

/// 纹理 cooking settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextureSettings {
    pub compression: TextureCompression,
    pub generate_mips: bool,
    pub quality: u8,   // 0–100
    pub max_size: u32, // 0 = unlimited
}

impl Default for TextureSettings {
    fn default() -> Self {
        Self {
            compression: TextureCompression::Rgba8,
            generate_mips: true,
            quality: 80,
            max_size: 0,
        }
    }
}

/// 网格 cooking settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshSettings {
    pub vertex_compression: bool,
    pub optimize: bool,
    pub generate_tangents: bool,
}

impl Default for MeshSettings {
    fn default() -> Self {
        Self {
            vertex_compression: false,
            optimize: true,
            generate_tangents: false,
        }
    }
}

/// 着色器 cooking settings (实现).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShaderSettings {
    pub target: String,
}

impl Default for ShaderSettings {
    fn default() -> Self {
        Self {
            target: "spirv".to_owned(),
        }
    }
}

/// Package-level 压缩 settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressionSettings {
    pub algorithm: String, // "zstd" or "none"
    pub level: i32,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            algorithm: "zstd".to_owned(),
            level: 3,
        }
    }
}

/// Final merged 运行时 cooking settings.
///
/// All resolver and 覆盖 逻辑 has been applied before constructing this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CookSettings {
    pub platform: String,
    pub texture: TextureSettings,
    pub mesh: MeshSettings,
    pub shader: ShaderSettings,
    pub compression: CompressionSettings,
    pub streaming: bool,
    /// Assets at or above this uncompressed size get `FLAG_STREAMED` when
    /// `streaming` is enabled. Streamed assets stay uncompressed so a reader
    /// can serve them straight from the package offset range.
    #[serde(default = "default_streaming_threshold_bytes")]
    pub streaming_threshold_bytes: u64,
    pub chunk_size: u64,
    pub custom: HashMap<String, serde_json::Value>,
}

/// Default large-asset cutoff for streaming profiles (4 MiB).
pub const DEFAULT_STREAMING_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

fn default_streaming_threshold_bytes() -> u64 {
    DEFAULT_STREAMING_THRESHOLD_BYTES
}

// ===========================================================================
// Partial overrides (what a profile file actually contains)
//
// A profile file only describes the fields it wants to change. Using the
// fully-materialised `*Settings` structs for that is wrong: `#[serde(default)]`
// fills every missing field with a default value, so merging cannot tell
// "absent" from "explicitly set to the default".
// ===========================================================================

/// Partial [`TextureSettings`] override from a profile file.
///
/// `None` = "not specified here", which keeps whatever the parent profile or
/// the built-in default resolved to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextureOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<TextureCompression>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_mips: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u32>,
}

/// Partial [`MeshSettings`] override from a profile file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeshOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertex_compression: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_tangents: Option<bool>,
}

/// Partial [`ShaderSettings`] override from a profile file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShaderOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Partial [`CompressionSettings`] override from a profile file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompressionOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
}

impl TextureSettings {
    /// Overlay only the fields the profile explicitly specified.
    pub fn apply_overrides(&mut self, overrides: &TextureOverrides) {
        if let Some(compression) = overrides.compression {
            self.compression = compression;
        }
        if let Some(generate_mips) = overrides.generate_mips {
            self.generate_mips = generate_mips;
        }
        if let Some(quality) = overrides.quality {
            self.quality = quality;
        }
        if let Some(max_size) = overrides.max_size {
            self.max_size = max_size;
        }
    }
}

impl MeshSettings {
    /// Overlay only the fields the profile explicitly specified.
    pub fn apply_overrides(&mut self, overrides: &MeshOverrides) {
        if let Some(vertex_compression) = overrides.vertex_compression {
            self.vertex_compression = vertex_compression;
        }
        if let Some(optimize) = overrides.optimize {
            self.optimize = optimize;
        }
        if let Some(generate_tangents) = overrides.generate_tangents {
            self.generate_tangents = generate_tangents;
        }
    }
}

impl ShaderSettings {
    /// Overlay only the fields the profile explicitly specified.
    pub fn apply_overrides(&mut self, overrides: &ShaderOverrides) {
        if let Some(ref target) = overrides.target {
            self.target = target.clone();
        }
    }
}

impl CompressionSettings {
    /// Overlay only the fields the profile explicitly specified.
    pub fn apply_overrides(&mut self, overrides: &CompressionOverrides) {
        if let Some(ref algorithm) = overrides.algorithm {
            self.algorithm = algorithm.clone();
        }
        if let Some(level) = overrides.level {
            self.level = level;
        }
    }
}

impl Default for CookSettings {
    fn default() -> Self {
        Self {
            platform: "desktop".to_owned(),
            texture: TextureSettings::default(),
            mesh: MeshSettings::default(),
            shader: ShaderSettings::default(),
            compression: CompressionSettings::default(),
            streaming: false,
            streaming_threshold_bytes: DEFAULT_STREAMING_THRESHOLD_BYTES,
            chunk_size: 64 * 1024,
            custom: HashMap::new(),
        }
    }
}

impl CookSettings {
    /// Whether an asset of `uncompressed_size` bytes should be marked
    /// `FLAG_STREAMED` under this profile.
    ///
    /// The cutoff is ignored when streaming is disabled, so desktop builds
    /// keep their existing compressed layout unless the profile opts in.
    pub fn should_stream(&self, uncompressed_size: usize) -> bool {
        self.streaming && uncompressed_size as u64 >= self.streaming_threshold_bytes
    }

    /// 计算一个稳定的确定性哈希，供增量构建缓存使用。
    ///
    /// `custom` 是 `HashMap`，迭代顺序每个进程都随机化，所以先去掉它再序列化，
    /// 然后把**按 key 排序**后的键值对追加到哈希输入。否则同样的设置在不同进程
    /// 或不同插入顺序下会得到不同哈希，缓存会被误判为失效。
    pub fn settings_hash(&self) -> u64 {
        let mut canonical = self.clone();
        canonical.custom.clear();
        let mut buffer = serde_json::to_string(&canonical)
            .expect("CookSettings should always serialize")
            .into_bytes();

        let mut keys: Vec<&String> = self.custom.keys().collect();
        keys.sort();
        for key in keys {
            buffer.extend_from_slice(key.as_bytes());
            buffer.push(0x1f); // unit separator：key 与 value 之间
            buffer.extend_from_slice(
                serde_json::to_string(&self.custom[key])
                    .expect("custom values should always serialize")
                    .as_bytes(),
            );
            buffer.push(0x1e); // record separator：不同条目之间
        }

        xxhash_rust::xxh3::xxh3_64(&buffer)
    }
}

// ===========================================================================
// 配置 结构体 (JSON-deserializable, 部分
// ===========================================================================

/// A single 配置 file. Every field is optional — only the fields that
/// should 覆盖 the base/parent are present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CookProfile {
    /// Optional parent 配置 name for 继承
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,

    /// Optional 配置 version for 格式 evolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,

    /// Platform identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub texture: Option<TextureOverrides>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh: Option<MeshOverrides>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub shader: Option<ShaderOverrides>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionOverrides>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming_threshold_bytes: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u64>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, serde_json::Value>,
}

/// CLI-level overrides that sit above the project 配置
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub platform: Option<String>,
    pub profile: Option<String>,
    pub texture_compression: Option<TextureCompression>,
    pub no_mipmaps: bool,
    pub streaming: Option<bool>,
    pub streaming_threshold_bytes: Option<u64>,
    pub compression_algo: Option<String>,
    pub compression_level: Option<i32>,
    pub custom: HashMap<String, serde_json::Value>,
}

// ===========================================================================
// 配置 管理器
// ===========================================================================

/// Loads, resolves 继承 chains, merges, and computes final CookSettings.
pub struct ProfileManager {
    /// Directory containing 配置 JSON files.
    profiles_dir: PathBuf,
    /// In-memory cache of parsed profiles (name → CookProfile).
    loaded: HashMap<String, CookProfile>,
}

impl ProfileManager {
    /// 创建 a new 管理器 that looks for profiles under `profiles_dir`.
    ///
    /// No files are loaded until [`load_profile`] or 解析 is called.
    pub fn new<P: Into<PathBuf>>(profiles_dir: P) -> Self {
        Self {
            profiles_dir: profiles_dir.into(),
            loaded: HashMap::new(),
        }
    }

    /// 加载 a single 配置 file by name (without `.json` suffix).
    pub fn load_profile(&mut self, name: &str) -> Result<CookProfile, ProfileError> {
        // Check built-in cache 第一个 (only built-ins are cached).
        if let Some(cached) = self.loaded.get(name) {
            return Ok(cached.clone());
        }

        // Check for a user file. Not cached to avoid shadowing the built-in
        // when a user 配置 inherits via `base: "desktop"`.
        let file_path = self.profiles_dir.join(format!("{name}.json"));
        if file_path.exists() {
            let raw = std::fs::read_to_string(&file_path).map_err(ProfileError::Io)?;
            let profile: CookProfile =
                serde_json::from_str(&raw).map_err(|e| ProfileError::ParseError {
                    path: file_path.display().to_string(),
                    detail: e.to_string(),
                })?;
            return Ok(profile);
        }

        // Fall 后 to built-in 默认
        if let Some(builtin) = BUILTIN_DEFAULTS.get(name) {
            let profile = (*builtin).clone();
            self.loaded.insert(name.to_owned(), profile.clone());
            return Ok(profile);
        }

        Err(ProfileError::NotFound(name.to_owned()))
    }

    /// 解析 a 配置 name into the final merged CookSettings, applying
    /// 继承 and the priority 链
    ///
    /// 继承 detection: a cycle-detection 集合 tracking the 当前 链
    /// of base→child 配置 names is maintained. If a name repeats, it's an
    /// 错误
    pub fn resolve(&mut self, profile_name: &str) -> Result<CookSettings, ProfileError> {
        let mut seen = Vec::new();
        self.resolve_internal(profile_name, &mut seen)
    }

    fn resolve_internal(
        &mut self,
        name: &str,
        seen: &mut Vec<String>,
    ) -> Result<CookSettings, ProfileError> {
        if seen.contains(&name.to_owned()) {
            seen.push(name.to_owned());
            return Err(ProfileError::Cycle(seen.join(" → ")));
        }
        seen.push(name.to_owned());

        let profile = self.load_profile(name)?;

        // Start from either the parent's resolved settings or the defaults.
        let mut settings = match profile.base {
            Some(ref base_name) => self.resolve_internal(base_name, seen)?,
            None => CookSettings::default(),
        };

        // 叠加 only the fields this profile explicitly specifies.
        if let Some(ref p) = profile.platform {
            settings.platform = p.clone();
        }
        if let Some(ref t) = profile.texture {
            settings.texture.apply_overrides(t);
        }
        if let Some(ref m) = profile.mesh {
            settings.mesh.apply_overrides(m);
        }
        if let Some(ref s) = profile.shader {
            settings.shader.apply_overrides(s);
        }
        if let Some(ref c) = profile.compression {
            settings.compression.apply_overrides(c);
        }
        if let Some(v) = profile.streaming {
            settings.streaming = v;
        }
        if let Some(v) = profile.streaming_threshold_bytes {
            settings.streaming_threshold_bytes = v;
        }
        if let Some(v) = profile.chunk_size {
            settings.chunk_size = v;
        }
        settings.custom.extend(profile.custom.clone());

        seen.pop();
        Ok(settings)
    }

    /// Apply CLI overrides on 顶部 of resolved CookSettings.
    pub fn apply_cli_overrides(
        settings: &mut CookSettings,
        overrides: &CliOverrides,
    ) -> Result<(), ProfileError> {
        if let Some(ref p) = overrides.platform {
            settings.platform = p.clone();
        }
        if let Some(ref tc) = overrides.texture_compression {
            settings.texture.compression = *tc;
        }
        if overrides.no_mipmaps {
            settings.texture.generate_mips = false;
        }
        if let Some(s) = overrides.streaming {
            settings.streaming = s;
        }
        if let Some(threshold) = overrides.streaming_threshold_bytes {
            settings.streaming_threshold_bytes = threshold;
        }
        if let Some(ref a) = overrides.compression_algo {
            settings.compression.algorithm = a.clone();
        }
        if let Some(l) = overrides.compression_level {
            settings.compression.level = l;
        }
        settings.custom.extend(overrides.custom.clone());
        Ok(())
    }

    /// 列表 all available 配置 names (built-in + user files).
    pub fn list_profiles(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_DEFAULTS.keys().map(|s| s.to_string()).collect();

        if let Ok(entries) = std::fs::read_dir(&self.profiles_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if !names.contains(&stem.to_owned()) {
                            names.push(stem.to_owned());
                        }
                    }
                }
            }
        }

        names.sort();
        names
    }

    /// Show the fully resolved settings for a 配置
    pub fn show(&mut self, name: &str) -> Result<CookSettings, ProfileError> {
        self.resolve(name)
    }
}

// ===========================================================================
// Built-in 默认 Profiles
// ===========================================================================

static BUILTIN_DEFAULTS: LazyLock<HashMap<&'static str, CookProfile>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    // ── base.json ────────────────────────────────────────────────
    m.insert(
        "base",
        CookProfile {
            version: Some(1),
            texture: Some(TextureOverrides {
                compression: Some(TextureCompression::Rgba8),
                generate_mips: Some(true),
                quality: Some(80),
                max_size: Some(0),
            }),
            mesh: Some(MeshOverrides {
                vertex_compression: Some(false),
                optimize: Some(true),
                generate_tangents: Some(false),
            }),
            shader: Some(ShaderOverrides {
                target: Some("spirv".into()),
            }),
            compression: Some(CompressionOverrides {
                algorithm: Some("zstd".into()),
                level: Some(3),
            }),
            streaming: Some(false),
            chunk_size: Some(64 * 1024),
            ..Default::default()
        },
    );

    // ── desktop.json ─────────────────────────────────────────────
    m.insert(
        "desktop",
        CookProfile {
            version: Some(1),
            base: Some("base".into()),
            platform: Some("desktop".into()),
            texture: Some(TextureOverrides {
                compression: Some(TextureCompression::Bc7),
                generate_mips: Some(true),
                quality: Some(90),
                max_size: Some(4096),
            }),
            mesh: Some(MeshOverrides {
                vertex_compression: Some(false),
                optimize: Some(true),
                generate_tangents: Some(true),
            }),
            streaming: Some(false),
            ..Default::default()
        },
    );

    // ── android.json ─────────────────────────────────────────────
    m.insert(
        "android",
        CookProfile {
            version: Some(1),
            base: Some("base".into()),
            platform: Some("android".into()),
            texture: Some(TextureOverrides {
                compression: Some(TextureCompression::Astc8x8),
                generate_mips: Some(true),
                quality: Some(75),
                max_size: Some(2048),
            }),
            mesh: Some(MeshOverrides {
                vertex_compression: Some(true),
                optimize: Some(true),
                generate_tangents: Some(false),
            }),
            streaming: Some(true),
            streaming_threshold_bytes: Some(1024 * 1024),
            chunk_size: Some(32 * 1024),
            ..Default::default()
        },
    );

    // ── ios.json ─────────────────────────────────────────────────
    m.insert(
        "ios",
        CookProfile {
            version: Some(1),
            base: Some("base".into()),
            platform: Some("ios".into()),
            texture: Some(TextureOverrides {
                compression: Some(TextureCompression::Astc8x8),
                generate_mips: Some(true),
                quality: Some(80),
                max_size: Some(2048),
            }),
            mesh: Some(MeshOverrides {
                vertex_compression: Some(true),
                optimize: Some(true),
                generate_tangents: Some(false),
            }),
            streaming: Some(true),
            streaming_threshold_bytes: Some(1024 * 1024),
            chunk_size: Some(32 * 1024),
            ..Default::default()
        },
    );

    // ── embedded.json ────────────────────────────────────────────
    m.insert(
        "embedded",
        CookProfile {
            version: Some(1),
            base: Some("base".into()),
            platform: Some("embedded".into()),
            texture: Some(TextureOverrides {
                compression: Some(TextureCompression::Etc2Rgba),
                generate_mips: Some(false),
                quality: Some(50),
                max_size: Some(1024),
            }),
            mesh: Some(MeshOverrides {
                vertex_compression: Some(true),
                optimize: Some(true),
                generate_tangents: Some(false),
            }),
            compression: Some(CompressionOverrides {
                algorithm: Some("zstd".into()),
                level: Some(5),
            }),
            streaming: Some(true),
            streaming_threshold_bytes: Some(1024 * 1024),
            chunk_size: Some(16 * 1024),
            ..Default::default()
        },
    );

    m
});

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
