//! Scene-level GPU resources with lifetime independent of the swapchain.
//!
//! [`SceneScope`] owns the probe-volume GI resources (3D texture + info UBO +
//! descriptor set). These survive swapchain recreation and are only rebuilt
//! when the scene/level changes (`recreate_probe_volume`).
//!
//! Design: mirrors the IBL pattern — GraphRenderer holds `SceneScope`, borrows
//! its 描述符 集合 + 布局 into `ForwardPass` 集合 5).

use anyhow::Context as _;
use anyhow::Result;
use ash::vk;
use std::sync::Arc;

use crate::context::VulkanContext;
use crate::shader_bindings::scene_frag;

/// Scene-level probe-volume GI resources (set 5).
///
/// Lifetime: created once per scene load; survives swapchain recreation.
/// Destroyed explicitly via [`SceneScope::destroy`] or on `Drop`.
pub struct SceneScope {
    device: ash::Device,
    context: Arc<VulkanContext>,
    // ---- set 5 descriptor resources ----
    /// Descriptor set layout (binding 0: SAMPLED_IMAGE, binding 1: UBO).
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    /// The single descriptor set written with the probe volume + info UBO.
    pub descriptor_set: vk::DescriptorSet,
    descriptor_pool: vk::DescriptorPool,
    // ---- probe volume 3D texture ----
    volume_image: vk::Image,
    volume_view: vk::ImageView,
    volume_memory: vk::DeviceMemory,
    // ---- ProbeVolumeInfo UBO (48 bytes) ----
    info_buffer: vk::Buffer,
    info_memory: vk::DeviceMemory,
}

impl SceneScope {
    /// Create the scene-scope GI resources with a synthetic analytical sky SH
    /// field (Phase A placeholder). Phase E replaces with real baked data via
    /// [`SceneScope::recreate_probe_volume`].
    pub fn new(context: Arc<VulkanContext>) -> Result<Self> {
        let device = context.device.clone();

        // ---- Descriptor set layout (set 5) ----
        let gi_bindings = [
            // binding 0: probe volume 3D texture (SAMPLED_IMAGE, no sampler).
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::PROBE_VOLUME_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            // binding 1: ProbeVolumeInfo UBO (48 bytes).
            vk::DescriptorSetLayoutBinding::default()
                .binding(scene_frag::PROBE_INFO_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let descriptor_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&gi_bindings),
                None,
            )
        }
        .context("SceneScope: create set5 (GI) ds layout")?;

        let descriptor_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::SAMPLED_IMAGE,
                            descriptor_count: 1,
                        },
                        vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::UNIFORM_BUFFER,
                            descriptor_count: 1,
                        },
                    ]),
                None,
            )
        }
        .context("SceneScope: create set5 (GI) ds pool")?;

        let descriptor_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&descriptor_set_layout)),
            )
        }
        .context("SceneScope: allocate set5 (GI) ds")?[0];

        let mut scope = Self {
            device,
            context,
            descriptor_set_layout,
            descriptor_set,
            descriptor_pool,
            volume_image: vk::Image::null(),
            volume_view: vk::ImageView::null(),
            volume_memory: vk::DeviceMemory::null(),
            info_buffer: vk::Buffer::null(),
            info_memory: vk::DeviceMemory::null(),
        };

        // Populate with synthetic sky SH field (Phase A default).
        scope.upload_synthetic_probe_volume()?;

        Ok(scope)
    }

    /// Rebuild the probe volume from external data (Phase C/E: loaded from
    /// `.bin` via prism-asset). Destroys prior volume resources first.
    ///
    /// `pixels` is RGBA32F texel data laid out as
    /// `dims[0] x dims[1] x (dims[2]*9)` (coefficient-major depth slices).
    pub fn recreate_probe_volume(
        &mut self,
        info: &crate::gi::ProbeVolumeInfo,
        dims: [u32; 3],
        pixels: &[f32],
    ) -> Result<()> {
        self.destroy_volume_resources();
        self.upload_probe_volume(info, dims, pixels)
    }

    /// Upload probe volume from a `ProbeVolumeData` (loaded via
    /// [`crate::probe_loader`]). Converts the per-probe coefficient layout into
    /// the 3D texture's coefficient-major depth-slice layout and calls
    /// `recreate_probe_volume`.
    pub fn from_probe_data(&mut self, data: &crate::probe_loader::ProbeVolumeData) -> Result<()> {
        anyhow::ensure!(
            data.is_valid(),
            "SceneScope::from_probe_data: invalid ProbeVolumeData (dims={:?}, coeffs.len()={})",
            data.dims,
            data.coeffs.len()
        );
        let info = crate::gi::ProbeVolumeInfo::new(data.origin, data.spacing, data.dims);
        let pixels = Self::probe_data_to_pixels(data);
        self.recreate_probe_volume(&info, data.dims, &pixels)
    }

    /// Convert `ProbeVolumeData` (per-probe coefficient array) into the 3D
    /// texture pixel layout: `dims[0] x dims[1] x (dims[2]*9)` RGBA32F.
    /// Coefficient c occupies depth slice `[c*dims.z, (c+1)*dims.z)`.
    fn probe_data_to_pixels(data: &crate::probe_loader::ProbeVolumeData) -> Vec<f32> {
        let dx = data.dims[0] as usize;
        let dy = data.dims[1] as usize;
        let dz = data.dims[2] as usize;
        let mut pixels = vec![0.0f32; dx * dy * (dz * 9) * 4];

        for z in 0..dz {
            for y in 0..dy {
                for x in 0..dx {
                    let probe_idx = x + y * dx + z * dx * dy;
                    for coeff in 0..9 {
                        let c = data.coeffs[probe_idx * 9 + coeff];
                        let tex_z = coeff * dz + z;
                        let texel_idx = (tex_z * dy * dx) + y * dx + x;
                        let base = texel_idx * 4;
                        pixels[base] = c[0];
                        pixels[base + 1] = c[1];
                        pixels[base + 2] = c[2];
                        pixels[base + 3] = 0.0; // alpha unused
                    }
                }
            }
        }
        pixels
    }

    // -------------------------------------------------------------------
    // Internal: upload helpers
    // -------------------------------------------------------------------

    /// Upload synthetic analytical sky-hemisphere SH field (Phase A).
    fn upload_synthetic_probe_volume(&mut self) -> Result<()> {
        let info = crate::gi::ProbeVolumeInfo::new(
            [-6.0, 0.0, -6.0], // origin
            [3.0, 3.0, 3.0],   // spacing
            [5, 4, 5],         // dims
        );
        let dims = [5u32, 4, 5];
        let pixels = Self::generate_synthetic_sh_field([5, 4, 5]);
        self.upload_probe_volume(&info, dims, &pixels)
    }

    /// Core upload path: create 3D texture + info UBO + write descriptors.
    fn upload_probe_volume(
        &mut self,
        info: &crate::gi::ProbeVolumeInfo,
        dims: [u32; 3],
        pixels: &[f32],
    ) -> Result<()> {
        let device = &self.device;
        let context = &self.context;

        let tex_w = dims[0];
        let tex_h = dims[1];
        let tex_d = dims[2] * 9; // 9 coefficient layers

        // ---- Create 3D image (RGBA32SFLOAT, device-local) ----
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_3D)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .extent(vk::Extent3D {
                width: tex_w,
                height: tex_h,
                depth: tex_d,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let volume_image = unsafe { device.create_image(&image_info, None) }
            .context("SceneScope: create GI probe volume 3D image")?;
        let mem_reqs = unsafe { device.get_image_memory_requirements(volume_image) };
        let mem_type = crate::buffer::find_memory_type(
            context,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("SceneScope: find device-local memory for GI volume")?;
        let volume_memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(mem_reqs.size)
                    .memory_type_index(mem_type),
                None,
            )
        }
        .context("SceneScope: allocate GI volume memory")?;
        unsafe { device.bind_image_memory(volume_image, volume_memory, 0) }
            .context("SceneScope: bind GI volume memory")?;

        // ---- Upload via staging buffer ----
        let data_bytes: &[u8] = bytemuck::cast_slice(pixels);
        let staging_size = data_bytes.len() as vk::DeviceSize;
        let (staging_buf, staging_mem) = crate::buffer::create_buffer(
            context,
            staging_size,
            crate::buffer::BufferUsage::TRANSFER_SRC,
            crate::buffer::MemoryProperties::HOST_VISIBLE
                | crate::buffer::MemoryProperties::HOST_COHERENT,
        )
        .context("SceneScope: create GI staging buffer")?;
        unsafe {
            let ptr = device
                .map_memory(staging_mem, 0, staging_size, vk::MemoryMapFlags::empty())
                .context("SceneScope: map GI staging")?;
            std::ptr::copy_nonoverlapping(data_bytes.as_ptr(), ptr as *mut u8, data_bytes.len());
            device.unmap_memory(staging_mem);
        }

        // One-shot command buffer: layout transition + copy.
        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(context.graphics_queue_family)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )
        }
        .context("SceneScope: create GI upload cmd pool")?;
        let cmd_buf = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        unsafe {
            device.begin_command_buffer(
                cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            // UNDEFINED -> TRANSFER_DST_OPTIMAL
            device.cmd_pipeline_barrier(
                cmd_buf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(volume_image)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })],
            );
            // Copy staging -> 3D image.
            device.cmd_copy_buffer_to_image(
                cmd_buf,
                staging_buf,
                volume_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
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
                    .image_extent(vk::Extent3D {
                        width: tex_w,
                        height: tex_h,
                        depth: tex_d,
                    })],
            );
            // TRANSFER_DST -> SHADER_READ_ONLY_OPTIMAL
            device.cmd_pipeline_barrier(
                cmd_buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(volume_image)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })],
            );
            device.end_command_buffer(cmd_buf)?;
        }
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd_buf));
        let fence = unsafe {
            device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .context("SceneScope: create upload fence")?
        };
        unsafe {
            device
                .queue_submit(context.graphics_queue, std::slice::from_ref(&submit), fence)
                .context("SceneScope: submit probe volume upload")?;
            device
                .wait_for_fences(&[fence], true, u64::MAX)
                .context("SceneScope: wait for upload fence")?;
            device.destroy_fence(fence, None);
        }
        unsafe {
            device.free_command_buffers(cmd_pool, std::slice::from_ref(&cmd_buf));
            device.destroy_command_pool(cmd_pool, None);
            device.destroy_buffer(staging_buf, None);
            device.free_memory(staging_mem, None);
        }

        // ---- Image view (3D, single mip/layer) ----
        let volume_view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(volume_image)
                    .view_type(vk::ImageViewType::TYPE_3D)
                    .format(vk::Format::R32G32B32A32_SFLOAT)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        }
        .context("SceneScope: create GI volume image view")?;

        // ---- ProbeVolumeInfo UBO (48 bytes, host-visible) ----
        let info_size = std::mem::size_of::<crate::gi::ProbeVolumeInfo>() as vk::DeviceSize;
        let (info_buffer, info_memory) = crate::buffer::create_buffer(
            context,
            info_size,
            crate::buffer::BufferUsage::UNIFORM_BUFFER,
            crate::buffer::MemoryProperties::HOST_VISIBLE
                | crate::buffer::MemoryProperties::HOST_COHERENT,
        )
        .context("SceneScope: create GI info UBO")?;
        unsafe {
            let ptr = device
                .map_memory(info_memory, 0, info_size, vk::MemoryMapFlags::empty())
                .context("SceneScope: map GI info UBO")?;
            let bytes = bytemuck::bytes_of(info);
            debug_assert_eq!(bytes.len() as vk::DeviceSize, info_size);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            device.unmap_memory(info_memory);
        }

        // ---- Write descriptor set ----
        let img_info = vk::DescriptorImageInfo::default()
            .image_view(volume_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let buf_info = vk::DescriptorBufferInfo::default()
            .buffer(info_buffer)
            .offset(0)
            .range(info_size);
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_set)
                .dst_binding(scene_frag::PROBE_VOLUME_BINDING)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(std::slice::from_ref(&img_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_set)
                .dst_binding(scene_frag::PROBE_INFO_BINDING)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&buf_info)),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        self.volume_image = volume_image;
        self.volume_view = volume_view;
        self.volume_memory = volume_memory;
        self.info_buffer = info_buffer;
        self.info_memory = info_memory;

        log::info!(
            "SceneScope: GI probe volume created ({}x{}x{}, {} coeff layers)",
            dims[0],
            dims[1],
            dims[2],
            dims[2] * 9,
        );
        Ok(())
    }

    /// Generate a synthetic analytical sky-hemisphere SH probe field.
    /// All probes share the same SH (position-independent sky dome) so the
    /// visual result is a uniform indirect diffuse from above. Used in Phase A
    /// to validate the sampling pipeline before real baked data is available.
    fn generate_synthetic_sh_field(dims: [usize; 3]) -> Vec<f32> {
        use crate::gi::{sh_basis, SH_COEFF_COUNT};
        const N: usize = 64; // samples per probe (uniform sphere)
        let total_probes = dims[0] * dims[1] * dims[2];
        // RGBA pixels: (dims[0]) x (dims[1]) x (dims[2]*9)
        let mut pixels = vec![0.0f32; dims[0] * dims[1] * (dims[2] * 9) * 4];

        // Precompute SH projection of a sky hemisphere (same for all probes).
        let mut sh = [[0.0f32; 3]; SH_COEFF_COUNT];
        for si in 0..N {
            // Fibonacci sphere for uniform direction distribution.
            let y = 1.0 - 2.0 * (si as f32 + 0.5) / N as f32;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let phi = si as f32 * 2.399963; // golden angle
            let d = [r * phi.cos(), y, r * phi.sin()];
            // Radiance: sky (upper hemisphere) + ground (lower).
            let radiance = if d[1] > 0.0 {
                [0.35f32, 0.55, 0.95] // sky blue
            } else {
                [0.12f32, 0.10, 0.08] // ground brown
            };
            let basis = sh_basis(d);
            // Monte Carlo SH projection with cosine weighting:
            //   c_l = (4*pi/N) * sum_i L(d_i) * max(d_i.y, 0) * B_l(d_i)
            let cos_w = d[1].max(0.0); // surface normal = up
            let w = 4.0 * std::f32::consts::PI / N as f32 * cos_w;
            for c in 0..SH_COEFF_COUNT {
                sh[c][0] += radiance[0] * basis[c] * w;
                sh[c][1] += radiance[1] * basis[c] * w;
                sh[c][2] += radiance[2] * basis[c] * w;
            }
        }

        // Fill all probes with the same SH (position-independent).
        let dz = dims[2];
        for probe_idx in 0..total_probes {
            let pi = probe_idx % dims[0];
            let pj = (probe_idx / dims[0]) % dims[1];
            let pk = probe_idx / (dims[0] * dims[1]);
            for (coeff, shc) in sh.iter().enumerate() {
                let z = coeff * dz + pk;
                let texel_idx = (z * dims[1] * dims[0]) + pj * dims[0] + pi;
                let base = texel_idx * 4;
                pixels[base] = shc[0];
                pixels[base + 1] = shc[1];
                pixels[base + 2] = shc[2];
                pixels[base + 3] = 0.0; // alpha unused
            }
        }
        pixels
    }

    // -------------------------------------------------------------------
    // Teardown
    // -------------------------------------------------------------------

    /// Destroy only the volume + UBO resources (not the descriptor layout/pool).
    /// Called before re-uploading a new probe volume.
    fn destroy_volume_resources(&mut self) {
        let device = &self.device;
        if self.volume_view != vk::ImageView::null() {
            unsafe { device.destroy_image_view(self.volume_view, None) };
            self.volume_view = vk::ImageView::null();
        }
        if self.volume_image != vk::Image::null() {
            unsafe { device.destroy_image(self.volume_image, None) };
            self.volume_image = vk::Image::null();
        }
        if self.volume_memory != vk::DeviceMemory::null() {
            unsafe { device.free_memory(self.volume_memory, None) };
            self.volume_memory = vk::DeviceMemory::null();
        }
        if self.info_buffer != vk::Buffer::null() {
            unsafe { device.destroy_buffer(self.info_buffer, None) };
            self.info_buffer = vk::Buffer::null();
        }
        if self.info_memory != vk::DeviceMemory::null() {
            unsafe { device.free_memory(self.info_memory, None) };
            self.info_memory = vk::DeviceMemory::null();
        }
    }

    /// Destroy ALL GPU resources owned by this SceneScope.
    /// Caller must ensure `device_wait_idle` has been called.
    pub fn destroy(&mut self) {
        self.destroy_volume_resources();
        if self.descriptor_pool != vk::DescriptorPool::null() {
            unsafe {
                self.device
                    .destroy_descriptor_pool(self.descriptor_pool, None)
            };
            self.descriptor_pool = vk::DescriptorPool::null();
        }
        if self.descriptor_set_layout != vk::DescriptorSetLayout::null() {
            unsafe {
                self.device
                    .destroy_descriptor_set_layout(self.descriptor_set_layout, None)
            };
            self.descriptor_set_layout = vk::DescriptorSetLayout::null();
        }
        self.descriptor_set = vk::DescriptorSet::null();
    }
}

impl Drop for SceneScope {
    fn drop(&mut self) {
        self.destroy();
    }
}
