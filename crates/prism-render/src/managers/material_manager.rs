//! `RenderMaterialManager` — PBR 材质 槽 池 with a per-FIF 设备
//! SSBO.
//!
//! Each [`MaterialData`] the engine hands in gets a 稳定 槽 索引 in
//! the 材质 SSBO. The 槽 is what the 着色器 uses to look 上 the
//! 材质 parameters; the 材质 handle itself is just CPU-side
//! identity used by the engine to translate 资源 材质 handles
//! into a render-side handle.
//!
//! ## 布局
//!
//! `GpuMaterial` is a `#[repr(C)]` POD 结构体 the 着色器 mirrors exactly.
//! The 总计 大小 and field offsets are pinned by a compile-time assertion
//! so changes to the Rust 结构体 also require updating the 着色器 (and
//! vice versa).
//!
//! ## 同步
//!
//! P0: one 材质 SSBO updated synchronously when `upload` is called.
//! No double-buffering — the 渲染器 is expected to 调用 `upload` after
//! all 材质 mutations for a 帧 are done, before the frame's
//! `cmd_draw_indexed` calls start. A future pass splits the 存储 into
//! a per-FIF pair to 重叠 CPU upload with GPU consumption.

use anyhow::Context as _;
use ash::vk;
use ash::vk::Handle as _;
use slotmap::{new_key_type, SlotMap};

use crate::buffer::{self, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;

/// 最大 number of materials. Caps the SSBO 大小 at 1024 entries; beyond
/// that the 渲染器 logs a 警告 and stops allocating new slots. The
/// number is deliberately small in P0 — a real production engine would
/// 大小 this from a 配置 设置
pub const MATERIAL_SSBO_MAX: u32 = 1024;

new_key_type! {
    /// Slotmap handle into [`RenderMaterialManager`].
    pub struct MaterialHandle;
}

/// Shader-visible 材质 record. The Slang `GpuMaterial` 结构体 in
/// `shaders/slang/scene_frag.slang` mirrors this exactly; field order and
/// 大小 are pinned by the 静态 assertion below.
///
/// 布局 (96 字节 16-byte aligned):
///   @0   base_color[4]                          (float4)
///   @16  metallic_roughness_emissive[4]          (float4: x=metallic, y=roughness, z=emissive, w=emissive_strength)
///   @32  albedo_idx, normal_idx, mr_idx, emissive_idx  (4 x uint)
///   @48  transmission_factor[4]                  (float4: x=transmission, y=ior, z=translucency, w=anisotropy)
///   @64  clearcoat[4]                            (float4: x=clearcoat, y=clearcoat_roughness, z=reserved, w=reserved)
///   @80  transmission_tex_idx, occlusion_idx     (2 x uint)
/// @88 normal_scale, occlusion_strength (2 x 浮点数 padded to 96)
#[repr(C, align(16))]
#[derive(Copy, Clone, Debug)]
pub struct GpuMaterial {
    /// Linear-space base 颜色 RGBA When an albedo 纹理 is bound it is
    /// created as `R8G8B8A8_SRGB`, so the hardware converts the sampled value
    /// to 线性 and no manual sRGB 解码 is needed in the 着色器 this 标量
    /// factor is always 线性 (per glTF spec).
    pub base_color: [f32; 4],
    /// Packed metallic/roughness/emissive/emissive_strength: x=metallic,
    /// y=roughness, z=emissive intensity, w=emissive_strength multiplier.
    pub metallic_roughness_emissive: [f32; 4],
    /// Bindless SRV 槽 of the albedo (base 颜色 纹理 Use
    /// `TextureHandle::INVALID.0` for "no 纹理 use the 标量
    /// base_color" - the 着色器 will fall 后 to the 标量
    pub albedo_idx: u32,
    /// Bindless SRV 槽 of the tangent-space 法线 映射表 无效 for
    /// "no 法线 映射表 - the 着色器 uses the geometric 法线
    pub normal_idx: u32,
    /// Bindless SRV 槽 of the packed metallic-roughness 纹理
    /// (glTF: G=roughness, B=metallic). 无效 for "use the 标量
    /// metallic + roughness fields".
    pub metallic_roughness_idx: u32,
    /// Bindless SRV 槽 of the emissive 纹理 无效 for "use the
    /// 标量 emissive field".
    pub emissive_idx: u32,
    // ---- 秒 48-byte 块 (advanced PBR) ----
    /// Packed transmission/ior/translucency/anisotropy.
    /// x=transmission factor, y=index of refraction, z=translucency, w=anisotropy.
    pub transmission_factor: [f32; 4],
    /// Packed clearcoat parameters.
    /// x=clearcoat factor, y=clearcoat roughness, z=reserved, w=reserved.
    pub clearcoat: [f32; 4],
    /// Bindless SRV 槽 of the transmission 纹理 (reserved, 0xFFFFFFFF if none).
    pub transmission_tex_idx: u32,
    /// Bindless SRV 槽 of the 遮挡 纹理 (R 通道 0xFFFFFFFF if
    /// none; the 着色器 attenuates IBL diffuse+specular by `mix(1, tex.r,
    /// occlusion_strength)` and never touches direct lighting.
    pub occlusion_idx: u32,
    /// 法线 映射表 strength (scales decoded tangent-space 法线 xy before
    /// 归一化 1.0 = verbatim.
    pub normal_scale: f32,
    /// 遮挡 strength 插值 factor between 1.0 and the 遮挡 texture's
    /// R 通道 applied to IBL diffuse+specular only). Defaults to 1.0.
    pub occlusion_strength: f32,
}

// 静态 assertions for 大小 and 对齐
const _: [(); 96] = [(); std::mem::size_of::<GpuMaterial>()];
const _: [(); 16] = [(); std::mem::align_of::<GpuMaterial>()];

/// Plain-data 材质 描述 used at the 管理器 boundary. The
/// engine 层 translates `prism_asset::MaterialData` into this; the
/// four optional 纹理 slots carry the render-side bindless SRV 槽
/// (or `u32::MAX` for "no 纹理
#[derive(Debug, Clone)]
pub struct MaterialUploadInput {
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub albedo_tex: Option<u32>,
    pub normal_tex: Option<u32>,
    pub metallic_roughness_tex: Option<u32>,
    pub emissive_tex: Option<u32>,
    pub occlusion_tex: Option<u32>,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
    // Advanced PBR fields
    pub transmission: f32,
    pub ior: f32,
    pub translucency: f32,
    pub anisotropy: f32,
    pub clearcoat: f32,
    pub clearcoat_roughness: f32,
    pub emissive_strength: f32,
}

impl MaterialUploadInput {
    /// 打包 标量 parameters into a [`GpuMaterial`]. The 纹理
    /// indices are 左 at `u32::MAX` when `None`.
    pub fn to_gpu(&self) -> GpuMaterial {
        GpuMaterial {
            base_color: self.base_color,
            metallic_roughness_emissive: [
                self.metallic,
                self.roughness,
                self.emissive[0],
                self.emissive_strength,
            ],
            albedo_idx: self.albedo_tex.unwrap_or(u32::MAX),
            normal_idx: self.normal_tex.unwrap_or(u32::MAX),
            metallic_roughness_idx: self.metallic_roughness_tex.unwrap_or(u32::MAX),
            emissive_idx: self.emissive_tex.unwrap_or(u32::MAX),
            transmission_factor: [
                self.transmission,
                self.ior,
                self.translucency,
                self.anisotropy,
            ],
            clearcoat: [self.clearcoat, self.clearcoat_roughness, 0.0, 0.0],
            transmission_tex_idx: u32::MAX,
            occlusion_idx: self.occlusion_tex.unwrap_or(u32::MAX),
            normal_scale: self.normal_scale,
            occlusion_strength: self.occlusion_strength,
        }
    }
}

/// 管理器 of GPU materials. Holds a 槽 池 + a single device-local
/// 存储 缓冲区 that the 材质 SSBO 描述符 references.
pub struct RenderMaterialManager {
    /// Slotmap-typed CPU handles; 索引 in this 映射表 is *not* the SSBO
    /// 槽 — use `slot_of()` to translate.
    materials: SlotMap<MaterialHandle, MaterialUploadInput>,
    /// 反转 索引 from SSBO 槽 → 材质 handle. `slots[slot]`
    /// is the handle currently occupying that 槽 or `None` if free.
    slots: Vec<Option<MaterialHandle>>,
    /// Free-list of SSBO 槽 indices.
    free_list: Vec<u32>,
    /// The 材质 SSBO (device-local, STORAGE_BUFFER 用法 Sized to
    /// `MATERIAL_SSBO_MAX * size_of::<GpuMaterial>()`.
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Cached 视图 of the SSBO contents, indexed by 槽 Uploaded into
    /// 缓冲区 when `upload` is called.
    gpu_data: Vec<GpuMaterial>,
    /// Dirty bits: `dirty_slots[slot] = true` means the GPU data at
    /// this 槽 needs to be re-uploaded.
    dirty_slots: Vec<bool>,
    destroyed: bool,
}

impl RenderMaterialManager {
    /// Allocate the 材质 SSBO. The 缓冲区 is initialized to 零
    /// (all slots 无效 until populated).
    pub fn new(context: &VulkanContext) -> anyhow::Result<Self> {
        let slot_size = std::mem::size_of::<GpuMaterial>() as vk::DeviceSize;
        let total = slot_size * (MATERIAL_SSBO_MAX as vk::DeviceSize);

        let (buffer, memory) = buffer::create_buffer(
            context,
            total,
            BufferUsage::STORAGE_BUFFER | BufferUsage::TRANSFER_DST,
            MemoryProperties::DEVICE_LOCAL,
        )
        .context("RenderMaterialManager::new: create SSBO")?;

        let gpu_data = vec![
            GpuMaterial {
                base_color: [0.0; 4],
                metallic_roughness_emissive: [0.0; 4],
                albedo_idx: u32::MAX,
                normal_idx: u32::MAX,
                metallic_roughness_idx: u32::MAX,
                emissive_idx: u32::MAX,
                transmission_factor: [0.0; 4],
                clearcoat: [0.0; 4],
                transmission_tex_idx: u32::MAX,
                occlusion_idx: u32::MAX,
                normal_scale: 1.0,
                occlusion_strength: 1.0,
            };
            MATERIAL_SSBO_MAX as usize
        ];
        let dirty_slots = vec![true; MATERIAL_SSBO_MAX as usize];
        let free_list: Vec<u32> = (0..MATERIAL_SSBO_MAX).rev().collect();

        Ok(Self {
            materials: SlotMap::with_key(),
            slots: vec![None; MATERIAL_SSBO_MAX as usize],
            free_list,
            buffer,
            memory,
            gpu_data,
            dirty_slots,
            destroyed: false,
        })
    }

    /// Register a new 材质 and return its handle. The 槽 is taken
    /// from the free 列表 if the 池 is exhausted, an 错误 is returned
    /// and the handle is not assigned.
    pub fn register(&mut self, data: MaterialUploadInput) -> anyhow::Result<MaterialHandle> {
        let slot = self
            .free_list
            .pop()
            .ok_or_else(|| anyhow::anyhow!("RenderMaterialManager: pool exhausted"))?;
        self.gpu_data[slot as usize] = data.to_gpu();
        self.dirty_slots[slot as usize] = true;
        let handle = self.materials.insert(data);
        self.slots[slot as usize] = Some(handle);
        Ok(handle)
    }

    /// 更新 an existing 材质 in place. Marks its 槽 dirty so the
    /// 下一个 `upload` re-writes the SSBO.
    pub fn update(
        &mut self,
        handle: MaterialHandle,
        data: MaterialUploadInput,
    ) -> anyhow::Result<()> {
        let slot = self.slot_of(handle).ok_or_else(|| {
            anyhow::anyhow!("RenderMaterialManager::update: unknown handle {handle:?}")
        })?;
        self.gpu_data[slot as usize] = data.to_gpu();
        self.dirty_slots[slot as usize] = true;
        self.materials[handle] = data;
        Ok(())
    }

    /// Translate a CPU handle to its SSBO 槽 Returns `None` if the
    /// handle is unknown (it has been removed, or was never registered).
    pub fn slot_of(&self, handle: MaterialHandle) -> Option<u32> {
        self.slots
            .iter()
            .position(|h| *h == Some(handle))
            .map(|i| i as u32)
    }

    /// Underlying Vulkan 缓冲区 The 描述符 集合 the 渲染器 builds
    /// references this 缓冲区 at `materials_binding`.
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Uploads all dirty slots to the 设备 P0 实现 uploads
    /// the entire SSBO (cheap because the 缓冲区 is small — 1024 * 48B =
    /// 48KB). A future pass uploads only the dirty range and keeps a
    /// per-FIF pair.
    pub fn upload(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
    ) -> anyhow::Result<()> {
        // Use a tiny staging 缓冲区 to 写入 the entire SSBO. We don't
        // bother with the dirty range 优化 yet because the
        // upload 大小 is so small (48KB) that the savings are 噪声
        let total_size = self.gpu_data.len() * std::mem::size_of::<GpuMaterial>();
        let bytes =
            unsafe { std::slice::from_raw_parts(self.gpu_data.as_ptr() as *const u8, total_size) };
        unsafe {
            buffer::upload_to_buffer(
                context,
                command_pool,
                graphics_queue,
                self.buffer,
                total_size as vk::DeviceSize,
                bytes,
            )
        }
        .context("RenderMaterialManager::upload")?;
        // 清空 dirty bits; everything is now on the GPU.
        for d in self.dirty_slots.iter_mut() {
            *d = false;
        }
        Ok(())
    }

    /// 释放 a 材质 槽 后 to the free 列表
    pub fn unregister(&mut self, handle: MaterialHandle) {
        if let Some(slot) = self.slot_of(handle) {
            // Reset the GPU data so a future register at this 槽
            // doesn't leak old values.
            self.gpu_data[slot as usize] = GpuMaterial {
                base_color: [0.0; 4],
                metallic_roughness_emissive: [0.0; 4],
                albedo_idx: u32::MAX,
                normal_idx: u32::MAX,
                metallic_roughness_idx: u32::MAX,
                emissive_idx: u32::MAX,
                transmission_factor: [0.0; 4],
                clearcoat: [0.0; 4],
                transmission_tex_idx: u32::MAX,
                occlusion_idx: u32::MAX,
                normal_scale: 1.0,
                occlusion_strength: 1.0,
            };
            self.dirty_slots[slot as usize] = true;
            self.slots[slot as usize] = None;
            self.free_list.push(slot);
        }
        self.materials.remove(handle);
    }

    /// 按 **SSBO 槽位** 释放（#495 热重载 / 实体销毁）。
    ///
    /// 组件里存的是 `MaterialRef.material_slot`，不是渲染器的句柄，所以这里
    /// 用槽位反查句柄再走 `unregister`（复位 GPU 数据 + 归还空闲表）。
    pub fn unregister_slot(&mut self, slot: u32) -> bool {
        let Some(handle) = self.slots.get(slot as usize).copied().flatten() else {
            return false;
        };
        self.unregister(handle);
        true
    }

    /// 释放 every 材质 Idempotent.
    pub fn destroy(&mut self, device: &ash::Device) {
        for (_, _) in self.materials.drain() {
            // no per-material GPU 状态 to 释放
        }
        self.slots.iter_mut().for_each(|s| *s = None);
        self.free_list = (0..MATERIAL_SSBO_MAX).rev().collect();
        if !self.buffer.is_null() {
            unsafe { device.destroy_buffer(self.buffer, None) };
            self.buffer = vk::Buffer::null();
        }
        if !self.memory.is_null() {
            unsafe { device.free_memory(self.memory, None) };
            self.memory = vk::DeviceMemory::null();
        }
        self.destroyed = true;
    }

    pub fn len(&self) -> usize {
        self.materials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.materials.is_empty()
    }

    /// 驻留材质槽数与估算字节数（#518 Phase 2）。
    pub fn stats(&self) -> (usize, u64) {
        let count = self.materials.len();
        (
            count,
            count as u64 * std::mem::size_of::<GpuMaterial>() as u64,
        )
    }
}

impl Drop for RenderMaterialManager {
    fn drop(&mut self) {
        debug_assert!(
            self.destroyed || self.materials.is_empty(),
            "RenderMaterialManager dropped without explicit destroy()"
        );
    }
}

#[cfg(test)]
#[path = "material_manager_tests.rs"]
mod tests;
