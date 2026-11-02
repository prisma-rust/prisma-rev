//! Render-settings bundle passed into [`render_system`] each 帧
//!
//! Consolidates all per-frame render-state knobs 调试 色调映射 path tracing)
//! into a single 结构体 so [`render_system`] has a clean, 可扩展 参数
//! instead of a growing 列表 of 标量 arguments.

pub use prism_render::{InternalScaleMode, RenderMode};

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

/// Aggregate of all per-frame 渲染 knobs.
///
/// `App` owns one 实例 mutates it in response to keyboard/UI 输入 and
/// passes `&self` to [`crate::render_system::render_system`] each 帧
///
/// Extend this 结构体 when adding new 渲染器 knobs — do **not** add more
/// 标量 parameters to `render_system`.
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
    /// PostPass 调试 render-target viewer (Tab cycles). 0 = 法线 tonemapped
    /// 高动态范围 1 = linearized 深度 2 = view-space 法线
    pub debug_rt: u32,
    /// 前向管线用 `POLYGON_MODE_LINE` 画网格（#431）。设备不支持时为 no-op。
    pub wireframe: bool,
    /// 正常填充渲染 + 线框叠加（#431）。设备不支持 `fillModeNonSolid` 时为 no-op；
    /// `wireframe == true` 时它被纯线框覆盖。
    pub shaded_wireframe: bool,
    /// 当前 渲染 众数 光栅化 (PBR) or PathTrace (real-time PT).
    pub render_mode: RenderMode,
    /// 最大 path 深度 (bounces) for path tracing.
    pub pt_max_bounces: u32,
    /// 最大值 world-space 长度 of PT primary + shadow rays.
    pub pt_ray_max_distance: f32,
    /// 最大 iterations (samples per 像素 0 = accumulate forever.
    pub pt_max_iterations: u32,
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

/// 默认 PBR 众数 法线 full-PBR 渲染 Reinhard 色调映射 光栅化
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
            wireframe: false,
            shaded_wireframe: false,
            render_mode: RenderMode::Raster,
            pt_max_bounces: 3,
            pt_ray_max_distance: 1000.0,
            pt_max_iterations: 0,
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
        let grade = settings.post_grade();
        assert_eq!(grade.temperature, -0.4);
        assert_eq!(grade.tint, 0.25);
        assert_eq!(grade.dof_focus_distance, 3.5);
        assert_eq!(grade.dof_aperture, 0.6);
        assert_eq!(grade.dof_max_blur, 12.0);
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
