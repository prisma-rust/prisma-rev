//! 交换链 and per-frame 同步
//!
//! Owns the [`VkSurfaceKHR`], the 交换链 + its 图像 views, and the
//! 同步 primitives used to pace acquire vs. present.
//!
//! 同步 模型
//! - `FRAMES_IN_FLIGHT` **acquire semaphores** (`image_available`),
//! rotated by `current_frame`. An acquire 信号量 is only reused once its
//! frame's 围栏 has been waited on, so it is guaranteed unsignaled.
//! - One **render-finished 信号量 per 交换链 image**, indexed by
//! `image_index`. Present always waits on the 信号量 that the matching
//! submit signaled, so a render-finished 信号量 is never reused while a
//! present still holds it -- even when two acquires return the same 索引
//! - `FRAMES_IN_FLIGHT` fences for host pacing, rotated by `current_frame`.
//!
//! With 3 交换链 images and 2 frames in flight, at least one 图像 is
//! always free for acquire, so no per-image 围栏 tracking is needed.

use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use ash::vk;

use crate::context::VulkanContext;

/// 最大 frames submitted to the GPU ahead of the host.
pub(crate) const FRAMES_IN_FLIGHT: usize = 2;

/// The 交换链 plus the 表面 it presents to.
pub struct Swapchain {
    pub surface: vk::SurfaceKHR,
    /// Kept so it outlives any surface-destroy calls.
    _surface_ext: ash::khr::surface::Instance,
    _debug_utils: Option<ash::ext::debug_utils::Instance>,

    pub extent: vk::Extent2D,
    pub format: vk::SurfaceFormatKHR,
    /// 变换 the presentation engine applies to the 交换链 图像 before
    /// displaying it (e.g. `ROTATE_90` on a 横屏 app running on a
    /// portrait-native 设备 等于 to `current_transform` at creation 时间
    pub pre_transform: vk::SurfaceTransformFlagsKHR,

    /// Presentation 众数 used when (re)creating the 交换链 Defaults to
    /// `FIFO` when v-synced (it actually caps the frame rate), changeable via
    /// [`Swapchain::set_present_mode`].
    present_mode: vk::PresentModeKHR,

    swapchain: vk::SwapchainKHR,
    swapchain_ext: ash::khr::swapchain::Device,

    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,

    /// Acquire semaphores, one per frame-in-flight, rotated by `current_frame`.
    image_available: Vec<vk::Semaphore>,
    /// Render-finished semaphores, one per 交换链 图像 索引 by 图像 idx).
    render_finished: Vec<vk::Semaphore>,
    /// Host pacing fences, one per frame-in-flight, rotated by `current_frame`.
    in_flight_fences: Vec<vk::Fence>,
    /// Rotating 帧 索引 advanced each present.
    current_frame: usize,
}

impl Swapchain {
    /// 创建 the 表面 (from the 窗口 and an initial 交换链
    ///
    /// Defaults to a v-synced present mode. Use [`Swapchain::new_with_vsync`]
    /// when the caller has an explicit `WindowConfig.vsync` preference.
    pub fn new(
        context: &Arc<VulkanContext>,
        window: &dyn raw_window_handle::HasDisplayHandle,
        window_handle: &dyn raw_window_handle::HasWindowHandle,
    ) -> anyhow::Result<Self> {
        Self::new_with_vsync(context, window, window_handle, true)
    }

    /// 创建 the 表面 and 交换链 with an explicit vsync preference.
    ///
    /// `vsync = false` prefers `IMMEDIATE` (uncapped, may tear) and falls
    /// back to `MAILBOX` / `FIFO`; `true` prefers `FIFO` (vblank cap) then
    /// `MAILBOX`.
    pub fn new_with_vsync(
        context: &Arc<VulkanContext>,
        window: &dyn raw_window_handle::HasDisplayHandle,
        window_handle: &dyn raw_window_handle::HasWindowHandle,
        vsync: bool,
    ) -> anyhow::Result<Self> {
        let surface_ext = ash::khr::surface::Instance::new(&context.entry, &context.instance);

        let display_handle = window
            .display_handle()
            .map_err(|e| anyhow!(e).context("get display handle"))?;
        let raw_window = window_handle
            .window_handle()
            .map_err(|e| anyhow!(e).context("get window handle"))?;

        let surface = unsafe {
            ash_window::create_surface(
                &context.entry,
                &context.instance,
                display_handle.into(),
                raw_window.into(),
                None,
            )
        }
        .map_err(|e| anyhow!(e).context("create surface"))?;

        let present_mode =
            choose_present_mode(&surface_ext, context.physical_device, surface, vsync);

        let SwapchainOutput {
            format,
            extent,
            pre_transform,
            swapchain,
            images,
            views,
        } = create_swapchain(context, surface, vk::SwapchainKHR::null(), present_mode)?;
        let n_images = images.len();
        let sem_info = vk::SemaphoreCreateInfo::default();
        let image_available = (0..FRAMES_IN_FLIGHT)
            .map(|_| unsafe { context.device.create_semaphore(&sem_info, None) })
            .collect::<Result<Vec<_>, _>>()
            .context("create image_available semaphores")?;
        let render_finished = (0..n_images)
            .map(|_| unsafe { context.device.create_semaphore(&sem_info, None) })
            .collect::<Result<Vec<_>, _>>()
            .context("create render_finished semaphores")?;

        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        let in_flight_fences = (0..FRAMES_IN_FLIGHT)
            .map(|_| unsafe { context.device.create_fence(&fence_info, None) })
            .collect::<Result<Vec<_>, _>>()
            .context("create in_flight fences")?;

        Ok(Self {
            surface,
            _surface_ext: surface_ext,
            _debug_utils: None,
            extent,
            format,
            pre_transform,
            present_mode,
            swapchain,
            swapchain_ext: ash::khr::swapchain::Device::new(&context.instance, &context.device),
            images,
            views,
            image_available,
            render_finished,
            in_flight_fences,
            current_frame: 0,
        })
    }

    /// 变换 the presentation engine applies to the 交换链 图像
    /// Used by the 渲染器 to pre-rotate the view-projection so the final
    /// on-screen 图像 is upright and correctly proportioned.
    pub fn pre_transform(&self) -> vk::SurfaceTransformFlagsKHR {
        self.pre_transform
    }

    /// 当前 presentation 众数
    pub fn present_mode(&self) -> vk::PresentModeKHR {
        self.present_mode
    }

    /// Change the presentation 众数 Takes 效果 on the 下一个
    /// [`Swapchain::recreate`]. `MAILBOX` reduces 延迟 but may not be
    /// supported everywhere; `FIFO` is always available.
    pub fn set_present_mode(&mut self, mode: vk::PresentModeKHR) {
        self.present_mode = mode;
    }

    /// Recreate the 交换链 for a new 窗口 大小 Waits for the 设备 to
    /// be idle 第一个 Transactional: if creating the new 交换链 fails, the
    /// existing one (and its semaphores) are 左 intact so 渲染 can
    /// retry later rather than 结束 上 with dangling handles.
    pub fn recreate(&mut self, context: &VulkanContext) -> anyhow::Result<()> {
        unsafe { context.device.device_wait_idle() }.context("wait idle during recreate")?;

        let old_swapchain = self.swapchain;
        // 构建 the new 交换链 第一个 handing off the old one so the
        // 实现 can retire it cleanly (avoids NATIVE_WINDOW_IN_USE).
        let SwapchainOutput {
            format,
            extent,
            pre_transform,
            swapchain,
            images,
            views,
        } = create_swapchain(context, self.surface, old_swapchain, self.present_mode).map_err(
            |e| {
                log::warn!("swapchain recreate failed, keeping old swapchain: {e}");
                e
            },
        )?;

        // Old views and per-image render-finished semaphores go with the old
        // 交换链 构建 replacements sized to the new 图像 集合
        let sem_info = vk::SemaphoreCreateInfo::default();
        let new_render_finished = (0..images.len())
            .map(|_| unsafe { context.device.create_semaphore(&sem_info, None) })
            .collect::<Result<Vec<_>, _>>()
            .context("recreate render_finished semaphores")?;

        // 提交 销毁 old, install new.
        for view in self.views.drain(..) {
            unsafe { context.device.destroy_image_view(view, None) };
        }
        for sem in self.render_finished.drain(..) {
            unsafe { context.device.destroy_semaphore(sem, None) };
        }
        // The old 交换链 was retired by create_swapchain; 销毁 it now.
        unsafe {
            self.swapchain_ext.destroy_swapchain(old_swapchain, None);
        }

        self.format = format;
        self.extent = extent;
        self.pre_transform = pre_transform;
        self.swapchain = swapchain;
        self.images = images;
        self.views = views;
        self.render_finished = new_render_finished;
        Ok(())
    }

    /// Acquire the 下一个 图像 returning `(image_index, 帧 image_available,
    /// render_finished, 围栏
    ///
    /// 同步 follows the vulkan-tutorial 模式 `FRAMES_IN_FLIGHT`
    /// fences (rotated by `current_frame`) pace the CPU vs GPU. We wait on the
    /// 当前 frame's 围栏 before acquiring, so its 命令 缓冲区 is done and
    /// its acquire 信号量 has been consumed by the prior submit. With 3
    /// 交换链 images and 2 frames in flight, at least one 图像 is always
    /// free, so acquire never blocks indefinitely.
    pub fn acquire_next_image(
        &mut self,
        device: &ash::Device,
    ) -> anyhow::Result<(u32, usize, vk::Semaphore, vk::Semaphore, vk::Fence)> {
        let frame = self.current_frame;
        let image_available = self.image_available[frame];
        let fence = self.in_flight_fences[frame];

        // Wait for the 上一个 submission using this frame's 围栏. This ensures
        // the frame's 命令 缓冲区 is no longer in use and its acquire 信号量
        // has been consumed by the prior submit.
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }
            .context("wait for in_flight fence")?;

        // Reset only after acquire succeeds. `ERROR_OUT_OF_DATE_KHR` returns
        // without a submit, so a fence reset beforehand would never be
        // signaled and the next `wait_for_fences(..., u64::MAX)` would hang.
        let (image_index, _sub) = unsafe {
            self.swapchain_ext.acquire_next_image(
                self.swapchain,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        }
        .map_err(|e| match e {
            vk::Result::ERROR_OUT_OF_DATE_KHR => anyhow!("swapchain out of date"),
            _ => anyhow!(e).context("acquire next image"),
        })?;
        unsafe { device.reset_fences(&[fence]) }.context("reset in_flight fence")?;

        let render_finished = self.render_finished[image_index as usize];
        Ok((image_index, frame, image_available, render_finished, fence))
    }

    /// Present the 当前 图像 Returns `Ok(true)` if the 交换链 is
    /// suboptimal/out-of-date and should be recreated.
    pub fn present(
        &mut self,
        queue: vk::Queue,
        image_index: u32,
        render_finished: vk::Semaphore,
    ) -> anyhow::Result<bool> {
        let swapchains = [self.swapchain];
        let image_indices = [image_index];
        let wait_semaphores = [render_finished];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&wait_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);

        let result = unsafe { self.swapchain_ext.queue_present(queue, &present_info) };
        let out_of_date = match result {
            Ok(false) => false,
            Ok(true) => {
                log::debug!("swapchain suboptimal at present");
                true
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => true,
            Err(e) => return Err(anyhow!(e).context("queue present")),
        };

        self.current_frame = (self.current_frame + 1) % FRAMES_IN_FLIGHT;
        Ok(out_of_date)
    }

    /// Tear 下 all swapchain-owned resources. Must be called before the
    /// 设备 is destroyed; the 设备 handle lives in [`VulkanContext`].
    ///
    /// # 安全性
    ///
    /// 设备 must be the same [`ash::Device`] the 交换链 was created
    /// with, and must 已 have been destroyed. After this 调用 the
    /// 交换链 and all its handles are 无效 and must not be used.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe { device.device_wait_idle() }.ok();
        for view in self.views.drain(..) {
            unsafe { device.destroy_image_view(view, None) };
        }
        for sem in self.image_available.drain(..) {
            unsafe { device.destroy_semaphore(sem, None) };
        }
        for sem in self.render_finished.drain(..) {
            unsafe { device.destroy_semaphore(sem, None) };
        }
        for fence in self.in_flight_fences.drain(..) {
            unsafe { device.destroy_fence(fence, None) };
        }
        unsafe { self.swapchain_ext.destroy_swapchain(self.swapchain, None) };
        unsafe {
            self._surface_ext.destroy_surface(self.surface, None);
        }
        self._debug_utils.take();
    }
}

// ---------------------------------------------------------------------------
// 交换链 creation helpers
// ---------------------------------------------------------------------------

/// 结果 of creating or recreating a 交换链
struct SwapchainOutput {
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    pre_transform: vk::SurfaceTransformFlagsKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
}

fn create_swapchain(
    context: &VulkanContext,
    surface: vk::SurfaceKHR,
    old_swapchain: vk::SwapchainKHR,
    present_mode: vk::PresentModeKHR,
) -> anyhow::Result<SwapchainOutput> {
    let surface_ext = ash::khr::surface::Instance::new(&context.entry, &context.instance);

    let capabilities = unsafe {
        surface_ext.get_physical_device_surface_capabilities(context.physical_device, surface)
    }
    .context("get surface capabilities")?;

    // `set_present_mode` is intentionally a lightweight setter and may be
    // called before a surface is recreated (or with a mode unsupported by
    // the current driver). Validate at the Vulkan boundary and fall back to
    // FIFO, which the spec guarantees for every surface.
    let present_modes = unsafe {
        surface_ext.get_physical_device_surface_present_modes(context.physical_device, surface)
    }
    .context("get surface present modes")?;
    let present_mode = if present_modes.contains(&present_mode) {
        present_mode
    } else {
        log::warn!(
            "present mode {:?} is unsupported; falling back to FIFO",
            present_mode
        );
        vk::PresentModeKHR::FIFO
    };

    let formats = unsafe {
        surface_ext.get_physical_device_surface_formats(context.physical_device, surface)
    }
    .context("get surface formats")?;

    let format = choose_surface_format(&formats);
    let extent = choose_extent(&capabilities);
    // Honor the presentation engine's 当前 orientation. On a 横屏 app
    // running on a portrait-native 设备 this is `ROTATE_90`/`ROTATE_270`, so
    // the compositor rotates the 竖屏 交换链 缓冲区 to 横屏
    let pre_transform = capabilities.current_transform;
    let image_count = capabilities.min_image_count + 1;
    let image_count = if capabilities.max_image_count > 0 {
        image_count.min(capabilities.max_image_count)
    } else {
        image_count
    };

    let queue_families = [context.graphics_queue_family];
    let create_info = vk::SwapchainCreateInfoKHR::default()
        .surface(surface)
        .min_image_count(image_count)
        .image_format(format.format)
        .image_color_space(format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .queue_family_indices(&queue_families)
        .pre_transform(pre_transform)
        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
        .present_mode(present_mode)
        .clipped(true)
        .old_swapchain(old_swapchain);

    let swapchain_ext = ash::khr::swapchain::Device::new(&context.instance, &context.device);
    let swapchain = unsafe { swapchain_ext.create_swapchain(&create_info, None) }
        .context("create swapchain")?;

    let images =
        unsafe { swapchain_ext.get_swapchain_images(swapchain) }.context("get swapchain images")?;

    let views = images
        .iter()
        .map(|image| create_image_view(context, *image, format.format))
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(SwapchainOutput {
        format,
        extent,
        pre_transform,
        swapchain,
        images,
        views,
    })
}

fn choose_surface_format(available: &[vk::SurfaceFormatKHR]) -> vk::SurfaceFormatKHR {
    // Prefer sRGB B8G8R8A8 for 颜色 accuracy; fall 后 to the 第一个
    available
        .iter()
        .cloned()
        .find(|f| {
            f.format == vk::Format::B8G8R8A8_SRGB
                && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .unwrap_or_else(|| available[0])
}

/// Pick a present mode from the surface's supported list.
///
/// With vsync enabled, prefer `FIFO` (blocks until vblank, so it actually caps
/// the frame rate) then `MAILBOX` as a tear-free fallback. `MAILBOX` alone is
/// not a vsync cap: the driver may present as fast as it can.
/// With vsync disabled, prefer `IMMEDIATE` (uncapped, may tear), then
/// `MAILBOX`, then `FIFO`. `FIFO` is always supported by the spec.
fn pick_present_mode(modes: &[vk::PresentModeKHR], vsync: bool) -> vk::PresentModeKHR {
    let preferred: &[vk::PresentModeKHR] = if vsync {
        &[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX]
    } else {
        &[
            vk::PresentModeKHR::IMMEDIATE,
            vk::PresentModeKHR::MAILBOX,
            vk::PresentModeKHR::FIFO,
        ]
    };
    preferred
        .iter()
        .copied()
        .find(|mode| modes.contains(mode))
        .unwrap_or(vk::PresentModeKHR::FIFO)
}

/// Prefer `MAILBOX`/`FIFO` when v-synced, `IMMEDIATE` when not, falling
/// back to `FIFO` (always supported).
fn choose_present_mode(
    surface_ext: &ash::khr::surface::Instance,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    vsync: bool,
) -> vk::PresentModeKHR {
    let modes = unsafe {
        surface_ext
            .get_physical_device_surface_present_modes(physical_device, surface)
            .unwrap_or_default()
    };
    let mode = pick_present_mode(&modes, vsync);
    log::info!("swapchain present mode: {mode:?} (vsync={vsync})");
    mode
}

fn choose_extent(caps: &vk::SurfaceCapabilitiesKHR) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        return caps.current_extent;
    }
    // 回退 for some platforms (e.g. some Android configs) that report
    // 0xFFFFFFFF; 限定 a minimal extent to the allowed range.
    vk::Extent2D {
        width: caps
            .min_image_extent
            .width
            .clamp(1, caps.max_image_extent.width),
        height: caps
            .min_image_extent
            .height
            .clamp(1, caps.max_image_extent.height),
    }
}

fn create_image_view(
    context: &VulkanContext,
    image: vk::Image,
    format: vk::Format,
) -> anyhow::Result<vk::ImageView> {
    let components = vk::ComponentMapping {
        r: vk::ComponentSwizzle::IDENTITY,
        g: vk::ComponentSwizzle::IDENTITY,
        b: vk::ComponentSwizzle::IDENTITY,
        a: vk::ComponentSwizzle::IDENTITY,
    };
    let subresource_range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let create_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(components)
        .subresource_range(subresource_range);

    unsafe { context.device.create_image_view(&create_info, None) }
        .map_err(|e| anyhow!(e).context("create image view"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsync_prefers_fifo_then_mailbox() {
        assert_eq!(
            pick_present_mode(
                &[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX],
                true
            ),
            vk::PresentModeKHR::FIFO
        );
        assert_eq!(
            pick_present_mode(&[vk::PresentModeKHR::MAILBOX], true),
            vk::PresentModeKHR::MAILBOX
        );
        assert_eq!(
            pick_present_mode(&[vk::PresentModeKHR::FIFO], true),
            vk::PresentModeKHR::FIFO
        );
    }

    #[test]
    fn no_vsync_prefers_immediate() {
        assert_eq!(
            pick_present_mode(
                &[
                    vk::PresentModeKHR::FIFO,
                    vk::PresentModeKHR::MAILBOX,
                    vk::PresentModeKHR::IMMEDIATE,
                ],
                false
            ),
            vk::PresentModeKHR::IMMEDIATE
        );
    }

    #[test]
    fn no_vsync_falls_back_to_mailbox_then_fifo() {
        assert_eq!(
            pick_present_mode(
                &[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX],
                false
            ),
            vk::PresentModeKHR::MAILBOX
        );
        assert_eq!(
            pick_present_mode(&[vk::PresentModeKHR::FIFO], false),
            vk::PresentModeKHR::FIFO
        );
    }
}
