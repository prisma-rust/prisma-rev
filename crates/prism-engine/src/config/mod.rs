//! Application 配置 loaded from `assets/settings.ini`.
//!
//! 所有字段都有默认值，因此文件完全是可选的——缺少 section / key 时安静地
//! 沿用默认值；值写坏时 `log::warn!` 一条并回到默认值（见 [`ini`]）。
//!
//! 旧项目里的 `assets/settings.toml` 仍然可读（`load()` 会回退并提示迁移），
//! 但**写端**一律产出 `settings.ini`；一次性迁移由
//! `prism-build-pipeline build-scenes` 完成。

pub mod ini;

use ini::Ini;

use crate::render_settings::{InternalScaleMode, RenderMode, RenderSettings};

// ---------------------------------------------------------------------------
// Top-level 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub app: AppInfo,
    pub window: WindowConfig,
    pub render: RenderConfig,
}

// ---------------------------------------------------------------------------
// Render defaults (`[render]` in settings.ini)
// ---------------------------------------------------------------------------

/// `[render]` 段：启动时的渲染 / 调试 / 路径追踪默认值。
///
/// 全部字段可选——缺省沿用 [`RenderSettings::default`]。编辑器（Tauri / egui）
/// 会读写同一份文件，因此这里的字段名就是项目级渲染设置的外部契约。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderConfig {
    /// `"raster"` 或 `"path_trace"`。
    pub render_mode: Option<String>,
    /// PBR isolate 位掩码（0 = 正常渲染）。
    pub debug_flags: Option<u32>,
    /// 0 = Reinhard, 1 = ACES, 2 = passthrough（2D / 像素画）。
    pub tonemap_mode: Option<u32>,
    /// 后处理抗锯齿：0 = 关（默认），1 = FXAA。
    pub aa_mode: Option<u32>,
    /// 显示空间对比度（`1.0` = 不变，#502）。
    pub contrast: Option<f32>,
    /// 主方向光阴影覆盖距离（米，见 `RenderSettings::shadow_distance`）。
    pub shadow_distance: Option<f32>,
    /// 阴影贴图边长（texel，见 `RenderSettings::shadow_map_size`，#526）。
    pub shadow_map_size: Option<u32>,
    /// 显示空间饱和度（`1.0` = 不变，`0.0` = 灰度）。
    pub saturation: Option<f32>,
    /// 暗角强度（`0.0` = 关，`1.0` = 四角全黑）。
    pub vignette: Option<f32>,
    /// 0 = HDR tonemap, 1 = depth, 2 = view-space normal。
    pub debug_rt: Option<u32>,
    /// 前向管线线框模式（#431）。
    pub wireframe: Option<bool>,
    /// 填充 + 线框叠加（#431）。
    pub shaded_wireframe: Option<bool>,
    pub pt_max_bounces: Option<u32>,
    pub pt_ray_max_distance: Option<f32>,
    /// 0 = accumulate forever。
    pub pt_max_iterations: Option<u32>,
    /// 内部渲染分辨率 `"640x360"`（#613）；`"off"` / 空 = 按窗口渲染。
    pub internal_resolution: Option<String>,
    /// 放大方式：`"stretch"`（默认，铺满窗口）或 `"integer"`（整数倍 + 黑边）。
    pub internal_scale_mode: Option<String>,
}

impl RenderConfig {
    /// 把配置覆盖到运行时设置上（仅覆盖显式给出的字段）。
    pub fn apply(&self, settings: &mut RenderSettings) {
        if let Some(mode) = &self.render_mode {
            settings.render_mode = match mode.as_str() {
                "path_trace" | "path-trace" | "pathtrace" => RenderMode::PathTrace,
                "raster" => RenderMode::Raster,
                other => {
                    log::warn!("settings: unknown render_mode '{other}'; using raster");
                    RenderMode::Raster
                }
            };
        }
        if let Some(flags) = self.debug_flags {
            settings.debug_flags = flags;
        }
        if let Some(wireframe) = self.wireframe {
            settings.wireframe = wireframe;
        }
        if let Some(shaded_wireframe) = self.shaded_wireframe {
            settings.shaded_wireframe = shaded_wireframe;
        }
        if let Some(mode) = self.tonemap_mode {
            settings.tonemap_mode = mode;
        }
        if let Some(mode) = self.aa_mode {
            settings.aa_mode = mode;
        }
        if let Some(distance) = self.shadow_distance {
            // 0 或负数的盒子没有任何意义，挡住并留个日志。
            if distance.is_finite() && distance > 0.0 {
                settings.shadow_distance = distance;
            } else {
                log::warn!("settings: shadow_distance must be positive, got {distance}");
            }
        }
        if let Some(size) = self.shadow_map_size {
            if (prism_render::MIN_SHADOW_MAP_SIZE..=prism_render::MAX_SHADOW_MAP_SIZE)
                .contains(&size)
            {
                settings.shadow_map_size = size;
            } else {
                log::warn!(
                    "settings: shadow_map_size must be {}..={}, got {size}",
                    prism_render::MIN_SHADOW_MAP_SIZE,
                    prism_render::MAX_SHADOW_MAP_SIZE
                );
            }
        }
        if let Some(value) = self.contrast {
            // 负对比度会把画面翻负，这里挡在 0。
            settings.contrast = value.max(0.0);
        }
        if let Some(value) = self.saturation {
            settings.saturation = value.max(0.0);
        }
        if let Some(value) = self.vignette {
            settings.vignette = value.clamp(0.0, 1.0);
        }
        if let Some(rt) = self.debug_rt {
            settings.debug_rt = rt;
        }
        if let Some(bounces) = self.pt_max_bounces {
            settings.pt_max_bounces = bounces;
        }
        if let Some(distance) = self.pt_ray_max_distance {
            settings.pt_ray_max_distance = distance;
        }
        if let Some(iterations) = self.pt_max_iterations {
            settings.pt_max_iterations = iterations;
        }
        if let Some(value) = &self.internal_resolution {
            match parse_internal_resolution(value) {
                Some(resolution) => settings.internal_resolution = resolution,
                None => log::warn!(
                    "settings: internal_resolution '{value}' is not '<w>x<h>' or 'off'; \
                     rendering at window resolution"
                ),
            }
        }
        if let Some(value) = &self.internal_scale_mode {
            match InternalScaleMode::parse(value) {
                Some(mode) => settings.internal_scale_mode = mode,
                None => {
                    log::warn!("settings: unknown internal_scale_mode '{value}'; using 'stretch'")
                }
            }
        }
    }

    fn from_ini(ini: &Ini) -> Self {
        Self {
            render_mode: ini.get("render", "render_mode").map(str::to_string),
            debug_flags: ini.get_u32("render", "debug_flags"),
            tonemap_mode: ini.get_u32("render", "tonemap_mode"),
            aa_mode: ini.get_u32("render", "aa_mode"),
            contrast: ini.get_f32("render", "contrast"),
            shadow_distance: ini.get_f32("render", "shadow_distance"),
            shadow_map_size: ini.get_u32("render", "shadow_map_size"),
            saturation: ini.get_f32("render", "saturation"),
            vignette: ini.get_f32("render", "vignette"),
            debug_rt: ini.get_u32("render", "debug_rt"),
            wireframe: ini.get_bool("render", "wireframe"),
            shaded_wireframe: ini.get_bool("render", "shaded_wireframe"),
            pt_max_bounces: ini.get_u32("render", "pt_max_bounces"),
            pt_ray_max_distance: ini.get_f32("render", "pt_ray_max_distance"),
            pt_max_iterations: ini.get_u32("render", "pt_max_iterations"),
            internal_resolution: ini.get("render", "internal_resolution").map(str::to_string),
            internal_scale_mode: ini.get("render", "internal_scale_mode").map(str::to_string),
        }
    }
}

/// 解析 `"640x360"` / `"640,360"` / `"640*360"`；`"off"` / `"native"` / 空 → `[0, 0]`。
fn parse_internal_resolution(value: &str) -> Option<[u32; 2]> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() || lower == "off" || lower == "native" || lower == "none" {
        return Some([0, 0]);
    }
    let mut parts = lower.split(|ch| ch == 'x' || ch == ',' || ch == '*');
    let width: u32 = parts.next()?.trim().parse().ok()?;
    let height: u32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([width, height])
}

// ---------------------------------------------------------------------------
// App identity (company name, app name — like 's PlayerSettings)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    pub company: String,
    pub name: String,
}

impl Default for AppInfo {
    fn default() -> Self {
        Self {
            company: default_company(),
            name: default_app_name(),
        }
    }
}

impl AppInfo {
    fn from_ini(ini: &Ini) -> Self {
        let mut info = Self::default();
        if let Some(value) = ini.get("app", "company") {
            info.company = value.to_string();
        }
        if let Some(value) = ini.get("app", "name") {
            info.name = value.to_string();
        }
        info
    }
}

fn default_company() -> String {
    "Excurs1ons".to_string()
}

fn default_app_name() -> String {
    "PrismaRev".to_string()
}

// ---------------------------------------------------------------------------
// 窗口 defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct WindowConfig {
    pub title: String,

    pub width: u32,
    pub height: u32,

    pub min_width: Option<u32>,
    pub min_height: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,

    pub position_x: Option<i32>,
    pub position_y: Option<i32>,

    pub resizable: bool,

    pub fullscreen: bool,

    pub maximized: bool,

    pub visible: bool,

    pub decorations: bool,

    pub vsync: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            width: default_width(),
            height: default_height(),
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            position_x: None,
            position_y: None,
            resizable: default_resizable(),
            fullscreen: false,
            maximized: false,
            visible: default_visible(),
            decorations: default_decorations(),
            vsync: default_vsync(),
        }
    }
}

impl WindowConfig {
    fn from_ini(ini: &Ini) -> Self {
        let mut window = Self::default();
        if let Some(value) = ini.get("window", "title") {
            window.title = value.to_string();
        }
        window.width = ini.get_u32("window", "width").unwrap_or(window.width);
        window.height = ini.get_u32("window", "height").unwrap_or(window.height);
        window.min_width = ini.get_u32("window", "min_width");
        window.min_height = ini.get_u32("window", "min_height");
        window.max_width = ini.get_u32("window", "max_width");
        window.max_height = ini.get_u32("window", "max_height");
        window.position_x = ini.get_i32("window", "position_x");
        window.position_y = ini.get_i32("window", "position_y");
        window.resizable = ini
            .get_bool("window", "resizable")
            .unwrap_or(window.resizable);
        window.fullscreen = ini
            .get_bool("window", "fullscreen")
            .unwrap_or(window.fullscreen);
        window.maximized = ini
            .get_bool("window", "maximized")
            .unwrap_or(window.maximized);
        window.visible = ini.get_bool("window", "visible").unwrap_or(window.visible);
        window.decorations = ini
            .get_bool("window", "decorations")
            .unwrap_or(window.decorations);
        window.vsync = ini.get_bool("window", "vsync").unwrap_or(window.vsync);
        window
    }
}

fn default_title() -> String {
    "PrismaRev".to_string()
}

const fn default_width() -> u32 {
    1600
}

const fn default_height() -> u32 {
    900
}

const fn default_resizable() -> bool {
    true
}

const fn default_visible() -> bool {
    true
}

const fn default_decorations() -> bool {
    true
}

const fn default_vsync() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

const CONFIG_PATH: &str = "settings.ini";
const LEGACY_CONFIG_PATH: &str = "settings.toml";

/// 引擎认识的 section / key。其余内容一律忽略（`log::debug!` 记一条），
/// 这样编辑器把 `[build]` 等自有段写进同一份文件时引擎不会报错。
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    ("app", &["company", "name"]),
    (
        "window",
        &[
            "title",
            "width",
            "height",
            "min_width",
            "min_height",
            "max_width",
            "max_height",
            "position_x",
            "position_y",
            "resizable",
            "fullscreen",
            "maximized",
            "visible",
            "decorations",
            "vsync",
        ],
    ),
    (
        "render",
        &[
            "render_mode",
            "debug_flags",
            "tonemap_mode",
            "aa_mode",
            "contrast",
            "shadow_distance",
            "shadow_map_size",
            "saturation",
            "vignette",
            "debug_rt",
            "wireframe",
            "shaded_wireframe",
            "pt_max_bounces",
            "pt_ray_max_distance",
            "pt_max_iterations",
            "internal_resolution",
            "internal_scale_mode",
        ],
    ),
];

fn log_unknown_entries(ini: &Ini) {
    for section in ini.sections() {
        let known = KNOWN_KEYS
            .iter()
            .find(|(name, _)| *name == section.name.as_str())
            .map(|(_, keys)| *keys);
        let Some(keys) = known else {
            log::debug!("settings: ignoring unknown section [{}]", section.name);
            continue;
        };
        for (key, _) in &section.entries {
            if !keys.contains(&key.as_str()) {
                log::debug!("settings: ignoring unknown key [{}.{}]", section.name, key);
            }
        }
    }
}

impl AppConfig {
    /// 从调用方提供的文本解析配置；不访问文件系统。
    pub fn from_ini(text: &str) -> Self {
        let ini = Ini::parse(text);
        log_unknown_entries(&ini);
        Self {
            app: AppInfo::from_ini(&ini),
            window: WindowConfig::from_ini(&ini),
            render: RenderConfig::from_ini(&ini),
        }
    }

    /// 加载 `assets/settings.ini`；不存在时回退读旧的 `assets/settings.toml`
    /// 并提示迁移。两者都没有时返回默认值。
    ///
    /// 路径经 [`crate::asset_paths`] 解析，不依赖进程 cwd。
    #[deprecated(note = "use prism_app::load_config or AppConfig::from_ini")]
    pub fn load() -> Self {
        for (name, legacy) in [(CONFIG_PATH, false), (LEGACY_CONFIG_PATH, true)] {
            let Some(path) = crate::asset_paths::asset_path(name) else {
                return Self::default();
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    log::warn!("{name} ({}): {error} — using defaults", path.display());
                    return Self::default();
                }
            };
            if legacy {
                log::warn!(
                    "{} is deprecated: run `prism-build-pipeline build-scenes` to migrate it \
                     to {CONFIG_PATH}",
                    path.display()
                );
            }
            return Self::from_ini(&text);
        }
        Self::default()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app: AppInfo::default(),
            window: WindowConfig::default(),
            render: RenderConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, Ini};

    #[test]
    fn parses_in_memory_config_without_filesystem() {
        let cfg = AppConfig::from_ini("[window]\nwidth = 1024\nheight = 576\n");
        assert_eq!(cfg.window.width, 1024);
        assert_eq!(cfg.window.height, 576);
        assert_eq!(cfg.window.title, "PrismaRev");
    }

    #[test]
    fn malformed_in_memory_config_uses_defaults() {
        let cfg = AppConfig::from_ini("[window\nwidth = nope");
        assert_eq!(cfg.window.width, 1600);
        assert_eq!(cfg.window.height, 900);
    }

    #[test]
    fn quotes_bools_and_negative_numbers() {
        let cfg = AppConfig::from_ini(
            "[app]\ncompany = \"Acme\"\nname = \"Game\"\n\
             [window]\ntitle = \"My Game\"\nwidth = 1280\nvsync = false\nposition_x = -40\n",
        );
        assert_eq!(cfg.app.company, "Acme");
        assert_eq!(cfg.app.name, "Game");
        assert_eq!(cfg.window.title, "My Game");
        assert_eq!(cfg.window.width, 1280);
        assert!(!cfg.window.vsync);
        assert_eq!(cfg.window.position_x, Some(-40));
    }

    #[test]
    fn render_section_overrides_runtime_settings() {
        use crate::render_settings::{RenderMode, RenderSettings};

        let cfg = AppConfig::from_ini(
            "[render]\nrender_mode = \"path_trace\"\ntonemap_mode = 1\naa_mode = 1\ncontrast = 1.4\nsaturation = 0.6\nvignette = 0.3\nshadow_distance = 120.0\nshadow_map_size = 1024\nwireframe = true\nshaded_wireframe = true\npt_max_bounces = 7\n",
        );
        let mut settings = RenderSettings::default();
        cfg.render.apply(&mut settings);
        assert_eq!(settings.render_mode, RenderMode::PathTrace);
        assert_eq!(settings.tonemap_mode, 1);
        assert_eq!(settings.aa_mode, 1);
        // #502 color grading knobs ride in the same section.
        assert_eq!(settings.contrast, 1.4);
        assert_eq!(settings.saturation, 0.6);
        assert_eq!(settings.vignette, 0.3);
        assert_eq!(settings.pt_max_bounces, 7);
        // #526：阴影覆盖距离与贴图边长都是可配置的质量旋钮。
        assert_eq!(settings.shadow_distance, 120.0);
        assert_eq!(settings.shadow_map_size, 1024);
        assert!(settings.wireframe);
        assert!(settings.shaded_wireframe);
        // Unspecified fields keep their defaults.
        assert_eq!(settings.pt_max_iterations, 0);
    }

    #[test]
    fn shadow_map_size_outside_the_supported_range_is_ignored() {
        use crate::render_settings::RenderSettings;

        let cfg = AppConfig::from_ini("[render]\nshadow_map_size = 64\n");
        let mut settings = RenderSettings::default();
        cfg.render.apply(&mut settings);
        assert_eq!(
            settings.shadow_map_size,
            prism_render::SHADOW_MAP_SIZE,
            "out-of-range values must not reach the renderer"
        );
    }

    #[test]
    fn internal_resolution_and_scale_mode_come_from_the_render_section() {
        use crate::render_settings::{InternalScaleMode, RenderSettings};

        // #613：`[render] internal_resolution = 640x360` + `internal_scale_mode = integer`
        let cfg = AppConfig::from_ini(
            "[render]\ninternal_resolution = 640x360\ninternal_scale_mode = integer\n",
        );
        let mut settings = RenderSettings::default();
        cfg.render.apply(&mut settings);
        assert_eq!(settings.internal_resolution, [640, 360]);
        assert_eq!(settings.internal_scale_mode, InternalScaleMode::Integer);

        // 其它分隔符与关闭写法都接受。
        for (value, expected) in [
            ("640,360", [640, 360]),
            ("640*360", [640, 360]),
            ("off", [0, 0]),
            ("native", [0, 0]),
            ("", [0, 0]),
        ] {
            let key = format!("[render]\ninternal_resolution = {value}\n");
            let mut settings = RenderSettings::default();
            AppConfig::from_ini(&key).render.apply(&mut settings);
            assert_eq!(settings.internal_resolution, expected, "{value}");
        }

        // 无法解析时保持默认（不把整份配置打成无效）。
        let mut settings = RenderSettings::default();
        AppConfig::from_ini(
            "[render]\ninternal_resolution = 640x360x2\ninternal_scale_mode = nope\n",
        )
        .render
        .apply(&mut settings);
        assert_eq!(settings.internal_resolution, [0, 0]);
        assert_eq!(settings.internal_scale_mode, InternalScaleMode::Stretch);
    }

    #[test]
    fn unknown_sections_and_keys_are_ignored() {
        let ini =
            Ini::parse("[build]\noutput_dir = \"out\"\n[app]\nnickname = \"x\"\nname = \"ok\"\n");
        let cfg = AppConfig::from_ini(&ini.render());
        assert_eq!(cfg.app.name, "ok");
    }
}

// ---------------------------------------------------------------------------
// App data directory (like 's Application.persistentDataPath)
// ---------------------------------------------------------------------------

/// Returns the platform-specific data directory for the application
/// (e.g., `%APPDATA%\Excurs1ons\PrismaRev\` on Windows
///
/// Reads company/app name from the default [`AppConfig`] (it does **not** read
/// `settings.ini`; callers with a loaded config should use
/// [`app_data_dir_for`]).  Creates the directory if it doesn't exist.  Returns
/// `None` if the platform data dir cannot be determined (unlikely on desktop,
/// possible on unusual platforms).
pub fn app_data_dir() -> Option<std::path::PathBuf> {
    let cfg = AppConfig::default();
    app_data_dir_for(&cfg)
}

/// 根据调用方已加载的配置计算持久化目录，不读取文件系统中的配置。
pub fn app_data_dir_for(cfg: &AppConfig) -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    let dir = base.join(&cfg.app.company).join(&cfg.app.name);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}
