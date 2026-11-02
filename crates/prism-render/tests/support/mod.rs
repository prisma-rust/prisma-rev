//! Reusable headless GPU test harness.
//!
//! The existing GPU tests each rebuild the same Vulkan boilerplate. This
//! module keeps the common compute path in one place:
//!
//! ```ignore
//! let harness = GpuHarness::new().expect("Vulkan");
//! let result = harness.dispatch(
//!     include_bytes!(".../kernel.comp.spv"),
//!     c"main",
//!     &[BufferSpec::input(&input), BufferSpec::output(output_len)],
//!     &push_constants,
//!     [groups_x, groups_y, groups_z],
//! )?;
//! ```
//!
//! Buffers are host-visible for simplicity: this is a test harness, not the
//! production upload path. Every binding is a storage buffer at set 0, with
//! bindings assigned in slice order. An optional push-constant block is
//! exposed to the compute stage at offset 0.
//!
//! `GpuHarness::new()` returns `None` when no Vulkan driver is available so
//! tests can gracefully skip on hosts without a GPU (the same convention as
//! `headless_clear.rs`). In CI, `PRISM_REQUIRE_VALIDATION=1` makes a missing
//! validation layer a hard failure before this point.

use std::ffi::CStr;

use anyhow::{Context as _, Result};
use ash::vk;
use prism_render::buffer::{create_buffer, BufferUsage, MemoryProperties};
use prism_render::VulkanContext;

/// One storage-buffer binding for [`GpuHarness::dispatch`].
#[derive(Clone, Copy)]
pub struct BufferSpec<'a> {
    /// Initial contents. Empty for an output-only buffer.
    pub data: &'a [u8],
    /// Bytes to read back after the dispatch. Zero means "input only".
    pub readback_bytes: usize,
}

impl<'a> BufferSpec<'a> {
    /// A read-only input buffer.
    pub fn input(data: &'a [u8]) -> Self {
        Self {
            data,
            readback_bytes: 0,
        }
    }

    /// A write-only output buffer with `size` bytes read back afterwards.
    pub fn output(size: usize) -> Self {
        Self {
            data: &[],
            readback_bytes: size,
        }
    }
}

struct AllocatedBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: vk::DeviceSize,
}

/// Headless Vulkan context plus a reusable command pool.
pub struct GpuHarness {
    context: VulkanContext,
    command_pool: vk::CommandPool,
}

impl GpuHarness {
    /// Create a harness, or return `None` when the host has no Vulkan driver.
    pub fn new() -> Option<Self> {
        let context = match VulkanContext::new(&[]) {
            Ok(context) => context,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                    || message.contains("VK_ERROR_EXTENSION_NOT_PRESENT")
                    || message.contains("no devices")
                    || message.contains("No valid")
                    || message.contains("PhysicalDevice")
                    || message.contains("failed to load Vulkan loader")
                {
                    eprintln!("[SKIP] no Vulkan driver: {error:#}");
                    return None;
                }
                panic!("VulkanContext::new failed: {error:#}");
            }
        };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { context.device.create_command_pool(&pool_info, None) }
            .expect("create compute command pool");

        Some(Self {
            context,
            command_pool,
        })
    }

    /// Compile-time SPIR-V + runtime binding, dispatch, readback.
    ///
    /// `buffers` are bound to set 0 in order (`binding = index`), all as
    /// `VK_DESCRIPTOR_TYPE_STORAGE_BUFFER`. `push_constants` may be empty.
    /// The result has one entry per buffer; only buffers with
    /// `readback_bytes > 0` contain data.
    pub fn dispatch(
        &self,
        spirv: &[u8],
        entry_point: &CStr,
        buffers: &[BufferSpec<'_>],
        push_constants: &[u8],
        groups: [u32; 3],
    ) -> Result<Vec<Vec<u8>>> {
        assert!(!buffers.is_empty(), "dispatch needs at least one buffer");
        assert!(
            spirv.len().is_multiple_of(4),
            "SPIR-V length must be a multiple of 4"
        );
        assert!(
            push_constants.len().is_multiple_of(4),
            "push constants must be 4-byte aligned"
        );

        let device = &self.context.device;

        let shader_module = prism_render::shader::load_shader_module(device, spirv)
            .context("load harness shader")?;

        let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..buffers.len())
            .map(|index| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(index as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            })
            .collect();
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }
            .context("create descriptor set layout")?;

        let push_range = if push_constants.is_empty() {
            None
        } else {
            Some(
                vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
                    .offset(0)
                    .size(push_constants.len() as u32),
            )
        };
        let push_ranges: Vec<vk::PushConstantRange> = push_range.into_iter().collect();
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&set_layout))
            .push_constant_ranges(&push_ranges);
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pipeline_layout_info, None) }
            .context("create pipeline layout")?;

        let stage = prism_render::shader::shader_stage(
            vk::ShaderStageFlags::COMPUTE,
            shader_module,
            entry_point,
        );
        let pipeline_info = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout);
        let pipeline = unsafe {
            device.create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        }
        .map_err(|(_, error)| anyhow::anyhow!("create compute pipeline: {error:?}"))?
        .into_iter()
        .next()
        .context("create_compute_pipelines returned no pipeline")?;

        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(buffers.len() as u32);
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(std::slice::from_ref(&pool_size));
        let descriptor_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .context("create descriptor pool")?;

        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(std::slice::from_ref(&set_layout));
        let descriptor_sets =
            unsafe { device.allocate_descriptor_sets(&allocate_info) }.context("allocate set")?;
        let descriptor_set = descriptor_sets[0];

        let mut allocated = Vec::with_capacity(buffers.len());
        let mut buffer_infos = Vec::with_capacity(buffers.len());
        for spec in buffers {
            let size = spec.data.len().max(spec.readback_bytes).max(4) as vk::DeviceSize;
            let (buffer, memory) = create_buffer(
                &self.context,
                size,
                BufferUsage::STORAGE_BUFFER,
                MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
            )
            .context("create harness storage buffer")?;

            if !spec.data.is_empty() {
                let mapped =
                    unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
                        .context("map input buffer")?;
                unsafe {
                    std::ptr::write_bytes(mapped as *mut u8, 0, size as usize);
                    std::ptr::copy_nonoverlapping(
                        spec.data.as_ptr(),
                        mapped as *mut u8,
                        spec.data.len(),
                    );
                    device.unmap_memory(memory);
                }
            }

            buffer_infos.push(
                vk::DescriptorBufferInfo::default()
                    .buffer(buffer)
                    .offset(0)
                    .range(size),
            );
            allocated.push(AllocatedBuffer {
                buffer,
                memory,
                size,
            });
        }

        let writes: Vec<vk::WriteDescriptorSet> = buffer_infos
            .iter()
            .enumerate()
            .map(|(index, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(index as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(info))
            })
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        let command_buffer = {
            let allocate_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            unsafe { device.allocate_command_buffers(&allocate_info) }
                .context("allocate command buffer")?[0]
        };
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .context("create dispatch fence")?;

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { device.begin_command_buffer(command_buffer, &begin_info) }
            .context("begin dispatch command buffer")?;
        unsafe {
            device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, pipeline);
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                pipeline_layout,
                0,
                &[descriptor_set],
                &[],
            );
            if !push_constants.is_empty() {
                device.cmd_push_constants(
                    command_buffer,
                    pipeline_layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    push_constants,
                );
            }
            device.cmd_dispatch(command_buffer, groups[0], groups[1], groups[2]);
            let barrier = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ);
            device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[barrier],
                &[],
                &[],
            );
            device.end_command_buffer(command_buffer)
        }
        .context("record dispatch command buffer")?;

        let command_buffers = [command_buffer];
        let submit_info = vk::SubmitInfo::default().command_buffers(&command_buffers);
        unsafe { device.queue_submit(self.context.graphics_queue, &[submit_info], fence) }
            .context("submit dispatch")?;
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }.context("wait for dispatch")?;

        let mut results = Vec::with_capacity(buffers.len());
        for (spec, buffer) in buffers.iter().zip(&allocated) {
            if spec.readback_bytes == 0 {
                results.push(Vec::new());
                continue;
            }
            let mapped = unsafe {
                device.map_memory(buffer.memory, 0, buffer.size, vk::MemoryMapFlags::empty())
            }
            .context("map output buffer")?;
            let mut bytes = vec![0u8; spec.readback_bytes];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    mapped as *const u8,
                    bytes.as_mut_ptr(),
                    spec.readback_bytes,
                );
                device.unmap_memory(buffer.memory);
            }
            results.push(bytes);
        }

        unsafe {
            device.destroy_fence(fence, None);
            device.free_command_buffers(self.command_pool, &[command_buffer]);
            device.destroy_descriptor_pool(descriptor_pool, None);
            device.destroy_pipeline(pipeline, None);
            device.destroy_pipeline_layout(pipeline_layout, None);
            device.destroy_descriptor_set_layout(set_layout, None);
            device.destroy_shader_module(shader_module, None);
            for buffer in allocated {
                device.destroy_buffer(buffer.buffer, None);
                device.free_memory(buffer.memory, None);
            }
        }

        Ok(results)
    }
}

impl Drop for GpuHarness {
    fn drop(&mut self) {
        unsafe {
            self.context
                .device
                .destroy_command_pool(self.command_pool, None)
        };
    }
}
