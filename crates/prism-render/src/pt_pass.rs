//! 实时路径追踪计算通道——每帧 1 个样本，带时间累积。
//!
//! [`PathTracePass`] 分派一个计算着色器，每像素每帧通过 `VK_KHR_ray_query`
//! 追踪一条光线，跨帧累积辐射度，并将解析后的（累积量/计数）结果写入
//! `PT_COLOR_H` 图资源，`PostPass` 读取该资源进行色调映射。
//!
//! ## 热切换
//!
//! 所有 PT 资源（管线累积缓冲区、展平几何体 BLAS/TLAS）针对每个场景
//! 创建一次并保持存活。当 `RenderMode::PathTrace` 激活时，通道分派；
//! 当光栅化激活时，通道为空操作。
//!
//! ## 相机运动重置
//!
//! 通道跟踪上一帧的相机位置和视图投影矩阵。
//! 当任一变化超过小阈值时，累积缓冲区被清除（着色器中的重置标志被设置）。

use std::ptr;

use anyhow::Context as _;
use ash::vk;

use crate::compute::ComputePipeline;
use crate::context::VulkanContext;
use crate::descriptor::{PtAnalyticLight, ReSTIRReservoir, PT_LIGHT_MAX};
use crate::math::mat_inverse;
use crate::render_graph::{
    GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderPassNode,
    RenderSettings, ResourceType, ResourceUsage, PT_COLOR_H,
};
use crate::shader;
use crate::shader_bindings;

/// GPU push-constant block size for PtPush (std140 rounds to 16-byte boundary).
/// The auto-generated `shader_bindings::pt_render::PtPush` is 136 bytes in
/// `#[repr(C)]`; add 8 bytes of std140 trailing padding for the actual range.
const PT_PUSH_RANGE_SIZE: u32 = 144;

/// `PtPush::params.w` bit layout, mirrored in `pt_render.slang`
/// (`PT_PARAMS_*`). Bits 0..=29 are the frame counter, bit 30 marks an
/// orthographic camera and bit 31 asks the shader to clear accumulation.
const PT_PARAMS_FRAME_MASK: u32 = 0x3FFF_FFFF;
const PT_PARAMS_ORTHOGRAPHIC: u32 = 1 << 30;
const PT_PARAMS_RESET: u32 = 1 << 31;

/// Pack the per-frame flags the path tracer reads out of `params.w`.
fn pack_pt_params(frame_count: u32, reset: bool, orthographic: bool) -> u32 {
    (frame_count & PT_PARAMS_FRAME_MASK)
        | if orthographic {
            PT_PARAMS_ORTHOGRAPHIC
        } else {
            0
        }
        | if reset { PT_PARAMS_RESET } else { 0 }
}

/// Real-time path tracing compute pass.
pub struct PathTracePass {
    // Pipeline
    pipeline: Option<ComputePipeline>,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    ds: vk::DescriptorSet,

    // Bindless texture table (set 1) - shared with the rasterizer. Not owned;
    // stored so the pipeline layout can reference its layout and the command
    // recorder can bind its set. Wired via `set_material_resources`.
    bindless_set: vk::DescriptorSet,
    bindless_layout: vk::DescriptorSetLayout,

    // IBL environment cubemap (set 2) - sampled on ray miss for HDR sky.
    // Wired via `set_ibl_resources`. Not owned.
    ibl_set: vk::DescriptorSet,
    ibl_layout: vk::DescriptorSetLayout,

    // Accumulation buffers (persistent across frames)
    accum_image: vk::Image,
    accum_view: vk::ImageView,
    accum_memory: vk::DeviceMemory,
    sample_count_image: vk::Image,
    sample_count_view: vk::ImageView,
    sample_count_memory: vk::DeviceMemory,

    // PT_COLOR_H output image (sampled+storage, published to graph for PostPass)
    output_image: vk::Image,
    output_view: vk::ImageView,
    output_memory: vk::DeviceMemory,

    // Per-instance ray-traceable scene (combined vertex/index buffers,
    // per-instance BLAS, TLAS, instance_meta SSBO). Built once per scene by
    // `set_geometry` via `bake_common::build_pt_scene`. Owns its GPU resources.
    pt_scene: Option<crate::bake_common::PtScene>,

    // Materials SSBO (shared `RenderMaterialManager::buffer()`), bound at set 0
    // binding 7. Wired via `set_material_resources`. The real-time pass does
    // NOT own this buffer (the material manager does) - it only holds the
    // handle for descriptor writes.
    materials_buffer: Option<vk::Buffer>,

    // Lights SSBO (binding 8) — HOST_VISIBLE | HOST_COHERENT storage buffer
    // for PtAnalyticLight[PT_LIGHT_MAX]. Created once, written each frame.
    lights_buffer: vk::Buffer,
    lights_memory: vk::DeviceMemory,
    lights_mapped: *mut u8,

    // Emissive triangle SSBO (binding 9) — device-local storage buffer built
    // from `set_emissive`. Read-only during PT dispatch; owned directly by
    // the pass (not through PtScene).
    emissive_buffer: Option<vk::Buffer>,
    emissive_memory: Option<vk::DeviceMemory>,
    emissive_count: u32,

    // ReSTIR DI reservoir ping-pong buffers (bindings 10/11).
    // Two storage buffers, each sized for (width × height) ReSTIRReservoir
    // entries. Each frame one serves as prev (read, b10) and the other as
    // curr (write, b11); they swap roles every frame.
    reservoir_buffers: [vk::Buffer; 2],
    reservoir_memories: [vk::DeviceMemory; 2],
    reservoir_size: vk::DeviceSize, // current buffer size in bytes
    reservoir_swap: usize,          // index of curr buffer (0 or 1) for this frame

    // State tracking
    img_width: u32,
    img_height: u32,
    frame_counter: u32,
    prev_camera_pos: Option<[f32; 3]>,
    prev_view_proj: Option<[[f32; 4]; 4]>,
    // Global accumulation-reset flag. Set by either (a) camera motion
    // (`should_reset`) or (b) an external `request_reset()` call when the app
    // knows a render parameter changed (max bounces, exposure, light
    // color/direction, scene reload, ...). Without this, stale samples
    // accumulated under the old parameters keep dominating the running
    // average and parameter tweaks look like they do nothing. This keeps the
    // "what changed?" decision in the caller rather than diffing every PT
    // input per frame.
    accum_dirty: bool,

    // Device handles
    device: Option<ash::Device>,
}

impl PathTracePass {
    pub fn new(context: &VulkanContext) -> anyhow::Result<Self> {
        let device = &context.device;

        // Set 0 bindings (must match pt_render.slang / path_integrator.slang):
        // b0: RWTexture2D<float4> accumImage
        // b1: RWTexture2D<uint>   sampleCount
        // b2: AccelerationStructure tlas
        // b3: ByteAddressBuffer vertexData  (combined world-space vertices)
        // b4: StructuredBuffer<uint> indices (combined index buffer)
        // b5: RWTexture2D<float4> outputImage
        // b6: StructuredBuffer<PtInstanceMeta> instance_meta (per-instance
        //     material slot + vertex/index base offsets, indexed by
        //     q.CommittedInstanceID())
        // b7: StructuredBuffer<GpuMaterial> materials (shared SSBO)
        // Set 1 (bindless, bound separately): globalSamplers[] + bindlessSrvs[]
        let bindings = [
            b(
                0,
                vk::DescriptorType::STORAGE_IMAGE,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                1,
                vk::DescriptorType::STORAGE_IMAGE,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                2,
                vk::DescriptorType::ACCELERATION_STRUCTURE_KHR,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                3,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                4,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                5,
                vk::DescriptorType::STORAGE_IMAGE,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                6,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                7,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                8,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                9,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ),
            b(
                10,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ), // prevReservoir (read)
            b(
                11,
                vk::DescriptorType::STORAGE_BUFFER,
                vk::ShaderStageFlags::COMPUTE,
            ), // currReservoir (write)
        ];

        // All bindings get UPDATE_AFTER_BIND + PARTIALLY_BOUND because
        // update_ds() is called every frame while previous-frame command
        // buffers may still be in flight. Without these flags Vulkan
        // validation complains about updating in-use descriptor sets.
        let binding_flags = [
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b0  accumImage
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b1  sampleCount
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b2  TLAS
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b3  vertexData
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b4  indices
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b5  outputImage
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b6  instance_meta
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b7  materials
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b8  ptLights
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b9  ptEmissive
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b10 prevReservoir
            vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND, // b11 currReservoir
        ];
        let mut flags_info =
            vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);

        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default()
                    .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                    .bindings(&bindings)
                    .push_next(&mut flags_info),
                None,
            )
        }
        .context("PathTracePass: ds layout")?;

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 3,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 8,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::ACCELERATION_STRUCTURE_KHR,
                descriptor_count: 1,
            },
        ];
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .context("PathTracePass: ds pool")?;

        let ds = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(std::slice::from_ref(&ds_layout)),
            )
        }
        .context("PathTracePass: allocate ds")?[0];

        // Placeholder images (1×1 — resized on first execution)
        let mem_props = &context.physical_device_memory_properties;
        let (ai, av, am) =
            make_accum_image(device, mem_props, 1, 1).context("PathTracePass: accum image")?;
        let (si, sv, sm) = make_sample_count_image(device, mem_props, 1, 1)
            .context("PathTracePass: sample count image")?;
        let (oi, ov, om) =
            make_pt_output_image(device, mem_props, 1, 1).context("PathTracePass: output image")?;

        // Create persistent HOST_VISIBLE lights buffer for PtAnalyticLight[]
        let light_buf_size = (PT_LIGHT_MAX as vk::DeviceSize)
            * std::mem::size_of::<PtAnalyticLight>() as vk::DeviceSize;
        let light_buf_create = vk::BufferCreateInfo::default()
            .size(light_buf_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let lights_buffer = unsafe { device.create_buffer(&light_buf_create, None) }
            .context("PathTracePass: create lights buffer")?;
        let light_mem_reqs = unsafe { device.get_buffer_memory_requirements(lights_buffer) };
        let light_mem_type = crate::render_pass::find_memory_type(
            &context.physical_device_memory_properties,
            light_mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .context("PathTracePass: no suitable memory for lights buffer")?;
        let light_mem_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(light_mem_reqs.size)
            .memory_type_index(light_mem_type);
        let lights_memory = unsafe { device.allocate_memory(&light_mem_alloc, None) }
            .context("PathTracePass: allocate lights memory")?;
        unsafe { device.bind_buffer_memory(lights_buffer, lights_memory, 0) }
            .context("PathTracePass: bind lights memory")?;
        let lights_mapped = unsafe {
            device.map_memory(
                lights_memory,
                0,
                light_buf_size,
                vk::MemoryMapFlags::empty(),
            )
        }
        .context("PathTracePass: map lights memory")? as *mut u8;

        // Write descriptor for the lights SSBO (binding 8, initially zeroed)
        unsafe {
            ptr::write_bytes(lights_mapped, 0, light_buf_size as usize);
        }

        Ok(Self {
            pipeline: None,
            ds_layout,
            ds_pool,
            ds,
            bindless_set: vk::DescriptorSet::null(),
            bindless_layout: vk::DescriptorSetLayout::null(),
            ibl_set: vk::DescriptorSet::null(),
            ibl_layout: vk::DescriptorSetLayout::null(),
            accum_image: ai,
            accum_view: av,
            accum_memory: am,
            sample_count_image: si,
            sample_count_view: sv,
            sample_count_memory: sm,
            output_image: oi,
            output_view: ov,
            output_memory: om,
            pt_scene: None,
            materials_buffer: None,
            lights_buffer,
            lights_memory,
            lights_mapped,
            emissive_buffer: None,
            emissive_memory: None,
            emissive_count: 0,
            reservoir_buffers: [vk::Buffer::null(), vk::Buffer::null()],
            reservoir_memories: [vk::DeviceMemory::null(), vk::DeviceMemory::null()],
            reservoir_size: 0,
            reservoir_swap: 0,
            img_width: 1,
            img_height: 1,
            frame_counter: 0,
            prev_camera_pos: None,
            prev_view_proj: None,
            accum_dirty: false,
            device: Some(device.clone()),
        })
    }

    /// Upload per-instance world-space geometry and build BLAS/TLAS.
    ///
    /// Builds a single combined vertex + index buffer (so the shader's
    /// `vertexData`/`indices` reads resolve any instance's vertices), then one
    /// BLAS per instance pointing at that instance's slice of the combined
    /// buffers, and one TLAS whose `instanceCustomIndex` carries the instance
    /// index (0..N) used to look up `PtInstanceMeta` -> `material_slot`.
    ///
    /// `instances` is produced by the engine crate's ECS walk of
    /// `MeshRef`/`MaterialRef` entities.
    pub fn set_geometry(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        instances: &[crate::bake_common::PtGeometryInstance],
    ) -> anyhow::Result<()> {
        if instances.is_empty() {
            anyhow::bail!("PathTracePass::set_geometry: no instances");
        }

        // The materials SSBO bytes: if `set_material_resources` wired the
        // shared `RenderMaterialManager` buffer we use an empty slice (the
        // buffer is bound separately at b7 and never owned by the scene); if
        // not wired (shouldn't happen for the real-time pass), fall back to a
        // single neutral material so the build still succeeds.
        // `build_pt_scene` only allocates its OWN materials buffer from these
        // bytes; the real-time pass overrides it with the shared manager
        // buffer in `update_ds`, so we pass a minimal placeholder here.
        let placeholder_material = [0u8; 96]; // one GpuMaterial's worth of zeroes
        let scene = crate::bake_common::build_pt_scene(
            context,
            command_pool,
            instances,
            &placeholder_material,
        )
        .context("PathTracePass: build_pt_scene")?;

        // Drop the previous scene (frees its buffers/BLAS/TLAS via Drop).
        self.pt_scene = Some(scene);

        log::info!("PathTracePass: {} instances uploaded", instances.len());
        Ok(())
    }

    /// Wire the shared bindless texture table (set 1) + materials SSBO (set 0
    /// binding 7). Must be called before the first frame so the pipeline layout
    /// can include the bindless set and `update_ds` can write the materials
    /// binding. The materials buffer is `RenderMaterialManager::buffer()`.
    pub fn set_material_resources(
        &mut self,
        materials_buffer: vk::Buffer,
        bindless_set: vk::DescriptorSet,
        bindless_layout: vk::DescriptorSetLayout,
    ) {
        self.materials_buffer = Some(materials_buffer);
        self.bindless_set = bindless_set;
        self.bindless_layout = bindless_layout;
        // Force pipeline rebuild so it picks up the 2-set layout.
        self.pipeline = None;
    }

    /// Wire the IBL environment cubemap descriptor set (set 2) so the shader
    /// can sample envCube on ray miss for HDR sky. Must be called before the
    /// first frame so the pipeline layout includes set 2.
    pub fn set_ibl_resources(
        &mut self,
        ibl_set: vk::DescriptorSet,
        ibl_layout: vk::DescriptorSetLayout,
    ) {
        self.ibl_set = ibl_set;
        self.ibl_layout = ibl_layout;
        // Force pipeline rebuild to pick up the 3-set layout.
        self.pipeline = None;
    }

    /// Build (or rebuild) the emissive triangle SSBO from actual geometry +
    /// material data. Must be called after `set_geometry` so the scene
    /// instances are fully built; the material bytes come from the scene's
    /// actual material data (not placeholder).
    ///
    /// Destroys any previous emissive buffer and triggers an accumulation reset.
    pub fn set_emissive(
        &mut self,
        context: &VulkanContext,
        instances: &[crate::bake_common::PtGeometryInstance],
        materials_bytes: &[u8],
    ) {
        use crate::bake_common::create_emissive_buffer;
        // Destroy previous buffer.
        let device = &context.device;
        if let Some(eb) = self.emissive_buffer.take() {
            if let Some(em) = self.emissive_memory.take() {
                unsafe {
                    device.destroy_buffer(eb, None);
                    device.free_memory(em, None);
                }
            }
        }
        let (buf, mem, count) = create_emissive_buffer(context, instances, materials_bytes);
        self.emissive_buffer = buf;
        self.emissive_memory = mem;
        self.emissive_count = count;
        if count > 0 {
            log::info!("PathTracePass: {} emissive triangles uploaded", count);
        }
        // Reset accumulation so the new emissive data takes effect immediately.
        self.accum_dirty = true;
    }

    /// Ensure the ReSTIR reservoir ping-pong buffers are large enough for
    /// the given width × height. Re-allocates (destroys + creates) when the
    /// image size grows; does NOT shrink.
    fn ensure_reservoir_buffers(
        &mut self,
        device: &ash::Device,
        context: &VulkanContext,
        width: u32,
        height: u32,
    ) {
        let needed =
            (width as u64) * (height as u64) * std::mem::size_of::<ReSTIRReservoir>() as u64;
        if needed <= self.reservoir_size {
            return;
        }
        // Destroy previous
        for i in 0..2 {
            if self.reservoir_buffers[i] != vk::Buffer::null() {
                unsafe {
                    device.destroy_buffer(self.reservoir_buffers[i], None);
                    device.free_memory(self.reservoir_memories[i], None);
                }
            }
        }
        // Create two device-local storage buffers
        let buf_info = vk::BufferCreateInfo::default()
            .size(needed)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        for i in 0..2 {
            let buf = unsafe { device.create_buffer(&buf_info, None) }
                .expect("ensure_reservoir_buffers: create_buffer");
            let mem_reqs = unsafe { device.get_buffer_memory_requirements(buf) };
            let mem_type = crate::buffer::find_memory_type(
                &context,
                mem_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .expect("ensure_reservoir_buffers: no suitable memory type");
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(mem_reqs.size)
                .memory_type_index(mem_type);
            let mem = unsafe { device.allocate_memory(&alloc, None) }
                .expect("ensure_reservoir_buffers: allocate_memory");
            unsafe { device.bind_buffer_memory(buf, mem, 0) }
                .expect("ensure_reservoir_buffers: bind_buffer_memory");
            self.reservoir_buffers[i] = buf;
            self.reservoir_memories[i] = mem;
        }
        self.reservoir_size = needed;
        self.reservoir_swap = 0;
        // Force pipeline rebuild so it picks up the new buffer handles.
        self.pipeline = None;
        log::info!("ReSTIR reservoir buffers: {} bytes each", needed);
    }

    /// Write the PT analytic light list into the lights SSBO (binding 8).
    ///
    /// Copies up to `PT_LIGHT_MAX` lights into the mapped HOST_VISIBLE buffer
    /// and zeros the remaining entries. The pass owns this buffer - no external
    /// descriptor wiring needed beyond the initial `update_ds` call.
    /// `GraphRenderer::execute` calls this before dispatch each frame.
    pub fn set_lights(&mut self, lights: &[PtAnalyticLight]) {
        let max_lights = PT_LIGHT_MAX as usize;
        let count = lights.len().min(max_lights);
        let light_size = std::mem::size_of::<PtAnalyticLight>();
        unsafe {
            ptr::copy_nonoverlapping(
                bytemuck::cast_slice(lights).as_ptr(),
                self.lights_mapped,
                count * light_size,
            );
            // Zero remaining entries
            if count < max_lights {
                let dst = self.lights_mapped.add(count * light_size);
                ptr::write_bytes(dst, 0, (max_lights - count) * light_size);
            }
        }
    }

    /// Request an accumulation-buffer reset on the next frame. Call this when a
    /// render parameter that affects the traced radiance changes (max bounces,
    /// exposure, light color/direction/intensity, scene geometry, ...). The
    /// pass also resets automatically on camera motion; this is the hook for
    /// non-camera changes it can't otherwise detect.
    pub fn request_reset(&mut self) {
        self.accum_dirty = true;
    }

    /// Current frame counter (number of accumulated samples per pixel).
    /// Resets to 0 when accumulation is cleared (camera motion or
    /// [`request_reset`]).
    pub fn frame_count(&self) -> u32 {
        self.frame_counter
    }

    fn resize_images(
        &mut self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        w: u32,
        h: u32,
    ) -> anyhow::Result<()> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        // Skip if size hasn't changed — avoids unnecessary destroy+recreate
        // cycles and keeps the graph's resource references valid across frames.
        if self.img_width == w && self.img_height == h {
            return Ok(());
        }
        self.img_width = w;
        self.img_height = h;
        unsafe {
            device.destroy_image_view(self.accum_view, None);
            device.destroy_image(self.accum_image, None);
            device.free_memory(self.accum_memory, None);
            device.destroy_image_view(self.sample_count_view, None);
            device.destroy_image(self.sample_count_image, None);
            device.free_memory(self.sample_count_memory, None);
            device.destroy_image_view(self.output_view, None);
            device.destroy_image(self.output_image, None);
            device.free_memory(self.output_memory, None);
        }
        let (ai, av, am) = make_accum_image(device, mem_props, w, h)?;
        let (si, sv, sm) = make_sample_count_image(device, mem_props, w, h)?;
        let (oi, ov, om) = make_pt_output_image(device, mem_props, w, h)?;
        self.accum_image = ai;
        self.accum_view = av;
        self.accum_memory = am;
        self.sample_count_image = si;
        self.sample_count_view = sv;
        self.sample_count_memory = sm;
        self.output_image = oi;
        self.output_view = ov;
        self.output_memory = om;
        self.frame_counter = 0;
        self.prev_camera_pos = None;
        self.prev_view_proj = None;
        Ok(())
    }

    fn ensure_pipeline(&mut self, device: &ash::Device) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        const SPV: &[u8] = include_bytes!("../../../assets/shaders/pt_render.comp.spv");
        let mod_ = shader::load_shader_module(device, SPV).context("PathTracePass: load spv")?;
        let entry = std::ffi::CString::new("ptMain").unwrap();
        // Three sets: set 0 = PT-local (accum/output/TLAS/vertex/index/meta/
        // materials), set 1 = shared bindless texture table (samplers + SRVs),
        // set 2 = IBL environment cubemap (HDR sky).
        let mut layouts = vec![self.ds_layout, self.bindless_layout];
        if self.ibl_layout != vk::DescriptorSetLayout::null() {
            layouts.push(self.ibl_layout);
        }
        let push = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: PT_PUSH_RANGE_SIZE,
        }];
        let pl = ComputePipeline::new(device, mod_, entry.as_c_str(), &layouts, &push)
            .context("PathTracePass: pipeline")?;
        unsafe {
            device.destroy_shader_module(mod_, None);
        }
        self.pipeline = Some(pl);
        Ok(())
    }

    fn should_reset(&self, pos: [f32; 3], ivp: [[f32; 4]; 4]) -> bool {
        const E: f32 = 1e-4;
        let (Some(pp), Some(pv)) = (self.prev_camera_pos, self.prev_view_proj) else {
            return true;
        };
        let dp = (pos[0] - pp[0]).abs() + (pos[1] - pp[1]).abs() + (pos[2] - pp[2]).abs();
        let mut dv = 0.0f32;
        for c in 0..4 {
            for r in 0..4 {
                dv += (ivp[c][r] - pv[c][r]).abs();
            }
        }
        dp > E || dv > E
    }

    fn clear_accum_images(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        // Transition accum to GENERAL
        let b1 = vk::ImageMemoryBarrier::default()
            .image(self.accum_image)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&b1),
            );
        }
        let cc = vk::ClearColorValue {
            float32: [0.0, 0.0, 0.0, 0.0],
        };
        let sub = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        unsafe {
            device.cmd_clear_color_image(
                cmd,
                self.accum_image,
                vk::ImageLayout::GENERAL,
                &cc,
                &[sub],
            );
        }

        // Transition sample count to GENERAL
        let b2 = vk::ImageMemoryBarrier::default()
            .image(self.sample_count_image)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(sub)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&b2),
            );
        }
        let cu = vk::ClearColorValue {
            uint32: [0, 0, 0, 0],
        };
        unsafe {
            device.cmd_clear_color_image(
                cmd,
                self.sample_count_image,
                vk::ImageLayout::GENERAL,
                &cu,
                &[sub],
            );
        }
    }

    /// Update descriptor set bindings.
    fn update_ds(&self, device: &ash::Device) {
        let ai = vk::DescriptorImageInfo::default()
            .image_view(self.accum_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let si = vk::DescriptorImageInfo::default()
            .image_view(self.sample_count_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let oi = vk::DescriptorImageInfo::default()
            .image_view(self.output_view)
            .image_layout(vk::ImageLayout::GENERAL);

        // Geometry buffers come from the PtScene (vertex/index/instance_meta).
        // The materials buffer is the shared RenderMaterialManager SSBO (wired
        // via set_material_resources), NOT the PtScene's placeholder buffer.
        let (vbuf, ibuf, mbuf) = match self.pt_scene.as_ref() {
            Some(s) => (s.vertex_buffer, s.index_buffer, s.instance_meta_buffer),
            None => (vk::Buffer::null(), vk::Buffer::null(), vk::Buffer::null()),
        };
        let vbi = vk::DescriptorBufferInfo::default()
            .buffer(vbuf)
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let ibi = vk::DescriptorBufferInfo::default()
            .buffer(ibuf)
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let mbi = vk::DescriptorBufferInfo::default()
            .buffer(mbuf)
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let matbi = vk::DescriptorBufferInfo::default()
            .buffer(self.materials_buffer.unwrap_or(vk::Buffer::null()))
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let lbi = vk::DescriptorBufferInfo::default()
            .buffer(self.lights_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let ebi = vk::DescriptorBufferInfo::default()
            .buffer(self.emissive_buffer.unwrap_or(vk::Buffer::null()))
            .offset(0)
            .range(vk::WHOLE_SIZE);
        // ReSTIR reservoir buffers (ping-pong, b10/b11).
        // Swap roles each frame: b10 gets the buffer written last frame (prev),
        // b11 gets the buffer to write this frame (curr).
        let prev_buf = self.reservoir_buffers[1 - self.reservoir_swap];
        let curr_buf = self.reservoir_buffers[self.reservoir_swap];
        let prev_bi = vk::DescriptorBufferInfo::default()
            .buffer(prev_buf)
            .offset(0)
            .range(vk::WHOLE_SIZE);
        let curr_bi = vk::DescriptorBufferInfo::default()
            .buffer(curr_buf)
            .offset(0)
            .range(vk::WHOLE_SIZE);

        let writes = vec![
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&ai)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&si)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&vbi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&ibi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&oi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(6)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&mbi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(7)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&matbi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(8)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&lbi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(9)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&ebi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(10)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&prev_bi)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(11)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(std::slice::from_ref(&curr_bi)),
        ];
        unsafe {
            device.update_descriptor_sets(&writes, &[]);
        }

        // Acceleration structure write (binding 2) - done as a separate call
        // because its push_next reference must stay alive for the update call.
        if let Some(handle) = self
            .pt_scene
            .as_ref()
            .and_then(|s| s.tlas.as_ref().map(|t| t.handle))
        {
            let mut as_info = vk::WriteDescriptorSetAccelerationStructureKHR::default()
                .acceleration_structures(std::slice::from_ref(&handle));
            let as_write = vk::WriteDescriptorSet::default()
                .dst_set(self.ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                .descriptor_count(1)
                .push_next(&mut as_info);
            unsafe {
                device.update_descriptor_sets(&[as_write], &[]);
            }
        }
    }

    /// Destroy all GPU resources.
    pub fn destroy(&mut self, device: &ash::Device) {
        self.pipeline = None;
        unsafe {
            device.destroy_image_view(self.accum_view, None);
            device.destroy_image(self.accum_image, None);
            device.free_memory(self.accum_memory, None);
            device.destroy_image_view(self.sample_count_view, None);
            device.destroy_image(self.sample_count_image, None);
            device.free_memory(self.sample_count_memory, None);
            device.destroy_image_view(self.output_view, None);
            device.destroy_image(self.output_image, None);
            device.free_memory(self.output_memory, None);
        }
        // PtScene drops its own vertex/index/meta/materials buffers + BLAS + TLAS.
        self.pt_scene = None;
        self.materials_buffer = None;
        unsafe {
            device.unmap_memory(self.lights_memory);
            device.destroy_buffer(self.lights_buffer, None);
            device.free_memory(self.lights_memory, None);
            device.destroy_descriptor_set_layout(self.ds_layout, None);
            device.destroy_descriptor_pool(self.ds_pool, None);
        }
        // Destroy emissive SSBO.
        if let Some(eb) = self.emissive_buffer.take() {
            if let Some(em) = self.emissive_memory.take() {
                unsafe {
                    device.destroy_buffer(eb, None);
                    device.free_memory(em, None);
                }
            }
        }
        // Destroy ReSTIR reservoir buffers.
        for i in 0..2 {
            if self.reservoir_buffers[i] != vk::Buffer::null() {
                unsafe {
                    device.destroy_buffer(self.reservoir_buffers[i], None);
                    device.free_memory(self.reservoir_memories[i], None);
                }
            }
        }
        // Clear the cached device handle so Drop::drop becomes a no-op
        // (graph_renderer calls destroy() explicitly after device_wait_idle).
        self.device = None;
    }
}

impl Drop for PathTracePass {
    fn drop(&mut self) {
        if let Some(d) = self.device.take() {
            self.destroy(&d);
        }
    }
}

impl RenderPassNode for PathTracePass {
    fn name(&self) -> &str {
        "PathTracePass"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        graph.create_resource_at(
            PT_COLOR_H,
            ResourceType::StorageImage {
                format: vk::Format::R32G32B32A32_SFLOAT,
                extent: vk::Extent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                },
            },
        );
        graph.write_usage(ResourceUsage {
            handle: PT_COLOR_H,
            access: vk::AccessFlags::SHADER_WRITE,
            stage: vk::PipelineStageFlags::COMPUTE_SHADER,
            // Declared as SHADER_READ_ONLY_OPTIMAL (not GENERAL) so that when this
            // pass is skipped (e.g. no camera) the state tracker always reads the
            // same layout that PostPass wants, avoiding a trampoline barrier with a
            // stale old_layout (the image was never actually transitioned to GENERAL).
            // The actual post-dispatch GENERAL→SHADER_READ_ONLY_OPTIMAL transition
            // is emitted manually inside `execute`.
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        });
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        if ctx.frame.render_mode != crate::render_graph::RenderMode::PathTrace {
            return Ok(());
        }
        if !ctx.frame.has_camera {
            log::debug!("PathTracePass: no camera, skipping (PostPass reads FORWARD_COLOR_H)");
            return Ok(());
        }
        if self.pt_scene.is_none() || self.materials_buffer.is_none() {
            log::debug!("PathTracePass: no geometry/materials, skipping");
            return Ok(());
        }

        let device = ctx.device;
        let cmd = ctx.cmd;
        // #613：内部渲染分辨率开启时 PT 也在内部尺寸上累积。
        let w = ctx.render_extent.width.max(1);
        let h = ctx.render_extent.height.max(1);

        // Resize accumulation buffers if needed
        self.resize_images(device, &ctx.context.physical_device_memory_properties, w, h)?;
        // Resize ReSTIR reservoir ping-pong buffers if needed
        self.ensure_reservoir_buffers(device, &ctx.context, w, h);

        // Write analytic lights into the SSBO (binding 8) — do this before
        // borrowing `self.pipeline` so the mutable borrow for set_lights
        // doesn't conflict with the immutable `pl` borrow below.
        self.set_lights(ctx.frame.pt_lights);

        // Pipeline
        self.ensure_pipeline(device)?;
        let pl = self.pipeline.as_ref().unwrap();

        // Camera detection
        let cam_pos = ctx.frame.camera_pos;
        let cam_xyz = [cam_pos[0], cam_pos[1], cam_pos[2]];
        let inv_vp = mat_inverse(&ctx.frame.view_proj);
        // Reset on camera motion, explicit accum-dirty (geometry), or the
        // directional-light accumulation-dirty flag from the frame (intensity,
        // color, or direction changed). Cleared after one frame in both cases.
        let reset =
            self.should_reset(cam_xyz, inv_vp) || self.accum_dirty || ctx.frame.pt_accum_dirty;
        self.accum_dirty = false;

        self.prev_camera_pos = Some(cam_xyz);
        self.prev_view_proj = Some(inv_vp);

        if reset {
            self.clear_accum_images(device, cmd);
            self.frame_counter = 0;
        }

        // Barrier: accum → GENERAL (compute write).
        // On frame 1 (after clear_accum_images) the previous write was a
        // TRANSFER_WRITE from vkCmdClearColorImage; on subsequent frames it
        // was a SHADER_WRITE from the previous-frame compute dispatch.
        // Both access/stage masks must be present to properly order the
        // dependency regardless of which producer ran last — omitting
        // SHADER_WRITE would mean frame 2's compute shader reads undefined
        // garbage from the accumulation images (causing all-white output).
        let accum_to_gen = vk::ImageMemoryBarrier::default()
            .image(self.accum_image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&accum_to_gen),
            );
        }

        // Barrier: sampleCount → GENERAL (compute write).
        // Same dual-src logic as accum above.
        let sc_to_gen = vk::ImageMemoryBarrier::default()
            .image(self.sample_count_image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&sc_to_gen),
            );
        }

        // Barrier: PT_COLOR_H → GENERAL (compute write).
        // Use UNDEFINED as old_layout — this is always valid per the Vulkan
        // spec regardless of the image's actual layout (SHADER_READ_ONLY_OPTIMAL
        // from PostPass's read barrier in the previous frame, or UNDEFINED on
        // the first frame / after resize). The graph does not issue a barrier
        // for write-only edges, so we must handle this ourselves.
        let out_to_gen = vk::ImageMemoryBarrier::default()
            .image(self.output_image)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&out_to_gen),
            );
        }

        // Update descriptors
        self.update_ds(device);

        // Bind: set 0 = PT-local, set 1 = shared bindless texture table,
        // set 2 = IBL environment cubemap (if wired).
        let sets = if self.ibl_set != vk::DescriptorSet::null() {
            vec![self.ds, self.bindless_set, self.ibl_set]
        } else {
            vec![self.ds, self.bindless_set]
        };
        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pl.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                pl.layout,
                0,
                &sets,
                &[],
            );
        }

        // Pack the per-frame flags into params.w: bit 30 = orthographic
        // camera, bit 31 = reset accumulation (see `PT_PARAMS_*`).
        let params_w = pack_pt_params(self.frame_counter, reset, ctx.frame.camera_orthographic);

        let light_dir = ctx.frame.light_dir;
        let light_color = ctx.frame.light_color;
        // Pack exposure into camera_pos.w (PT only uses camera_pos.xyz for
        // ray origin; the .w slot was previously unused).
        let mut camera_pos = cam_pos;
        camera_pos[3] = ctx.frame.exposure;
        let push = shader_bindings::pt_render::PtPush {
            inv_view_proj: inv_vp,
            camera_pos,
            light_dir,
            light_color,
            params: [w, h, ctx.frame.pt_max_bounces, params_w],
            ray_max_distance: ctx.frame.pt_ray_max_distance,
            max_iterations: ctx.frame.pt_max_iterations,
            num_lights: ctx.frame.pt_lights.len() as u32,
            num_emissive: self.emissive_count,
        };
        unsafe {
            device.cmd_push_constants(
                cmd,
                pl.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&push),
            );
        }

        // Dispatch (16×16 thread groups)
        let gx = (w + 15) / 16;
        let gy = (h + 15) / 16;
        unsafe {
            device.cmd_dispatch(cmd, gx, gy, 1);
        }

        // Post-dispatch barrier: PT_COLOR_H GENERAL → SHADER_READ_ONLY_OPTIMAL
        // so PostPass can sample it. Must be manual because the write edge
        // declares SHADER_READ_ONLY_OPTIMAL as the post-pass layout (to avoid
        // stale-tracker barriers when the pass is skipped).
        let out_to_read = vk::ImageMemoryBarrier::default()
            .image(self.output_image)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&out_to_read),
            );
        }

        // Advance reservoir swap index for next frame
        self.reservoir_swap = (self.reservoir_swap + 1) & 1;

        self.frame_counter = self.frame_counter.wrapping_add(1);

        // Publish for PostPass
        resources.set_image_view(PT_COLOR_H, self.output_view);
        resources.set_image(PT_COLOR_H, self.output_image);

        log::trace!(
            "PathTracePass: dispatch {}×{} reset={} frame={}",
            w,
            h,
            reset,
            self.frame_counter
        );
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::Pt,
            inputs: Vec::new(),
            outputs: vec![PT_COLOR_H],
        }
    }

    fn warmup(&mut self, device: &ash::Device, _context: &VulkanContext) -> anyhow::Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        self.ensure_pipeline(device)
    }
}

// ---- helpers ----

fn b(
    binding: u32,
    ty: vk::DescriptorType,
    stage: vk::ShaderStageFlags,
) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(binding)
        .descriptor_type(ty)
        .descriptor_count(1)
        .stage_flags(stage)
}

fn make_accum_image(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
) -> anyhow::Result<(vk::Image, vk::ImageView, vk::DeviceMemory)> {
    make_image(
        device,
        mem_props,
        w,
        h,
        vk::Format::R32G32B32A32_SFLOAT,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST,
    )
}

fn make_sample_count_image(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
) -> anyhow::Result<(vk::Image, vk::ImageView, vk::DeviceMemory)> {
    make_image(
        device,
        mem_props,
        w,
        h,
        vk::Format::R32_UINT,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST,
    )
}

fn make_pt_output_image(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
) -> anyhow::Result<(vk::Image, vk::ImageView, vk::DeviceMemory)> {
    make_image(
        device,
        mem_props,
        w,
        h,
        vk::Format::R32G32B32A32_SFLOAT,
        vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED,
    )
}

fn make_image(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fmt: vk::Format,
    usage: vk::ImageUsageFlags,
) -> anyhow::Result<(vk::Image, vk::ImageView, vk::DeviceMemory)> {
    let extent = vk::Extent3D {
        width: w.max(1),
        height: h.max(1),
        depth: 1,
    };
    let img = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(fmt)
                .extent(extent)
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
    }?;
    let req = unsafe { device.get_image_memory_requirements(img) };
    let mt = find_mem_type(
        mem_props,
        req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .ok_or_else(|| anyhow::anyhow!("no device-local memory"))?;
    let mem = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo {
                allocation_size: req.size,
                memory_type_index: mt,
                ..Default::default()
            },
            None,
        )
    }?;
    unsafe {
        device.bind_image_memory(img, mem, 0)?;
    }
    let view = unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(img)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(fmt)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )
    }?;
    Ok((img, view, mem))
}

fn find_mem_type(
    mp: &vk::PhysicalDeviceMemoryProperties,
    filter: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mp.memory_type_count).find(|&i| {
        (filter & (1 << i)) != 0 && mp.memory_types[i as usize].property_flags.contains(flags)
    })
}

#[cfg(test)]
mod mat_inverse_tests {
    #![allow(clippy::approx_constant, clippy::needless_range_loop)]
    use super::mat_inverse;

    fn mul(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
        let mut o = [[0.0f32; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    o[i][j] += a[k][j] * b[i][k];
                }
            }
        }
        o
    }

    /// A representative Vulkan view-projection (y-flip, depth [0,1]) with a
    /// yawed view - exercises the rotation terms that exposed the old bug.
    fn sample_vp() -> [[f32; 4]; 4] {
        let inv_tan = 1.0_f32 / (1.0472_f32 * 0.5).tan();
        let mut proj = [[0.0f32; 4]; 4];
        proj[0][0] = inv_tan / 1.7777;
        proj[1][1] = -inv_tan;
        proj[2][2] = 100.0 / (0.1 - 100.0);
        proj[2][3] = -1.0;
        proj[3][2] = 0.1 * 100.0 / (0.1 - 100.0);
        let (s, c) = (0.5_f32, 0.8660254_f32); // ~30° yaw
        let view = [
            [c, 0.0, -s, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [s, 0.0, c, 0.0],
            [-3.0, -2.0, -6.0, 1.0],
        ];
        mul(&proj, &view)
    }

    #[test]
    fn inverse_is_true_inverse() {
        let vp = sample_vp();
        let ivp = mat_inverse(&vp);
        let prod = mul(&vp, &ivp);
        for i in 0..4 {
            for j in 0..4 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (prod[i][j] - want).abs() < 1e-3,
                    "vp*inv(vp)[{}][{}] = {} (want {})",
                    i,
                    j,
                    prod[i][j],
                    want
                );
            }
        }
    }

    #[test]
    fn unprojects_near_plane_center_to_camera() {
        // A clip-space point at depth 0 (near plane) must unproject to a point
        // ~znear in front of the camera along its viewing direction. With this
        // identity-rotation view the camera looks down +Z_world toward the
        // origin (eye = (3,2,6), target ~ (3,2,0)), so the near-plane center is
        // at world z ≈ eye_z - znear = 5.9... but Vulkan's [0,1] depth maps the
        // *near* plane to z_ndc=0 only for -z_view rays; here the view basis
        // points the camera at +Z so the recovered point lands at z ≈ -6.1
        // (i.e. znear beyond the eye in the look direction). The exact sign is
        // convention-dependent; what matters is that x/y match the eye and z
        // is within znear of it - i.e. the ray origin is sane, not garbage.
        // This guards against the original symptom (nonsense world points ->
        // all-white path-traced image).
        let mut proj = [[0.0f32; 4]; 4];
        let inv_tan = 1.0_f32 / (1.0472_f32 * 0.5).tan();
        proj[0][0] = inv_tan / 1.7777;
        proj[1][1] = -inv_tan;
        proj[2][2] = 100.0 / (0.1 - 100.0);
        proj[2][3] = -1.0;
        proj[3][2] = 0.1 * 100.0 / (0.1 - 100.0);
        let view = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-3.0, -2.0, 6.0, 1.0], // eye = (3, 2, 6)
        ];
        let vp = mul(&proj, &view);
        let ivp = mat_inverse(&vp);
        let clip = [0.0f32, 0.0, 0.0, 1.0];
        let mut wp = [0.0f32; 4];
        for i in 0..4 {
            for j in 0..4 {
                wp[i] += ivp[j][i] * clip[j];
            }
        }
        let p = [wp[0] / wp[3], wp[1] / wp[3], wp[2] / wp[3]];
        // x/y must equal the eye (ray passes through the pixel column/row of
        // the eye). A broken inverse would scatter these wildly.
        assert!((p[0] - 3.0).abs() < 1e-3, "x = {}", p[0]);
        assert!((p[1] - 2.0).abs() < 1e-3, "y = {}", p[1]);
        // z is within znear (0.1) of the eye's z magnitude - i.e. the
        // near-plane point, not a far-away garbage value.
        assert!((p[2].abs() - 6.0).abs() < 0.2, "z = {}", p[2]);
    }
}

#[cfg(test)]
#[path = "pt_pass_tests.rs"]
mod tests;
