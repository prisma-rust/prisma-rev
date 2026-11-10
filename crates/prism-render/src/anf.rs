//! Qualcomm Adreno Neural Fusion（ANF）移动端神经超分辨率与插帧支持（#1161）。
//!
//! 专为骁龙 Adreno GPU（如高通骁龙 8 至尊版 / Adreno 850+ 矩阵加速单元）设计的
//! 神经网络渲染增强管线：
//! - 神经超分辨率（Super Resolution, SR）：从低分辨率带 Jitter 的 Color / Depth / Motion Vector 重建全高清画面
//! - 动态动态库加载（`libanf.so`）：在非 Adreno、桌面或缺少动态库时优雅降级，维持跨平台统一二进制契约

use anyhow::{Context as _, Result};
use ash::vk;
use std::ffi::c_void;

use crate::buffer::find_memory_type;
use crate::context::VulkanContext;
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceType, ResourceUsage, ANF_COLOR_H, FORWARD_COLOR_H, FORWARD_DEPTH_H,
    FORWARD_VELOCITY_H,
};

/// Qualcomm 厂商 ID（0x5143）。
pub const QUALCOMM_VENDOR_ID: u32 = 0x5143;

/// 探测物理设备是否为 Qualcomm Adreno GPU。
pub fn is_qualcomm_adreno(properties: &vk::PhysicalDeviceProperties) -> bool {
    if properties.vendor_id == QUALCOMM_VENDOR_ID {
        return true;
    }
    let name_bytes = properties
        .device_name
        .iter()
        .map(|&c| c as u8)
        .collect::<Vec<_>>();
    let name_str = std::ffi::CStr::from_bytes_until_nul(&name_bytes)
        .ok()
        .and_then(|s| s.to_str().ok())
        .unwrap_or("");
    name_str.contains("Adreno") || name_str.contains("adreno")
}

/// ANF 运行能力配置概况。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnfCaps {
    pub is_adreno: bool,
    pub sr_supported: bool,
    pub fg_supported: bool,
}

impl AnfCaps {
    pub fn probe(properties: &vk::PhysicalDeviceProperties) -> Self {
        let is_adreno = is_qualcomm_adreno(properties);
        Self {
            is_adreno,
            sr_supported: is_adreno,
            fg_supported: is_adreno,
        }
    }
}

// ---------------------------------------------------------------------------
// Native ANF FFI 定义
// ---------------------------------------------------------------------------

pub type AnfContext = *mut c_void;
pub type AnfSR = *mut c_void;
pub type AnfResult = i32;

pub const ANF_SUCCESS: AnfResult = 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AnfSRCreateInfo {
    pub input_width: u32,
    pub input_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub color_format: vk::Format,
    pub depth_format: vk::Format,
    pub motion_vector_format: vk::Format,
    pub output_format: vk::Format,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AnfSRDispatchInfo {
    pub color_view: vk::ImageView,
    pub depth_view: vk::ImageView,
    pub motion_vector_view: vk::ImageView,
    pub output_view: vk::ImageView,
    pub jitter_x: f32,
    pub jitter_y: f32,
    pub sharpness: f32,
    pub reset: u32,
}

pub struct AnfBinding {
    pub create_context: unsafe extern "C" fn(
        instance: vk::Instance,
        physical_device: vk::PhysicalDevice,
        device: vk::Device,
        out_ctx: *mut AnfContext,
    ) -> AnfResult,
    pub destroy_context: unsafe extern "C" fn(ctx: AnfContext),
    pub create_sr: unsafe extern "C" fn(
        ctx: AnfContext,
        info: *const AnfSRCreateInfo,
        out_sr: *mut AnfSR,
    ) -> AnfResult,
    pub destroy_sr: unsafe extern "C" fn(ctx: AnfContext, sr: AnfSR),
    pub dispatch_sr: unsafe extern "C" fn(
        ctx: AnfContext,
        sr: AnfSR,
        cmd: vk::CommandBuffer,
        dispatch: *const AnfSRDispatchInfo,
    ) -> AnfResult,
}

impl AnfBinding {
    /// 尝试动态加载 `libanf.so`。若在非 Android 平台或缺失库则返回 `None`。
    #[cfg(target_os = "android")]
    pub fn try_load() -> Option<Self> {
        let lib_name = b"libanf.so\0";
        let handle = unsafe { dlopen(lib_name.as_ptr() as *const _, 1) }; // RTLD_LAZY
        if handle.is_null() {
            log::info!("ANF: libanf.so not found; neural fusion pass disabled");
            return None;
        }

        unsafe {
            let create_ctx = dlsym(handle, b"anfCreateContext\0".as_ptr() as *const _);
            let destroy_ctx = dlsym(handle, b"anfDestroyContext\0".as_ptr() as *const _);
            let create_sr = dlsym(handle, b"anfCreateSR\0".as_ptr() as *const _);
            let destroy_sr = dlsym(handle, b"anfDestroySR\0".as_ptr() as *const _);
            let dispatch_sr = dlsym(handle, b"anfDispatchSR\0".as_ptr() as *const _);

            if create_ctx.is_null()
                || destroy_ctx.is_null()
                || create_sr.is_null()
                || destroy_sr.is_null()
                || dispatch_sr.is_null()
            {
                log::warn!("ANF: libanf.so missing required entry points");
                dlclose(handle);
                return None;
            }

            Some(Self {
                create_context: std::mem::transmute(create_ctx),
                destroy_context: std::mem::transmute(destroy_ctx),
                create_sr: std::mem::transmute(create_sr),
                destroy_sr: std::mem::transmute(destroy_sr),
                dispatch_sr: std::mem::transmute(dispatch_sr),
            })
        }
    }

    #[cfg(not(target_os = "android"))]
    pub fn try_load() -> Option<Self> {
        None
    }
}

#[cfg(target_os = "android")]
extern "C" {
    fn dlopen(filename: *const std::os::raw::c_char, flag: std::os::raw::c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> std::os::raw::c_int;
}

// ---------------------------------------------------------------------------
// AnfPass — RenderGraph 节点
// ---------------------------------------------------------------------------

/// 负责将低分辨率渲染输出通过 ANF 超采样放大到全屏交换链尺寸的渲染通道。
pub struct AnfPass {
    output_extent: vk::Extent2D,
    input_extent: vk::Extent2D,
    output_image: vk::Image,
    output_memory: vk::DeviceMemory,
    output_view: vk::ImageView,
    binding: Option<AnfBinding>,
    context: Option<AnfContext>,
    sr_instance: Option<AnfSR>,
    device: Option<ash::Device>,
    last_jitter: [f32; 2],
}

impl AnfPass {
    pub fn new(
        context: &VulkanContext,
        input_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<Self> {
        let binding = AnfBinding::try_load();
        let (output_image, output_memory, output_view) =
            create_anf_output_image(context, output_extent)?;

        let mut anf_ctx: Option<AnfContext> = None;
        let mut sr_inst: Option<AnfSR> = None;

        if let Some(ref b) = binding {
            let mut ctx_raw: AnfContext = std::ptr::null_mut();
            let res = unsafe {
                (b.create_context)(
                    context.instance.handle(),
                    context.physical_device,
                    context.device.handle(),
                    &mut ctx_raw,
                )
            };
            if res == ANF_SUCCESS && !ctx_raw.is_null() {
                anf_ctx = Some(ctx_raw);

                let create_info = AnfSRCreateInfo {
                    input_width: input_extent.width.max(1),
                    input_height: input_extent.height.max(1),
                    output_width: output_extent.width.max(1),
                    output_height: output_extent.height.max(1),
                    color_format: vk::Format::R16G16B16A16_SFLOAT,
                    depth_format: vk::Format::D32_SFLOAT,
                    motion_vector_format: vk::Format::R16G16B16A16_SFLOAT,
                    output_format: vk::Format::R16G16B16A16_SFLOAT,
                    flags: 0,
                };
                let mut sr_raw: AnfSR = std::ptr::null_mut();
                let sr_res = unsafe { (b.create_sr)(ctx_raw, &create_info, &mut sr_raw) };
                if sr_res == ANF_SUCCESS && !sr_raw.is_null() {
                    sr_inst = Some(sr_raw);
                }
            }
        }

        Ok(Self {
            output_extent,
            input_extent,
            output_image,
            output_memory,
            output_view,
            binding,
            context: anf_ctx,
            sr_instance: sr_inst,
            device: Some(context.device.clone()),
            last_jitter: [0.0, 0.0],
        })
    }

    pub fn recreate_target(
        &mut self,
        context: &VulkanContext,
        input_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<()> {
        if self.output_extent == output_extent && self.input_extent == input_extent {
            return Ok(());
        }

        let device = &context.device;
        unsafe {
            device.destroy_image_view(self.output_view, None);
            device.destroy_image(self.output_image, None);
            device.free_memory(self.output_memory, None);
        }

        let (image, memory, view) = create_anf_output_image(context, output_extent)?;
        self.output_image = image;
        self.output_memory = memory;
        self.output_view = view;
        self.output_extent = output_extent;
        self.input_extent = input_extent;

        // 重建 SR 实例
        if let (Some(b), Some(ctx)) = (&self.binding, self.context) {
            if let Some(sr) = self.sr_instance.take() {
                unsafe { (b.destroy_sr)(ctx, sr) };
            }

            let create_info = AnfSRCreateInfo {
                input_width: input_extent.width.max(1),
                input_height: input_extent.height.max(1),
                output_width: output_extent.width.max(1),
                output_height: output_extent.height.max(1),
                color_format: vk::Format::R16G16B16A16_SFLOAT,
                depth_format: vk::Format::D32_SFLOAT,
                motion_vector_format: vk::Format::R16G16B16A16_SFLOAT,
                output_format: vk::Format::R16G16B16A16_SFLOAT,
                flags: 0,
            };
            let mut sr_raw: AnfSR = std::ptr::null_mut();
            let sr_res = unsafe { (b.create_sr)(ctx, &create_info, &mut sr_raw) };
            if sr_res == ANF_SUCCESS && !sr_raw.is_null() {
                self.sr_instance = Some(sr_raw);
            }
        }

        Ok(())
    }

    pub fn is_available(&self) -> bool {
        self.sr_instance.is_some()
    }
}

impl Drop for AnfPass {
    fn drop(&mut self) {
        if let (Some(b), Some(ctx)) = (&self.binding, self.context) {
            if let Some(sr) = self.sr_instance.take() {
                unsafe { (b.destroy_sr)(ctx, sr) };
            }
            unsafe { (b.destroy_context)(ctx) };
        }

        if let Some(device) = &self.device {
            unsafe {
                device.destroy_image_view(self.output_view, None);
                device.destroy_image(self.output_image, None);
                device.free_memory(self.output_memory, None);
            }
        }
    }
}

impl RenderPassNode for AnfPass {
    fn name(&self) -> &'static str {
        "AnfPass"
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: "AnfPass".into(),
            kind: PassKind::Anf,
            inputs: vec![FORWARD_COLOR_H, FORWARD_DEPTH_H, FORWARD_VELOCITY_H],
            outputs: vec![ANF_COLOR_H],
        }
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        graph.create_resource_at(
            ANF_COLOR_H,
            ResourceType::ColorAttachment {
                format: vk::Format::R16G16B16A16_SFLOAT,
                extent: self.output_extent,
                sample_count: vk::SampleCountFlags::TYPE_1,
            },
        );
        graph.read_usage(ResourceUsage {
            handle: FORWARD_COLOR_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_DEPTH_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            layout: vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        });
        graph.read_usage(ResourceUsage {
            handle: FORWARD_VELOCITY_H,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
        graph.write_usage(ResourceUsage {
            handle: ANF_COLOR_H,
            access: vk::AccessFlags::SHADER_WRITE,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            layout: vk::ImageLayout::GENERAL,
        });
    }

    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()> {
        if !ctx.settings.anf_sr {
            return Ok(());
        }

        let Some(color_view) = resources.published_view(FORWARD_COLOR_H) else {
            return Ok(());
        };
        let Some(depth_view) = resources.published_view(FORWARD_DEPTH_H) else {
            return Ok(());
        };
        let Some(velocity_view) = resources.published_view(FORWARD_VELOCITY_H) else {
            return Ok(());
        };

        self.recreate_target(ctx.context, ctx.render_extent, ctx.extent)?;

        if let (Some(b), Some(ctx_anf), Some(sr)) = (&self.binding, self.context, self.sr_instance)
        {
            let jitter = ctx.frame.view_proj_jitter;
            let reset = if self.last_jitter == jitter { 1 } else { 0 };
            self.last_jitter = jitter;

            let dispatch = AnfSRDispatchInfo {
                color_view,
                depth_view,
                motion_vector_view: velocity_view,
                output_view: self.output_view,
                jitter_x: jitter[0],
                jitter_y: jitter[1],
                sharpness: ctx.settings.anf_sharpness,
                reset,
            };

            unsafe {
                (b.dispatch_sr)(ctx_anf, sr, ctx.cmd, &dispatch);
            }

            // 转换输出图像到 SHADER_READ_ONLY_OPTIMAL 给下游 PostPass 采样
            let barrier = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.output_image)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            unsafe {
                ctx.device.cmd_pipeline_barrier(
                    ctx.cmd,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
            }

            resources.set_image_view(ANF_COLOR_H, self.output_view);
            resources.set_image(ANF_COLOR_H, self.output_image);
        }

        Ok(())
    }
}

fn create_anf_output_image(
    context: &VulkanContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let device = &context.device;
    let format = vk::Format::R16G16B16A16_SFLOAT;

    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: extent.width.max(1),
            height: extent.height.max(1),
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::STORAGE,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let image = unsafe { device.create_image(&image_info, None) }
        .context("AnfPass: create output image")?;

    let reqs = unsafe { device.get_image_memory_requirements(image) };
    let memory_type = find_memory_type(
        context,
        reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("AnfPass: no suitable memory type for output image")?;

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(memory_type);

    let memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .context("AnfPass: allocate memory for output image")?;
    unsafe { device.bind_image_memory(image, memory, 0) }
        .context("AnfPass: bind output image memory")?;

    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });

    let view = unsafe { device.create_image_view(&view_info, None) }
        .context("AnfPass: create output image view")?;

    Ok((image, memory, view))
}
