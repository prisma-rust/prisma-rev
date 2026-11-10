//! 后处理通道——HDR 场景色调映射（+ 可选 FXAA）→ sRGB 交换链
//!
//! 全屏三角形片元通道，对 ForwardPass 的 HDR 中间颜色附件进行采样，
//! 应用 Reinhard 或 ACES 色调映射（根据 `tonemap_mode`），
//! 并将结果写入交换链图像。取代了之前位于 `scene_frag.slang` 中的内联色调映射，
//! 使场景输出保持线性 HDR（可被未来的后处理效果使用：泛光、时域抗锯齿等）。
//!
//! `aa_mode = 1` 时在色调映射之后再做一遍 FXAA（后处理抗锯齿，见 #494）：
//! 只在本通道内多采样几次已经色调映射过的颜色，**不改任何管线状态**，也
//! 不需要多采样目标 / resolve，因此交换链与离屏（编辑器视口）路径拿到的是
//! 同一份结果。调试图（`debug_rt != 0`）会强制关掉它。
//!
//! ## 资源
//! - 一个描述符集，将 HDR 颜色绑定为组合图像采样器
//! - 拥有自己的渲染通道+管线（1 个颜色附件 = 交换链格式，无深度）
//! - HDR 输入视图每帧通过 `set_input` 更新（ForwardPass 每交换链槽轮换一个 HDR 图像，
//!   匹配其刚写入的帧缓冲）。
//!
//! ## `execute` 中记录的布局转换
//! 1. 屏障：HDR `COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`
//! 2. 屏障：交换链 `UNDEFINED → COLOR_ATTACHMENT_OPTIMAL`（通过渲染通道的
//!    load op).
//! 3. begin render pass (writes swapchain), draw fullscreen triangle, end.
//! 4. The caller (GraphRenderer::render) barriers swapchain
//! `COLOR_ATTACHMENT_OPTIMAL -> PRESENT_SRC_KHR` (or the external overlay does
//! it via its own load+transition pass

use anyhow::Context as _;
use ash::vk;

use crate::buffer::create_buffer;
use crate::context::VulkanContext;
use crate::lut::Lut3d;
use crate::pipeline::{GraphicsPipeline, PipelineDesc};
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderMode,
    RenderPassNode, RenderSettings, ResourceHandle, ResourceUsage, ANF_COLOR_H, FORWARD_COLOR_H,
    FORWARD_DEPTH_H, FORWARD_NORMAL_H, PT_COLOR_H, TAA_COLOR_H,
};
use crate::render_pass::find_memory_type;
use crate::shader;
use crate::shader_bindings;

// Keep the reflected push-constant size and the generated Rust layout in sync.
const _: () = assert!(
    std::mem::size_of::<shader_bindings::post::PostPush>() as u32
        == shader_bindings::post::PUSH_CONSTANT_SIZE
);

/// Camera matrices for the camera-motion-blur effect (#502 Phase 2).
///
/// `invViewProj` reconstructs world space from the current depth buffer;
/// `prevViewProj` projects that point into the previous frame. `resolution`
/// converts the normalized velocity into pixels for the trail clamp. Laid out
/// to match `MotionBlurData` in `post.slang`.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct MotionBlurUniform {
    inv_view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    resolution: [f32; 2],
    _pad: [f32; 2],
}

/// Fullscreen-triangle tonemap pass: HDR scene color -> sRGB swapchain.
pub struct PostPass {
    render_pass: Option<vk::RenderPass>,
    /// Swapchain color format (set in `new`, used to rebuild the render pass on
    /// `drop_target`/`set_target`). Stored so the visualizer can read it.
    color_format: vk::Format,
    /// Layout the render pass leaves the color attachment in.
    ///
    /// Window targets end in `PRESENT_SRC_KHR` so the swapchain image can be
    /// presented; offscreen targets stay in `COLOR_ATTACHMENT_OPTIMAL` — a
    /// headless `VulkanContext` never enables `VK_KHR_swapchain` (no surface),
    /// and `PRESENT_SRC_KHR` is not a valid layout without it.
    final_layout: vk::ImageLayout,
    /// One framebuffer per swapchain image (each wraps its swapchain view).
    framebuffers: Vec<Option<vk::Framebuffer>>,
    /// Cached swapchain views the framebuffers were built against (for
    /// rebuild detection, mirroring ForwardPass's 模式
    target_views: Vec<vk::ImageView>,
    extent: vk::Extent2D,
    pipeline: Option<GraphicsPipeline>,
    /// One descriptor set per frame-in-flight so `set_input` can update frame
    /// N's set without disturbing frame N-1's still-in-flight set
    /// (VUID-vkUpdateDescriptorSets-None-03047). Each binds the HDR color view
    /// as a combined image sampler.
    descriptor_sets: Vec<vk::DescriptorSet>,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    /// 最近邻采样器（`InternalScaleMode::Integer`，#613）：像素画放大不糊。
    sampler_nearest: vk::Sampler,
    /// 深度采样器（`ShaderMode::Depth` / 景深）：最近邻，避免在深度边上插值。
    sampler_depth: vk::Sampler,
    /// The HDR view currently bound to each frame-in-flight's descriptor set.
    /// Tracked so we skip redundant descriptor rewrites.
    bound_hdrs: Vec<vk::ImageView>,
    /// 每个 in-flight 帧当前绑定的是否是最近邻采样器（与 `bound_hdrs` 一起判缓存）。
    bound_nearest: Vec<bool>,
    /// 每个 in-flight 帧当前绑定的深度视图（#502 DOF）。
    bound_depths: Vec<vk::ImageView>,
    /// 3D color-grading LUT (#502). Always bound (identity by default), so
    /// `lutStrength = 0` can skip the sample without an unbound descriptor.
    lut_image: vk::Image,
    lut_view: vk::ImageView,
    lut_memory: vk::DeviceMemory,
    /// Edge length of the bound LUT, forwarded to the shader for texel-centre
    /// sampling. A loaded `.cube` may have any size in `2..=128`.
    lut_size: u32,
    /// Motion-blur matrix UBOs (set 0 binding 3), one per frame in flight.
    motion_buffers: Vec<vk::Buffer>,
    motion_memories: Vec<vk::DeviceMemory>,
    /// Previous frame's view-projection, used to reproject static pixels.
    /// `None` on the first frame (velocity is zero).
    prev_view_proj: Option<[[f32; 4]; 4]>,
    device: Option<ash::Device>,
}

impl PostPass {
    /// Create the pass + persistent resources (sampler, ds layout/pool/sets,
    /// render pass). `color_format` is the swapchain format. `frames_in_flight`
    /// is the number of descriptor sets to allocate (one per concurrent frame
    /// so `set_input` doesn't disturb an in-flight set). The pipeline +
    /// framebuffers are created lazily once a render pass + target exist.
    ///
    /// `final_layout` is where the render pass leaves the color attachment:
    /// `PRESENT_SRC_KHR` for a swapchain image, `COLOR_ATTACHMENT_OPTIMAL` for
    /// an offscreen target (see [`Self::final_layout`]).
    pub fn new(
        context: &VulkanContext,
        color_format: vk::Format,
        frames_in_flight: u32,
        final_layout: vk::ImageLayout,
    ) -> anyhow::Result<Self> {
        let device = &context.device;

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("PostPass: create sampler")?;

        let sampler_nearest = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("PostPass: create nearest sampler")?;

        let sampler_depth = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0)
                    .max_lod(vk::LOD_CLAMP_NONE),
                None,
            )
        }
        .context("PostPass: create depth sampler")?;

        // binding 0 = HDR colour, binding 1 = scene depth for depth of field
        // (#502 Phase 2), binding 2 = 3D color-grading LUT (#502). All three
        // bindings are always present so the pipeline layout never changes
        // between frames, even when no LUT is configured.
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(shader_bindings::post::HDR_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(shader_bindings::post::DEPTH_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(shader_bindings::post::LUT_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(shader_bindings::post::MOTION_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .context("PostPass: create ds layout")?;

        let fif = frames_in_flight.max(1);
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(fif)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            // One colour + one depth + one LUT per frame-in-flight.
                            descriptor_count: fif * 3,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: fif,
                        },
                    ]),
                None,
            )
        }
        .context("PostPass: create ds pool")?;

        let layouts = vec![ds_layout; fif as usize];
        let descriptor_sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("PostPass: allocate ds")?;
        let descriptor_sets: Vec<vk::DescriptorSet> = descriptor_sets;

        // Every project gets an identity LUT so the descriptor is valid even
        // before `[render] lut` is set. 32³ matches the common `.cube` size
        // and makes `lut_strength = 1` with no file a no-op.
        let identity = Lut3d::identity(32);
        let (lut_image, lut_memory, lut_view) =
            create_lut_image(context, &identity).context("PostPass: create default LUT")?;
        for &set in &descriptor_sets {
            let info = vk::DescriptorImageInfo::default()
                .image_view(lut_view)
                .sampler(sampler)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(shader_bindings::post::LUT_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&info));
            unsafe { device.update_descriptor_sets(&[write], &[]) };
        }
        // Motion-blur UBO: one host-visible buffer per frame in flight. The
        // descriptor never changes; `set_motion_blur` only rewrites the bytes.
        let motion_size = std::mem::size_of::<MotionBlurUniform>() as vk::DeviceSize;
        let mut motion_buffers = Vec::with_capacity(fif as usize);
        let mut motion_memories = Vec::with_capacity(fif as usize);
        for _ in 0..fif {
            let (buffer, memory) = crate::buffer::create_buffer(
                context,
                motion_size,
                crate::buffer::BufferUsage::UNIFORM_BUFFER,
                crate::buffer::MemoryProperties::HOST_VISIBLE
                    | crate::buffer::MemoryProperties::HOST_COHERENT,
            )
            .context("PostPass: create motion-blur UBO")?;
            let ptr =
                unsafe { device.map_memory(memory, 0, motion_size, vk::MemoryMapFlags::empty()) }
                    .context("PostPass: map motion-blur UBO")?;
            unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, motion_size as usize) };
            unsafe { device.unmap_memory(memory) };
            motion_buffers.push(buffer);
            motion_memories.push(memory);
        }
        let motion_infos: Vec<vk::DescriptorBufferInfo> = motion_buffers
            .iter()
            .map(|buffer| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buffer)
                    .offset(0)
                    .range(motion_size)
            })
            .collect();
        let motion_writes: Vec<vk::WriteDescriptorSet> = descriptor_sets
            .iter()
            .zip(motion_infos.iter())
            .map(|(set, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(shader_bindings::post::MOTION_BINDING)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(std::slice::from_ref(info))
            })
            .collect();
        unsafe { device.update_descriptor_sets(&motion_writes, &[]) };

        let render_pass = create_render_pass(device, color_format, final_layout)?;

        Ok(Self {
            render_pass: Some(render_pass),
            color_format,
            final_layout,
            framebuffers: Vec::new(),
            target_views: Vec::new(),
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            pipeline: None,
            descriptor_sets,
            ds_layout,
            ds_pool,
            sampler,
            sampler_nearest,
            sampler_depth,
            bound_hdrs: vec![vk::ImageView::null(); fif as usize],
            bound_nearest: vec![false; fif as usize],
            bound_depths: vec![vk::ImageView::null(); fif as usize],
            lut_image,
            lut_view,
            lut_memory,
            lut_size: identity.size,
            motion_buffers,
            motion_memories,
            prev_view_proj: None,
            device: Some(device.clone()),
        })
    }

    /// Swapchain extent PostPass tonemaps into. Exposed for the visualizer.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Swapchain color format PostPass targets. Exposed for the visualizer.
    pub fn color_format(&self) -> vk::Format {
        self.color_format
    }

    /// Layout the render pass leaves the target image in.
    ///
    /// `PRESENT_SRC_KHR` for swapchain targets, `COLOR_ATTACHMENT_OPTIMAL` for
    /// offscreen targets. Exposed so tests can assert a headless renderer never
    /// asks for a swapchain-only layout.
    pub fn final_layout(&self) -> vk::ImageLayout {
        self.final_layout
    }

    /// Ensure the framebuffer for `image_index` exists and is built against the
    /// 当前 交换链 views + extent. Mirrors ForwardPass::set_target's
    /// per-slot rebuild logic (only rebuild an entry when its view changes, so
    /// in-flight framebuffers are never touched).
    pub fn set_target(
        &mut self,
        device: &ash::Device,
        swapchain_views: &[vk::ImageView],
        image_index: u32,
        extent: vk::Extent2D,
    ) -> anyhow::Result<()> {
        if extent.width == 0 || extent.height == 0 {
            return Ok(());
        }
        let idx = image_index as usize;
        if idx >= swapchain_views.len() {
            return Ok(());
        }
        let view = swapchain_views[idx];

        let swapchain_changed = self.target_views.len() != swapchain_views.len()
            || self.extent != extent
            || self
                .target_views
                .iter()
                .zip(swapchain_views.iter())
                .any(|(a, b)| a != b);
        if swapchain_changed {
            self.drop_target(device);
            self.target_views = swapchain_views.to_vec();
            self.extent = extent;
            self.framebuffers = (0..swapchain_views.len()).map(|_| None).collect();
        }

        let already_current = idx < self.target_views.len()
            && self.target_views[idx] == view
            && self.framebuffers[idx].is_some();
        if !already_current {
            let rp = self
                .render_pass
                .context("PostPass: render_pass missing in set_target")?;
            if let Some(old_fb) = self.framebuffers[idx].take() {
                unsafe { device.destroy_framebuffer(old_fb, None) };
            }
            let attachments = [view];
            let fb = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(rp)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }
            .context("PostPass: create framebuffer")?;
            self.framebuffers[idx] = Some(fb);
            self.target_views[idx] = view;
        }
        Ok(())
    }

    /// Replace the 3D color-grading LUT (#502).
    ///
    /// Uploads `lut` as an `R32G32B32A32_SFLOAT` 3D image and rebinds it on
    /// every frame-in-flight descriptor set. The old image is destroyed only
    /// after `vkDeviceWaitIdle`, because a previously submitted frame may still
    /// be sampling it. LUT changes are user-driven and rare, so the stall is
    /// preferable to tracking per-frame retirement.
    pub fn set_lut(&mut self, context: &VulkanContext, lut: &Lut3d) -> anyhow::Result<()> {
        let device = &context.device;
        let (image, memory, view) =
            create_lut_image(context, lut).context("PostPass: upload LUT")?;

        // No frame may still reference the old image before it is freed.
        unsafe { device.device_wait_idle() }.context("PostPass: wait before LUT swap")?;

        for &set in &self.descriptor_sets {
            let info = vk::DescriptorImageInfo::default()
                .image_view(view)
                .sampler(self.sampler)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            let write = vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(shader_bindings::post::LUT_TEX_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&info));
            unsafe { device.update_descriptor_sets(&[write], &[]) };
        }

        unsafe {
            device.destroy_image_view(self.lut_view, None);
            device.destroy_image(self.lut_image, None);
            device.free_memory(self.lut_memory, None);
        }
        self.lut_image = image;
        self.lut_view = view;
        self.lut_memory = memory;
        self.lut_size = lut.size;
        Ok(())
    }

    /// Drop the swapchain-derived framebuffers (called before swapchain
    /// recreate, mirroring ForwardPass::drop_target).
    pub fn drop_target(&mut self, device: &ash::Device) {
        for fb in self.framebuffers.drain(..).flatten() {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        self.target_views.clear();
        self.extent = vk::Extent2D {
            width: 0,
            height: 0,
        };
    }

    /// Update the HDR input view bound to the frame-in-flight's descriptor set.
    /// Called every frame from `GraphRenderer::render` before `execute`.
    /// Bind `view` (sampled with `image_layout`) as the input texture for this
    /// frame-in-flight's descriptor set. Skips the write when `view` matches
    /// the currently-bound one. `image_layout` must match the image's actual
    /// layout at draw time (depth uses `DEPTH_STENCIL_READ_ONLY_OPTIMAL`,
    /// color/normal use `SHADER_READ_ONLY_OPTIMAL`).
    pub fn set_input(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        view: vk::ImageView,
        image_layout: vk::ImageLayout,
        nearest: bool,
    ) {
        let i = (frame_index as usize) % self.descriptor_sets.len();
        if view == self.bound_hdrs[i] && nearest == self.bound_nearest[i] {
            return;
        }
        self.bound_hdrs[i] = view;
        self.bound_nearest[i] = nearest;
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .sampler(if nearest {
                self.sampler_nearest
            } else {
                self.sampler
            })
            .image_layout(image_layout);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.descriptor_sets[i])
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    /// Bind the scene depth view used by depth of field (#502 Phase 2).
    ///
    /// Called every frame from `execute` when the graph published
    /// `FORWARD_DEPTH_H`; the view is always in
    /// `DEPTH_STENCIL_READ_ONLY_OPTIMAL` (declared in `setup`).
    pub fn set_depth_input(&mut self, device: &ash::Device, frame_index: u32, view: vk::ImageView) {
        let i = (frame_index as usize) % self.descriptor_sets.len();
        if view == self.bound_depths[i] {
            return;
        }
        self.bound_depths[i] = view;
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .sampler(self.sampler_depth)
            .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.descriptor_sets[i])
            .dst_binding(shader_bindings::post::DEPTH_TEX_BINDING)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&image_info));
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    /// Upload the camera matrices used by camera motion blur (#502 Phase 2).
    fn set_motion_blur(
        &mut self,
        device: &ash::Device,
        frame_index: u32,
        inv_view_proj: [[f32; 4]; 4],
        prev_view_proj: [[f32; 4]; 4],
        resolution: [f32; 2],
    ) -> anyhow::Result<()> {
        if self.motion_memories.is_empty() {
            return Ok(());
        }
        let index = (frame_index as usize) % self.motion_memories.len();
        let memory = self.motion_memories[index];
        if memory == vk::DeviceMemory::null() {
            return Ok(());
        }
        let uniform = MotionBlurUniform {
            inv_view_proj,
            prev_view_proj,
            resolution,
            _pad: [0.0; 2],
        };
        let size = std::mem::size_of::<MotionBlurUniform>() as vk::DeviceSize;
        let ptr = unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
            .context("PostPass::set_motion_blur: map")?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytemuck::bytes_of(&uniform).as_ptr(),
                ptr as *mut u8,
                size as usize,
            );
            device.unmap_memory(memory);
        }
        Ok(())
    }

    /// Record the PostPass into `cmd`. Must run AFTER ForwardPass (which leaves
    /// the HDR color in COLOR_ATTACHMENT_OPTIMAL). The caller barriers the
    /// 交换链 to PRESENT_SRC_KHR (or the external overlay handles it) after this.
    /// `frame_index` selects the per-frame-in-flight descriptor set; `image_index`
    /// selects the per-swapchain-image framebuffer.
    pub fn execute(
        &mut self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        frame_index: u32,
        image_index: u32,
        hdr_image: vk::Image,
        dest: vk::Rect2D,
        push: &shader_bindings::post::PostPush,
    ) -> anyhow::Result<()> {
        self.ensure_pipeline(device)?;
        let rp = self.render_pass.unwrap();
        let pipeline = self.pipeline.as_ref().unwrap();
        let fb = self
            .framebuffers
            .get(image_index as usize)
            .copied()
            .flatten()
            .context("PostPass: no framebuffer for image_index (call set_target first)")?;
        let ds = self
            .descriptor_sets
            .get(frame_index as usize)
            .copied()
            .context("PostPass: no descriptor set for frame_index")?;

        // The HDR input COLOR_ATTACHMENT_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL
        // barrier used to live here. It is now inserted automatically by
        // `RenderGraph::execute` from the `read_usage` edge declared in
        // `setup`. `hdr_image` is therefore no longer needed in this function
        // (it was only referenced by the deleted barrier).
        let _ = hdr_image;

        // The swapchain image transitions UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL
        // via the 渲染 pass `initial_layout` (the external overlay or the caller's
        // PRESENT_SRC_KHR barrier handles the final transition out).
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        let begin_info = vk::RenderPassBeginInfo::default()
            .render_pass(rp)
            .framebuffer(fb)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            })
            .clear_values(&clear_values);
        unsafe { device.cmd_begin_render_pass(cmd, &begin_info, vk::SubpassContents::INLINE) };

        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.layout,
                0,
                std::slice::from_ref(&ds),
                &[],
            );
            // #613：内部分辨率按整数倍/拉伸放到 `dest`，其余区域由清屏色填黑边。
            device.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: dest.offset.x as f32,
                    y: dest.offset.y as f32,
                    width: dest.extent.width as f32,
                    height: dest.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            device.cmd_set_scissor(cmd, 0, &[dest]);
            device.cmd_push_constants(
                cmd,
                pipeline.layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::bytes_of(push),
            );
            // Fullscreen triangle (3 verts, no vertex buffer - SV_VertexID).
            device.cmd_draw(cmd, 3, 1, 0, 0);
        }

        unsafe { device.cmd_end_render_pass(cmd) };

        log::trace!(
            "PostPass: tonemapped HDR -> swapchain image {} ({}x{})",
            image_index,
            self.extent.width,
            self.extent.height
        );
        Ok(())
    }

    fn ensure_pipeline(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let rp = self
            .render_pass
            .context("PostPass: render_pass not created before pipeline")?;

        const VERT_SPV: &[u8] = include_bytes!("../../../assets/shaders/post.vert.spv");
        const FRAG_SPV: &[u8] = include_bytes!("../../../assets/shaders/post.frag.spv");
        let vert_module =
            shader::load_shader_module(device, VERT_SPV).context("PostPass: load vert")?;
        let frag_module =
            shader::load_shader_module(device, FRAG_SPV).context("PostPass: load frag")?;

        let vert_entry = std::ffi::CString::new(shader_bindings::post::ENTRY_VERTEX_MAIN).unwrap();
        let frag_entry =
            std::ffi::CString::new(shader_bindings::post::ENTRY_FRAGMENT_MAIN).unwrap();
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

        let binding_descs: [vk::VertexInputBindingDescription; 0] = [];
        let attr_descs: [vk::VertexInputAttributeDescription; 0] = [];
        let set_layouts = [self.ds_layout];

        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(shader_bindings::post::PUSH_CONSTANT_SIZE)];

        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false);

        let pipeline = GraphicsPipeline::new(&PipelineDesc {
            device,
            shader_stages: &shader_stages,
            vertex_binding_desc: &binding_descs,
            vertex_attr_descs: &attr_descs,
            descriptor_set_layouts: &set_layouts,
            push_constant_ranges: &push,
            render_pass: rp,
            subpass: 0,
            cull_mode: Some(vk::CullModeFlags::NONE),
            depth_bias_enable: None,
            depth_bias_constant_factor: None,
            depth_bias_slope_factor: None,
            depth_write_enable: Some(false),
            depth_test_enable: None,
            color_attachment_count: None,
            color_blend_attachments: Some(std::slice::from_ref(&blend_attachment)),
            front_face: None,
            polygon_mode: None,
        })
        .context("PostPass: create pipeline")?;

        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }
        self.pipeline = Some(pipeline);
        Ok(())
    }

    /// Destroy all GPU resources. Called from GraphRenderer::destroy on
    /// shutdown. `device_wait_idle` must already have been called by the caller.
    pub fn destroy(&mut self, device: &ash::Device) {
        self.drop_target(device);
        if let Some(rp) = self.render_pass.take() {
            unsafe { device.destroy_render_pass(rp, None) };
        }
        self.pipeline = None;
        unsafe { device.destroy_descriptor_set_layout(self.ds_layout, None) };
        unsafe { device.destroy_descriptor_pool(self.ds_pool, None) };
        unsafe { device.destroy_sampler(self.sampler, None) };
        unsafe { device.destroy_sampler(self.sampler_nearest, None) };
        unsafe { device.destroy_sampler(self.sampler_depth, None) };
        unsafe {
            device.destroy_image_view(self.lut_view, None);
            device.destroy_image(self.lut_image, None);
            device.free_memory(self.lut_memory, None);
        }
        for buffer in self.motion_buffers.drain(..) {
            if buffer != vk::Buffer::null() {
                unsafe { device.destroy_buffer(buffer, None) };
            }
        }
        for memory in self.motion_memories.drain(..) {
            if memory != vk::DeviceMemory::null() {
                unsafe { device.free_memory(memory, None) };
            }
        }
        self.device = None;
    }
}

impl Drop for PostPass {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            self.destroy(&device);
        }
    }
}

impl RenderPassNode for PostPass {
    fn name(&self) -> &str {
        "PostPass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // 高动态范围 输入 is published by ForwardPass under FORWARD_COLOR_H; 读取 in
        // `execute`. PostPass owns no graph-managed resources of its own.
        //
        // Declare the read edge so the render graph inserts the
        // COLOR_ATTACHMENT_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL barrier
        // automatically before this pass (replacing the hand-rolled
        // `cmd_pipeline_barrier` that used to live in `execute`).
        graph.read_usage(ResourceUsage {
            handle: FORWARD_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        // TAA output (#883) when enabled; declared unconditionally so the
        // graph knows PostPass may depend on it.
        graph.read_usage(ResourceUsage {
            handle: TAA_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        // PT output color (read in path-trace mode). Declare unconditionally
        // so the graph reserves the barrier slot even when in raster mode.
        graph.read_usage(ResourceUsage {
            handle: PT_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        // Debug RT viewer (Tab) can also sample the scene depth (mode 1) and
        // view-space normal (mode 2). Declare these read edges unconditionally
        // so the automatic barrier pipeline keeps them in a sampled layout
        // even when mode 0 doesn't read them (GTAO already transitions depth
        // and normal to read-only layouts, so this is usually a cache hit and
        // emits no extra barrier).
        graph.read_usage(ResourceUsage {
            handle: FORWARD_DEPTH_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_NORMAL_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: ANF_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        // Pick the input RT based on the render mode and debug viewer (Tab).
        // In path-trace 众数 we 读取 PT_COLOR_H instead of FORWARD_COLOR_H,
        // unless no usable Camera exists — in that case the PT pass was skipped
        // so fall 后 to FORWARD_COLOR_H (the gray 清空 颜色 from ForwardPass).
        // If PT_COLOR_H was not published (e.g. no RT hardware, pass skipped,
        // or geometry/materials not yet uploaded), also fall back to FORWARD_COLOR_H
        // so PostPass can tonemap and transition the swapchain image to PRESENT_SRC_KHR.
        // Debug modes 1 (depth) and 2 (normal) always read from scene output.
        let is_pt = ctx.frame.render_mode == RenderMode::PathTrace;
        let has_camera = ctx.frame.has_camera;
        let has_pt_view = resources.published_view(PT_COLOR_H).is_some();
        // ANF 超分辨率输出在启用时优先级最高（#1161）
        let anf_view = if ctx.settings.anf_sr && !is_pt && ctx.frame.debug_rt == 0 {
            resources.published_view(ANF_COLOR_H)
        } else {
            None
        };
        // TAA output (#883) takes precedence in the raster path when enabled and ANF is off.
        let taa_view = if ctx.settings.taa && !is_pt && ctx.frame.debug_rt == 0 && anf_view.is_none() {
            resources.published_view(TAA_COLOR_H)
        } else {
            None
        };
        let (handle, image_layout) = if anf_view.is_some() {
            (ANF_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        } else if taa_view.is_some() {
            (TAA_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        } else {
            select_input_target(ctx.frame.debug_rt, is_pt, has_camera, has_pt_view)
        };
        let input_view = match resources.published_view(handle) {
            Some(v) => v,
            None => {
                log::warn!("PostPass: no {:?} view published; skipping", handle);
                return Ok(());
            }
        };
        let input_image = resources
            .published_image(handle)
            .unwrap_or(vk::Image::null());

        // (Re)build this swapchain image's framebuffer if missing or the
        // 交换链 changed - mirrors `ForwardPass::ensure_target`. Before PR-1
        // this was `GraphRenderer`'s job (it called `set_target` every frame);
        // now the graph drives it so the framebuffer lifecycle is owned here.
        self.set_target(
            ctx.device,
            ctx.frame.swapchain_views,
            ctx.image_index,
            ctx.extent,
        )
        .context("PostPass: set_target")?;

        // Bind the selected input view into this frame's descriptor set. The
        // cache (`bound_hdrs`) keys on the view handle, so switching modes
        // (which changes the view) triggers a rewrite automatically.
        // #613：内部渲染分辨率 → 目标矩形与采样方式。
        let nearest = ctx.settings.internal_scale_mode
            == crate::render_graph::InternalScaleMode::Integer
            && ctx.render_extent != ctx.extent;
        let dest = crate::render_graph::post_destination(
            ctx.extent,
            ctx.render_extent,
            ctx.settings.internal_scale_mode,
        );
        self.set_input(
            ctx.device,
            ctx.frame_index,
            input_view,
            image_layout,
            nearest,
        );

        // Depth of field (#502 Phase 2) needs the scene depth in a read-only
        // layout. `setup` already declares the edge; reuse the published view.
        let depth_view = resources.published_view(FORWARD_DEPTH_H);
        if let Some(depth_view) = depth_view {
            self.set_depth_input(ctx.device, ctx.frame_index, depth_view);
        }

        // Camera motion blur (#502 Phase 2). Keep the previous VP up to date
        // even when the effect is off, so enabling it does not smear a jump
        // from an arbitrarily old camera. Multi-camera composites and path
        // tracing have no single valid previous view; they disable the effect.
        let current_view_proj = ctx.frame.view_proj;
        let prev_view_proj = self
            .prev_view_proj
            .replace(current_view_proj)
            .unwrap_or(current_view_proj);
        let inv_view_proj = crate::math::mat_inverse(&current_view_proj);
        let motion_extent = if ctx.render_extent.width > 0 && ctx.render_extent.height > 0 {
            ctx.render_extent
        } else {
            ctx.extent
        };
        self.set_motion_blur(
            ctx.device,
            ctx.frame_index,
            inv_view_proj,
            prev_view_proj,
            [motion_extent.width as f32, motion_extent.height as f32],
        )?;
        let motion_blur = if ctx.frame.debug_rt == 0
            && ctx.frame.render_mode == RenderMode::Raster
            && ctx.frame.cameras.len() <= 1
            && depth_view.is_some()
        {
            ctx.frame.grade.motion_blur
        } else {
            0.0
        };

        // The generated PostPush carries tonemapMode (Reinhard / ACES /
        // passthrough), aaMode (0 = off, 1 = FXAA) and invTexelSize (one texel in
        // UV units, used by the FXAA taps). debug_rt, proj22/proj32, near/far
        // etc. are consumed on the Rust side 输入 绑定 selection,
        // depth linearization) and are NOT part of the GPU push constant.
        // proj22/proj32/near/far depth linearization values are still computed
        // here as a reference for future shader extensions.
        let proj22 = ctx.frame.proj22;
        let proj32 = ctx.frame.proj32;
        let near = if (proj22 - 1.0).abs() > 1e-6 {
            proj32 / (proj22 + 1.0)
        } else {
            0.1
        };
        let far = if (proj22 - 1.0).abs() > 1e-6 {
            proj32 / (proj22 - 1.0)
        } else {
            100.0
        };
        // Debug RT views show depth / normals, not the lit scene; blurring
        // them would make the visualization useless. A missing depth view also
        // disables the effect instead of sampling an unbound texture.
        let dof_aperture = if ctx.frame.debug_rt == 0 && depth_view.is_some() {
            ctx.frame.grade.dof_aperture
        } else {
            0.0
        };
        let push = shader_bindings::post::PostPush {
            tonemapMode: ctx.frame.tonemap_mode,
            aaMode: effective_aa_mode(ctx.frame.debug_rt, ctx.frame.aa_mode),
            contrast: ctx.frame.grade.contrast,
            saturation: ctx.frame.grade.saturation,
            vignette: ctx.frame.grade.vignette,
            bloomIntensity: ctx.frame.grade.bloom_intensity,
            bloomThreshold: ctx.frame.grade.bloom_threshold,
            chromaticAberration: ctx.frame.grade.chromatic_aberration,
            filmGrain: ctx.frame.grade.film_grain,
            temperature: ctx.frame.grade.temperature,
            tint: ctx.frame.grade.tint,
            nearPlane: near,
            farPlane: far,
            dofFocusDistance: ctx.frame.grade.dof_focus_distance,
            dofAperture: dof_aperture,
            dofMaxBlur: ctx.frame.grade.dof_max_blur,
            // Debug RT views (depth / normals) must stay readable; a grade LUT
            // would tint the visualization, so it is disabled there like DOF.
            lutStrength: if ctx.frame.debug_rt == 0 {
                ctx.frame.grade.lut_strength
            } else {
                0.0
            },
            lutSize: self.lut_size as f32,
            motionBlur: motion_blur,
            motionBlurMaxPixels: ctx.frame.grade.motion_blur_max_pixels,
            // #613：FXAA 采样的是**内部渲染目标**，一个 texel 的 UV 尺寸按它的
            // 分辨率算；用交换链尺寸会在内部分辨率缩小时把采样步长算小。
            invTexelSize: [
                if ctx.render_extent.width > 0 {
                    1.0 / ctx.render_extent.width as f32
                } else {
                    0.0
                },
                if ctx.render_extent.height > 0 {
                    1.0 / ctx.render_extent.height as f32
                } else {
                    0.0
                },
            ],
        };
        self.execute(
            ctx.device,
            ctx.cmd,
            ctx.frame_index,
            ctx.image_index,
            input_image,
            dest,
            &push,
        )
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Post,
            // 高动态范围 颜色 comes from ForwardPass via FORWARD_COLOR_H.
            inputs: vec![FORWARD_COLOR_H],
            // PostPass writes the swapchain (not a graph-managed resource).
            outputs: Vec::new(),
        }
    }

    fn warmup(&mut self, device: &ash::Device, _context: &VulkanContext) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        if self.render_pass.is_none() {
            self.render_pass = Some(create_render_pass(
                device,
                self.color_format,
                self.final_layout,
            )?);
        }
        self.ensure_pipeline(device)
    }
}

/// 创建 the PostPass 渲染 pass 1 color-format 颜色 附件
/// (CLEAR -> STORE), no depth. `initial_layout = UNDEFINED` so the GPU
/// transitions the target 图像 from whatever 布局 it was in (typically
/// `PRESENT_SRC_KHR` from 最后一个 帧 into COLOR_ATTACHMENT_OPTIMAL as part of
/// the 加载 op.
///
/// `final_layout` is supplied by the caller: `PRESENT_SRC_KHR` for a swapchain
/// image, `COLOR_ATTACHMENT_OPTIMAL` for an offscreen image. Headless contexts
/// do not enable `VK_KHR_swapchain`, so the swapchain layout would be an
/// invalid enum value there (`VUID-VkAttachmentDescription-finalLayout-parameter`).
fn create_render_pass(
    device: &ash::Device,
    format: vk::Format,
    final_layout: vk::ImageLayout,
) -> anyhow::Result<vk::RenderPass> {
    let color_attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(final_layout);

    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));

    let dependency = vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

    let rp_create_info = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&color_attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(std::slice::from_ref(&dependency));

    let rp = unsafe { device.create_render_pass(&rp_create_info, None) }
        .context("PostPass: create render pass")?;
    Ok(rp)
}

// Re-export the memory-type finder so this module is self-contained for
// future HDR-image helpers (currently none - the HDR image is owned by
// ForwardPass). Kept here as a placeholder 导入 to avoid an unused 警告
// if no callers use it.
#[allow(dead_code)]
fn _memory_type_for_hdr(context: &VulkanContext, mem_type_bits: u32) -> anyhow::Result<u32> {
    find_memory_type(
        &context.physical_device_memory_properties,
        mem_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("PostPass: no suitable memory type for HDR image")
}

/// Anti-aliasing mode actually handed to the shader.
///
/// The debug render-target viewer (`debug_rt` 1 = depth, 2 = view-space normal)
/// shows a diagnostic image: FXAA would blend neighbouring texels and hide
/// exactly the stair-stepping those views exist to expose, so it is forced off
/// there.
pub(crate) fn effective_aa_mode(debug_rt: u32, aa_mode: u32) -> u32 {
    if debug_rt == 0 {
        aa_mode
    } else {
        0
    }
}

/// Helper to select the input resource handle and expected image layout for PostPass.
///
/// In path-trace mode we read PT_COLOR_H instead of FORWARD_COLOR_H, unless no usable camera
/// exists or PT_COLOR_H was not published (e.g. acceleration structures unsupported, or PT pass
/// skipped). In that case, falls back to FORWARD_COLOR_H so PostPass can tonemap and transition
/// the presentation image properly.
pub(crate) fn select_input_target(
    debug_rt: u32,
    is_pt: bool,
    has_camera: bool,
    has_pt_view: bool,
) -> (ResourceHandle, vk::ImageLayout) {
    match debug_rt {
        1 => (
            FORWARD_DEPTH_H,
            vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        ),
        2 => (FORWARD_NORMAL_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        _ if is_pt && has_camera && has_pt_view => {
            (PT_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        }
        _ if is_pt && has_camera => {
            log::warn!(
                "PostPass: PT_COLOR_H requested but not published; falling back to FORWARD_COLOR_H"
            );
            (FORWARD_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        }
        _ => (FORWARD_COLOR_H, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
    }
}

/// Create a sampled 3D image and upload `lut` into it.
///
/// The image is left in `SHADER_READ_ONLY_OPTIMAL`. `R32G32B32A32_SFLOAT`
/// keeps the interpolation exact enough that an identity LUT is a no-op up to
/// 8-bit display quantization; a 32³ LUT costs 512 KiB, which is negligible
/// next to the HDR targets.
fn create_lut_image(
    context: &VulkanContext,
    lut: &Lut3d,
) -> anyhow::Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let device = &context.device;
    let n = lut.size.max(2);
    debug_assert_eq!(lut.data.len(), (n * n * n) as usize);

    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_3D)
        .format(vk::Format::R32G32B32A32_SFLOAT)
        .extent(vk::Extent3D {
            width: n,
            height: n,
            depth: n,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&image_info, None) }.context("create LUT image")?;

    let requirements = unsafe { device.get_image_memory_requirements(image) };
    let memory_type = find_memory_type(
        &context.physical_device_memory_properties,
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("find LUT image memory type")?;
    let memory = match unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    } {
        Ok(memory) => memory,
        Err(error) => {
            unsafe { device.destroy_image(image, None) };
            return Err(error).context("allocate LUT image memory");
        }
    };
    if let Err(error) = unsafe { device.bind_image_memory(image, memory, 0) } {
        unsafe {
            device.destroy_image(image, None);
            device.free_memory(memory, None);
        }
        return Err(error).context("bind LUT image memory");
    }

    let view = match unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_3D)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .base_mip_level(0)
                        .level_count(1)
                        .base_array_layer(0)
                        .layer_count(1),
                ),
            None,
        )
    } {
        Ok(view) => view,
        Err(error) => {
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
            return Err(error).context("create LUT image view");
        }
    };

    let pixels = lut.to_rgba_f32();
    let byte_len = (pixels.len() * std::mem::size_of::<f32>()) as vk::DeviceSize;
    let (staging, staging_memory) = create_buffer(
        context,
        byte_len,
        vk::BufferUsageFlags::TRANSFER_SRC,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .context("create LUT staging buffer")?;
    let ptr =
        unsafe { device.map_memory(staging_memory, 0, byte_len, vk::MemoryMapFlags::empty()) }
            .context("map LUT staging buffer")?;
    unsafe {
        std::ptr::copy_nonoverlapping(
            pixels.as_ptr() as *const u8,
            ptr as *mut u8,
            byte_len as usize,
        );
        device.unmap_memory(staging_memory);
    }

    let pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::TRANSIENT)
                .queue_family_index(context.graphics_queue_family),
            None,
        )
    }
    .context("create LUT upload command pool")?;
    let cmd = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .context("allocate LUT upload command buffer")?[0];

    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1);
    let to_transfer = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .image(image)
        .subresource_range(range);
    let to_shader = vk::ImageMemoryBarrier::default()
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .image(image)
        .subresource_range(range);
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(0)
                .base_array_layer(0)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
        .image_extent(vk::Extent3D {
            width: n,
            height: n,
            depth: n,
        });

    let record = unsafe {
        device.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer],
        );
        device.cmd_copy_buffer_to_image(
            cmd,
            staging,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[copy],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_shader],
        );
        device.end_command_buffer(cmd)
    };
    if let Err(error) = record {
        unsafe {
            device.free_command_buffers(pool, &[cmd]);
            device.destroy_command_pool(pool, None);
            device.destroy_buffer(staging, None);
            device.free_memory(staging_memory, None);
            device.destroy_image_view(view, None);
            device.destroy_image(image, None);
            device.free_memory(memory, None);
        }
        return Err(error).context("record LUT upload");
    }

    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
        .context("create LUT upload fence")?;
    let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
    let submitted = unsafe { device.queue_submit(context.graphics_queue, &[submit], fence) };
    if let Err(error) = submitted {
        unsafe {
            device.destroy_fence(fence, None);
            device.free_command_buffers(pool, &[cmd]);
            device.destroy_command_pool(pool, None);
            device.destroy_buffer(staging, None);
            device.free_memory(staging_memory, None);
            device.destroy_image_view(view, None);
            device.destroy_image(image, None);
            device.free_memory(memory, None);
        }
        return Err(error).context("submit LUT upload");
    }
    let waited = unsafe { device.wait_for_fences(&[fence], true, u64::MAX) };
    unsafe {
        device.destroy_fence(fence, None);
        device.free_command_buffers(pool, &[cmd]);
        device.destroy_command_pool(pool, None);
        device.destroy_buffer(staging, None);
        device.free_memory(staging_memory, None);
    }
    waited.context("wait for LUT upload")?;

    Ok((image, memory, view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_input_target_raster() {
        let (handle, layout) = select_input_target(0, false, true, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_with_pt_view() {
        let (handle, layout) = select_input_target(0, true, true, true);
        assert_eq!(handle, PT_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_fallback_when_view_missing() {
        // PT requested with camera, but PT_COLOR_H was not published -> fall back to FORWARD_COLOR_H
        let (handle, layout) = select_input_target(0, true, true, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_select_input_target_path_trace_no_camera() {
        // PT requested without camera -> forward color
        let (handle, layout) = select_input_target(0, true, false, false);
        assert_eq!(handle, FORWARD_COLOR_H);
        assert_eq!(layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    #[test]
    fn test_effective_aa_mode_is_off_for_debug_views() {
        assert_eq!(effective_aa_mode(0, 0), 0);
        assert_eq!(effective_aa_mode(0, 1), 1);
        // Depth / view-space normal debug views stay unfiltered on purpose.
        assert_eq!(effective_aa_mode(1, 1), 0);
        assert_eq!(effective_aa_mode(2, 1), 0);
    }

    #[test]
    fn test_post_push_layout_matches_the_shader() {
        use shader_bindings::post::{PostPush, PUSH_CONSTANT_SIZE};
        assert_eq!(std::mem::offset_of!(PostPush, tonemapMode), 0);
        assert_eq!(std::mem::offset_of!(PostPush, aaMode), 4);
        assert_eq!(std::mem::offset_of!(PostPush, invTexelSize), 8);
        // #502 color grading rides in the same push-constant block.
        assert_eq!(std::mem::offset_of!(PostPush, contrast), 16);
        assert_eq!(std::mem::offset_of!(PostPush, saturation), 20);
        assert_eq!(std::mem::offset_of!(PostPush, vignette), 24);
        assert_eq!(std::mem::offset_of!(PostPush, bloomIntensity), 28);
        assert_eq!(std::mem::offset_of!(PostPush, bloomThreshold), 32);
        assert_eq!(std::mem::offset_of!(PostPush, chromaticAberration), 36);
        assert_eq!(std::mem::offset_of!(PostPush, filmGrain), 40);
        // #502 white balance rides after the Phase 2 effects.
        assert_eq!(std::mem::offset_of!(PostPush, temperature), 44);
        assert_eq!(std::mem::offset_of!(PostPush, tint), 48);
        // #502 depth of field: near/far planes plus focus / aperture / radius.
        assert_eq!(std::mem::offset_of!(PostPush, nearPlane), 52);
        assert_eq!(std::mem::offset_of!(PostPush, farPlane), 56);
        assert_eq!(std::mem::offset_of!(PostPush, dofFocusDistance), 60);
        assert_eq!(std::mem::offset_of!(PostPush, dofAperture), 64);
        assert_eq!(std::mem::offset_of!(PostPush, dofMaxBlur), 68);
        // #502 3D LUT: strength plus the edge length used for texel centres.
        assert_eq!(std::mem::offset_of!(PostPush, lutStrength), 72);
        assert_eq!(std::mem::offset_of!(PostPush, lutSize), 76);
        // #502 camera motion blur: strength then pixel cap.
        assert_eq!(std::mem::offset_of!(PostPush, motionBlur), 80);
        assert_eq!(std::mem::offset_of!(PostPush, motionBlurMaxPixels), 84);
        assert_eq!(std::mem::size_of::<PostPush>(), 88);
        assert_eq!(PUSH_CONSTANT_SIZE, 88);
    }

    #[test]
    fn test_motion_blur_is_clamped_and_off_by_default() {
        use crate::render_graph::PostGrade;
        assert!(PostGrade::default().is_neutral());
        assert_eq!(PostGrade::default().motion_blur, 0.0);
        assert_eq!(std::mem::size_of::<MotionBlurUniform>(), 144);

        let clamped = PostGrade {
            motion_blur: 2.0,
            motion_blur_max_pixels: 100.0,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(clamped.motion_blur, 1.0);
        assert_eq!(clamped.motion_blur_max_pixels, 64.0);

        let broken = PostGrade {
            motion_blur: f32::NAN,
            motion_blur_max_pixels: f32::NEG_INFINITY,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(broken.motion_blur, 0.0);
        assert_eq!(broken.motion_blur_max_pixels, 16.0);
    }

    #[test]
    fn test_depth_of_field_is_clamped_and_off_by_default() {
        use crate::render_graph::PostGrade;
        assert!(PostGrade::default().is_neutral());
        assert_eq!(PostGrade::default().dof_aperture, 0.0);

        let clamped = PostGrade {
            dof_focus_distance: -4.0,
            dof_aperture: 2.0,
            dof_max_blur: 100.0,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(clamped.dof_focus_distance, 0.001);
        assert_eq!(clamped.dof_aperture, 1.0);
        assert_eq!(clamped.dof_max_blur, 32.0);

        let broken = PostGrade {
            dof_focus_distance: f32::NAN,
            dof_aperture: f32::NAN,
            dof_max_blur: f32::INFINITY,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(broken.dof_focus_distance, 1.0);
        assert_eq!(broken.dof_aperture, 0.0);
        assert_eq!(broken.dof_max_blur, 8.0);
    }

    #[test]
    fn test_white_balance_is_clamped_and_neutral_at_zero() {
        use crate::render_graph::PostGrade;
        assert!(PostGrade::default().is_neutral());
        let clamped = PostGrade {
            temperature: 2.0,
            tint: -3.0,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(clamped.temperature, 1.0);
        assert_eq!(clamped.tint, -1.0);

        let broken = PostGrade {
            temperature: f32::NAN,
            tint: f32::INFINITY,
            ..PostGrade::default()
        }
        .sanitized();
        assert_eq!(broken.temperature, 0.0);
        assert_eq!(broken.tint, 0.0);
    }

    #[test]
    fn test_chromatic_aberration_is_clamped_and_neutral_at_zero() {
        use crate::render_graph::PostGrade;
        assert!(PostGrade::default().is_neutral());
        assert_eq!(
            PostGrade {
                chromatic_aberration: 2.0,
                ..PostGrade::default()
            }
            .sanitized()
            .chromatic_aberration,
            1.0
        );
        assert_eq!(
            PostGrade {
                chromatic_aberration: -1.0,
                ..PostGrade::default()
            }
            .sanitized()
            .chromatic_aberration,
            0.0
        );
        assert_eq!(
            PostGrade {
                chromatic_aberration: f32::NAN,
                ..PostGrade::default()
            }
            .sanitized()
            .chromatic_aberration,
            0.0
        );
    }

    #[test]
    fn test_film_grain_is_clamped_and_neutral_at_zero() {
        use crate::render_graph::PostGrade;
        assert!(PostGrade::default().is_neutral());
        assert_eq!(
            PostGrade {
                film_grain: 2.0,
                ..PostGrade::default()
            }
            .sanitized()
            .film_grain,
            1.0
        );
        assert_eq!(
            PostGrade {
                film_grain: -1.0,
                ..PostGrade::default()
            }
            .sanitized()
            .film_grain,
            0.0
        );
        assert_eq!(
            PostGrade {
                film_grain: f32::NAN,
                ..PostGrade::default()
            }
            .sanitized()
            .film_grain,
            0.0
        );
    }

    #[test]
    fn test_select_input_target_debug_modes() {
        let (h1, l1) = select_input_target(1, true, true, true);
        assert_eq!(h1, FORWARD_DEPTH_H);
        assert_eq!(l1, vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL);

        let (h2, l2) = select_input_target(2, true, true, true);
        assert_eq!(h2, FORWARD_NORMAL_H);
        assert_eq!(l2, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
}
