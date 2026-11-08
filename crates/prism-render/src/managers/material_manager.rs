//! `RenderMaterialManager` — PBR material slot pool with a per-FIF device
//! SSBO.
//!
//! Each [`MaterialData`] the engine hands in gets a stable slot index in
//! the material SSBO. The slot is what the shader uses to look up the
//! material parameters; the material handle itself is just CPU-side
//! identity used by the engine to translate asset material handles
//! into a render-side handle.
//!
//! ## Layout
//!
//! `GpuMaterial` is a `#[repr(C)]` POD struct the shader mirrors exactly.
//! The total size and field offsets are pinned by a compile-time assertion
//! so changes to the Rust struct also require updating the shader (and
//! vice versa).
//!
//! ## Synchronization
//!
//! P0: one material SSBO updated synchronously when `upload` is called.
//! No double-buffering — the renderer is expected to call `upload` after
//! all material mutations for a frame are done, before the frame's
//! `cmd_draw_indexed` calls start. A future pass splits the storage into
//! a per-FIF pair to overlap CPU upload with GPU consumption.

use anyhow::Context as _;
use ash::vk;
use ash::vk::Handle as _;
use slotmap::{new_key_type, SlotMap};

use crate::buffer::{self, BufferUsage, MemoryProperties};
use crate::context::VulkanContext;

/// Maximum number of materials. Caps the SSBO size at 1024 entries; beyond
/// that the renderer logs a warning and stops allocating new slots. The
/// number is deliberately small in P0 — a real production engine would
/// size this from a config setting.
pub const MATERIAL_SSBO_MAX: u32 = 1024;

new_key_type! {
    /// Slotmap handle into [`RenderMaterialManager`].
    pub struct MaterialHandle;
}

/// Shader-visible material record. The Slang `GpuMaterial` struct in
/// `shaders/slang/scene_frag.slang` mirrors this exactly; field order and
/// size are pinned by the static assertion below.
///
/// Layout (96 bytes, 16-byte aligned):
///   @0   base_color[4]                          (float4)
///   @16  metallic_roughness_emissive[4]          (float4: x=metallic, y=roughness, z=emissive, w=emissive_strength)
///   @32  albedo_idx, normal_idx, mr_idx, emissive_idx  (4 x uint)
///   @48  transmission_factor[4]                  (float4: x=transmission, y=ior, z=translucency, w=anisotropy)
///   @64  clearcoat[4]                            (float4: x=clearcoat, y=clearcoat_roughness, z=reserved, w=reserved)
///   @80  transmission_tex_idx, occlusion_idx     (2 x uint)
///   @88  normal_scale, occlusion_strength         (2 x float, padded to 96)
#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuMaterial {
    /// Linear-space base color (rgba). When an albedo texture is bound it is
    /// created as `R8G8B8A8_SRGB`, so the hardware converts the sampled value
    /// to linear and no manual sRGB decode is needed in the shader; this scalar
    /// factor is always linear (per glTF spec).
    pub base_color: [f32; 4],
    /// Packed metallic/roughness/emissive/emissive_strength: x=metallic,
    /// y=roughness, z=emissive intensity, w=emissive_strength multiplier.
    pub metallic_roughness_emissive: [f32; 4],
    /// Bindless SRV slot of the albedo (base color) texture. Use
    /// `TextureHandle::INVALID.0` for "no texture, use the scalar
    /// base_color" - the shader will fall back to the scalar.
    pub albedo_idx: u32,
    /// Bindless SRV slot of the tangent-space normal map. `INVALID` for
    /// "no normal map" - the shader uses the geometric normal.
    pub normal_idx: u32,
    /// Bindless SRV slot of the packed metallic-roughness texture
    /// (glTF: G=roughness, B=metallic). `INVALID` for "use the scalar
    /// metallic + roughness fields".
    pub metallic_roughness_idx: u32,
    /// Bindless SRV slot of the emissive texture. `INVALID` for "use the
    /// scalar emissive field".
    pub emissive_idx: u32,
    // ---- Second 48-byte block (advanced PBR) ----
    /// Packed transmission/ior/translucency/anisotropy.
    /// x=transmission factor, y=index of refraction, z=translucency, w=anisotropy.
    pub transmission_factor: [f32; 4],
    /// Packed clearcoat parameters.
    /// x=clearcoat factor, y=clearcoat roughness, z=reserved, w=reserved.
    pub clearcoat: [f32; 4],
    /// Bindless SRV slot of the transmission texture (reserved, 0xFFFFFFFF if none).
    pub transmission_tex_idx: u32,
    /// Bindless SRV slot of the occlusion texture (R channel). 0xFFFFFFFF if
    /// none; the shader attenuates IBL diffuse+specular by `mix(1, tex.r,
    /// occlusion_strength)` and never touches direct lighting.
    pub occlusion_idx: u32,
    /// Normal map strength (scales decoded tangent-space normal xy before
    /// normalize). 1.0 = verbatim.
    pub normal_scale: f32,
    /// Occlusion strength (lerp factor between 1.0 and the occlusion texture's
    /// R channel, applied to IBL diffuse+specular only). Defaults to 1.0.
    pub occlusion_strength: f32,
}

// Static assertions for size and alignment.
const _: [(); 96] = [(); std::mem::size_of::<GpuMaterial>()];
const _: [(); 16] = [(); std::mem::align_of::<GpuMaterial>()];

/// Plain-data material description used at the manager boundary. The
/// engine layer translates `prism_asset::MaterialData` into this; the
/// four optional texture slots carry the render-side bindless SRV slot
/// (or `u32::MAX` for "no texture").
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
    /// Pack scalar parameters into a [`GpuMaterial`]. The texture
    /// indices are left at `u32::MAX` when `None`.
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

/// Manager of GPU materials. Holds a slot pool + a single device-local
/// storage buffer that the material SSBO descriptor references.
pub struct RenderMaterialManager {
    /// Slotmap-typed CPU handles; index in this map is *not* the SSBO
    /// slot — use `slot_of()` to translate.
    materials: SlotMap<MaterialHandle, MaterialUploadInput>,
    /// Reverse index from SSBO slot → material handle. `slots[slot]`
    /// is the handle currently occupying that slot, or `None` if free.
    slots: Vec<Option<MaterialHandle>>,
    /// Free-list of SSBO slot indices.
    free_list: Vec<u32>,
    /// The material SSBO (device-local, STORAGE_BUFFER usage). Sized to
    /// `MATERIAL_SSBO_MAX * size_of::<GpuMaterial>()`.
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Cached view of the SSBO contents, indexed by slot. Uploaded into
    /// `buffer` when `upload` is called.
    gpu_data: Vec<GpuMaterial>,
    /// Dirty bits: `dirty_slots[slot] = true` means the GPU data at
    /// this slot needs to be re-uploaded.
    dirty_slots: Vec<bool>,
    destroyed: bool,
}

impl RenderMaterialManager {
    /// Allocate the material SSBO. The buffer is initialized to zero
    /// (all slots invalid until populated).
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

    /// Register a new material and return its handle. The slot is taken
    /// from the free list; if the pool is exhausted, an error is returned
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

    /// Update an existing material in place. Marks its slot dirty so the
    /// next `upload` re-writes the SSBO.
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

    /// Translate a CPU handle to its SSBO slot. Returns `None` if the
    /// handle is unknown (it has been removed, or was never registered).
    pub fn slot_of(&self, handle: MaterialHandle) -> Option<u32> {
        self.slots
            .iter()
            .position(|h| *h == Some(handle))
            .map(|i| i as u32)
    }

    /// Underlying Vulkan buffer. The descriptor set the renderer builds
    /// references this buffer at `materials_binding`.
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Uploads all dirty slots to the device. P0 implementation uploads
    /// the entire SSBO (cheap because the buffer is small — 1024 * 48B =
    /// 48KB). A future pass uploads only the dirty range and keeps a
    /// per-FIF pair.
    pub fn upload(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
    ) -> anyhow::Result<()> {
        // Use a tiny staging buffer to write the entire SSBO. We don't
        // bother with the dirty range optimization yet because the
        // upload size is so small (48KB) that the savings are noise.
        let bytes = bytemuck::cast_slice(&self.gpu_data);
        let total_size = bytes.len();
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
        // Clear dirty bits; everything is now on the GPU.
        for d in self.dirty_slots.iter_mut() {
            *d = false;
        }
        Ok(())
    }

    /// Release a material slot back to the free list.
    pub fn unregister(&mut self, handle: MaterialHandle) {
        if let Some(slot) = self.slot_of(handle) {
            // Reset the GPU data so a future register at this slot
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

    /// Release every material. Idempotent.
    pub fn destroy(&mut self, device: &ash::Device) {
        for (_, _) in self.materials.drain() {
            // no per-material GPU state to release
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
