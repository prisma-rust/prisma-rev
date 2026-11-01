//! 环境天空盒通道。
//!
//! [`SkyboxPass`] 绘制 IBL 环境立方体贴图背景。由
//! [`crate::forward_pass::ForwardPass`] 持有：在 ForwardPass 的渲染
//! pass 内最先绘制（共享同一 render pass 与帧缓冲）。

use ash::vk;

#[cfg(feature = "skybox")]
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
#[cfg(feature = "skybox")]
use crate::shader;
#[cfg(feature = "skybox")]
use anyhow::Context as _;
use anyhow::Result;

/// Per-frame skybox parameters extracted from the ECS `Skybox` component.
///
/// 这个类型始终编译：它是 `FrameInput` / `GraphFrame` 的公共数据形状，
/// 关掉 `skybox` 只是不画天空盒（`SkyboxPass` 退化成空实现）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyboxSettings {
    /// Background yaw rotation in radians.
    pub rotation: f32,
    /// Background brightness multiplier.
    pub intensity: f32,
}

/// Skybox pass - draws the IBL environment cubemap as a background behind the
/// scene.
///
/// Reuses the env cubemap already produced by [`crate::ibl::IblResources`] from
/// the user-supplied 高动态范围 (e.g. `kloppenheim_05_4k.hdr`) — there is no
/// separate loader: the skybox is just that env 映射表 rendered at the 远 平面
///
/// The cube is generated in the 顶点 着色器 from `SV_VertexID` (no 顶点
/// 缓冲区 is bound). The 顶点 阶段 strips the 相机 平移 (by
/// rotating the corner with the inverse-view 旋转 w=0) so the 盒 stays
/// at 无穷 then places it at NDC z=1 远 平面 The 管线 disables
/// 深度 writes and uses `LESS_OR_EQUAL` 深度 test, so the sky only shows
/// where no scene geometry has drawn.
///
/// 描述符 集合 布局 (mirrors `skybox.slang`):
/// 集合 2, 绑定 0 - IBL environment cubemap (SamplerCube, combined)
#[cfg(feature = "skybox")]
pub struct SkyboxPass {
    /// IBL env cubemap 描述符 集合 集合 0 绑定 0). Borrowed from
    /// `IblResources`; not owned by `SkyboxPass`.
    ibl_descriptor_set: vk::DescriptorSet,
    /// IBL 描述符 集合 布局 (borrowed from `IblResources`). 包含
    /// bindings 0=envCube, 1=irradiance, 2=prefiltered; the skybox 着色器
    /// only reads 绑定 0 (envCube).
    ibl_layout: vk::DescriptorSetLayout,
    /// Owned 管线 + 布局 (created lazily on 第一个 执行
    pipeline: Option<GraphicsPipeline>,
    /// 渲染 pass the 当前 管线 was 内置 against (to detect when a
    /// rebuild is needed, e.g. after a 交换链 recreate rebuilds the
    /// ForwardPass 渲染 pass
    built_for_render_pass: Option<vk::RenderPass>,
    /// Cached 设备 handle for 放置
    device: Option<ash::Device>,
}

#[cfg(feature = "skybox")]
impl SkyboxPass {
    pub fn new(ibl_descriptor_set: vk::DescriptorSet, ibl_layout: vk::DescriptorSetLayout) -> Self {
        Self {
            ibl_descriptor_set,
            ibl_layout,
            pipeline: None,
            built_for_render_pass: None,
            device: None,
        }
    }

    /// 场景切换重建 IBL 时，更新当前绑定的 set2 描述符 集合 句柄
    /// （管线布局共享，无需重建管线）。
    pub fn set_descriptor_set(&mut self, set: vk::DescriptorSet) {
        self.ibl_descriptor_set = set;
    }

    /// 构建 (once) the skybox 管线
    fn ensure_pipeline(&mut self, device: &ash::Device) -> Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        self.device = Some(device.clone());

        // The 渲染 pass + color/depth formats are provided at 绘制 时间 via
        // `execute_with` because the skybox must 渲染 into the *same*
        // 帧缓冲 the ForwardPass uses. We can't 构建 a fixed 管线
        // here without that 渲染 pass so the 管线 is created lazily
        // inside `execute_with` (which has the 渲染 pass
        Ok(())
    }

    /// 绘制 the skybox into the currently-bound 渲染 pass (begun by the
    /// 调用者 `ForwardPass`). `render_pass` + `extent` are needed to lazily
    /// 创建 the 管线 `inv_view_rot` is the inverse 视图 旋转
    /// 世界 <- 视图 used to 旋转 the view-space look direction into
    /// 世界 空间 for cubemap sampling.
    pub fn draw(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        _extent: vk::Extent2D,
        inv_view_rot: &[[f32; 4]; 4],
        params: SkyboxSettings,
    ) -> Result<()> {
        self.ensure_pipeline(device)?;

        // Lazily (re)build the 管线 if the 渲染 pass differs (e.g. after
        // a 交换链 recreate that rebuilt ForwardPass' 渲染 pass
        let rebuild = self.built_for_render_pass != Some(render_pass);
        if rebuild {
            // 放置 the old 管线 via GraphicsPipeline::Drop (which destroys
            // the 管线 + 布局 Do NOT 调用 destroy_pipeline manually -
            // that double-frees.
            self.pipeline = None;

            const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/skybox.vert.spv");
            const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/skybox.frag.spv");
            let vert_module =
                shader::load_shader_module(device, VERT_SPV).context("SkyboxPass: load vert")?;
            let frag_module =
                shader::load_shader_module(device, FRAG_SPV).context("SkyboxPass: load frag")?;

            let vert_entry = std::ffi::CString::new("vertexMain").unwrap();
            let frag_entry = std::ffi::CString::new("fragmentMain").unwrap();
            let vert_stage = shader::shader_stage(
                vk::ShaderStageFlags::VERTEX,
                vert_module,
                vert_entry.as_c_str(),
            );
            let frag_stage = shader::shader_stage(
                vk::ShaderStageFlags::FRAGMENT,
                frag_module,
                frag_entry.as_c_str(),
            );
            let shader_stages = [vert_stage, frag_stage];

            // No 顶点 缓冲区 positions come from SV_VertexID in the 着色器
            let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
            let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];

            // 推送 constants: SkyboxPush 结构体 (108 字节 in the compiled
            // 着色器 舍入 上 to 128 for 对齐 margin).
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(128)];

            // MRT 混合 状态 ForwardPass's 渲染 pass now has 2 颜色
            // attachments 颜色 + view-normal). Every 管线 bound inside
            // that 渲染 pass must declare a matching attachmentCount, so the
            // skybox 管线 lists 2 混合 states even though it only writes
            // SV_Target0. 附件 1's 写入 遮罩 is 0 so the 法线 目标
            // is untouched (the cleared value remains for sky pixels).
            let blend_attachments = [
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
                    .blend_enable(false),
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::empty())
                    .blend_enable(false),
            ];

            let pipeline = GraphicsPipeline::new(&PipelineDesc {
                device,
                shader_stages: &shader_stages,
                vertex_binding_desc: &binding_descs,
                vertex_attr_descs: &attr_descs,
                descriptor_set_layouts: std::slice::from_ref(&self.ibl_layout),
                push_constant_ranges: &push,
                render_pass,
                subpass: 0,
                cull_mode: Some(vk::CullModeFlags::NONE),
                depth_bias_enable: None,
                depth_bias_constant_factor: None,
                depth_bias_slope_factor: None,
                // Disable 深度 写入 so the sky never occludes scene geometry;
                // 深度 test LEQUAL lets it 绘制 where 深度 == 1.0 (cleared).
                depth_write_enable: Some(false),
                depth_test_enable: None,
                color_attachment_count: None,
                color_blend_attachments: Some(&blend_attachments),
                front_face: None,
                polygon_mode: None,
            })
            .context("SkyboxPass: create pipeline")?;

            unsafe {
                device.destroy_shader_module(vert_module, None);
                device.destroy_shader_module(frag_module, None);
            }
            self.pipeline = Some(pipeline);
            self.built_for_render_pass = Some(render_pass);
        }

        let pipeline = self.pipeline.as_ref().unwrap();

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
            // Bind the IBL 集合 at 集合 0. The IBL 布局 has bindings
            // 0=envCube, 1=irradiance, 2=prefiltered; the skybox 着色器
            // only reads 绑定 0 (envCube).
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                0,
                std::slice::from_ref(&self.ibl_descriptor_set),
                &[],
            );

            // 推送 `invViewRot` (inverse 视图 旋转 as the SkyboxPush
            // (128-byte range; only the 第一个 mat4 is used by the 着色器
            let mut push_data = [0u8; 128];
            push_data[..64].copy_from_slice(std::slice::from_raw_parts(
                inv_view_rot as *const _ as *const u8,
                64,
            ));
            push_data[64..68].copy_from_slice(&params.rotation.to_le_bytes());
            push_data[68..72].copy_from_slice(&params.intensity.max(0.0).to_le_bytes());
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                &push_data,
            );

            // 6 vertices = 2 triangles full-screen quad (see skybox.slang).
            device.cmd_draw(cmd, 6, 1, 0, 0);
        }

        Ok(())
    }

    /// Tear 下 GPU resources.
    ///
    /// `GraphicsPipeline` owns its own Vulkan handles and destroys them in its
    /// 放置 impl, so we just 放置 the 选项 here -- do NOT 调用
    /// `destroy_pipeline` manually (that double-frees, since 放置 would then
    /// 销毁 the same handle again).
    pub fn destroy(&mut self, _device: &ash::Device) {
        // Dropping 管线 runs `GraphicsPipeline::drop`, which calls
        // `destroy_pipeline` + `destroy_pipeline_layout`.
        self.pipeline = None;
        self.device = None;
    }
}

#[cfg(feature = "skybox")]
impl Drop for SkyboxPass {
    fn drop(&mut self) {
        // `GraphicsPipeline::drop` handles destroy_pipeline + destroy_layout,
        // so just 放置 the 选项 We gate on `self.device` so that an
        // un-initialized `SkyboxPass` (device=None) doesn't 放置 a 管线
        // that was never created.
        if self.device.take().is_some() {
            self.pipeline = None;
        }
    }
}

/// `skybox` feature 关闭时的空实现。
///
/// `ForwardPass` 的调用点保持原样（`new` / `set_descriptor_set` / `draw` /
/// `destroy`），所以场景 pass 里**没有**运行期分支；天空盒的管线、SPIR-V
/// （`skybox.vert/frag.spv`）与深度状态全部不参与编译。IBL 的 set 2 布局不变。
#[cfg(not(feature = "skybox"))]
pub struct SkyboxPass;

#[cfg(not(feature = "skybox"))]
impl SkyboxPass {
    pub fn new(
        _ibl_descriptor_set: vk::DescriptorSet,
        _ibl_layout: vk::DescriptorSetLayout,
    ) -> Self {
        Self
    }

    /// 场景切换重建 IBL 时的钩子；空实现下没有可更新的描述符。
    pub fn set_descriptor_set(&mut self, _set: vk::DescriptorSet) {}

    /// 不画天空盒：直接返回成功（清屏色继续显示）。
    pub fn draw(
        &mut self,
        _device: &ash::Device,
        _cmd: vk::CommandBuffer,
        _render_pass: vk::RenderPass,
        _extent: vk::Extent2D,
        _inv_view_rot: &[[f32; 4]; 4],
        _params: SkyboxSettings,
    ) -> Result<()> {
        Ok(())
    }

    pub fn destroy(&mut self, _device: &ash::Device) {}
}
