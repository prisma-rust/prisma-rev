//! Render-settings bundle passed into [`render_system`] each frame.
//!
//! Consolidates all per-frame render-state knobs (debug, tonemap, path tracing)
//! into a single struct so [`render_system`] has a clean, extensible parameter
//! instead of a growing list of scalar arguments.

pub use prism_render::{DlssMode, GiMode, InternalScaleMode, RenderMode};

/// Debug flag names, ordered by shader bit (`PBR_FLAG_*` in
/// `scene_frag.slang`). Bit `i` corresponds to `PBR_FLAG_NAMES[i]`.
///
/// Bits 0-14 isolate a single PBR term; bit 15 is *not* a component: it turns
/// on the skin-weight heatmap (#560), which the vertex stage resolves into a
/// colour before the material is sampled.
pub const PBR_FLAG_NAMES: [&str; 16] = [
    "Direct",
    "Shadow",
    "Specular",
    "Metallic",
    "Roughness",
    "DiffuseIBL",
    "SpecularIBL",
    "MultiLight",
    "AO",
    "Emissive",
    "Transmission",
    "Translucency",
    "Anisotropy",
    "ClearCoat",
    "GI",
    "SkinWeights",
];

/// Human-readable label for a PBR debug-flag mask (`0` = normal render).
pub fn pbr_flag_label(flags: u32) -> &'static str {
    if flags == 0 {
        return "(normal render)";
    }
    PBR_FLAG_NAMES
        .iter()
        .enumerate()
        .find(|(bit, _)| flags == (1u32 << bit))
        .map(|(_, name)| *name)
        .unwrap_or("(custom flags)")
}

/// 默认阴影覆盖距离（米）：2048² 阴影贴图下单 texel ≈ 2.9 cm（#526）。
pub const DEFAULT_SHADOW_DISTANCE: f32 = 60.0;

/// Aggregate of all per-frame render knobs.
///
/// `App` owns one instance, mutates it in response to keyboard/UI input, and
/// passes `&self` to [`crate::render_system::render_system`] each frame.
///
/// Extend this struct when adding new renderer knobs — do **not** add more
/// scalar parameters to `render_system`.
#[derive(Clone, Debug)]
pub struct RenderSettings {
    /// Debug flag mask (16 bits, see `scene_frag.slang` `PBR_FLAG_*`).
    /// `0` = normal full-PBR render (all components on); `1 << bit` for
    /// bits 0-14 isolates that one component as a grayscale visualization;
    /// bit 15 (`1 << 15`) = skin-weight heatmap (#560).
    pub debug_flags: u32,
    /// 色调映射 operator: 0 = Reinhard, 1 = ACES (Narkowicz),
    /// 2 = passthrough (2D 像素画：保留原始调色板，只做 saturate).
    pub tonemap_mode: u32,
    /// 后处理抗锯齿: 0 = 关, 1 = FXAA（在 `PostPass` 色调映射之后执行）。
    ///
    /// 不依赖 MSAA / resolve 目标，交换链与编辑器离屏视口共用同一条路径；
    /// 调试图（`debug_rt != 0`）会自动忽略它。
    pub aa_mode: u32,
    /// PostPass debug render-target viewer (Tab cycles). 0 = normal tonemapped
    /// HDR, 1 = linearized depth, 2 = view-space normal.
    pub debug_rt: u32,
    /// 直方图自动曝光测光（默认关）。
    ///
    /// 关闭时 `AutoExposurePass` 写入零亮度，色调映射读到 0 就是增益 1.0，
    /// 画面与引入自动曝光之前完全一致；打开后按屏幕实际亮度收敛到
    /// `AUTO_EXPOSURE_KEY`（0.18 中灰）。
    pub auto_exposure: bool,
    /// 遮挡剔除（#1007）：用上一帧的粗深度剔掉被完全挡住的绘制项。
    ///
    /// 默认关。开启后画面应保持一致（保守测试 + 一帧延迟），只是少提交
    /// draw；阴影通道不受影响，被挡住的物体仍然会投影。
    pub occlusion_culling: bool,
    /// 前向管线用 `POLYGON_MODE_LINE` 画网格（#431）。设备不支持时为 no-op。
    pub wireframe: bool,
    /// 正常填充渲染 + 线框叠加（#431）。设备不支持 `fillModeNonSolid` 时为 no-op；
    /// `wireframe == true` 时它被纯线框覆盖。
    pub shaded_wireframe: bool,
    /// Current render mode: Raster (PBR) or PathTrace (real-time PT).
    pub render_mode: RenderMode,
    /// Maximum path depth (bounces) for path tracing.
    pub pt_max_bounces: u32,
    /// Max world-space length of PT primary + shadow rays.
    pub pt_ray_max_distance: f32,
    /// Maximum iterations (samples per pixel). 0 = accumulate forever.
    pub pt_max_iterations: u32,
    /// Global Illumination technique mode (Off, Sharc, Nrc).
    pub gi_mode: GiMode,
    /// RTXGI SHARC: Hash-map capacity (number of voxel slots). Changing it
    /// rebuilds the cache buffers, which clears all cached radiance.
    pub sharc_capacity: u32,
    /// RTXGI SHARC: World-space scale of the hash grid voxels.
    pub sharc_scene_scale: f32,
    /// RTXGI SHARC: Cache clear toggle.
    pub sharc_enable_clear: bool,
    /// RTXGI SHARC: Cache update toggle.
    pub sharc_enable_update: bool,
    /// RTXGI SHARC: Cache resolve toggle.
    pub sharc_enable_resolve: bool,
    /// RTXGI SHARC: Anti-firefly filter toggle.
    pub sharc_enable_anti_firefly: bool,
    /// RTXGI SHARC: Maximum stale frames before entry eviction.
    pub sharc_stale_frame_num: u32,
    /// RTXGI SHARC: Downscale factor for the sparse path update.
    pub sharc_downscale_factor: u32,
    /// RTXGI SHARC: Temporal accumulation frame window size.
    pub sharc_accumulation_frame_num: u32,
    /// RTXGI SHARC: Roughness threshold below which mirror rays query direct radiance instead of cache.
    pub sharc_roughness_threshold: f32,
    /// ReSTIR GI: resolution scale relative to the internal render extent
    /// (0.5 = half resolution). 0 disables the pass.
    pub restir_gi_scale: f32,
    /// ReSTIR GI: secondary rays traced per pixel each frame.
    pub restir_gi_rays_per_pixel: u32,
    /// ReSTIR GI: spatial reuse taps per pixel (0 disables spatial reuse).
    pub restir_gi_spatial_samples: u32,
    /// ReSTIR GI: maximum number of frames a reservoir may be reused.
    pub restir_gi_max_history: u32,
    /// ReSTIR GI: reuse the previous frame reservoirs (temporal reuse).
    pub restir_gi_temporal_reuse: bool,
    /// ReSTIR GI: reuse neighbouring pixels reservoirs (spatial reuse).
    pub restir_gi_spatial_reuse: bool,
    /// ReSTIR GI: maximum world-space length of a secondary ray.
    pub restir_gi_max_secondary_dist: f32,
    /// DLSS Mode (Off, Quality, Balanced, Performance, UltraPerformance, Dlaa).
    pub dlss_mode: DlssMode,
    /// 是否在场景视图中渲染地面基准网格 (Y = 0)。
    pub show_grid: bool,
    /// 显示空间对比度（围绕中灰 `0.5` 缩放，`1.0` = 不变）。色调映射之后应用。
    ///
    /// 这三个字段是 #502 的后处理调色参数：和 `tonemap_mode` / `aa_mode` 一样
    /// 在 `PostPass` 里生效，交换链与编辑器离屏视口共用同一条路径。
    pub contrast: f32,
    /// 显示空间饱和度（`1.0` = 不变，`0.0` = 灰度）。Rec.709 luma 插值。
    pub saturation: f32,
    /// 暗角强度（`0.0` = 关闭，`1.0` = 四角全黑）。
    pub vignette: f32,
    /// HDR bloom 开关（#502）。关闭时 `bloom_intensity` 不参与渲染。
    pub bloom: bool,
    /// bloom 加性强度（`0.0` = 关闭，`1.0` = 最强）。
    pub bloom_intensity: f32,
    /// bloom 亮度阈值；高于该 HDR 亮度的像素才会发光。
    pub bloom_threshold: f32,
    /// 镜头色差强度（#502 Phase 2）：`0.0` = 关闭，`1.0` = 最强。
    ///
    /// 在 HDR 采样阶段按到画面中心的距离把红/蓝通道向相反方向偏移，
    /// 绿色保持不动；默认 0 与旧画面逐位一致。
    pub chromatic_aberration: f32,
    /// 胶片颗粒强度（#502 Phase 2）：`0.0` = 关闭，`1.0` = 最强。
    ///
    /// 在色调映射、调色与暗角之后叠加显示空间单色噪声；默认 0 与旧画面
    /// 逐位一致。
    pub film_grain: f32,
    /// 白平衡色温（#502 Phase 1）：`-1.0` = 冷，`0.0` = 中性，`1.0` = 暖。
    ///
    /// 在调色阶段按 RGB 增益移动白点；`0.0` 时增益精确为 `(1, 1, 1)`，
    /// 默认路径与旧画面逐位一致。
    pub temperature: f32,
    /// 白平衡色调（#502 Phase 1）：`-1.0` = 偏绿，`0.0` = 中性，`1.0` = 偏品红。
    pub tint: f32,
    /// 景深对焦距离（米，#502 Phase 2）。
    ///
    /// `dof_aperture > 0` 时，深度与这个距离差得越远越模糊；默认 1 米。
    pub dof_focus_distance: f32,
    /// 景深强度（`0.0` = 关闭，`1.0` = 最强，#502 Phase 2）。
    ///
    /// 关闭时后处理完全跳过深度采样，默认路径逐位不变。
    pub dof_aperture: f32,
    /// 景深最大模糊半径（像素，0–32）。
    pub dof_max_blur: f32,
    /// 3D 调色 LUT 的资产相对路径（`assets/` 下的 `.cube`，#502）。
    ///
    /// 空字符串 = 不使用 LUT。文件由渲染器在路径变化时惰性加载；加载失败
    /// 只记录警告并保留上一张 LUT，不会中断渲染。
    pub lut: String,
    /// LUT 混合强度（`0.0` = 关闭，`1.0` = 完全使用 LUT）。
    pub lut_strength: f32,
    /// 相机运动模糊强度（`0.0` = 关闭，`1.0` = 最强，#502 Phase 2）。
    ///
    /// 关闭时后处理完全跳过上一帧矩阵与深度重建，默认路径逐位不变。
    pub motion_blur: f32,
    /// 运动模糊最大拖影长度（像素，0–64）。
    pub motion_blur_max_pixels: f32,
    /// Temporal anti-aliasing (#883). Default off; when on the renderer jitters
    /// the camera and resolves against the previous frame.
    pub taa: bool,
    /// TAA history weight (`0.0..=0.95`); ignored when `taa` is off.
    pub taa_strength: f32,
    /// Qualcomm Adreno Neural Fusion Super Resolution (#1161).
    pub anf_sr: bool,
    /// ANF Super Resolution sharpness parameter [0.0..=1.0].
    pub anf_sharpness: f32,
    /// Qualcomm Adreno Neural Fusion Frame Generation (#1161).
    pub anf_fg: bool,
    /// 内部渲染分辨率 `[宽, 高]`（#613）；`[0, 0]` = 关闭。
    ///
    /// 开启后场景按这个尺寸渲染，`PostPass` 再放大到窗口；相机的宽高比也跟着
    /// 变成 `宽 / 高`，所以窗口尺寸变化不再改变取景。
    pub internal_resolution: [u32; 2],
    /// 内部图像放大到窗口的方式（拉伸 / 整数倍 + 黑边）。
    pub internal_scale_mode: InternalScaleMode,
    /// 主方向光的阴影覆盖距离（世界单位，米）。
    ///
    /// 阴影正交盒子以**相机前方 `distance * 0.5` 处**为中心、边长 `distance`；
    /// 这个值越大覆盖越远、每 texel 越粗（2048² 时 60 米 ≈ 2.9 cm/texel）。
    /// 没有级联（见 #526），所以它同时是「阴影能投多远」和「精度」的取舍旋钮。
    pub shadow_distance: f32,
    /// 阴影贴图边长（texel，#526）；构造渲染器时生效，运行中改需要重建渲染图。
    pub shadow_map_size: u32,
    /// 方向光级联数（#526 CSM）：`1` = 单张阴影图（默认），`2` = 2×1 atlas
    /// 的近/远两级联。构造渲染器时生效。
    pub shadow_cascade_count: u32,
}

/// Default PBR mode: normal full-PBR render, Reinhard tonemap, Raster.
impl RenderSettings {
    pub fn post_grade(&self) -> prism_render::PostGrade {
        prism_render::PostGrade {
            contrast: self.contrast,
            saturation: self.saturation,
            vignette: self.vignette,
            bloom_intensity: if self.bloom {
                self.bloom_intensity
            } else {
                0.0
            },
            bloom_threshold: self.bloom_threshold,
            chromatic_aberration: self.chromatic_aberration,
            film_grain: self.film_grain,
            temperature: self.temperature,
            tint: self.tint,
            dof_focus_distance: self.dof_focus_distance,
            dof_aperture: self.dof_aperture,
            dof_max_blur: self.dof_max_blur,
            lut_strength: self.lut_strength,
            motion_blur: self.motion_blur,
            motion_blur_max_pixels: self.motion_blur_max_pixels,
        }
    }

    /// 导出成渲染图报告用的纯数据（`prism_render::render_graph_report`）。
    ///
    /// 报告 DTO 在 `prism-render` 里（不依赖引擎），映射留在这里，编辑器与
    /// 玩家进程共用同一份，避免两处各写一遍字段表。
    pub fn to_report_settings(&self) -> prism_render::render_graph_report::ReportSettings {
        prism_render::render_graph_report::ReportSettings::new(
            self.render_mode,
            self.debug_flags,
            self.tonemap_mode,
            self.debug_rt,
            self.pt_max_bounces,
            self.pt_ray_max_distance,
            self.pt_max_iterations,
        )
    }
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            debug_flags: 0,
            tonemap_mode: 0,
            aa_mode: 0,
            debug_rt: 0,
            auto_exposure: false,
            occlusion_culling: false,
            wireframe: false,
            shaded_wireframe: false,
            render_mode: RenderMode::Raster,
            pt_max_bounces: 3,
            pt_ray_max_distance: 1000.0,
            pt_max_iterations: 0,
            gi_mode: GiMode::Off,
            sharc_capacity: 1 << 20,
            sharc_scene_scale: 50.0,
            sharc_enable_clear: false,
            sharc_enable_update: true,
            sharc_enable_resolve: true,
            sharc_enable_anti_firefly: true,
            sharc_stale_frame_num: 60,
            sharc_downscale_factor: 5,
            sharc_accumulation_frame_num: 20,
            sharc_roughness_threshold: 0.4,
            restir_gi_scale: 0.5,
            restir_gi_rays_per_pixel: 1,
            restir_gi_spatial_samples: 5,
            restir_gi_max_history: 20,
            restir_gi_temporal_reuse: true,
            restir_gi_spatial_reuse: true,
            restir_gi_max_secondary_dist: 30.0,
            dlss_mode: DlssMode::Off,
            show_grid: false,
            contrast: 1.0,
            saturation: 1.0,
            vignette: 0.0,
            bloom: false,
            bloom_intensity: 0.15,
            bloom_threshold: 1.0,
            chromatic_aberration: 0.0,
            film_grain: 0.0,
            temperature: 0.0,
            tint: 0.0,
            dof_focus_distance: 1.0,
            dof_aperture: 0.0,
            dof_max_blur: 8.0,
            lut: String::new(),
            lut_strength: 0.0,
            motion_blur: 0.0,
            motion_blur_max_pixels: 16.0,
            taa: false,
            taa_strength: 0.9,
            anf_sr: false,
            anf_sharpness: 0.5,
            anf_fg: false,
            shadow_distance: DEFAULT_SHADOW_DISTANCE,
            shadow_map_size: prism_render::SHADOW_MAP_SIZE,
            shadow_cascade_count: 1,
            internal_resolution: [0, 0],
            internal_scale_mode: InternalScaleMode::Stretch,
        }
    }
}

impl RenderSettings {
    /// 内部渲染分辨率生效时的相机宽高比；否则沿用窗口宽高比（#613）。
    ///
    /// 只有一个维度非零时另一边按窗口比例推导，宽高比与窗口一致，直接返回
    /// `window_aspect`。
    pub fn render_aspect(&self, window_aspect: f32) -> f32 {
        let [width, height] = self.internal_resolution;
        if width > 0 && height > 0 {
            width as f32 / height as f32
        } else if window_aspect.is_finite() && window_aspect > 0.0 {
            window_aspect
        } else {
            16.0 / 9.0
        }
    }

    /// 场景实际渲染的像素高度（#978）。
    ///
    /// `internal_resolution` 生效时是内部高度；只给了一个维度时按窗口比例
    /// 推导（与 `prism_render::render_graph::internal_extent` 一致）。都没给
    /// 时就是窗口高度。
    pub fn render_height(&self, window_width: u32, window_height: u32) -> f32 {
        let window_height = window_height.max(1);
        let [width, height] = self.internal_resolution;
        if width == 0 && height == 0 {
            return window_height as f32;
        }
        if height > 0 {
            return height as f32;
        }
        let window_width = window_width.max(1);
        let derived = (width as f32 * window_height as f32 / window_width as f32).round();
        derived.max(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{pbr_flag_label, InternalScaleMode, RenderSettings, PBR_FLAG_NAMES};

    #[test]
    fn every_flag_bit_has_a_label_and_the_heatmap_bit_is_not_a_pbr_component() {
        for bit in 0..PBR_FLAG_NAMES.len() {
            let label = pbr_flag_label(1u32 << bit);
            assert_ne!(label, "(custom flags)", "bit {bit} has no name");
        }
        // 第 16 位是蒙皮权重热图（#560），不是 PBR 分量；组合值仍然算自定义。
        assert_eq!(pbr_flag_label(1 << 15), "SkinWeights");
        assert_eq!(pbr_flag_label((1 << 0) | (1 << 15)), "(custom flags)");
        assert_eq!(pbr_flag_label(0), "(normal render)");
        assert_eq!(pbr_flag_label(1 << 16), "(custom flags)");
    }

    #[test]
    fn render_height_follows_the_internal_resolution() {
        let mut settings = RenderSettings::default();
        assert_eq!(settings.render_height(1920, 1080), 1080.0, "内部分辨率关闭");

        settings.internal_resolution = [640, 360];
        assert_eq!(settings.render_height(1920, 1080), 360.0);

        // 只给宽度：按窗口比例推导高度（与 internal_extent 一致）。
        settings.internal_resolution = [640, 0];
        assert_eq!(settings.render_height(1920, 1080), 360.0);

        // 只给高度：直接用。
        settings.internal_resolution = [0, 240];
        assert_eq!(settings.render_height(1920, 1080), 240.0);

        // 退化输入不产生 0 / NaN。
        settings.internal_resolution = [0, 0];
        assert_eq!(settings.render_height(0, 0), 1.0);
    }

    #[test]
    fn post_grade_only_passes_bloom_when_enabled() {
        let mut settings = RenderSettings::default();
        settings.bloom_intensity = 0.7;
        assert_eq!(settings.post_grade().bloom_intensity, 0.0);

        settings.bloom = true;
        let grade = settings.post_grade();
        assert_eq!(grade.bloom_intensity, 0.7);
        assert_eq!(grade.bloom_threshold, settings.bloom_threshold);

        settings.chromatic_aberration = 0.35;
        assert_eq!(settings.post_grade().chromatic_aberration, 0.35);

        settings.film_grain = 0.5;
        assert_eq!(settings.post_grade().film_grain, 0.5);

        settings.temperature = -0.4;
        settings.tint = 0.25;
        settings.dof_focus_distance = 3.5;
        settings.dof_aperture = 0.6;
        settings.dof_max_blur = 12.0;
        settings.motion_blur = 0.7;
        settings.motion_blur_max_pixels = 24.0;
        let grade = settings.post_grade();
        assert_eq!(grade.temperature, -0.4);
        assert_eq!(grade.tint, 0.25);
        assert_eq!(grade.dof_focus_distance, 3.5);
        assert_eq!(grade.dof_aperture, 0.6);
        assert_eq!(grade.dof_max_blur, 12.0);
        assert_eq!(grade.motion_blur, 0.7);
        assert_eq!(grade.motion_blur_max_pixels, 24.0);
    }

    #[test]
    fn render_aspect_prefers_the_internal_resolution() {
        // #613：内部分辨率生效时相机必须按内部比例投影，而不是窗口比例。
        let mut settings = RenderSettings::default();
        assert_eq!(settings.internal_resolution, [0, 0]);
        assert_eq!(settings.internal_scale_mode, InternalScaleMode::Stretch);
        // 没有内部分辨率时沿用窗口比例。
        assert!((settings.render_aspect(21.0 / 9.0) - 21.0 / 9.0).abs() < 1e-6);

        settings.internal_resolution = [640, 360];
        assert!((settings.render_aspect(21.0 / 9.0) - 16.0 / 9.0).abs() < 1e-6);

        // 只有一个维度时宽高比与窗口一致，回落到窗口比例。
        settings.internal_resolution = [640, 0];
        assert!((settings.render_aspect(4.0 / 3.0) - 4.0 / 3.0).abs() < 1e-6);

        // 坏窗口比例也有确定的结果，不会把 NaN 传给投影矩阵。
        settings.internal_resolution = [0, 0];
        assert_eq!(settings.render_aspect(f32::NAN), 16.0 / 9.0);
        assert_eq!(settings.render_aspect(0.0), 16.0 / 9.0);
    }
}
