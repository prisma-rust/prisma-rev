//! Swapchain and per-frame synchronization.
//!
//! Owns the [`VkSurfaceKHR`], the swapchain + its image views, and the
//! synchronization primitives used to pace acquire vs. present.
//!
//! Synchronization model:
//! - `FRAMES_IN_FLIGHT` **acquire semaphores** (`image_available`),
//!   rotated by `current_frame`. An acquire semaphore is only reused once its
//!   frame's fence has been waited on, so it is guaranteed unsignaled.
//! - One **render-finished semaphore per swapchain image**, indexed by
//!   `image_index`. Present always waits on the semaphore that the matching
//!   submit signaled, so a render-finished semaphore is never reused while a
//!   present still holds it -- even when two acquires return the same index.
//! - `FRAMES_IN_FLIGHT` fences for host pacing, rotated by `current_frame`.
//!
//! With 3 swapchain images and 2 frames in flight, at least one image is
//! always free for acquire, so no per-image fence tracking is needed.

use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use ash::vk;

use crate::context::VulkanContext;

/// Maximum frames submitted to the GPU ahead of the host.
pub(crate) const FRAMES_IN_FLIGHT: usize = 2;

/// The swapchain plus the surface it presents to.
pub struct Swapchain {
    pub surface: vk::SurfaceKHR,
    /// Kept so it outlives any surface-destroy calls.
    _surface_ext: ash::khr::surface::Instance,
    _debug_utils: Option<ash::ext::debug_utils::Instance>,

    pub extent: vk::Extent2D,
    pub format: vk::SurfaceFormatKHR,
    /// Transform the presentation engine applies to the swapchain image before
    /// displaying it (e.g. `ROTATE_90` on a landscape app running on a
    /// portrait-native device). Equal to `current_transform` at creation time.
    pub pre_transform: vk::SurfaceTransformFlagsKHR,

    /// Presentation mode used when (re)creating the swapchain. Defaults to
    /// `FIFO` when v-synced (it actually caps the frame rate), changeable via
    /// [`Swapchain::set_present_mode`].
    present_mode: vk::PresentModeKHR,

    swapchain: vk::SwapchainKHR,
    swapchain_ext: ash::khr::swapchain::Device,

    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,

    /// Acquire semaphores, one per frame-in-flight, rotated by `current_frame`.
    image_available: Vec<vk::Semaphore>,
    /// Render-finished semaphores, one per swapchain image (index by image idx).
    render_finished: Vec<vk::Semaphore>,
    /// Host pacing fences, one per frame-in-flight, rotated by `current_frame`.
    in_flight_fences: Vec<vk::Fence>,
    /// Rotating frame index, advanced each present.
    current_frame: usize,
}

impl Swapchain {
    /// Create the surface (from the window) and an initial swapchain.
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

    /// Transform the presentation engine applies to the swapchain image.
    /// Used by the renderer to pre-rotate the view-projection so the final
    /// on-screen image is upright and correctly proportioned.
    pub fn pre_transform(&self) -> vk::SurfaceTransformFlagsKHR {
        self.pre_transform
    }

    /// Current presentation mode.
    pub fn present_mode(&self) -> vk::PresentModeKHR {
        self.present_mode
    }

    /// Change the presentation mode. Takes effect on the next
    /// [`Swapchain::recreate`]. `MAILBOX` reduces latency but may not be
    /// supported everywhere; `FIFO` is always available.
    pub fn set_present_mode(&mut self, mode: vk::PresentModeKHR) {
        self.present_mode = mode;
    }

    /// Recreate the swapchain for a new window size. Waits for the device to
    /// be idle first. Transactional: if creating the new swapchain fails, the
    /// existing one (and its semaphores) are left intact so rendering can
    /// retry later rather than end up with dangling handles.
    pub fn recreate(&mut self, context: &VulkanContext) -> anyhow::Result<()> {
        unsafe { context.device.device_wait_idle() }.context("wait idle during recreate")?;

        let old_swapchain = self.swapchain;
        // Build the new swapchain first, handing off the old one so the
        // implementation can retire it cleanly (avoids NATIVE_WINDOW_IN_USE).
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
        // swapchain; build replacements sized to the new image set.
        let sem_info = vk::SemaphoreCreateInfo::default();
        let new_render_finished = (0..images.len())
            .map(|_| unsafe { context.device.create_semaphore(&sem_info, None) })
            .collect::<Result<Vec<_>, _>>()
            .context("recreate render_finished semaphores")?;

        // Commit: destroy old, install new.
        for view in self.views.drain(..) {
            unsafe { context.device.destroy_image_view(view, None) };
        }
        for sem in self.render_finished.drain(..) {
            unsafe { context.device.destroy_semaphore(sem, None) };
        }
        // The old swapchain was retired by create_swapchain; destroy it now.
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

    /// Acquire the next image, returning `(image_index, frame, image_available,
    /// render_finished, fence)`.
    ///
    /// Synchronization follows the vulkan-tutorial pattern: `FRAMES_IN_FLIGHT`
    /// fences (rotated by `current_frame`) pace the CPU vs GPU. We wait on the
    /// current frame's fence before acquiring, so its command buffer is done and
    /// its acquire semaphore has been consumed by the prior submit. With 3
    /// swapchain images and 2 frames in flight, at least one image is always
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

    /// Present the current image. Returns `Ok(true)` if the swapchain is
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

    /// Tear down all swapchain-owned resources. Must be called before the
    /// device is destroyed; the device handle lives in [`VulkanContext`].
    ///
    /// # Safety
    ///
    /// `device` must be the same [`ash::Device`] the swapchain was created
    /// with, and must 已 have been destroyed. After this 调用 the
    /// swapchain and all its handles are invalid and must not be used.
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
// swapchain creation helpers
// ---------------------------------------------------------------------------

/// Result of creating or recreating a swapchain.
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
    // Honor the presentation engine's current orientation. On a landscape app
    // running on a portrait-native device this is `ROTATE_90`/`ROTATE_270`, so
    // the compositor rotates the (portrait) swapchain buffer to landscape.
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
    // Prefer sRGB B8G8R8A8 for color accuracy; fall back to the first.
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
    // Fallback for some platforms (e.g. some Android configs) that report
    // 0xFFFFFFFF; clamp a minimal extent to the allowed range.
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
