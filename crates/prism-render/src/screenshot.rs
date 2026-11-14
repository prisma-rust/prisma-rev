//! 帧截图：把最后一帧的交换链图像读回 CPU，可选缩放到指定分辨率后写成 PNG。
//!
//! 用法是两段式，因为读回必须发生在帧提交之后：
//!
//! ```ignore
//! renderer.request_screenshot(ScreenshotRequest { path, size: Some([1920, 1080]) });
//! // … 下一帧 execute 时插入 copy，present 提交 …
//! renderer.finish_screenshot(&ctx)?;
//! ```
//!
//! `execute` 在把交换链图像交还给 present 之前插入一次
//! `vkCmdCopyImageToBuffer`（`PRESENT_SRC → TRANSFER_SRC → PRESENT_SRC`），
//! 所以截图拿到的是**屏幕上那一帧**，不是重渲染。
//!
//! 缩放用最近邻：像素画放大后保持硬边，不做插值。

use std::path::PathBuf;

use anyhow::{Context, Result};
use ash::vk;

use crate::buffer::find_memory_type;
use crate::context::VulkanContext;

/// 一次截图请求。
#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    /// PNG 输出路径（父目录必须已存在）。
    pub path: PathBuf,
    /// 目标分辨率；`None` 表示与交换链一致。
    pub size: Option<[u32; 2]>,
    /// 目标缩放倍数（相对交换链分辨率）；`size` 优先。
    pub scale: Option<f32>,
}

/// 把 RGBA8 像素写成 PNG，可选最近邻缩放到目标分辨率。
///
/// 供宿主（编辑器窗口截图、离屏出图）复用：调用方只负责拿到 RGBA8。
pub fn save_rgba_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[u8],
    size: Option<[u32; 2]>,
) -> Result<PathBuf> {
    let expected = (width as usize) * (height as usize) * 4;
    if pixels.len() != expected {
        anyhow::bail!(
            "screenshot: pixel buffer is {} bytes, expected {expected} for {width}x{height}",
            pixels.len()
        );
    }
    let mut image = image::RgbaImage::from_raw(width, height, pixels.to_vec())
        .context("screenshot: build image buffer")?;
    if let Some([target_w, target_h]) = size {
        if target_w != width || target_h != height {
            image = image::imageops::resize(
                &image,
                target_w,
                target_h,
                image::imageops::FilterType::Nearest,
            );
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("screenshot: create {}", parent.display()))?;
        }
    }
    image
        .save(path)
        .with_context(|| format!("screenshot: write {}", path.display()))?;
    log::info!(
        "screenshot: {}x{} -> {}",
        image.width(),
        image.height(),
        path.display()
    );
    Ok(path.to_path_buf())
}

/// 把 BGRA8 就地转成 RGBA8（交换链 / Win32 DIB 都是 BGRA 顺序）。
pub fn bgra_to_rgba_in_place(pixels: &mut [u8]) {
    for texel in pixels.as_chunks_mut::<4>().0 {
        texel.swap(0, 2);
    }
}

/// 一次进行中的截图：host-visible 缓冲区 + 请求。
pub(crate) struct ScreenshotCapture {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    request: ScreenshotRequest,
}

impl ScreenshotCapture {
    /// 按交换链尺寸分配读回缓冲区。
    pub(crate) fn new(
        context: &VulkanContext,
        extent: vk::Extent2D,
        mut request: ScreenshotRequest,
    ) -> Result<Self> {
        // 缩放倍数在这里解析：调用方不需要先知道交换链尺寸。
        if request.size.is_none() {
            if let Some(scale) = request.scale.filter(|s| *s > 0.0) {
                let w = ((extent.width as f32) * scale).round().max(1.0) as u32;
                let h = ((extent.height as f32) * scale).round().max(1.0) as u32;
                request.size = Some([w, h]);
            }
        }
        let size = (extent.width as u64) * (extent.height as u64) * 4;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { context.device.create_buffer(&buffer_info, None) }
            .context("screenshot: create buffer")?;
        let requirements = unsafe { context.device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            context,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .context("screenshot: no host-visible memory type")?;
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        let memory = match unsafe { context.device.allocate_memory(&alloc_info, None) } {
            Ok(memory) => memory,
            Err(err) => {
                unsafe { context.device.destroy_buffer(buffer, None) };
                return Err(err).context("screenshot: allocate memory");
            }
        };
        unsafe { context.device.bind_buffer_memory(buffer, memory, 0) }
            .context("screenshot: bind buffer memory")?;
        Ok(Self {
            buffer,
            memory,
            extent,
            request,
        })
    }

    /// 在命令缓冲区里记录：图像 → 缓冲区 → 恢复原布局。
    ///
    /// 支持窗口模式（`PRESENT_SRC_KHR`）与无窗口离屏模式（`COLOR_ATTACHMENT_OPTIMAL`）。
    pub(crate) fn record_copy(
        &self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        image: vk::Image,
        current_layout: vk::ImageLayout,
    ) -> Result<()> {
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_transfer = vk::ImageMemoryBarrier::default()
            .old_layout(current_layout)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer),
            );
        }

        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width: self.extent.width,
                height: self.extent.height,
                depth: 1,
            });
        unsafe {
            device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                std::slice::from_ref(&region),
            );
        }

        let restore_layout = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(current_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&restore_layout),
            );
        }
        Ok(())
    }

    /// 帧提交完成后读回并写 PNG，返回实际写入的路径。
    pub(crate) fn finish(
        &self,
        context: &VulkanContext,
        swapchain_format: vk::Format,
    ) -> Result<PathBuf> {
        let width = self.extent.width as usize;
        let height = self.extent.height as usize;
        let size = width * height * 4;
        let ptr = unsafe {
            context
                .device
                .map_memory(self.memory, 0, size as u64, vk::MemoryMapFlags::empty())
        }
        .context("screenshot: map readback buffer")?;
        let mut pixels = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(ptr as *const u8, pixels.as_mut_ptr(), size);
            context.device.unmap_memory(self.memory);
        }

        // 交换链常见格式是 B8G8R8A8；PNG 要 RGBA。
        let bgra = matches!(
            swapchain_format,
            vk::Format::B8G8R8A8_SRGB | vk::Format::B8G8R8A8_UNORM
        );
        if bgra {
            for texel in pixels.as_chunks_mut::<4>().0 {
                texel.swap(0, 2);
            }
        }
        // 交换链可能是 sRGB 存储；PNG 按 sRGB 存即可，不需要再做转换。

        let mut image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
            .context("screenshot: build image buffer")?;
        if let Some([target_w, target_h]) = self.request.size {
            if target_w != width as u32 || target_h != height as u32 {
                image = image::imageops::resize(
                    &image,
                    target_w,
                    target_h,
                    image::imageops::FilterType::Nearest,
                );
            }
        }
        image
            .save(&self.request.path)
            .with_context(|| format!("screenshot: write {}", self.request.path.display()))?;
        log::info!(
            "screenshot: {}x{} -> {}",
            image.width(),
            image.height(),
            self.request.path.display()
        );
        Ok(self.request.path.clone())
    }

    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}
