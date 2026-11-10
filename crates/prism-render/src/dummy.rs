//! 1×1 placeholder GPU resources.
//!
//! When pass-level building-block features (`shadows` / `gtao`) are disabled,
//! `ForwardPass` retains the descriptor layout on the shader side (set 3 = shadow depth map,
//! set 4 = previous-frame GTAO), binding a 1×1 constant image instead:
//!
//! * Shadow: `D32_SFLOAT` cleared to 1.0 (far plane) -> `LESS` compare always passes ("unoccluded");
//! * GTAO: `R8_UNORM` cleared to 1.0 -> visibility = 1 (no occlusion).
//!
//! This avoids runtime branching in the forward pass and prevents validation layer errors
//! (such as incomplete descriptors or layout mismatches) — matching the design of [`crate::ibl`],
//! which falls back to a 1×1 neutral grey image when disabled.
//!
//! Only compiled when the corresponding features are disabled; absent from full builds
//! (the other half of building blocks: disabled capabilities incur zero overhead).

use anyhow::Context as _;
use ash::vk;

use crate::context::VulkanContext;
use crate::render_pass::find_memory_type;

/// 一张 1×1、已经处于「可采样」布局的常量图。
pub(crate) struct SolidImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl SolidImage {
    pub(crate) fn view(&self) -> vk::ImageView {
        self.view
    }

    pub(crate) fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            if self.view != vk::ImageView::null() {
                device.destroy_image_view(self.view, None);
                self.view = vk::ImageView::null();
            }
            if self.image != vk::Image::null() {
                device.destroy_image(self.image, None);
                self.image = vk::Image::null();
            }
            if self.memory != vk::DeviceMemory::null() {
                device.free_memory(self.memory, None);
                self.memory = vk::DeviceMemory::null();
            }
        }
    }
}

impl Drop for SolidImage {
    fn drop(&mut self) {
        // 设备句柄由 `GraphRenderer::destroy` 显式传入；这里没有句柄可用，
        // 只能依赖显式销毁（与 `IblResources` / `GtaoPass` 的约定一致）。
        debug_assert_eq!(self.image, vk::Image::null());
    }
}

/// 1×1 `D32_SFLOAT`，清成远平面 1.0，布局 `DEPTH_STENCIL_READ_ONLY_OPTIMAL`。
///
/// 用于 `shadows` feature 关闭时填充 forward pass 的 set 3。
#[cfg(not(feature = "shadows"))]
pub(crate) fn shadow_map_dummy(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    queue: vk::Queue,
) -> anyhow::Result<SolidImage> {
    let image = alloc_image(
        context,
        vk::Format::D32_SFLOAT,
        vk::ImageAspectFlags::DEPTH,
        vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_DST,
    )?;

    one_shot(context, command_pool, queue, |device, cmd| unsafe {
        // `vkCmdClearDepthStencilImage` 只接受 TRANSFER_DST_OPTIMAL / GENERAL 布局，
        // 所以先过渡到 TRANSFER_DST，清成 1.0 后再过渡到只读采样布局。
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image.image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(depth_subresource())],
        );
        // 深度 1.0 = 远平面：fragment 深度 `LESS` 1.0 永远成立 → 全部受光。
        device.cmd_clear_depth_stencil_image(
            cmd,
            image.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
            &[depth_subresource()],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image.image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .subresource_range(depth_subresource())],
        );
    })?;

    Ok(image)
}

/// 1×1 `R8_UNORM`，清成 1.0（无遮蔽），布局 `SHADER_READ_ONLY_OPTIMAL`。
///
/// 用于 `gtao` feature 关闭时填充 forward pass 的 set 4。
#[cfg(not(feature = "gtao"))]
pub(crate) fn ao_texture_dummy(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    queue: vk::Queue,
) -> anyhow::Result<SolidImage> {
    let image = alloc_image(
        context,
        vk::Format::R8_UNORM,
        vk::ImageAspectFlags::COLOR,
        vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
    )?;

    one_shot(context, command_pool, queue, |device, cmd| unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image.image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(color_subresource())],
        );
        // 可见性 1.0 = 完全无遮蔽（shader 用 `1 - ao` 衰减环境光）。
        device.cmd_clear_color_image(
            cmd,
            image.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue {
                float32: [1.0, 1.0, 1.0, 1.0],
            },
            &[color_subresource()],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(image.image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .subresource_range(color_subresource())],
        );
    })?;

    Ok(image)
}

#[cfg(not(feature = "shadows"))]
fn depth_subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::DEPTH,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

#[cfg(not(feature = "gtao"))]
fn color_subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// 创建 1×1 OPTIMAL 图像 + 设备内存 + 全 aspect 视图。
fn alloc_image(
    context: &VulkanContext,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
    usage: vk::ImageUsageFlags,
) -> anyhow::Result<SolidImage> {
    let device = &context.device;
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: 1,
            height: 1,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let image = unsafe { device.create_image(&image_info, None) }.context("dummy: create image")?;

    let requirements = unsafe { device.get_image_memory_requirements(image) };
    let memory_type = find_memory_type(
        &context.physical_device_memory_properties,
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .context("dummy: no DEVICE_LOCAL memory type")?;
    let memory = match unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    } {
        Ok(memory) => memory,
        Err(err) => {
            unsafe { device.destroy_image(image, None) };
            return Err(err).context("dummy: allocate image memory");
        }
    };
    if let Err(err) = unsafe { device.bind_image_memory(image, memory, 0) } {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_image(image, None);
        }
        return Err(err).context("dummy: bind image memory");
    }

    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: aspect,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    let view = match unsafe { device.create_image_view(&view_info, None) } {
        Ok(view) => view,
        Err(err) => {
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return Err(err).context("dummy: create image view");
        }
    };

    Ok(SolidImage {
        image,
        memory,
        view,
    })
}

/// 在 `command_pool` 上录一条一次性命令并同步等到执行完（初始化期，不在帧循环里）。
fn one_shot(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    queue: vk::Queue,
    record: impl FnOnce(&ash::Device, vk::CommandBuffer),
) -> anyhow::Result<()> {
    let device = &context.device;
    let cmd = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .context("dummy: allocate one-shot command buffer")?[0];

    let result = (|| -> anyhow::Result<()> {
        unsafe {
            device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .context("dummy: begin one-shot command buffer")?;
        }
        record(device, cmd);
        unsafe {
            device
                .end_command_buffer(cmd)
                .context("dummy: end one-shot command buffer")?;
            device
                .queue_submit(
                    queue,
                    &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd))],
                    vk::Fence::null(),
                )
                .context("dummy: submit one-shot command buffer")?;
            device.queue_wait_idle(queue).context("dummy: wait idle")?;
        }
        Ok(())
    })();

    unsafe {
        device.free_command_buffers(command_pool, &[cmd]);
    }
    result
}
