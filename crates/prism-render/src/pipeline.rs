//! 图形管线创建。
//!
//! 为标准 PrismaRev 前向渲染路径构建 [`GraphicsPipeline`]：
//! 顶点输入（位置+法线+颜色）、模型变换的推送常量、
//! 帧 UBO 的单个描述符集、深度测试+背面剔除、
//! 无多重采样、一个带 Alpha 混合的颜色附件。
//!
//! 视口和裁剪矩形是动态的，因此窗口调整大小时无需重建管线。

use anyhow::Context as _;
use ash::vk;
use prism_gfx::{BlendMode, BlendOp, CullMode};

/// 把共享的 [`CullMode`]（`prism-gfx`，Vulkan-free）翻译成 Vulkan 剔除位。
///
/// 映射留在渲染器这一层，所以 `prism-gfx` 不需要知道 ash。
pub(crate) fn cull_flags(mode: CullMode) -> vk::CullModeFlags {
    match mode {
        CullMode::Off => vk::CullModeFlags::NONE,
        CullMode::Front => vk::CullModeFlags::FRONT,
        CullMode::Back => vk::CullModeFlags::BACK,
    }
}

/// [`BlendMode`] → Vulkan 混合因子。
///
/// **两边的数值顺序不同**（把 `DstColor` 放在 `SrcColor` 前面），所以必须
/// 显式映射，不能按 `as i32` 转换。
pub(crate) fn blend_factor(mode: BlendMode) -> vk::BlendFactor {
    match mode {
        BlendMode::Zero => vk::BlendFactor::ZERO,
        BlendMode::One => vk::BlendFactor::ONE,
        BlendMode::DstColor => vk::BlendFactor::DST_COLOR,
        BlendMode::SrcColor => vk::BlendFactor::SRC_COLOR,
        BlendMode::OneMinusDstColor => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        BlendMode::SrcAlpha => vk::BlendFactor::SRC_ALPHA,
        BlendMode::OneMinusSrcColor => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        BlendMode::DstAlpha => vk::BlendFactor::DST_ALPHA,
        BlendMode::OneMinusDstAlpha => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
        BlendMode::SrcAlphaSaturate => vk::BlendFactor::SRC_ALPHA_SATURATE,
        BlendMode::OneMinusSrcAlpha => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
    }
}

/// [`BlendOp`] → Vulkan 混合运算。
pub(crate) fn blend_operation(op: BlendOp) -> vk::BlendOp {
    match op {
        BlendOp::Add => vk::BlendOp::ADD,
        BlendOp::Subtract => vk::BlendOp::SUBTRACT,
        BlendOp::ReverseSubtract => vk::BlendOp::REVERSE_SUBTRACT,
        BlendOp::Min => vk::BlendOp::MIN,
        BlendOp::Max => vk::BlendOp::MAX,
    }
}

/// 创建 [`GraphicsPipeline`] 的参数。
///
/// 将之前过多的独立参数组合为单个结构体，
/// 使调用者无需传递 8 个位置参数。
///
/// The optional raster/depth fields (`cull_mode`, `depth_bias_*`,
/// `depth_write_enable`, `color_attachment_count`) 默认 to the legacy
/// behavior when `None`. Shadow-map pipelines 集合 them to 覆盖 剔除
/// enable 深度 bias, and 放置 颜色 输出
pub struct PipelineDesc<'a> {
    pub device: &'a ash::Device,
    pub shader_stages: &'a [vk::PipelineShaderStageCreateInfo<'a>],
    pub vertex_binding_desc: &'a [vk::VertexInputBindingDescription],
    pub vertex_attr_descs: &'a [vk::VertexInputAttributeDescription],
    pub descriptor_set_layouts: &'a [vk::DescriptorSetLayout],
    pub push_constant_ranges: &'a [vk::PushConstantRange],
    pub render_pass: vk::RenderPass,
    pub subpass: u32,
    /// 覆盖 the cull 众数 默认 后
    pub cull_mode: Option<vk::CullModeFlags>,
    /// Enable 深度 bias 默认 `false`). Used by shadow pipelines to
    /// avoid self-shadow acne.
    pub depth_bias_enable: Option<bool>,
    /// 深度 bias 常量 factor 默认 0). Only used when
    /// `depth_bias_enable` is `Some(true)`.
    pub depth_bias_constant_factor: Option<f32>,
    /// 深度 bias slope factor 默认 0).
    pub depth_bias_slope_factor: Option<f32>,
    /// 覆盖 深度 写入 enable 默认 `true`).
    pub depth_write_enable: Option<bool>,
    /// 覆盖 深度 测试 enable 默认 `true`）。2D 精灵管线关闭深度测试，
    /// 由 CPU 排序保证绘制顺序。
    pub depth_test_enable: Option<bool>,
    /// Number of 颜色 attachments the 渲染 pass 子 pass uses 默认 1).
    /// 集合 to `Some(0)` for a depth-only 管线 (e.g. shadow 映射表 the
    /// 颜色 混合 状态 then carries 零 attachments.
    pub color_attachment_count: Option<u32>,
    /// Explicit per-attachment 混合 states for MRT pipelines. When `Some`,
    /// this overrides `color_attachment_count` (the count is taken from the
    /// 切片 长度 pass one entry per 颜色 附件 the 子 pass writes;
    /// the i-th entry configures `SV_Target{i}`. Use this for MRT pipelines
    /// (e.g. ForwardPass writes 颜色 + view-normal) where each 目标 needs a
    /// different 混合 配置 颜色 = Alpha 混合 法线 = no 混合
    pub color_blend_attachments: Option<&'a [vk::PipelineColorBlendAttachmentState]>,
    /// 覆盖正面绕序（默认 [`vk::FrontFace::COUNTER_CLOCKWISE`]）。
    /// 阴影管线使用无 Y 翻转的正交光照投影，因此需要 [`vk::FrontFace::CLOCKWISE`]。
    pub front_face: Option<vk::FrontFace>,
    /// 光栅化模式（默认 [`vk::PolygonMode::FILL`]）。
    ///
    /// Wireframe 用 `LINE`；需要设备开启 `fillModeNonSolid`
    /// （`VulkanContext::wireframe_supported`）。
    pub polygon_mode: Option<vk::PolygonMode>,
}

/// A compiled graphics 管线 with its 布局
pub struct GraphicsPipeline {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    /// Cloned 设备 handle kept so 放置 can free the 管线 without an
    /// explicit 销毁 调用 (RAII).
    device: ash::Device,
}

impl GraphicsPipeline {
    /// 创建 the graphics 管线
    ///
    /// All parameters are provided via [`PipelineDesc`]. `render_pass` and
    /// 子 pass identify where this 管线 is used. `descriptor_set_layouts`
    /// are the layouts for the pipeline's 描述符 sets. `push_constant_ranges`
    /// define the 推送 常量 regions accessible from 着色器 stages.
    pub fn new(desc: &PipelineDesc) -> anyhow::Result<Self> {
        let device = desc.device;
        let shader_stages = desc.shader_stages;
        let vertex_binding_desc = desc.vertex_binding_desc;
        let vertex_attr_descs = desc.vertex_attr_descs;
        let descriptor_set_layouts = desc.descriptor_set_layouts;
        let push_constant_ranges = desc.push_constant_ranges;
        let render_pass = desc.render_pass;
        let subpass = desc.subpass;
        // --- 管线 布局 ---
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(descriptor_set_layouts)
            .push_constant_ranges(push_constant_ranges);
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }
            .context("create pipeline layout")?;

        // --- 顶点 输入 ---
        let vertex_input_info = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(vertex_binding_desc)
            .vertex_attribute_descriptions(vertex_attr_descs);

        // --- 输入 assembly ---
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST)
            .primitive_restart_enable(false);

        // --- 视口 & scissor 动力学 ---
        // 状态 is 集合 dynamically via cmd_set_viewport/cmd_set_scissor so the
        // 管线 does not need recreation when the 窗口 is resized.
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        // Dummy 视口 状态 (required by the API, but overridden by 动力学
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        // --- Rasterizer ---
        // `cull_mode` and `depth_bias_*` are optional overrides so shadow-map
        // pipelines can flip 剔除 and apply slope/constant 深度 bias to
        // avoid self-shadow acne. Legacy callers pass `None` -> 默认 后 /
        // no bias.
        let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_clamp_enable(false)
            .rasterizer_discard_enable(false)
            .polygon_mode(desc.polygon_mode.unwrap_or(vk::PolygonMode::FILL))
            .line_width(1.0)
            .cull_mode(desc.cull_mode.unwrap_or(vk::CullModeFlags::BACK))
            // 视图 矩阵 is now a proper 旋转 (det +1); the projection's
            // y-flip Vulkan NDC) is the single remaining reflection, so 前
            // faces wind counter-clockwise in 片段 空间
            .front_face(desc.front_face.unwrap_or(vk::FrontFace::COUNTER_CLOCKWISE))
            .depth_bias_enable(desc.depth_bias_enable.unwrap_or(false))
            .depth_bias_constant_factor(desc.depth_bias_constant_factor.unwrap_or(0.0))
            .depth_bias_slope_factor(desc.depth_bias_slope_factor.unwrap_or(0.0));

        // --- Multisampling (none) ---
        let multisampling = vk::PipelineMultisampleStateCreateInfo::default()
            .sample_shading_enable(false)
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // --- Depth/stencil ---
        // `depth_write_enable` is an optional 覆盖 shadow-map pipelines
        // may disable 颜色 writes but keep 深度 writes, so it defaults true.
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(desc.depth_test_enable.unwrap_or(true))
            .depth_write_enable(desc.depth_write_enable.unwrap_or(true))
            .depth_compare_op(vk::CompareOp::LESS);

        // --- 颜色 混合 ---
        // `color_blend_attachments` (when provided) drives the 完整 MRT 混合
        // 状态 otherwise `color_attachment_count` selects 0 (depth-only) or 1
        // (single 附件 with the legacy alpha-blend 配置 below).
        let color_blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ZERO)
            .alpha_blend_op(vk::BlendOp::ADD);
        let color_blend_state = if let Some(atts) = desc.color_blend_attachments {
            vk::PipelineColorBlendStateCreateInfo::default()
                .logic_op_enable(false)
                .logic_op(vk::LogicOp::COPY)
                .attachments(atts)
        } else {
            match desc.color_attachment_count.unwrap_or(1) {
                0 => vk::PipelineColorBlendStateCreateInfo::default()
                    .logic_op_enable(false)
                    .logic_op(vk::LogicOp::COPY)
                    .attachments(&[]),
                _ => vk::PipelineColorBlendStateCreateInfo::default()
                    .logic_op_enable(false)
                    .logic_op(vk::LogicOp::COPY)
                    .attachments(std::slice::from_ref(&color_blend_attachment)),
            }
        };

        // --- 管线 创建 ---
        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(shader_stages)
            .vertex_input_state(&vertex_input_info)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .dynamic_state(&dynamic_state)
            .rasterization_state(&rasterizer)
            .multisample_state(&multisampling)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend_state)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(subpass);

        let pipeline = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        }
        .map_err(|(_, e)| e)
        .context("create graphics pipeline")?[0];

        Ok(Self {
            pipeline,
            layout,
            device: device.clone(),
        })
    }
}

impl Drop for GraphicsPipeline {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cull_mode_maps_to_vulkan_and_defaults_to_back() {
        // 默认与 PrismaRev 既有管线一致（背面剔除）。
        assert_eq!(CullMode::default(), CullMode::Back);
        assert_eq!(cull_flags(CullMode::Off), vk::CullModeFlags::NONE);
        assert_eq!(cull_flags(CullMode::Front), vk::CullModeFlags::FRONT);
        assert_eq!(cull_flags(CullMode::Back), vk::CullModeFlags::BACK);

        // 管线缓存以 `CullMode` 做键，三种取值必须互不相等。
        let mut modes = [CullMode::Off, CullMode::Front, CullMode::Back];
        modes.sort_by_key(|m| cull_flags(*m).as_raw());
        let mut uniq = modes.to_vec();
        uniq.dedup();
        assert_eq!(uniq.len(), 3);
    }

    #[test]
    fn blend_factors_follow_semantics_not_numeric_order() {
        // 引擎的数值顺序与 Vulkan 不同：Zero/One 都是 0/1，但引擎的
        // DstColor=2 对应 Vulkan 的 DST_COLOR，SrcColor=3 对应 SRC_COLOR。
        assert_eq!(blend_factor(BlendMode::Zero), vk::BlendFactor::ZERO);
        assert_eq!(blend_factor(BlendMode::One), vk::BlendFactor::ONE);
        assert_eq!(
            blend_factor(BlendMode::DstColor),
            vk::BlendFactor::DST_COLOR
        );
        assert_eq!(
            blend_factor(BlendMode::SrcColor),
            vk::BlendFactor::SRC_COLOR
        );
        assert_eq!(
            blend_factor(BlendMode::OneMinusDstColor),
            vk::BlendFactor::ONE_MINUS_DST_COLOR
        );
        assert_eq!(
            blend_factor(BlendMode::SrcAlphaSaturate),
            vk::BlendFactor::SRC_ALPHA_SATURATE
        );
        assert_eq!(
            blend_factor(BlendMode::OneMinusSrcAlpha),
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );

        assert_eq!(blend_operation(BlendOp::Add), vk::BlendOp::ADD);
        assert_eq!(blend_operation(BlendOp::Subtract), vk::BlendOp::SUBTRACT);
        assert_eq!(
            blend_operation(BlendOp::ReverseSubtract),
            vk::BlendOp::REVERSE_SUBTRACT
        );
        assert_eq!(blend_operation(BlendOp::Min), vk::BlendOp::MIN);
        assert_eq!(blend_operation(BlendOp::Max), vk::BlendOp::MAX);
    }
}
