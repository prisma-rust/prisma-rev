//! 描述符集合布局、池和集合管理。
//!
//! 帧 UBO 位于描述符集 0、绑定 0（顶点+片元阶段）。
//! 每帧都有其自己的描述符集，这样无需管线停顿即可更新 UBO。

use anyhow::Context as _;
use ash::vk;

use crate::buffer::{self, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;

/// 相机 UBO 描述符集的布局（集合 = 0，绑定 = 0）。
/// Maximum number of cameras composited into a single frame (#461).
///
/// The per-frame UBO buffer holds one aligned block per camera slot, and the
/// forward pass binds slot `i` with a dynamic offset. Four is enough for the
/// Unity-style patterns this issue targets (game camera + UI overlay camera +
/// minimap + debug), and the memory cost is `4 * 352` bytes per frame.
pub const MAX_CAMERAS: usize = 4;

fn align_up(value: u64, alignment: u64) -> u64 {
    if alignment <= 1 {
        return value;
    }
    value.div_ceil(alignment) * alignment
}

pub struct DescriptorLayout {
    pub layout: vk::DescriptorSetLayout,
    /// Cloned 设备 handle kept so 放置 can free the 布局 (RAII).
    device: ash::Device,
}

impl DescriptorLayout {
    pub fn new(device: &ash::Device) -> anyhow::Result<Self> {
        // `UNIFORM_BUFFER_DYNAMIC` so the forward pass can point the same
        // descriptor set at a different camera slot (#461).
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)];

        let create_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let layout = unsafe { device.create_descriptor_set_layout(&create_info, None) }
            .context("create descriptor set layout")?;
        Ok(Self {
            layout,
            device: device.clone(),
        })
    }

    /// 创建 a 管线 布局 数组 with just this 布局 (for convenience).
    pub fn as_slice(&self) -> &[vk::DescriptorSetLayout] {
        std::slice::from_ref(&self.layout)
    }

    /// Combined set-0 布局 for the bindless PBR path:
    /// - 绑定 0: `FrameUBO` (UNIFORM_BUFFER, 顶点 | 片元
    /// - 绑定 1: materials `GpuMaterial` SSBO (STORAGE_BUFFER, 片元
    ///
    /// The legacy 管线 only reads 绑定 0; the extra 存储 绑定 is
    /// harmless there and required by the bindless 管线
    pub fn new_combined(device: &ash::Device) -> anyhow::Result<Self> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let create_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let layout = unsafe { device.create_descriptor_set_layout(&create_info, None) }
            .context("create combined descriptor set layout")?;
        Ok(Self {
            layout,
            device: device.clone(),
        })
    }
}

impl Drop for DescriptorLayout {
    fn drop(&mut self) {
        unsafe { self.device.destroy_descriptor_set_layout(self.layout, None) };
    }
}

/// 描述符 池 sized for `max_frames` 描述符 sets (each with 1 UBO).
pub struct DescriptorPool {
    pub pool: vk::DescriptorPool,
    /// Cloned 设备 handle kept so 放置 can free the 池 (RAII).
    device: ash::Device,
}

impl DescriptorPool {
    pub fn new(device: &ash::Device, max_frames: u32) -> anyhow::Result<Self> {
        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC,
            descriptor_count: max_frames,
        }];

        let create_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(max_frames)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { device.create_descriptor_pool(&create_info, None) }
            .context("create descriptor pool")?;
        Ok(Self {
            pool,
            device: device.clone(),
        })
    }

    /// Allocate one 描述符 集合 from the 池 for each 帧
    pub fn allocate_sets(
        &self,
        device: &ash::Device,
        layout: &DescriptorLayout,
        count: u32,
    ) -> anyhow::Result<Vec<vk::DescriptorSet>> {
        let layouts = vec![layout.layout; count as usize];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pool)
            .set_layouts(&layouts);
        let sets = unsafe { device.allocate_descriptor_sets(&alloc_info) }
            .context("allocate descriptor sets")?;
        Ok(sets)
    }

    /// 池 sized for `max_frames` combined (UBO + storage-buffer) sets, one
    /// per frame-in-flight, for the bindless PBR path.
    pub fn new_combined(device: &ash::Device, max_frames: u32) -> anyhow::Result<Self> {
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC,
                descriptor_count: max_frames,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: max_frames,
            },
        ];
        let create_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(max_frames)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { device.create_descriptor_pool(&create_info, None) }
            .context("create combined descriptor pool")?;
        Ok(Self {
            pool,
            device: device.clone(),
        })
    }
}

impl Drop for DescriptorPool {
    fn drop(&mut self) {
        unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
    }
}

/// 最大 number of **local** lights（点光 + 聚光）in the 光源 SSBO.
pub const LIGHT_MAX: u32 = 8;

/// [`GpuLight::color`] 的 `.w` 取值：点光源（全向）。
pub const GPU_LIGHT_KIND_POINT: f32 = 0.0;
/// [`GpuLight::color`] 的 `.w` 取值：聚光灯（锥形）。
pub const GPU_LIGHT_KIND_SPOT: f32 = 1.0;

/// GPU data 布局 for a single local 光源（点光或聚光，64 字节 / 16-byte aligned）。
///
/// Mirrors the Slang `GpuLight` 结构体 in `scene_frag.slang`.
/// Stored in a `StructuredBuffer<GpuLight>` at 集合 0 绑定 2.
///
/// 以前只有 `position` / `color` 两个 `float4`（32 字节，仅点光）；P3b 加了
/// `direction` / `params` 以支持聚光锥角 —— `.color.w` 是类型位（见
/// [`GPU_LIGHT_KIND_POINT`] / [`GPU_LIGHT_KIND_SPOT`]）。
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
pub struct GpuLight {
    /// xyz = world position, w = range (attenuation radius)
    pub position: [f32; 4],
    /// rgb = radiant intensity, w = kind ([`GPU_LIGHT_KIND_POINT`] / [`GPU_LIGHT_KIND_SPOT`])
    pub color: [f32; 4],
    /// xyz = 光源朝向（`transform.forward`）, w = cos(outer half-angle)
    pub direction: [f32; 4],
    /// x = cos(inner half-angle)；yzw 保留
    pub params: [f32; 4],
}

/// [`GpuLight`] 的字节大小（SSBO 步长，与着色器结构体必须一致）。
pub const GPU_LIGHT_SIZE: usize = std::mem::size_of::<GpuLight>();

/// 最大 number of analytic lights in the PT 光源 SSBO.
pub const PT_LIGHT_MAX: u32 = 64;

/// Kind discriminator for [`PtAnalyticLight`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtLightKind {
    Directional = 0,
    Point = 1,
    Spot = 2,
    Area = 3,
}

/// GPU data 布局 for a single path-tracer analytic 光源 (48 字节
///
/// Mirrors the Slang `PtAnalyticLight` 结构体 in `pt_render.slang`.
/// The `kind` field selects interpretation of the union-typed payload fields.
/// Stored in a `StructuredBuffer<PtAnalyticLight>` at PT 集合 0 绑定 8.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PtAnalyticLight {
    /// .xyz = 世界 position; .w = kind (PtLightKind as u32)
    pub position_kind: [f32; 4],
    /// .xyz = direction (for spot/directional); .w = inner_angle 余弦 (spot) or unused
    pub direction_params: [f32; 4],
    /// .xyz = linear-space radiance/color; .w = outer_angle 余弦 (spot) or range (point) or 面积
    pub color_params: [f32; 4],
}

impl PtAnalyticLight {
    pub fn directional(dir: [f32; 3], radiance: [f32; 3]) -> Self {
        Self {
            position_kind: [0.0; 4],
            direction_params: [dir[0], dir[1], dir[2], 1.0],
            color_params: [radiance[0], radiance[1], radiance[2], -1.0],
        }
    }
    pub fn point(pos: [f32; 3], color: [f32; 3], range: f32) -> Self {
        Self {
            position_kind: [pos[0], pos[1], pos[2], PtLightKind::Point as u32 as f32],
            direction_params: [0.0; 4],
            color_params: [color[0], color[1], color[2], range],
        }
    }
}

/// 最大 number of emissive triangles in the PT emissive SSBO 绑定 9).
pub const PT_EMISSIVE_MAX: u32 = 1024;

/// Per-pixel ReSTIR DI reservoir for temporal+spatial resampling of direct lights.
///
/// Stored in a ping-pong 缓冲区 (two `StructuredBuffer<ReSTIRReservoir>`) at
/// 集合 0 bindings 10 当前 写入 and 11 上一个 读取 Updated every
/// 帧 the path tracer reads 绑定 11 for temporal reuse, and writes
/// 绑定 10 for 下一个 frame's temporal reuse.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(non_snake_case)]
pub struct ReSTIRReservoir {
    pub light_idx: u32,  // 0=sun, 1..PT_LIGHT_MAX=analytic
    pub M: f32,          // effective sample count
    pub W: f32,          // sum of RIS weights (target_pdf / p_init)
    pub target_pdf: f32, // π(selected_light) for ReSTIR pdf = W/(M*π(y))
}

/// GPU data for a single emissive triangle 面积 光源 from emissive materials).
///
/// Each triangle is stored as 3 world-space 顶点 a shading 法线 the
/// pre-scaled emissive radiance, and the precomputed double-sided 面积
/// Stored in a `StructuredBuffer<PtEmissiveTri>` at PT 集合 0 绑定 9,
/// used for explicit emissive NEE in the path tracer.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PtEmissiveTri {
    pub v0: [f32; 4],       // .xyz = vertex 0
    pub v1: [f32; 4],       // .xyz = vertex 1
    pub v2: [f32; 4],       // .xyz = vertex 2
    pub normal: [f32; 4],   // .xyz = shading normal
    pub radiance: [f32; 4], // .rgb = pre-scaled emissive radiance
    pub area: f32,          // precomputed triangle area (for PDF)
}

/// GPU data 布局 for the per-frame uniform 缓冲区
///
/// Mirrors the Slang `FrameUBO` in `shaders/slang/common.slang` byte-for-byte
/// (std140). The RenderGraph ForwardPass reads `light_view_proj` here for the
/// shadow-map 投影 (keeping it out of 推送 constants so the 推送
/// 常量 块 stays under Vulkan's 128-byte 限制 the legacy shaders
/// simply ignore the trailing field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameUBOData {
    pub view_proj: [[f32; 4]; 4],       // 64 bytes, offset   0
    pub camera_position: [f32; 4],      // 16 bytes, offset  64 (xyz = camera pos, w = light_count)
    pub light_direction: [f32; 4], // 16 bytes, offset  80 (w = pre-scaled radiance = lux/(10000*PI))
    pub light_color: [f32; 4],     // 16 bytes, offset  96 (w = ambient factor)
    pub view: [[f32; 4]; 4],       // 64 bytes, offset 112 (world -> view)
    pub light_view_proj: [[f32; 4]; 4], // 64 bytes, offset 176 (light-space VP for shadow map)
    /// 色调映射 operator selector, applied to the final 高动态范围 颜色 before the
    /// sRGB 交换链 编码 0 = Reinhard (`x/(x+1)`), 1 = ACES (Narkowicz).
    /// Switchable at 运行时 from the 检查器 / `T` 调 偏移 240.
    pub tonemap_mode: u32, // offset 240
    /// Scene 颜色 视口 大小 in pixels (xy). Used by the 片元 着色器
    /// to derive screen-space UVs for sampling the screen-space 环境光遮蔽 纹理
    pub viewport_size: [f32; 2], // offset 244..251
    /// Exposure multiplier applied as a uniform 音阶 to the final composed 高动态范围
    /// 颜色 before tonemapping. 默认 1.0 = no scaling; 检查器 滑动条
    /// lets the user brighten/darken the entire 图像 uniformly. 偏移 252.
    pub exposure: f32, // offset 252
    /// 阴影强度 = `Light.shadowStrength`（0 = 无阴影，1 = 全强度）。
    ///
    /// 着色器在 `sample_shadow` 末尾用 `lerp(1, shadow, shadowStrength)` 应用。
    pub shadow_strength: f32, // offset 256
    /// std140 tail pad before the CSM fields.
    pub _pad2: [f32; 3], // offset 260..271
    /// Far cascade light-space VP (#526 CSM). The shadow map is a 2×1 atlas;
    /// cascade 0 occupies the left half and this matrix the right half.
    pub light_view_proj1: [[f32; 4]; 4], // offset 272..335
    /// Camera-forward distance where cascade 0 ends and cascade 1 starts.
    pub shadow_split: f32, // offset 336
    /// 1 = single shadow map (legacy), 2 = two-cascade atlas.
    pub shadow_cascade_count: u32, // offset 340
    /// 补到 352 字节（std140 要求 16 字节对齐）。
    pub _pad3: [f32; 2], // offset 344..351
}

/// Per-frame UBO 缓冲区 and its 描述符 集合
///
/// The buffer holds [`MAX_CAMERAS`] aligned slots (one per composited camera).
/// `descriptor_set` covers one slot; the forward pass supplies the slot offset
/// with `vkCmdBindDescriptorSets` dynamic offsets (#461).
pub struct FrameUBO {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    /// Size of one camera block (`size_of::<FrameUBOData>()`).
    pub size: vk::DeviceSize,
    /// Byte stride between camera blocks (aligned to
    /// `minUniformBufferOffsetAlignment`).
    pub stride: vk::DeviceSize,
    pub descriptor_set: vk::DescriptorSet,
    /// Cloned 设备 handle kept so 放置 can free the 缓冲区 + 内存 (RAII).
    device: ash::Device,
}

impl FrameUBO {
    /// 创建 a UBO 缓冲区 and 更新 the 描述符 集合 to point to it.
    pub fn new(context: &VulkanContext, descriptor_set: vk::DescriptorSet) -> anyhow::Result<Self> {
        let size = std::mem::size_of::<FrameUBOData>() as vk::DeviceSize; // 352
        let alignment = context
            .physical_device_properties
            .limits
            .min_uniform_buffer_offset_alignment
            .max(1);
        let stride = align_up(size, alignment);
        let total = stride * MAX_CAMERAS as u64;

        let (buffer, memory) = buffer::create_buffer(
            context,
            total,
            BufferUsage::UNIFORM_BUFFER,
            MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
        )
        .context("create frame UBO buffer")?;

        // 更新 描述符 集合
        let buffer_info = vk::DescriptorBufferInfo::default()
            .buffer(buffer)
            .offset(0)
            .range(size);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC)
            .buffer_info(std::slice::from_ref(&buffer_info));
        unsafe { context.device.update_descriptor_sets(&[write], &[]) };

        Ok(Self {
            buffer,
            memory,
            size,
            stride,
            descriptor_set,
            device: context.device.clone(),
        })
    }

    /// Dynamic-offset value for camera `slot` (#461).
    pub fn camera_offset(&self, slot: usize) -> u32 {
        (self.stride * slot.min(MAX_CAMERAS - 1) as u64) as u32
    }

    /// Upload new 帧 data to the GPU for camera `slot`.
    pub fn update(
        &self,
        device: &ash::Device,
        data: &FrameUBOData,
        slot: usize,
    ) -> anyhow::Result<()> {
        let slot = slot.min(MAX_CAMERAS - 1);
        let ptr = unsafe {
            device.map_memory(
                self.memory,
                self.stride * slot as u64,
                self.size,
                vk::MemoryMapFlags::empty(),
            )
        }
        .context("map frame UBO memory")?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                data as *const _ as *const u8,
                ptr as *mut u8,
                self.size as usize,
            );
        }
        unsafe { device.unmap_memory(self.memory) };
        Ok(())
    }
}

impl Drop for FrameUBO {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
#[path = "descriptor_tests.rs"]
mod tests;
