//! Screen-space ReSTIR GI pass (raster path, independent of path tracing).
//!
//! [`RestirGiPass`] reads the forward G-buffer (depth, view-space normal,
//! albedo + perceptual roughness, motion vectors), traces secondary rays with
//! `VK_KHR_ray_query`, reuses reservoirs temporally and spatially, and
//! terminates rays in the SHaRC cache once it is warm. Nothing here touches
//! [`crate::pt_pass`]: the pass builds its own bottom/top-level acceleration
//! structures from the draw list, exactly like the offline GI baker does.
//!
//! The output is a half-resolution RGBA16F indirect-diffuse buffer kept in
//! `VK_IMAGE_LAYOUT_GENERAL` (storage writes plus sampled reads). The forward
//! pass samples the previous frame's buffer (1-frame latency, the same
//! contract GTAO uses) and applies the albedo demodulation itself.

use anyhow::{Context as _, Result};
use ash::vk;
use std::ffi::CStr;

use crate::bake_common::{build_pt_scene, PtGeometryInstance, PtScene};
use crate::buffer::create_buffer;
use crate::compute::ComputePipeline;
use crate::context::VulkanContext;
use crate::math::mat_inverse;
use crate::render_graph::{
    GiMode, GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderMode,
    RenderPassNode, RenderSettings, ResourceHandle, ResourceType, ResourceUsage, FORWARD_ALBEDO_H,
    FORWARD_DEPTH_H, FORWARD_NORMAL_H, FORWARD_VELOCITY_H, RESTIR_GI_COLOR_H,
};
use crate::shader;
use crate::shader_bindings::restir_gi as bindings;

/// Bytes per reservoir element (must match `RestirGiReservoir` in restir_gi.slang).
pub const RESERVOIR_BYTES: vk::DeviceSize = 64;

/// GPU-side reservoir. Mirrors `RestirGiReservoir` in restir_gi.slang.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RestirGiReservoir {
    /// xyz = sampled unit direction, w = age in frames.
    pub direction: [f32; 4],
    /// rgb = radiance estimate, w = RIS target pdf.
    pub radiance: [f32; 4],
    /// x = M (sample count), y = W (weight), z = hit distance, w unused.
    pub stats: [f32; 4],
    /// xyz = secondary hit position (used for spatial validation).
    pub hit_pos: [f32; 4],
}

/// Per-frame constants. Mirrors `RestirGiFrame` in restir_gi.slang (336 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RestirGiFrame {
    pub inv_view_proj: [[f32; 4]; 4],
    pub inv_view_rot: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub light_dir: [f32; 4],
    pub light_color: [f32; 4],
    /// xy = GI extent, zw = full render extent.
    pub viewport: [f32; 4],
    /// x = frame index, y = reset flag, z = rays/pixel, w = max history.
    pub params: [f32; 4],
    /// x = spatial samples, y = max secondary distance, z/w = reuse toggles.
    pub gi_params: [f32; 4],
    /// x = SHaRC scene scale, y = capacity, z = flags, w = roughness threshold.
    pub sharc_params: [f32; 4],
    pub sharc_camera_pos: [f32; 4],
    pub prev_view_proj: [[f32; 4]; 4],
    pub prev_camera_pos: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<RestirGiFrame>() == 336);
const _: () =
    assert!(std::mem::size_of::<RestirGiReservoir>() as vk::DeviceSize == RESERVOIR_BYTES);

/// SHaRC query flag (bit 0), mirrors `GI_SHARC_FLAG_QUERY`.
pub const SHARC_FLAG_QUERY: f32 = 1.0;
/// SHaRC update flag (bit 1), mirrors `GI_SHARC_FLAG_UPDATE`.
pub const SHARC_FLAG_UPDATE: f32 = 2.0;

/// One RGBA16F ping-pong target kept in `VK_IMAGE_LAYOUT_GENERAL`.
struct GiImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// False until the first UNDEFINED -> GENERAL transition was recorded.
    initialized: bool,
}

impl GiImage {
    fn new(context: &VulkanContext, extent: vk::Extent2D) -> Result<Self> {
        let device = &context.device;
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R16G16B16A16_SFLOAT)
                    .extent(vk::Extent3D {
                        width: extent.width.max(1),
                        height: extent.height.max(1),
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .context("RestirGiPass: create GI image")?;
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let type_index = crate::render_pass::find_memory_type(
            &context.physical_device_memory_properties,
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("RestirGiPass: no DEVICE_LOCAL memory for GI image")?;
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("RestirGiPass: allocate GI image memory")?;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("RestirGiPass: bind GI image memory")?;
        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R16G16B16A16_SFLOAT)
                    .subresource_range(image_range()),
                None,
            )
        }
        .context("RestirGiPass: create GI image view")?;
        Ok(Self {
            image,
            memory,
            view,
            initialized: false,
        })
    }

    unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Host-visible frame constant buffer plus its persistent mapping.
struct FrameConstants {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    size: vk::DeviceSize,
}

impl FrameConstants {
    fn new(context: &VulkanContext) -> Result<Self> {
        let device = &context.device;
        let size = std::mem::size_of::<RestirGiFrame>() as vk::DeviceSize;
        let (buffer, memory) = create_buffer(
            context,
            size,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .context("RestirGiPass: create frame constants buffer")?;
        let mapped = unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
            .context("RestirGiPass: map frame constants")? as *mut u8;
        Ok(Self {
            buffer,
            memory,
            mapped,
            size,
        })
    }

    fn upload(&self, frame: &RestirGiFrame) {
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytemuck::bytes_of(frame).as_ptr(),
                self.mapped,
                self.size as usize,
            );
        }
    }

    unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.unmap_memory(self.memory);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Reservoir ping-pong buffers (64 bytes per GI pixel each).
struct ReservoirBuffers {
    buffers: [vk::Buffer; 2],
    memories: [vk::DeviceMemory; 2],
}

/// Number of frames the pass keeps in flight (matches the GTAO descriptor set
/// count so the G-buffer set for frame N never disturbs frame N-1).
const FRAMES_IN_FLIGHT: usize = 2;

/// Screen-space ReSTIR GI pass.
pub struct RestirGiPass {
    /// Full render extent (the GI pass runs at `restir_gi_scale` of this).
    extent: vk::Extent2D,
    gi_extent: vk::Extent2D,
    images: [Option<GiImage>; 2],
    /// Index of the image written this frame.
    write_index: usize,
    /// The view the forward pass should sample (last frame's output).
    last_written: vk::ImageView,
    reservoirs: Option<ReservoirBuffers>,
    reservoirs_dirty: bool,
    frame_constants: Option<FrameConstants>,
    scene: Option<PtScene>,
    materials_buffer: vk::Buffer,
    sharc_hash: Option<vk::Buffer>,
    sharc_accumulation: Option<vk::Buffer>,
    sharc_resolved: Option<vk::Buffer>,
    sharc_capacity: u32,
    bindless_set: vk::DescriptorSet,
    bindless_layout: vk::DescriptorSetLayout,
    ibl_set: vk::DescriptorSet,
    ibl_layout: vk::DescriptorSetLayout,
    ds0_layout: Option<vk::DescriptorSetLayout>,
    ds0_pool: Option<vk::DescriptorPool>,
    ds0: vk::DescriptorSet,
    gb_layout: Option<vk::DescriptorSetLayout>,
    gb_pool: Option<vk::DescriptorPool>,
    gb_sets: Vec<vk::DescriptorSet>,
    gb_views: Vec<[vk::ImageView; 5]>,
    sampler: vk::Sampler,
    pipeline: Option<ComputePipeline>,
    out_h: ResourceHandle,
    /// Unjittered view-projection of the previous dispatched frame.
    prev_view_proj: [[f32; 4]; 4],
    /// Camera position of the previous dispatched frame.
    prev_camera_pos: [f32; 4],
    frame_index: u64,
    reset_pending: bool,
    device: Option<ash::Device>,
}

impl Default for RestirGiPass {
    fn default() -> Self {
        Self::new()
    }
}

impl RestirGiPass {
    /// Create an uninitialised pass; GPU resources are built lazily in `execute`.
    pub fn new() -> Self {
        Self {
            extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            gi_extent: vk::Extent2D {
                width: 0,
                height: 0,
            },
            images: [None, None],
            write_index: 0,
            last_written: vk::ImageView::null(),
            reservoirs: None,
            reservoirs_dirty: true,
            frame_constants: None,
            scene: None,
            materials_buffer: vk::Buffer::null(),
            sharc_hash: None,
            sharc_accumulation: None,
            sharc_resolved: None,
            sharc_capacity: 0,
            bindless_set: vk::DescriptorSet::null(),
            bindless_layout: vk::DescriptorSetLayout::null(),
            ibl_set: vk::DescriptorSet::null(),
            ibl_layout: vk::DescriptorSetLayout::null(),
            ds0_layout: None,
            ds0_pool: None,
            ds0: vk::DescriptorSet::null(),
            gb_layout: None,
            gb_pool: None,
            gb_sets: Vec::new(),
            gb_views: Vec::new(),
            sampler: vk::Sampler::null(),
            pipeline: None,
            out_h: ResourceHandle::INVALID,
            prev_view_proj: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            prev_camera_pos: [0.0; 4],
            frame_index: 0,
            reset_pending: true,
            device: None,
        }
    }

    /// Borrow the bindless texture table and IBL descriptor sets (owned elsewhere).
    pub fn set_resources(
        &mut self,
        bindless_set: vk::DescriptorSet,
        bindless_layout: vk::DescriptorSetLayout,
        ibl_set: vk::DescriptorSet,
        ibl_layout: vk::DescriptorSetLayout,
        materials_buffer: vk::Buffer,
    ) {
        self.bindless_set = bindless_set;
        self.bindless_layout = bindless_layout;
        self.ibl_set = ibl_set;
        self.ibl_layout = ibl_layout;
        self.materials_buffer = materials_buffer;
        self.pipeline = None;
    }

    /// Bind the SHaRC cache buffers so rays can terminate in the cache.
    pub fn set_sharc_buffers(
        &mut self,
        hash: Option<vk::Buffer>,
        accumulation: Option<vk::Buffer>,
        resolved: Option<vk::Buffer>,
        capacity: u32,
    ) {
        self.sharc_hash = hash;
        self.sharc_accumulation = accumulation;
        self.sharc_resolved = resolved;
        self.sharc_capacity = capacity;
    }

    /// True when the pass already holds a ray-tracing scene for the draw list.
    pub fn has_geometry(&self) -> bool {
        self.scene.as_ref().is_some_and(|s| s.tlas.is_some())
    }

    /// Drop the cached acceleration structures so the next frame rebuilds them.
    pub fn invalidate_geometry(&mut self) {
        if let Some(mut scene) = self.scene.take() {
            if let Some(device) = scene.device.take() {
                scene.destroy(&device);
            }
        }
    }

    /// The GI view the forward pass samples (previous frame; may be null).
    pub fn gi_view(&self) -> vk::ImageView {
        self.last_written
    }

    /// Force a history reset (camera cut, teleport, scene edit).
    pub fn reset_history(&mut self) {
        self.reset_pending = true;
    }

    /// Number of dispatches recorded so far (diagnostics).
    pub fn frame_count(&self) -> u64 {
        self.frame_index
    }

    /// Rebuild the acceleration structures from the current draw list.
    pub fn set_geometry(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        instances: &[PtGeometryInstance],
    ) -> Result<()> {
        self.invalidate_geometry();
        if instances.is_empty() {
            return Ok(());
        }
        // The real material table lives in the shared manager SSBO bound to
        // b7; only the per-instance vertex/index data comes from here.
        let materials = vec![0u8; std::mem::size_of::<crate::managers::GpuMaterial>()];
        let scene = build_pt_scene(context, command_pool, instances, &materials)
            .context("RestirGiPass: build ray-tracing scene")?;
        self.scene = Some(scene);
        self.reset_pending = true;
        Ok(())
    }

    /// Ensure the ping-pong images and reservoir buffers match `extent`.
    fn ensure_targets(
        &mut self,
        context: &VulkanContext,
        extent: vk::Extent2D,
        scale: f32,
    ) -> Result<()> {
        let gi_extent = vk::Extent2D {
            width: ((extent.width as f32 * scale).round() as u32).max(1),
            height: ((extent.height as f32 * scale).round() as u32).max(1),
        };
        let changed = self.extent != extent || self.gi_extent != gi_extent;
        if !changed && self.images[0].is_some() {
            return Ok(());
        }
        let device = &context.device;
        for slot in self.images.iter_mut() {
            if let Some(mut image) = slot.take() {
                unsafe { image.destroy(device) };
            }
            *slot = Some(GiImage::new(context, gi_extent)?);
        }
        self.extent = extent;
        self.gi_extent = gi_extent;
        self.last_written = vk::ImageView::null();
        self.write_index = 0;
        self.reservoirs_dirty = true;
        self.reset_pending = true;
        if let Some(old) = self.reservoirs.take() {
            unsafe {
                for i in 0..2 {
                    device.destroy_buffer(old.buffers[i], None);
                    device.free_memory(old.memories[i], None);
                }
            }
        }
        let count = gi_extent.width as vk::DeviceSize * gi_extent.height as vk::DeviceSize;
        let size = count * RESERVOIR_BYTES;
        let mut buffers = [vk::Buffer::null(); 2];
        let mut memories = [vk::DeviceMemory::null(); 2];
        for i in 0..2 {
            let (buffer, memory) = create_buffer(
                context,
                size,
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .context("RestirGiPass: create reservoir buffer")?;
            buffers[i] = buffer;
            memories[i] = memory;
        }
        self.reservoirs = Some(ReservoirBuffers { buffers, memories });
        if self.frame_constants.is_none() {
            self.frame_constants = Some(FrameConstants::new(context)?);
        }
        Ok(())
    }

    fn ensure_sampler(&mut self, device: &ash::Device) {
        if self.sampler != vk::Sampler::null() {
            return;
        }
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        self.sampler = unsafe { device.create_sampler(&info, None) }.unwrap_or(vk::Sampler::null());
    }

    fn ensure_descriptors(&mut self, context: &VulkanContext) -> Result<()> {
        let device = &context.device;
        self.ensure_sampler(device);
        if self.ds0_layout.is_none() {
            let mut bindings: Vec<vk::DescriptorSetLayoutBinding> = Vec::new();
            let mut add = |binding: u32, ty: vk::DescriptorType| {
                bindings.push(
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(binding)
                        .descriptor_type(ty)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE),
                );
            };
            add(
                bindings::GI_OUTPUT_BINDING,
                vk::DescriptorType::STORAGE_IMAGE,
            );
            add(
                bindings::TLAS_BINDING,
                vk::DescriptorType::ACCELERATION_STRUCTURE_KHR,
            );
            add(
                bindings::VERTEX_DATA_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::INDICES_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::INSTANCE_META_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::MATERIALS_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::PT_LIGHTS_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::PT_EMISSIVE_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::PREV_RESERVOIR_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::CURR_RESERVOIR_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::SHARC_HASH_ENTRIES_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::SHARC_ACCUMULATION_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(
                bindings::SHARC_RESOLVED_BINDING,
                vk::DescriptorType::STORAGE_BUFFER,
            );
            add(bindings::FRAME_BINDING, vk::DescriptorType::UNIFORM_BUFFER);
            let flags = vec![
                vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                    | vk::DescriptorBindingFlags::PARTIALLY_BOUND;
                bindings.len()
            ];
            let mut flags_info =
                vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&flags);
            let layout = unsafe {
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                        .bindings(&bindings)
                        .push_next(&mut flags_info),
                    None,
                )
            }
            .context("RestirGiPass: set 0 layout")?;
            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: 1,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::ACCELERATION_STRUCTURE_KHR,
                    descriptor_count: 1,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: 11,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::UNIFORM_BUFFER,
                    descriptor_count: 1,
                },
            ];
            let pool = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                        .max_sets(1)
                        .pool_sizes(&pool_sizes),
                    None,
                )
            }
            .context("RestirGiPass: set 0 pool")?;
            let set = unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(std::slice::from_ref(&layout)),
                )
            }
            .context("RestirGiPass: allocate set 0")?[0];
            self.ds0_layout = Some(layout);
            self.ds0_pool = Some(pool);
            self.ds0 = set;
        }
        if self.gb_layout.is_none() {
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GB_DEPTH_BINDING)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GB_NORMAL_BINDING)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GB_ALBEDO_BINDING)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GB_VELOCITY_BINDING)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GI_HISTORY_BINDING)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(bindings::GB_SAMPLER_BINDING)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            let layout = unsafe {
                device.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
            }
            .context("RestirGiPass: G-buffer set layout")?;
            let pool_sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: 5 * FRAMES_IN_FLIGHT as u32,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLER,
                    descriptor_count: FRAMES_IN_FLIGHT as u32,
                },
            ];
            let pool = unsafe {
                device.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(FRAMES_IN_FLIGHT as u32)
                        .pool_sizes(&pool_sizes),
                    None,
                )
            }
            .context("RestirGiPass: G-buffer pool")?;
            let layouts = vec![layout; FRAMES_IN_FLIGHT];
            let sets = unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&layouts),
                )
            }
            .context("RestirGiPass: allocate G-buffer sets")?;
            self.gb_sets = sets;
            self.gb_views = vec![[vk::ImageView::null(); 5]; FRAMES_IN_FLIGHT];
            self.gb_layout = Some(layout);
            self.gb_pool = Some(pool);
        }
        Ok(())
    }

    fn ensure_pipeline(&mut self, context: &VulkanContext) -> Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let device = &context.device;
        let module = shader::load_shader_module(
            device,
            include_bytes!("../../../assets/shaders/restir_gi.comp.spv"),
        )
        .context("RestirGiPass: load shader module")?;
        let entry: &CStr = c"giMain";
        let layouts = [
            self.ds0_layout.unwrap_or(vk::DescriptorSetLayout::null()),
            self.bindless_layout,
            self.ibl_layout,
            self.gb_layout.unwrap_or(vk::DescriptorSetLayout::null()),
        ];
        let pipeline = ComputePipeline::new(device, module, entry, &layouts, &[])
            .context("RestirGiPass: create compute pipeline")?;
        unsafe { device.destroy_shader_module(module, None) };
        self.pipeline = Some(pipeline);
        Ok(())
    }

    fn destroy_targets(&mut self, device: &ash::Device) {
        for slot in self.images.iter_mut() {
            if let Some(mut image) = slot.take() {
                unsafe { image.destroy(device) };
            }
        }
        if let Some(res) = self.reservoirs.take() {
            unsafe {
                for i in 0..2 {
                    device.destroy_buffer(res.buffers[i], None);
                    device.free_memory(res.memories[i], None);
                }
            }
        }
        if let Some(mut fc) = self.frame_constants.take() {
            unsafe { fc.destroy(device) };
        }
        self.last_written = vk::ImageView::null();
        self.extent = vk::Extent2D {
            width: 0,
            height: 0,
        };
        self.gi_extent = vk::Extent2D {
            width: 0,
            height: 0,
        };
    }

    /// Tear down every GPU resource owned by the pass.
    pub fn destroy(&mut self, device: &ash::Device) {
        self.destroy_targets(device);
        self.invalidate_geometry();
        if self.sampler != vk::Sampler::null() {
            unsafe { device.destroy_sampler(self.sampler, None) };
            self.sampler = vk::Sampler::null();
        }
        if let Some(pool) = self.ds0_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        if let Some(layout) = self.ds0_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        if let Some(pool) = self.gb_pool.take() {
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        if let Some(layout) = self.gb_layout.take() {
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        self.gb_sets.clear();
        self.gb_views.clear();
        self.ds0 = vk::DescriptorSet::null();
        self.pipeline = None;
        self.device = None;
    }

    /// Record the dispatch for one frame.
    fn record(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
        scale: f32,
    ) -> Result<()> {
        self.ensure_targets(ctx.context, ctx.render_extent, scale)?;
        self.ensure_descriptors(ctx.context)?;
        self.ensure_pipeline(ctx.context)?;
        self.device = Some(ctx.context.device.clone());
        let Some(scene) = self.scene.as_ref() else {
            return Ok(());
        };
        let Some(tlas) = scene.tlas.as_ref() else {
            return Ok(());
        };
        let Some(reservoirs) = self.reservoirs.as_ref() else {
            return Ok(());
        };
        let Some(frame_constants) = self.frame_constants.as_ref() else {
            return Ok(());
        };
        let prev_index = 1 - self.write_index;
        let (Some(write_image), Some(history_image)) = (
            self.images[self.write_index].as_ref(),
            self.images[prev_index].as_ref(),
        ) else {
            return Ok(());
        };
        let write_view = write_image.view;
        let write_img = write_image.image;
        let write_initialized = write_image.initialized;
        let history_img = history_image.image;
        let history_view = history_image.view;
        let history_initialized = history_image.initialized;
        let device = &ctx.context.device;
        let cmd = ctx.cmd;
        let gi_extent = self.gi_extent;
        let frame = ctx.frame;
        let reset = self.reset_pending || self.last_written == vk::ImageView::null();

        // ---- Frame constants -------------------------------------------
        let sharc_enabled = self.sharc_hash.is_some()
            && self.sharc_accumulation.is_some()
            && self.sharc_resolved.is_some();
        let camera_pos = frame.camera_pos;
        let constants = RestirGiFrame {
            inv_view_proj: mat_inverse(&frame.view_proj),
            inv_view_rot: frame.inv_view_rot,
            camera_pos,
            light_dir: frame.light_dir,
            light_color: frame.light_color,
            viewport: [
                gi_extent.width as f32,
                gi_extent.height as f32,
                ctx.render_extent.width as f32,
                ctx.render_extent.height as f32,
            ],
            params: [
                self.frame_index as f32,
                if reset { 1.0 } else { 0.0 },
                ctx.settings.restir_gi_rays_per_pixel.max(1) as f32,
                ctx.settings.restir_gi_max_history.max(1) as f32,
            ],
            gi_params: [
                ctx.settings.restir_gi_spatial_samples as f32,
                ctx.settings.restir_gi_max_secondary_dist.max(0.0),
                if ctx.settings.restir_gi_temporal_reuse {
                    1.0
                } else {
                    0.0
                },
                if ctx.settings.restir_gi_spatial_reuse {
                    1.0
                } else {
                    0.0
                },
            ],
            sharc_params: [
                ctx.settings.sharc_scene_scale,
                self.sharc_capacity as f32,
                if sharc_enabled {
                    SHARC_FLAG_QUERY + SHARC_FLAG_UPDATE
                } else {
                    0.0
                },
                ctx.settings.sharc_roughness_threshold,
            ],
            // The cache is keyed off the camera that produced the samples, so
            // the sharc camera position mirrors the current one.
            sharc_camera_pos: camera_pos,
            prev_view_proj: self.prev_view_proj,
            prev_camera_pos: self.prev_camera_pos,
        };
        frame_constants.upload(&constants);

        // ---- First-use layout transitions for the ping-pong images -----
        let mut initial_barriers: Vec<vk::ImageMemoryBarrier> = Vec::new();
        for (image, initialized) in [
            (write_img, write_initialized),
            (history_img, history_initialized),
        ] {
            if !initialized {
                initial_barriers.push(
                    vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(
                            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                        )
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(image_range()),
                );
            }
        }
        if !initial_barriers.is_empty() {
            unsafe {
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &initial_barriers,
                )
            };
        }
        if !initial_barriers.is_empty() {
            for image in self.images.iter_mut().flatten() {
                image.initialized = true;
            }
        }
        // ---- Clear the reservoirs when the history was dropped ----------
        if self.reservoirs_dirty || reset {
            for buffer in reservoirs.buffers.iter() {
                unsafe { device.cmd_fill_buffer(cmd, *buffer, 0, vk::WHOLE_SIZE, 0) };
            }
            let barrier = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
            unsafe {
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    std::slice::from_ref(&barrier),
                    &[],
                    &[],
                )
            };
            self.reservoirs_dirty = false;
        }

        // ---- Descriptor updates (set 0) --------------------------------
        let output_info = [vk::DescriptorImageInfo::default()
            .image_view(write_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let mut tlas_write = [vk::WriteDescriptorSetAccelerationStructureKHR::default()
            .acceleration_structures(std::slice::from_ref(&tlas.handle))];
        let mut writes: Vec<vk::WriteDescriptorSet> = vec![
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds0)
                .dst_binding(bindings::GI_OUTPUT_BINDING)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&output_info),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds0)
                .dst_binding(bindings::TLAS_BINDING)
                .descriptor_type(vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                .push_next(&mut tlas_write[0]),
        ];
        let mut buffers: Vec<(u32, vk::Buffer, vk::DescriptorType)> = vec![
            (
                bindings::VERTEX_DATA_BINDING,
                scene.vertex_buffer,
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::INDICES_BINDING,
                scene.index_buffer,
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::INSTANCE_META_BINDING,
                scene.instance_meta_buffer,
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::MATERIALS_BINDING,
                self.materials_buffer,
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::PREV_RESERVOIR_BINDING,
                reservoirs.buffers[prev_index],
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::CURR_RESERVOIR_BINDING,
                reservoirs.buffers[self.write_index],
                vk::DescriptorType::STORAGE_BUFFER,
            ),
            (
                bindings::FRAME_BINDING,
                frame_constants.buffer,
                vk::DescriptorType::UNIFORM_BUFFER,
            ),
        ];
        if sharc_enabled {
            if let (Some(hash), Some(accum), Some(resolved)) = (
                self.sharc_hash,
                self.sharc_accumulation,
                self.sharc_resolved,
            ) {
                buffers.push((
                    bindings::SHARC_HASH_ENTRIES_BINDING,
                    hash,
                    vk::DescriptorType::STORAGE_BUFFER,
                ));
                buffers.push((
                    bindings::SHARC_ACCUMULATION_BINDING,
                    accum,
                    vk::DescriptorType::STORAGE_BUFFER,
                ));
                buffers.push((
                    bindings::SHARC_RESOLVED_BINDING,
                    resolved,
                    vk::DescriptorType::STORAGE_BUFFER,
                ));
            }
        }
        let buffer_infos: Vec<vk::DescriptorBufferInfo> = buffers
            .iter()
            .map(|(_, buffer, _)| {
                vk::DescriptorBufferInfo::default()
                    .buffer(*buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)
            })
            .collect();
        for (i, (binding, _, ty)) in buffers.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.ds0)
                    .dst_binding(*binding)
                    .descriptor_type(*ty)
                    .buffer_info(std::slice::from_ref(&buffer_infos[i])),
            );
        }
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        // ---- Descriptor updates (set 3, G-buffer + history) ------------
        let gb_slot = (ctx.frame_index as usize) % self.gb_sets.len();
        let gb_views = [
            resources
                .published_view(FORWARD_DEPTH_H)
                .unwrap_or(vk::ImageView::null()),
            resources
                .published_view(FORWARD_NORMAL_H)
                .unwrap_or(vk::ImageView::null()),
            resources
                .published_view(FORWARD_ALBEDO_H)
                .unwrap_or(vk::ImageView::null()),
            resources
                .published_view(FORWARD_VELOCITY_H)
                .unwrap_or(vk::ImageView::null()),
            history_view,
        ];
        if self.gb_views[gb_slot] != gb_views {
            let bindings_list = [
                bindings::GB_DEPTH_BINDING,
                bindings::GB_NORMAL_BINDING,
                bindings::GB_ALBEDO_BINDING,
                bindings::GB_VELOCITY_BINDING,
                bindings::GI_HISTORY_BINDING,
            ];
            // Build every image info up front: the write descriptors borrow them.
            let mut gb_infos: Vec<[vk::DescriptorImageInfo; 1]> =
                Vec::with_capacity(bindings_list.len() + 1);
            for (i, _binding) in bindings_list.iter().enumerate() {
                let layout = if i == 4 {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                };
                gb_infos.push([vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(gb_views[i])
                    .image_layout(layout)]);
            }
            gb_infos.push([vk::DescriptorImageInfo::default().sampler(self.sampler)]);
            let sampler_index = gb_infos.len() - 1;
            let mut gb_writes: Vec<vk::WriteDescriptorSet> = Vec::with_capacity(gb_infos.len());
            for (i, binding) in bindings_list.iter().enumerate() {
                gb_writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.gb_sets[gb_slot])
                        .dst_binding(*binding)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&gb_infos[i]),
                );
            }
            gb_writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.gb_sets[gb_slot])
                    .dst_binding(bindings::GB_SAMPLER_BINDING)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&gb_infos[sampler_index]),
            );
            unsafe { device.update_descriptor_sets(&gb_writes, &[]) };
            self.gb_views[gb_slot] = gb_views;
        }

        // ---- Dispatch --------------------------------------------------
        let Some(pipeline) = self.pipeline.as_ref() else {
            return Ok(());
        };
        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                pipeline.layout,
                0,
                &[
                    self.ds0,
                    self.bindless_set,
                    self.ibl_set,
                    self.gb_sets[gb_slot],
                ],
                &[],
            );
            device.cmd_dispatch(
                cmd,
                gi_extent.width.div_ceil(8),
                gi_extent.height.div_ceil(8),
                1,
            );
        }

        // ---- Make the result visible to the next frame's forward pass ---
        let write_to_read = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(write_img)
            .subresource_range(image_range());
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&write_to_read),
            )
        };

        // Publish for the visualiser and hand the forward pass its input.
        resources.set_image_view(RESTIR_GI_COLOR_H, write_view);
        resources.set_image(RESTIR_GI_COLOR_H, write_img);
        self.last_written = write_view;
        self.write_index = prev_index;
        self.prev_view_proj = frame.view_proj;
        self.prev_camera_pos = frame.camera_pos;
        self.reset_pending = false;
        self.frame_index += 1;
        Ok(())
    }
}

impl RenderPassNode for RestirGiPass {
    fn name(&self) -> &str {
        "ReSTIR GI"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // Placeholder extent: the pass owns its own ping-pong images, so the
        // graph resource only exists to make the edge visible to the viz.
        graph.create_resource_at(
            RESTIR_GI_COLOR_H,
            ResourceType::StorageImage {
                format: vk::Format::R16G16B16A16_SFLOAT,
                extent: vk::Extent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                },
            },
        );
        self.out_h = RESTIR_GI_COLOR_H;
        for handle in [
            FORWARD_DEPTH_H,
            FORWARD_NORMAL_H,
            FORWARD_ALBEDO_H,
            FORWARD_VELOCITY_H,
        ] {
            graph.read_usage(ResourceUsage {
                handle,
                access: vk::AccessFlags::SHADER_READ,
                stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            });
        }
        graph.write_usage(ResourceUsage {
            handle: RESTIR_GI_COLOR_H,
            access: vk::AccessFlags::SHADER_WRITE,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            layout: vk::ImageLayout::GENERAL,
        });
    }

    fn execute(&mut self, ctx: &RenderContext, resources: &mut GraphResources) -> Result<()> {
        // Raster path only: the path tracer owns GI when it runs.
        if ctx.frame.render_mode != RenderMode::Raster {
            return Ok(());
        }
        // `gi_mode` selects the producer; the forward pass falls back to the
        // baked probe volume when this pass is idle, so nothing runs unless the
        // caller asked for it.
        if ctx.settings.gi_mode != GiMode::Restir {
            return Ok(());
        }
        let scale = ctx.settings.restir_gi_scale;
        if !scale.is_finite() || scale <= 0.0 {
            return Ok(());
        }
        self.record(ctx, resources, scale)
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::RestirGi,
            inputs: vec![
                FORWARD_DEPTH_H,
                FORWARD_NORMAL_H,
                FORWARD_ALBEDO_H,
                FORWARD_VELOCITY_H,
            ],
            outputs: vec![self.out_h],
        }
    }
}

impl Drop for RestirGiPass {
    fn drop(&mut self) {
        debug_assert!(
            self.images.iter().all(|image| image.is_none()),
            "RestirGiPass dropped without destroy()"
        );
    }
}

fn image_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}
