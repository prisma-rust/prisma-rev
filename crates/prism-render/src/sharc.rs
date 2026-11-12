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

pub const DEFAULT_SHARC_CAPACITY: u32 = 1024 * 1024; // 1M entries (~40MB GPU memory)

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
        let shader_info = vk::ShaderModuleCreateInfo::default().code(bytemuck::cast_slice(SPV));
        let shader_module = unsafe { device.create_shader_module(&shader_info, None) }
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
