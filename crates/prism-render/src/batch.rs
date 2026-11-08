//! 批量暂存上传器。
//!
//! [`BatchUploader`] 将多次缓冲区/图像复制记录到单个一次性提交的命令缓冲区中，
//! 然后通过一次 `vkQueueSubmit` + 一次 `vkWaitForFences` 刷新。
//! 这取代了场景加载期间 [`crate::buffer::upload_to_buffer`] 和
//! [`crate::buffer::create_and_upload_image`] 使用的逐资源提交等待模式，
//! 该模式下数百次往返（Sponza: ~880）主导了加载时间。
//!
//! Usage:
//! ```ignore
//! let mut uploader = BatchUploader::new(&context, command_pool)?;
//! uploader.upload_buffer(device_buffer, data)?;
//! uploader.upload_image(image, width, height, mip_levels, pixels)?;
//! uploader.finish(graphics_queue)?; // single submit + wait, then cleanup
//! ```
//!
//! The uploader is synchronous (it blocks on `finish`), which keeps the
//! existing single-threaded load path simple. An async/timeline-semaphore
//! variant (like TruvisRenderer's `TextureUploadQueue`) is a follow-up.

use anyhow::{Context as _, Result};
use ash::vk;

use crate::buffer::{
    create_buffer, find_memory_type, mip_chain_byte_size, mip_level_byte_size, BufferUsage,
    MemoryProperties,
};
use crate::context::VulkanContext;
use crate::vulkan_compat::{LegacyCopy2, LegacySync2};

/// A staging resource that must outlive the submitted command buffer and is
/// destroyed together in [`BatchUploader::finish`].
enum Deferred {
    Buffer {
        buffer: vk::Buffer,
        memory: vk::DeviceMemory,
    },
}

/// Records buffer/image staging copies into one command buffer, flusheded
/// with a single submit + fence wait.
pub struct BatchUploader<'a> {
    context: &'a VulkanContext,
    command_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    sync2: LegacySync2<'a>,
    copy2: LegacyCopy2<'a>,
    deferred: Vec<Deferred>,
    started: bool,
}

impl<'a> BatchUploader<'a> {
    /// Allocate + begin a one-time-submit command buffer.
    pub fn new(context: &'a VulkanContext, command_pool: vk::CommandPool) -> Result<Self> {
        let device = &context.device;
        let cmd = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .context("BatchUploader: allocate command buffer")?[0];
        unsafe {
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .context("BatchUploader: begin command buffer")?;

        let sync2 = LegacySync2 {
            device: &context.device,
        };
        let copy2 = LegacyCopy2 {
            device: &context.device,
        };

        Ok(Self {
            context,
            command_pool,
            cmd,
            sync2,
            copy2,
            deferred: Vec::new(),
            started: true,
        })
    }

    /// Copy `data` into the device-local `destination_buffer` via a fresh
    /// staging buffer. The staging buffer is kept alive until [`finish`].
    pub fn upload_buffer(
        &mut self,
        destination_buffer: vk::Buffer,
        size: vk::DeviceSize,
        data: &[u8],
    ) -> Result<()> {
        let device = &self.context.device;
        let (staging, staging_memory) = create_buffer(
            self.context,
            size,
            BufferUsage::TRANSFER_SRC,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("BatchUploader: create staging buffer")?;
        unsafe {
            let ptr = device
                .map_memory(staging_memory, 0, size, vk::MemoryMapFlags::empty())
                .context("BatchUploader: map staging memory")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
            device.unmap_memory(staging_memory);
        }
        let copy_region = vk::BufferCopy2::default().size(size);
        unsafe {
            self.copy2.cmd_copy_buffer2(
                self.cmd,
                &vk::CopyBufferInfo2::default()
                    .src_buffer(staging)
                    .dst_buffer(destination_buffer)
                    .regions(std::slice::from_ref(&copy_region)),
            );
        }
        self.deferred.push(Deferred::Buffer {
            buffer: staging,
            memory: staging_memory,
        });
        Ok(())
    }

    /// Create a device-local 2D image with the given `format`, upload `pixels`
    /// (RGBA8, tightly packed) into mip 0 via a staging buffer, then blit the
    /// mip chain. The image is transitioned to `SHADER_READ_ONLY_OPTIMAL` by
    /// the time `finish` runs. Returns `(image, memory, view)`; the caller
    /// owns them.
    ///
    /// Pass `R8G8B8A8_SRGB` for color textures (albedo/emissive) so the
    /// hardware performs sRGB->linear on sample; `R8G8B8A8_UNORM` for data
    /// textures (normal/metallic-roughness/occlusion).
    ///
    /// `mip_levels` should be `mip_level_count(width, height)` for a full
    /// chain, or `1` to skip mip generation.
    pub fn upload_image(
        &mut self,
        width: u32,
        height: u32,
        mip_levels: u32,
        pixels: &[u8],
        format: vk::Format,
    ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
        let device = &self.context.device;
        let extent = vk::Extent3D {
            width,
            height,
            depth: 1,
        };

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(extent)
            .mip_levels(mip_levels)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let image = unsafe { device.create_image(&image_info, None) }
            .context("BatchUploader: create image")?;
        let mem_reqs = unsafe { device.get_image_memory_requirements(image) };
        let mem_type = find_memory_type(
            self.context,
            mem_reqs.memory_type_bits,
            MemoryProperties::DEVICE_LOCAL,
        )
        .context("BatchUploader: find image memory type")?;
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(mem_reqs.size)
                    .memory_type_index(mem_type),
                None,
            )
        }
        .context("BatchUploader: allocate image memory")?;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("BatchUploader: bind image memory")?;

        // Staging 缓冲区 for the base mip — sized by what the caller provided,
        // since `pixels` may hold a whole baked mip chain (KTX2).
        let size = pixels.len() as vk::DeviceSize;
        let (staging, staging_memory) = create_buffer(
            self.context,
            size,
            BufferUsage::TRANSFER_SRC,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("BatchUploader: create image staging buffer")?;
        unsafe {
            let ptr = device
                .map_memory(staging_memory, 0, size, vk::MemoryMapFlags::empty())
                .context("BatchUploader: map image staging")?;
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr as *mut u8, pixels.len());
            device.unmap_memory(staging_memory);
        }
        self.deferred.push(Deferred::Buffer {
            buffer: staging,
            memory: staging_memory,
        });

        // UNDEFINED -> TRANSFER_DST_OPTIMAL (all mip levels).
        barrier2(
            &self.sync2,
            self.cmd,
            image,
            color_sub(0, mip_levels),
            ImageBarrier {
                old_layout: vk::ImageLayout::UNDEFINED,
                new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                src_stage: vk::PipelineStageFlags2::NONE,
                src_access: vk::AccessFlags2::NONE,
                dst_stage: vk::PipelineStageFlags2::TRANSFER,
                dst_access: vk::AccessFlags2::TRANSFER_WRITE,
            },
        );

        // Copy staging -> mip 0.
        let copy = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(extent);
        unsafe {
            device.cmd_copy_buffer_to_image(
                self.cmd,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
        }

        // Generate mip chain (mirrors buffer.rs but uses the shared cmd).
        //
        // When `pixels` carries a fully baked chain (KTX2), upload the remaining
        // levels straight from staging — baked compressed blocks survive and no
        // blit feature is required. Level 0 was copied above.
        let has_all_mips = mip_levels > 1
            && mip_chain_byte_size(format, width, height, mip_levels)
                .is_some_and(|total| pixels.len() >= total);
        if has_all_mips {
            let mut offset = mip_level_byte_size(format, width, height, 0)
                .context("BatchUploader: unsupported format for mip chain")?
                as u64;
            for level in 1..mip_levels {
                let copy = vk::BufferImageCopy::default()
                    .buffer_offset(offset)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(level)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_extent(mip_extent(width, height, level));
                unsafe {
                    device.cmd_copy_buffer_to_image(
                        self.cmd,
                        staging,
                        image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[copy],
                    );
                }
                offset += mip_level_byte_size(format, width, height, level)
                    .context("BatchUploader: unsupported format for mip chain")?
                    as u64;
            }
            barrier2(
                &self.sync2,
                self.cmd,
                image,
                color_sub(0, mip_levels),
                ImageBarrier {
                    old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    src_stage: vk::PipelineStageFlags2::TRANSFER,
                    src_access: vk::AccessFlags2::TRANSFER_WRITE,
                    dst_stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    dst_access: vk::AccessFlags2::SHADER_READ,
                },
            );
        } else if mip_levels > 1 {
            // Blitting is a format feature, not a given: BC7/ASTC/ETC2 often
            // lack BLIT_SRC/BLIT_DST (notably on mobile), so fail loudly rather
            // than record a command the driver rejects.
            anyhow::ensure!(
                crate::buffer::format_supports_blit(self.context, format),
                "BatchUploader: {format:?} cannot be blitted on this device to generate mips; \
                 provide the full mip chain (KTX2) or upload level 0 only"
            );
            barrier2(
                &self.sync2,
                self.cmd,
                image,
                color_sub(0, 1),
                ImageBarrier {
                    old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    src_stage: vk::PipelineStageFlags2::TRANSFER,
                    src_access: vk::AccessFlags2::TRANSFER_WRITE,
                    dst_stage: vk::PipelineStageFlags2::TRANSFER,
                    dst_access: vk::AccessFlags2::TRANSFER_READ,
                },
            );
            for mip in 1..mip_levels {
                let src_level = mip - 1;
                let src_ext = mip_extent(width, height, src_level);
                let dst_ext = mip_extent(width, height, mip);
                let blit = vk::ImageBlit2::default()
                    .src_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(src_level)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .src_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: src_ext.width as i32,
                            y: src_ext.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(mip)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .dst_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: dst_ext.width as i32,
                            y: dst_ext.height as i32,
                            z: 1,
                        },
                    ]);
                unsafe {
                    self.copy2.cmd_blit_image2(
                        self.cmd,
                        &vk::BlitImageInfo2::default()
                            .src_image(image)
                            .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                            .dst_image(image)
                            .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .regions(std::slice::from_ref(&blit))
                            .filter(vk::Filter::LINEAR),
                    );
                }
                // src level done -> shader read
                barrier2(
                    &self.sync2,
                    self.cmd,
                    image,
                    color_sub(src_level, 1),
                    ImageBarrier {
                        old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        src_stage: vk::PipelineStageFlags2::TRANSFER,
                        src_access: vk::AccessFlags2::TRANSFER_READ,
                        dst_stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
                        dst_access: vk::AccessFlags2::SHADER_READ,
                    },
                );
                if mip + 1 < mip_levels {
                    barrier2(
                        &self.sync2,
                        self.cmd,
                        image,
                        color_sub(mip, 1),
                        ImageBarrier {
                            old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            src_stage: vk::PipelineStageFlags2::TRANSFER,
                            src_access: vk::AccessFlags2::TRANSFER_WRITE,
                            dst_stage: vk::PipelineStageFlags2::TRANSFER,
                            dst_access: vk::AccessFlags2::TRANSFER_READ,
                        },
                    );
                }
            }
            // final level -> shader read
            barrier2(
                &self.sync2,
                self.cmd,
                image,
                color_sub(mip_levels - 1, 1),
                ImageBarrier {
                    old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    src_stage: vk::PipelineStageFlags2::TRANSFER,
                    src_access: vk::AccessFlags2::TRANSFER_WRITE,
                    dst_stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    dst_access: vk::AccessFlags2::SHADER_READ,
                },
            );
        } else if mip_levels == 1 {
            // single mip: dst -> 着色器 读取 (the baked-chain path above already
            // transitioned every level, so it must not fall through to here)
            barrier2(
                &self.sync2,
                self.cmd,
                image,
                color_sub(0, 1),
                ImageBarrier {
                    old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    src_stage: vk::PipelineStageFlags2::TRANSFER,
                    src_access: vk::AccessFlags2::TRANSFER_WRITE,
                    dst_stage: vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    dst_access: vk::AccessFlags2::SHADER_READ,
                },
            );
        }

        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(mip_levels)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .context("BatchUploader: create image view")?;
        Ok((image, memory, view))
    }

    /// End the command buffer, submit it once with a dedicated fence, wait
    /// for completion, then destroy every staging resource. After this the
    /// uploader is consumed and cannot record more copies.
    pub fn finish(mut self, graphics_queue: vk::Queue) -> Result<()> {
        let device = &self.context.device;
        unsafe { device.end_command_buffer(self.cmd) }
            .context("BatchUploader: end command buffer")?;
        self.started = false;

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .context("BatchUploader: create fence")?;
        let cmd_bufs = [self.cmd];
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_bufs);
        unsafe { device.queue_submit(graphics_queue, &[submit_info], fence) }
            .context("BatchUploader: submit")?;
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }
            .context("BatchUploader: wait for fence")?;
        unsafe { device.destroy_fence(fence, None) };
        unsafe { device.free_command_buffers(self.command_pool, &[self.cmd]) };

        // Now that the GPU is done, all staging buffers can be released.
        for d in self.deferred.drain(..) {
            match d {
                Deferred::Buffer { buffer, memory } => unsafe {
                    device.destroy_buffer(buffer, None);
                    device.free_memory(memory, None);
                },
            }
        }
        Ok(())
    }
}

impl<'a> Drop for BatchUploader<'a> {
    fn drop(&mut self) {
        // Safety: if `finish` was called, `started` is false and the command
        // buffer has already been freed. If the uploader was dropped without
        // finishing (early return on error), we must release the command
        // buffer + staging resources ourselves so nothing leaks.
        if self.started {
            let device = &self.context.device;
            unsafe {
                let _ = device.end_command_buffer(self.cmd);
                device.free_command_buffers(self.command_pool, &[self.cmd]);
            }
        }
        let device = &self.context.device;
        for d in self.deferred.drain(..) {
            match d {
                Deferred::Buffer { buffer, memory } => unsafe {
                    device.destroy_buffer(buffer, None);
                    device.free_memory(memory, None);
                },
            }
        }
    }
}

/// Compute the number of mip levels for a 2D texture of the given size.
/// Matches `buffer::create_and_upload_image` and `ibl::mip_extent`.
pub fn mip_level_count(width: u32, height: u32) -> u32 {
    if width <= 1 || height <= 1 {
        1
    } else {
        (width.max(height) as f32).log2().floor() as u32 + 1
    }
}

/// Mip extent for a given level (each dimension halved, min 1).
fn mip_extent(width: u32, height: u32, level: u32) -> vk::Extent3D {
    vk::Extent3D {
        width: (width >> level).max(1),
        height: (height >> level).max(1),
        depth: 1,
    }
}

/// A full image-layout transition described as sync + layout endpoints.
struct ImageBarrier {
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
}

/// Build a single-layer COLOR `ImageSubresourceRange` over `level_count` mips
/// starting at `base_mip`, used by [`barrier2`].
fn color_sub(base_mip: u32, level_count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(base_mip)
        .level_count(level_count)
        .layer_count(1)
}

/// Record a synchronization2 image memory barrier on `cmd`. Helper used by
/// the mip-generation blit loop in [`BatchUploader::upload_image`].
fn barrier2(
    sync2: &LegacySync2<'_>,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    subresource: vk::ImageSubresourceRange,
    b: ImageBarrier,
) {
    let barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(b.src_stage)
        .src_access_mask(b.src_access)
        .dst_stage_mask(b.dst_stage)
        .dst_access_mask(b.dst_access)
        .old_layout(b.old_layout)
        .new_layout(b.new_layout)
        .image(image)
        .subresource_range(subresource);
    unsafe {
        sync2.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[barrier]),
        );
    }
}
