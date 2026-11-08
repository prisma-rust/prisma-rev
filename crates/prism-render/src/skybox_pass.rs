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
use crate::shader_bindings::skybox as bindings;
#[cfg(feature = "skybox")]
use anyhow::Context as _;
use anyhow::Result;

#[cfg(feature = "skybox")]
const _: () =
    assert!(std::mem::size_of::<bindings::SkyboxPush>() as u32 == bindings::PUSH_CONSTANT_SIZE);

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
/// the user-supplied `.hdr` (e.g. `kloppenheim_05_4k.hdr`) — there is no
/// separate loader: the skybox is just that env map rendered at the far plane.
///
/// The cube is generated in the vertex shader from `SV_VertexID` (no vertex
/// buffer is bound). The vertex stage strips the camera translation (by
/// rotating the corner with the inverse-view rotation, w=0) so the box stays
/// at infinity, then places it at NDC z=1 (far plane). The pipeline disables
/// depth writes and uses `LESS_OR_EQUAL` depth test, so the sky only shows
/// where no scene geometry has drawn.
///
/// Descriptor set layout (mirrors `skybox.slang`):
///   set 2, binding 0 - IBL environment cubemap (SamplerCube, combined)
#[cfg(feature = "skybox")]
pub struct SkyboxPass {
    /// IBL env cubemap descriptor set (set 0 binding 0). Borrowed from
    /// `IblResources`; not owned by `SkyboxPass`.
    ibl_descriptor_set: vk::DescriptorSet,
    /// IBL descriptor set layout (borrowed from `IblResources`). Contains
    /// bindings 0=envCube, 1=irradiance, 2=prefiltered; the skybox shader
    /// only reads binding 0 (envCube).
    ibl_layout: vk::DescriptorSetLayout,
    /// Owned pipeline + layout (created lazily on first `execute`).
    pipeline: Option<GraphicsPipeline>,
    /// Render pass the current `pipeline` was built against (to detect when a
    /// rebuild is needed, e.g. after a swapchain recreate rebuilds the
    /// ForwardPass 渲染 pass
    built_for_render_pass: Option<vk::RenderPass>,
    /// Cached device handle for Drop.
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

    /// Build (once) the skybox pipeline.
    fn ensure_pipeline(&mut self, device: &ash::Device) -> Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        self.device = Some(device.clone());

        // The render pass + color/depth formats are provided at draw time via
        // `execute_with` because the skybox must render into the *same*
        // 帧缓冲 the ForwardPass uses. We can't 构建 a fixed 管线
        // here without that render pass, so the pipeline is created lazily
        // inside `execute_with` (which has the render pass).
        Ok(())
    }

    /// Draw the skybox into the currently-bound render pass (begun by the
    /// 调用者 `ForwardPass`). `render_pass` + `extent` are needed to lazily
    /// create the pipeline. `inv_view_rot` is the inverse view rotation
    /// (world <- view), used to rotate the view-space look direction into
    /// world space for cubemap sampling.
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

        // Lazily (re)build the pipeline if the render pass differs (e.g. after
        // a 交换链 recreate that rebuilt ForwardPass' 渲染 pass
        let rebuild = self.built_for_render_pass != Some(render_pass);
        if rebuild {
            // Drop the old pipeline via GraphicsPipeline::Drop (which destroys
            // the pipeline + layout). Do NOT call destroy_pipeline manually -
            // that double-frees.
            self.pipeline = None;

            const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/skybox.vert.spv");
            const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/skybox.frag.spv");
            let vert_module =
                shader::load_shader_module(device, VERT_SPV).context("SkyboxPass: load vert")?;
            let frag_module =
                shader::load_shader_module(device, FRAG_SPV).context("SkyboxPass: load frag")?;

            let vert_entry = std::ffi::CString::new(bindings::ENTRY_VERTEX_MAIN).unwrap();
            let frag_entry = std::ffi::CString::new(bindings::ENTRY_FRAGMENT_MAIN).unwrap();
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

            // No vertex buffer: positions come from SV_VertexID in the shader.
            let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
            let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];

            // 推送 constants: SkyboxPush 结构体，大小直接来自 Slang 反射。
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(bindings::PUSH_CONSTANT_SIZE)];

            // MRT 混合 状态 ForwardPass's 渲染 pass now has 2 颜色
            // attachments (color + view-normal). Every pipeline bound inside
            // that render pass must declare a matching attachmentCount, so the
            // skybox pipeline lists 2 blend states even though it only writes
            // SV_Target0. attachment 1's write mask is 0 so the normal target
            // is untouched (the cleared value remains for sky pixels).
            let blend_attachments = [
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
                    .blend_enable(false),
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::empty())
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
                // Disable depth write so the sky never occludes scene geometry;
                // depth test LEQUAL lets it draw where depth == 1.0 (cleared).
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
            // Bind the IBL set at set 0. The IBL layout has bindings
            // 0=envCube, 1=irradiance, 2=prefiltered; the skybox shader
            // only reads binding 0 (envCube).
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                bindings::ENV_CUBE_SET,
                std::slice::from_ref(&self.ibl_descriptor_set),
                &[],
            );

            // The block layout comes from Slang reflection; `params` holds
            // rotation and intensity in its first two of four floats.
            let push = bindings::SkyboxPush {
                invViewRot: *inv_view_rot,
                params: [params.rotation, params.intensity.max(0.0), 0.0, 0.0],
                ..Default::default()
            };
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::bytes_of(&push),
            );

            // 6 vertices = 2 triangles full-screen quad (see skybox.slang).
            device.cmd_draw(cmd, 6, 1, 0, 0);
        }

        Ok(())
    }

    /// Tear down GPU resources.
    ///
    /// `GraphicsPipeline` owns its own Vulkan handles and destroys them in its
    /// `Drop` impl, so we just drop the `Option` here -- do NOT call
    /// `destroy_pipeline` manually (that double-frees, since `Drop` would then
    /// destroy the same handle again).
    pub fn destroy(&mut self, _device: &ash::Device) {
        // Dropping `pipeline` runs `GraphicsPipeline::drop`, which calls
        // `destroy_pipeline` + `destroy_pipeline_layout`.
        self.pipeline = None;
        self.device = None;
    }
}

#[cfg(feature = "skybox")]
impl Drop for SkyboxPass {
    fn drop(&mut self) {
        // `GraphicsPipeline::drop` handles destroy_pipeline + destroy_layout,
        // so just drop the Option. We gate on `self.device` so that an
        // un-initialized `SkyboxPass` (device=None) doesn't drop a pipeline
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
