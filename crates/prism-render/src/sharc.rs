//! NVIDIA RTXGI SHARC (Spatially Hashed Radiance Cache v1.6) integration.
//!
//! Provides world-space radiance caching for real-time path tracing:
//! - [`SharcTargets`]: owns the three GPU buffers (`hash_entries`, `accumulation`, `resolved`).
//! - [`SharcResolvePass`]: dispatches the 1D compute shader that resolves accumulated
//!   samples into the temporal resolved cache and handles cache eviction.

use anyhow::Context as _;
use ash::vk;
use std::ffi::CString;

use crate::buffer::create_buffer;
use crate::compute::ComputePipeline;
use crate::context::VulkanContext;
use crate::render_graph::{
    GiMode, GraphResources, PassInfo, PassKind, RenderContext, RenderGraphBuilder, RenderMode,
    RenderPassNode, RenderSettings, ResourceType, ResourceUsage, SHARC_ACCUMULATION_H,
    SHARC_HASH_ENTRIES_H, SHARC_RESOLVED_H,
};

pub const DEFAULT_SHARC_CAPACITY: u32 = 1024 * 1024; // 1M entries (~40MB GPU memory)

/// Whether the training dispatch should run this frame.
///
/// `SharcResolveEntry` in `assets/shaders/slang/sharc/sharc_common.slang` is the
/// only place that normalises the accumulation counts and clears the
/// accumulation buffer. Training with resolve disabled keeps adding to the same
/// `u32` counters while nothing consumes or resets them, so the counts saturate
/// and the cache ends up carrying unbounded radiance.
pub const fn training_enabled(update: bool, resolve: bool) -> bool {
    update && resolve
}

/// Why the world-space SHaRC cache stopped describing the scene.
///
/// The cache has two lifetimes (repair spec §8). The *screen* history —
/// accumulation, motion vectors — is invalidated by camera motion and by extent
/// changes. The *world* cache survives both of those: it stores radiance in
/// world space, so a moving camera only reprojects it. What it cannot survive
/// is the radiance field itself changing, which is what this struct records.
///
/// The host fills this in from its dirty flags and from the lifecycle hooks on
/// `crate::GraphRenderer` and from the frame's dirty dispatch plan;
/// `crate::GraphRenderer::execute` folds it into the per-frame clear decision and
/// then resets it to `Self::default`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharcInvalidation {
    /// Geometry or material semantics changed: a scene was unloaded, a mesh or
    /// material slot was released, or the host called
    /// `crate::GraphRenderer::invalidate_pt_geometry`.
    pub geometry: bool,
    /// The directional or a local light changed. Spec §8 asks for a hard clear
    /// here; a bounded refresh would need per-voxel timestamps the cache does
    /// not carry yet.
    pub lights: bool,
    /// The sky/IBL environment changed, so the illumination the cache was
    /// trained against is gone.
    pub sky: bool,
}

impl SharcInvalidation {
    /// Whether any reason is recorded, i.e. the cache must be cleared.
    pub const fn any(&self) -> bool {
        self.geometry || self.lights || self.sky
    }
}

/// The three persistent GPU buffers used by RTXGI SHARC.
pub struct SharcTargets {
    pub capacity: u32,
    pub hash_entries_buffer: vk::Buffer,
    pub hash_entries_memory: vk::DeviceMemory,
    pub accumulation_buffer: vk::Buffer,
    pub accumulation_memory: vk::DeviceMemory,
    pub resolved_buffer: vk::Buffer,
    pub resolved_memory: vk::DeviceMemory,
}

impl SharcTargets {
    pub fn new(context: &VulkanContext, capacity: u32) -> anyhow::Result<Self> {
        let usage = vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST;
        let props = vk::MemoryPropertyFlags::DEVICE_LOCAL;

        // Hash entries: 1 entry = uint64_t (8 bytes)
        let hash_size = (capacity as vk::DeviceSize) * 8;
        let (hash_entries_buffer, hash_entries_memory) =
            create_buffer(context, hash_size, usage, props)
                .context("create sharc hash entries buffer")?;

        // Accumulation: 1 entry = uint4 (16 bytes)
        let accum_size = (capacity as vk::DeviceSize) * 16;
        let (accumulation_buffer, accumulation_memory) =
            create_buffer(context, accum_size, usage, props)
                .context("create sharc accumulation buffer")?;

        // Resolved: 1 entry = float16_t4 radiance + 2x u32 sampleData (16 bytes)
        let resolved_size = (capacity as vk::DeviceSize) * 16;
        let (resolved_buffer, resolved_memory) =
            create_buffer(context, resolved_size, usage, props)
                .context("create sharc resolved buffer")?;

        Ok(Self {
            capacity,
            hash_entries_buffer,
            hash_entries_memory,
            accumulation_buffer,
            accumulation_memory,
            resolved_buffer,
            resolved_memory,
        })
    }

    /// Clear hash entries and accumulation buffer to zero on GPU.
    pub fn clear(&self, device: &ash::Device, cmd: vk::CommandBuffer) {
        unsafe {
            device.cmd_fill_buffer(cmd, self.hash_entries_buffer, 0, vk::WHOLE_SIZE, 0);
            device.cmd_fill_buffer(cmd, self.accumulation_buffer, 0, vk::WHOLE_SIZE, 0);
            device.cmd_fill_buffer(cmd, self.resolved_buffer, 0, vk::WHOLE_SIZE, 0);
        }
    }

    pub fn destroy(&mut self, context: &VulkanContext) {
        unsafe {
            context
                .device
                .destroy_buffer(self.hash_entries_buffer, None);
            context.device.free_memory(self.hash_entries_memory, None);
            context
                .device
                .destroy_buffer(self.accumulation_buffer, None);
            context.device.free_memory(self.accumulation_memory, None);
            context.device.destroy_buffer(self.resolved_buffer, None);
            context.device.free_memory(self.resolved_memory, None);
        }
    }
}

/// Push constant block matching `sharc_resolve.slang`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SharcResolvePush {
    pub camera_pos: [f32; 4],
    pub camera_pos_prev: [f32; 4],
    pub scene_scale: f32,
    pub capacity: u32,
    pub accumulation_frame_num: u32,
    pub stale_frame_num_max: u32,
    pub frame_index: u32,
    pub radiance_scale: f32,
    pub enable_anti_firefly: u32,
    pub pad: u32,
}

pub struct SharcResolvePass {
    pipeline: ComputePipeline,
    ds_layout: vk::DescriptorSetLayout,
    ds_pool: vk::DescriptorPool,
    ds: vk::DescriptorSet,
}

impl SharcResolvePass {
    pub fn new(context: &VulkanContext, targets: &SharcTargets) -> anyhow::Result<Self> {
        let device = &context.device;

        // Descriptor set layout: b0 = hash_entries, b1 = accumulation, b2 = resolved
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];

        let ds_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .context("SharcResolvePass: create ds layout")?;

        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: 3,
        }];
        let ds_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .context("SharcResolvePass: create ds pool")?;

        let ds = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(ds_pool)
                    .set_layouts(std::slice::from_ref(&ds_layout)),
            )
        }
        .context("SharcResolvePass: allocate ds")?[0];

        // Write descriptor set
        let hash_info = [vk::DescriptorBufferInfo {
            buffer: targets.hash_entries_buffer,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];
        let accum_info = [vk::DescriptorBufferInfo {
            buffer: targets.accumulation_buffer,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];
        let resolved_info = [vk::DescriptorBufferInfo {
            buffer: targets.resolved_buffer,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];

        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&hash_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&accum_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&resolved_info),
        ];
        unsafe {
            device.update_descriptor_sets(&writes, &[]);
        }

        // Pipeline
        const SPV: &[u8] = include_bytes!("../../../assets/shaders/sharc_resolve.comp.spv");
        let shader_module = crate::shader::load_shader_module(device, SPV)
            .context("SharcResolvePass: create shader module")?;

        let entry = CString::new("resolveMain").unwrap();
        let push_ranges = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: std::mem::size_of::<SharcResolvePush>() as u32,
        }];

        let pipeline = ComputePipeline::new(
            device,
            shader_module,
            entry.as_c_str(),
            &[ds_layout],
            &push_ranges,
        )
        .context("SharcResolvePass: create compute pipeline")?;

        unsafe {
            device.destroy_shader_module(shader_module, None);
        }

        Ok(Self {
            pipeline,
            ds_layout,
            ds_pool,
            ds,
        })
    }

    pub fn execute(&self, device: &ash::Device, cmd: vk::CommandBuffer, push: &SharcResolvePush) {
        unsafe {
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline.pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline.layout,
                0,
                &[self.ds],
                &[],
            );
            device.cmd_push_constants(
                cmd,
                self.pipeline.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(push),
            );
            let groups = (push.capacity + 255) / 256;
            device.cmd_dispatch(cmd, groups, 1, 1);
        }
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_descriptor_pool(self.ds_pool, None);
            device.destroy_descriptor_set_layout(self.ds_layout, None);
        }
    }
}

/// Whether the SHaRC cache is in use for this frame.
///
/// The cache has two consumers: the path tracer (trains it, then queries it)
/// and the raster ReSTIR GI pass (terminates its bounces in it). Neither exists
/// in the other mode, so the resolve has to run under exactly this predicate.
pub fn sharc_active(gi_mode: GiMode, render_mode: RenderMode) -> bool {
    match gi_mode {
        GiMode::Sharc => render_mode == RenderMode::PathTrace,
        GiMode::Restir => render_mode == RenderMode::Raster,
        _ => false,
    }
}

/// SHaRC accumulation resolve as a graph node.
///
/// The resolve reads what the training dispatch deposited and writes the
/// resolved cache the consumers query, so it has to be an ordered graph step
/// rather than a call buried inside the frame recorder (repair spec §4: no
/// invisible training or resolve outside the graph).
///
/// [`SharcTargets`] stay owned by `GraphRenderer`, which allocates and clears
/// them; the node only borrows them through the descriptor set it builds in
/// [`Self::set_targets`].
pub struct SharcResolveNode {
    pass: Option<SharcResolvePass>,
    capacity: u32,
    prev_camera_pos: Option<[f32; 4]>,
    dispatches: u64,
}

impl SharcResolveNode {
    pub fn new() -> Self {
        Self {
            pass: None,
            capacity: 0,
            prev_camera_pos: None,
            dispatches: 0,
        }
    }

    /// Bind the resolve to a (re)allocated cache. Replaces any previous
    /// descriptor set, so this is also the "capacity changed" path.
    pub fn set_targets(
        &mut self,
        context: &VulkanContext,
        targets: &SharcTargets,
    ) -> anyhow::Result<()> {
        let pass = SharcResolvePass::new(context, targets)?;
        self.pass = Some(pass);
        self.capacity = targets.capacity;
        self.prev_camera_pos = None;
        Ok(())
    }

    /// Drop the descriptor set and the cache size; used when the targets are
    /// reallocated or the renderer shuts down.
    pub fn clear(&mut self, device: &ash::Device) {
        if let Some(mut pass) = self.pass.take() {
            pass.destroy(device);
        }
        self.capacity = 0;
        self.prev_camera_pos = None;
    }

    /// Whether a resolve is currently bound to a cache.
    pub fn is_bound(&self) -> bool {
        self.pass.is_some() && self.capacity > 0
    }

    /// Forget the previous camera position.
    ///
    /// The resolve reprojects cached radiance through the camera delta, so a
    /// stale pair after a swapchain rebuild would smear a frame across the
    /// resize. Dropping the history makes the next resolve a no-op reprojection.
    pub fn reset_history(&mut self) {
        self.prev_camera_pos = None;
    }

    /// How many frames actually recorded a resolve dispatch.
    pub fn dispatches(&self) -> u64 {
        self.dispatches
    }
}

impl Default for SharcResolveNode {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPassNode for SharcResolveNode {
    fn name(&self) -> &str {
        "SharcResolve"
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        // The buffers are owned by GraphRenderer; the handles are declared so
        // the resolve is an explicit reader/writer of the cache in the graph
        // and runs after the trainer that wrote it.
        for handle in [SHARC_HASH_ENTRIES_H, SHARC_ACCUMULATION_H, SHARC_RESOLVED_H] {
            graph.create_resource_at(handle, ResourceType::StorageBuffer { size: 0 });
            graph.read_usage(ResourceUsage {
                handle,
                access: vk::AccessFlags::SHADER_READ,
                stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            });
            graph.write_usage(ResourceUsage {
                handle,
                access: vk::AccessFlags::SHADER_WRITE,
                stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            });
        }
    }

    fn execute(
        &mut self,
        ctx: &RenderContext,
        _resources: &mut GraphResources,
    ) -> anyhow::Result<()> {
        let camera_pos = ctx.frame.camera_pos;
        // The previous position is what the shader reprojects the cached
        // radiance with; it has to be the real previous frame, so it is
        // recorded here rather than reconstructed from the current frame.
        let camera_pos_prev = self.prev_camera_pos.unwrap_or(camera_pos);
        self.prev_camera_pos = Some(camera_pos);

        // The resolve only runs where the cache is actually in use: path-traced
        // SHaRC, or the raster ReSTIR GI variant that terminates its bounces in
        // the same cache.
        if !sharc_active(ctx.settings.gi_mode, ctx.frame.render_mode)
            || !ctx.settings.sharc_enable_resolve
        {
            return Ok(());
        }
        let Some(pass) = self.pass.as_ref() else {
            return Ok(());
        };
        if self.capacity == 0 {
            return Ok(());
        }

        let device = ctx.device;
        let cmd = ctx.cmd;

        // This frame's training dispatch just wrote the hash entries and the
        // accumulation buffer that resolve is about to read, and the two are
        // separate dispatches. Order them explicitly. With training disabled
        // the writes may come from an earlier frame, which this barrier also
        // covers.
        let prior_writes = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[prior_writes],
                &[],
                &[],
            );
        }

        let push = SharcResolvePush {
            camera_pos,
            camera_pos_prev,
            scene_scale: ctx.settings.sharc_scene_scale,
            capacity: self.capacity,
            accumulation_frame_num: ctx.settings.sharc_accumulation_frame_num,
            stale_frame_num_max: ctx.settings.sharc_stale_frame_num,
            frame_index: ctx.frame_index,
            radiance_scale: 1000.0,
            enable_anti_firefly: if ctx.settings.sharc_enable_anti_firefly {
                1
            } else {
                0
            },
            pad: 0,
        };
        pass.execute(device, cmd, &push);

        // The consumer reads the resolved cache from a later pass in the same
        // frame, so make the resolve writes visible to compute and fragment.
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[barrier],
                &[],
                &[],
            );
        }

        self.dispatches = self.dispatches.wrapping_add(1);
        Ok(())
    }

    fn graph_info(&self) -> PassInfo {
        PassInfo {
            index: usize::MAX,
            name: self.name().to_string(),
            kind: PassKind::SharcResolve,
            inputs: vec![SHARC_HASH_ENTRIES_H, SHARC_ACCUMULATION_H, SHARC_RESOLVED_H],
            outputs: vec![SHARC_RESOLVED_H],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sharc_resolve_push_layout() {
        assert_eq!(std::mem::size_of::<SharcResolvePush>(), 64);
        assert_eq!(std::mem::offset_of!(SharcResolvePush, camera_pos), 0);
        assert_eq!(std::mem::offset_of!(SharcResolvePush, camera_pos_prev), 16);
        assert_eq!(std::mem::offset_of!(SharcResolvePush, scene_scale), 32);
        assert_eq!(std::mem::offset_of!(SharcResolvePush, capacity), 36);
        assert_eq!(
            std::mem::offset_of!(SharcResolvePush, accumulation_frame_num),
            40
        );
        assert_eq!(
            std::mem::offset_of!(SharcResolvePush, stale_frame_num_max),
            44
        );
        assert_eq!(std::mem::offset_of!(SharcResolvePush, frame_index), 48);
        assert_eq!(std::mem::offset_of!(SharcResolvePush, radiance_scale), 52);
        assert_eq!(
            std::mem::offset_of!(SharcResolvePush, enable_anti_firefly),
            56
        );
        assert_eq!(std::mem::offset_of!(SharcResolvePush, pad), 60);
    }

    #[test]
    fn test_training_requires_resolve() {
        assert!(training_enabled(true, true));
        assert!(!training_enabled(true, false));
        assert!(!training_enabled(false, true));
        assert!(!training_enabled(false, false));
    }

    #[test]
    fn test_sharc_invalidation_any() {
        assert!(!SharcInvalidation::default().any());
        for reason in [
            SharcInvalidation {
                geometry: true,
                ..Default::default()
            },
            SharcInvalidation {
                lights: true,
                ..Default::default()
            },
            SharcInvalidation {
                sky: true,
                ..Default::default()
            },
        ] {
            assert!(reason.any(), "{reason:?} should count as an invalidation");
        }
    }

    #[test]
    fn test_sharc_buffer_sizes() {
        let capacity = DEFAULT_SHARC_CAPACITY;
        let hash_size = (capacity as u64) * 8;
        let accum_size = (capacity as u64) * 16;
        let resolved_size = (capacity as u64) * 16;
        let total = hash_size + accum_size + resolved_size;
        // 1M entries is exactly 40 MB
        assert_eq!(total, 40 * 1024 * 1024);
    }
}
