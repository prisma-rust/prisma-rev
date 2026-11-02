//! 网格 类型 顶点 + 索引 buffers on the GPU.
//!
//! A 网格 owns device-local vertex/index buffers and knows how to upload
//! data through a staging 缓冲区 The 顶点 格式 is interleaved
//! `(position, 法线 颜色 uv 切线 — see 顶点 uv + 切线
//! support the PBR 调试 法线 切线 空间 视图

use anyhow::Context as _;
use ash::vk;

use crate::buffer::{self, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;

/// A single 顶点 position + 法线 + 颜色 + uv + 切线 (interleaved).
/// 切线 is vec4: xyz = 切线 direction, w = handedness 符号 (+1/-1)
/// used to reconstruct the 副切线 as `cross(N, T) * tangent.w`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub color: [f32; 3],
    pub uv: [f32; 2],
    pub tangent: [f32; 4],
}

impl Vertex {
    /// 绑定 描述 one interleaved 顶点 缓冲区
    pub fn binding_description() -> vk::VertexInputBindingDescription {
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(std::mem::size_of::<Self>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)
    }

    /// 属性 descriptions:
    /// position (loc 0), 法线 (loc 1), 颜色 (loc 2), uv (loc 3), 切线 (loc 4).
    pub fn attribute_descriptions() -> [vk::VertexInputAttributeDescription; 5] {
        let f = std::mem::size_of::<f32>() as u32;
        let position = vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(0);
        let normal = vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(3 * f);
        let color = vk::VertexInputAttributeDescription::default()
            .location(2)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(6 * f);
        let uv = vk::VertexInputAttributeDescription::default()
            .location(3)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(9 * f);
        let tangent = vk::VertexInputAttributeDescription::default()
            .location(4)
            .binding(0)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .offset(11 * f);
        [position, normal, color, uv, tangent]
    }
}

/// Per-vertex skinning attributes (#560 P0-3), uploaded into a **second**
/// vertex buffer (binding 1).
///
/// Kept out of [`Vertex`] on purpose: unskinned meshes must not pay 24 extra
/// bytes per vertex, and the static-mesh pipeline does not declare this
/// binding at all.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SkinVertex {
    pub joints: [u16; 4],
    pub weights: [f32; 4],
}

impl SkinVertex {
    /// Binding 1, one entry per vertex.
    pub fn binding_description() -> vk::VertexInputBindingDescription {
        vk::VertexInputBindingDescription::default()
            .binding(1)
            .stride(std::mem::size_of::<Self>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)
    }

    /// joints (loc 5, `R16G16B16A16_UINT`), weights (loc 6, `R32G32B32A32_SFLOAT`).
    pub fn attribute_descriptions() -> [vk::VertexInputAttributeDescription; 2] {
        let joints = vk::VertexInputAttributeDescription::default()
            .location(5)
            .binding(1)
            .format(vk::Format::R16G16B16A16_UINT)
            .offset(0);
        let weights = vk::VertexInputAttributeDescription::default()
            .location(6)
            .binding(1)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .offset(8);
        [joints, weights]
    }
}

/// Usage flags for mesh vertex/index buffers.
///
/// `SHADER_DEVICE_ADDRESS` and `ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR`
/// are only legal when the device was created with the corresponding feature
/// and extension. Requesting them unconditionally makes buffer creation fail
/// on hardware without buffer-device-address or ray tracing support.
fn mesh_buffer_usage(context: &VulkanContext, base: BufferUsage) -> BufferUsage {
    let mut usage = base | BufferUsage::TRANSFER_DST;
    if context.rt_caps.buffer_device_address {
        usage |= BufferUsage::SHADER_DEVICE_ADDRESS;
    }
    if context.rt_caps.acceleration_structure {
        usage |= BufferUsage::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR;
    }
    usage
}

/// A GPU 网格 顶点 缓冲区 (+ optional 索引 缓冲区 and 绘制 metadata.
pub struct Mesh {
    pub vertex_buffer: vk::Buffer,
    pub vertex_memory: vk::DeviceMemory,
    pub vertex_count: u32,

    pub index_buffer: Option<vk::Buffer>,
    pub index_memory: Option<vk::DeviceMemory>,
    pub index_count: u32,
}

impl Mesh {
    /// 设备 address of the 顶点 缓冲区 (for 加速度 structure builds).
    /// Requires the 缓冲区 was created with `SHADER_DEVICE_ADDRESS` 用法
    pub fn vertex_buffer_device_address(&self, device: &ash::Device) -> vk::DeviceAddress {
        unsafe {
            device.get_buffer_device_address(
                &vk::BufferDeviceAddressInfo::default().buffer(self.vertex_buffer),
            )
        }
    }

    /// 设备 address of the 索引 缓冲区 (for 加速度 structure builds).
    /// Returns 0 if the 网格 has no 索引 缓冲区
    pub fn index_buffer_device_address(&self, device: &ash::Device) -> vk::DeviceAddress {
        self.index_buffer
            .map(|buf| unsafe {
                device
                    .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buf))
            })
            .unwrap_or(0)
    }

    /// 创建 a 网格 from a 切片 of 顶点 and (optional) indices.
    ///
    /// Uploads data through a temporary staging 缓冲区 The staging 命令
    /// 缓冲区 uses `command_pool` (which must belong to the graphics 队列
    /// family). After this returns the data is resident in device-local 内存
    pub fn new(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        vertices: &[Vertex],
        indices: Option<&[u32]>,
    ) -> anyhow::Result<Self> {
        let vertex_size = std::mem::size_of_val(vertices) as vk::DeviceSize;

        // 顶点 缓冲区 (device-local). On devices that support them, add
        // device-address + AS-build usages so the same buffer can feed BLAS
        // builds without reallocation.
        let (vertex_buffer, vertex_memory) = buffer::create_buffer(
            context,
            vertex_size,
            mesh_buffer_usage(context, BufferUsage::VERTEX_BUFFER),
            MemoryProperties::DEVICE_LOCAL,
        )
        .context("create vertex buffer")?;

        let vertex_bytes = unsafe {
            std::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                std::mem::size_of_val(vertices),
            )
        };
        unsafe {
            buffer::upload_to_buffer(
                context,
                command_pool,
                graphics_queue,
                vertex_buffer,
                vertex_size,
                vertex_bytes,
            )
        }
        .context("upload vertex data")?;

        // 索引 缓冲区 (optional).
        let (index_buffer, index_memory, index_count) = if let Some(indices) = indices {
            let index_size = std::mem::size_of_val(indices) as vk::DeviceSize;
            let (buf, mem) = buffer::create_buffer(
                context,
                index_size,
                mesh_buffer_usage(context, BufferUsage::INDEX_BUFFER),
                MemoryProperties::DEVICE_LOCAL,
            )
            .context("create index buffer")?;

            let index_bytes = unsafe {
                std::slice::from_raw_parts(
                    indices.as_ptr() as *const u8,
                    std::mem::size_of_val(indices),
                )
            };
            unsafe {
                buffer::upload_to_buffer(
                    context,
                    command_pool,
                    graphics_queue,
                    buf,
                    index_size,
                    index_bytes,
                )
            }
            .context("upload index data")?;

            (Some(buf), Some(mem), indices.len() as u32)
        } else {
            (None, None, 0)
        };

        Ok(Self {
            vertex_buffer,
            vertex_memory,
            vertex_count: vertices.len() as u32,
            index_buffer,
            index_memory,
            index_count,
        })
    }

    /// Like [`new`](Self::new) but records the staging copies into a
    /// [`BatchUploader`](crate::batch::BatchUploader) instead of submitting
    /// its own 命令 缓冲区 + 围栏 per upload. The 调用者 is responsible
    /// for finishing the uploader (which submits once and waits) before the
    /// 网格 is drawn.
    pub fn new_into(
        context: &VulkanContext,
        uploader: &mut crate::batch::BatchUploader<'_>,
        vertices: &[Vertex],
        indices: Option<&[u32]>,
    ) -> anyhow::Result<Self> {
        let vertex_size = std::mem::size_of_val(vertices) as vk::DeviceSize;

        let (vertex_buffer, vertex_memory) = buffer::create_buffer(
            context,
            vertex_size,
            mesh_buffer_usage(context, BufferUsage::VERTEX_BUFFER),
            MemoryProperties::DEVICE_LOCAL,
        )
        .context("create vertex buffer")?;

        let vertex_bytes = unsafe {
            std::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                std::mem::size_of_val(vertices),
            )
        };
        uploader
            .upload_buffer(vertex_buffer, vertex_size, vertex_bytes)
            .context("batch upload vertex data")?;

        let (index_buffer, index_memory, index_count) = if let Some(indices) = indices {
            let index_size = std::mem::size_of_val(indices) as vk::DeviceSize;
            let (buf, mem) = buffer::create_buffer(
                context,
                index_size,
                mesh_buffer_usage(context, BufferUsage::INDEX_BUFFER),
                MemoryProperties::DEVICE_LOCAL,
            )
            .context("create index buffer")?;

            let index_bytes = unsafe {
                std::slice::from_raw_parts(
                    indices.as_ptr() as *const u8,
                    std::mem::size_of_val(indices),
                )
            };
            uploader
                .upload_buffer(buf, index_size, index_bytes)
                .context("batch upload index data")?;
            (Some(buf), Some(mem), indices.len() as u32)
        } else {
            (None, None, 0)
        };

        Ok(Self {
            vertex_buffer,
            vertex_memory,
            vertex_count: vertices.len() as u32,
            index_buffer,
            index_memory,
            index_count,
        })
    }

    /// 销毁 the GPU resources for this 网格
    ///
    /// # 安全性
    ///
    /// 设备 must be a 有效 `ash::Device` that created these resources.
    /// Must not be called while the 网格 is still in use by any submitted
    /// 命令 缓冲区
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe { device.destroy_buffer(self.vertex_buffer, None) };
        unsafe { device.free_memory(self.vertex_memory, None) };
        if let Some(buf) = self.index_buffer.take() {
            unsafe { device.destroy_buffer(buf, None) };
        }
        if let Some(mem) = self.index_memory.take() {
            unsafe { device.free_memory(mem, None) };
        }
    }
}

#[cfg(test)]
#[path = "mesh_tests.rs"]
mod tests;
