//! Render pass, framebuffer, and depth image management.
//!
//! Owns the [`RenderPass`] (color + depth attachments, CLEAR → STORE/DONT_CARE),
//! a set of [`Framebuffers`]—one per swapchain image view—and [`DepthImage`]
//! instances for hardware depth testing.

use anyhow::Context as _;
use ash::vk;

use crate::context::VulkanContext;

/// A single-subpass render pass with one color attachment + depth/stencil.
///
/// Layout transitions:
/// - Color:  UNDEFINED → COLOR_ATTACHMENT_OPTIMAL → PRESENT_SRC_KHR
/// - Depth:  UNDEFINED → DEPTH_STENCIL_ATTACHMENT_OPTIMAL (→ stays there)
pub struct RenderPass {
    pub handle: vk::RenderPass,
    /// Cloned device handle kept so [`Drop`] can free the render pass (RAII).
    device: ash::Device,
}

impl RenderPass {
    /// Create the render pass for the given swapchain image and depth formats.
    pub fn new(
        device: &ash::Device,
        format: vk::Format,
        depth_format: vk::Format,
    ) -> anyhow::Result<Self> {
        let color_attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);

        let color_attachment_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

        let depth_attachment = vk::AttachmentDescription::default()
            .format(depth_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::DONT_CARE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        let depth_attachment_ref = vk::AttachmentReference::default()
            .attachment(1)
            .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_attachment_ref))
            .depth_stencil_attachment(&depth_attachment_ref);

        let attachments = [color_attachment, depth_attachment];

        // Dependency: wait for acquire semaphore before writing color + depth.
        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
            )
            .dst_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
            )
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                    | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            );

        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(std::slice::from_ref(&dependency));

        let handle = unsafe { device.create_render_pass(&create_info, None) }
            .context("create render pass")?;

        Ok(Self {
            handle,
            device: device.clone(),
        })
    }
}

impl Drop for RenderPass {
    fn drop(&mut self) {
        unsafe { self.device.destroy_render_pass(self.handle, None) };
    }
}

/// A depth image + view for one swapchain image.
pub struct DepthImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl DepthImage {
    /// Create a D32_SFLOAT depth image for the given extent.
    pub fn new(context: &VulkanContext, extent: vk::Extent2D) -> anyhow::Result<Self> {
        let device = &context.device;

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::D32_SFLOAT)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // DEPTH_STENCIL_ATTACHMENT for the scene render pass + SAMPLED so
            // the GTAO pass can 读取 it 后 as a 纹理 after ForwardPass
            // stores depth.
            .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let image =
            unsafe { device.create_image(&image_info, None) }.context("create depth image")?;

        let mem_reqs = unsafe { device.get_image_memory_requirements(image) };
        let mem_type = find_memory_type(
            &context.physical_device_memory_properties,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("no suitable memory type for depth image")?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);

        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .context("allocate depth image memory")?;

        unsafe { device.bind_image_memory(image, memory, 0) }.context("bind depth image memory")?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::D32_SFLOAT)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::DEPTH,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let view = unsafe { device.create_image_view(&view_info, None) }
            .context("create depth image view")?;

        Ok(Self {
            image,
            memory,
            view,
        })
    }

    /// Destroy the depth image, its memory, and its view.
    ///
    /// # Safety
    ///
    /// `device` must be a valid `ash::Device` that created these resources.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe { device.destroy_image_view(self.view, None) };
        unsafe { device.free_memory(self.memory, None) };
        unsafe { device.destroy_image(self.image, None) };
    }
}

/// Find a memory type index matching the given type filter and property flags.
pub fn find_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    properties: vk::MemoryPropertyFlags,
) -> Option<u32> {
    for i in 0..mem_props.memory_type_count {
        let i = i as usize;
        if (type_filter & (1 << i)) != 0
            && mem_props.memory_types[i].property_flags & properties == properties
        {
            return Some(i as u32);
        }
    }
    None
}

/// A color image + view used as an MRT attachment (e.g. the view-space normal
/// 目标 written by `ForwardPass` and sampled by the GTAO pass Mirrors
/// [`DepthImage`] but for a `COLOR_ATTACHMENT | SAMPLED` image with a
/// caller-supplied format.
pub struct NormalImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl NormalImage {
    /// Create a color image for the given extent + format. Usage is
    /// `COLOR_ATTACHMENT | SAMPLED` so the GTAO pass can read it back as a
    /// 纹理 after the ForwardPass stores it.
    pub fn new(
        context: &VulkanContext,
        extent: vk::Extent2D,
        format: vk::Format,
    ) -> anyhow::Result<Self> {
        let device = &context.device;

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let image =
            unsafe { device.create_image(&image_info, None) }.context("create normal image")?;

        let mem_reqs = unsafe { device.get_image_memory_requirements(image) };
        let mem_type = find_memory_type(
            &context.physical_device_memory_properties,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("no suitable memory type for normal image")?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);

        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .context("allocate normal image memory")?;

        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind normal image memory")?;

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
            .context("create normal image view")?;

        Ok(Self {
            image,
            memory,
            view,
        })
    }

    /// Destroy the image, its memory, and its view.
    ///
    /// # Safety
    ///
    /// `device` must be a valid `ash::Device` that created these resources.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe { device.destroy_image_view(self.view, None) };
        unsafe { device.free_memory(self.memory, None) };
        unsafe { device.destroy_image(self.image, None) };
    }
}

/// Collection of framebuffers, one per swapchain image view.
pub struct Framebuffers {
    pub handles: Vec<vk::Framebuffer>,
    extent: vk::Extent2D,
}

impl Framebuffers {
    /// Create framebuffers for each color/depth view pair.
    pub fn new(
        device: &ash::Device,
        render_pass: &RenderPass,
        color_views: &[vk::ImageView],
        depth_views: &[vk::ImageView],
        extent: vk::Extent2D,
    ) -> anyhow::Result<Self> {
        let handles = color_views
            .iter()
            .zip(depth_views.iter())
            .map(|(&cv, &dv)| {
                let attachments = [cv, dv];
                let create_info = vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass.handle)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1);
                unsafe { device.create_framebuffer(&create_info, None) }
                    .with_context(|| format!("create framebuffer for image view {cv:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self { handles, extent })
    }

    /// The extent these framebuffers were created with.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Get the framebuffer for a given swapchain image index.
    pub fn get(&self, image_index: usize) -> vk::Framebuffer {
        self.handles[image_index]
    }

    /// Destroy all framebuffers.
    ///
    /// # Safety
    ///
    /// `device` must be a valid `ash::Device` that created these framebuffers.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        for &fb in &self.handles {
            unsafe { device.destroy_framebuffer(fb, None) };
        }
        self.handles.clear();
    }
}
