//! Bindless texture table — modern separated SRV + global sampler model.
//!
//! Replaces the legacy combined-image-sampler approach with the modern idiom:
//!
//! - **`bindless_srvs[]`** — a runtime-sized array of `SAMPLED_IMAGE` (texture
//!   views without samplers baked in). This is where all textures live.
//! - **`global_samplers[]`** — a small fixed array of `SAMPLER` descriptors
//!   (one per [`SamplerType`]). There are only a handful of sampling modes;
//!   sharing them across all textures is more cache-efficient and avoids
//!   redundantly creating thousands of identical samplers.
//!
//! Shaders sample like:
//!
//! ```slang
//! Texture2D tex = bindless_srvs[NonUniformResourceIndex(handle.index)];
//! tex.Sample(global_samplers[sampler_type], uv);
//! ```
//!
//! ## INVALID handle fallback
//!
//! Unregistered or not-yet-ready textures get [`TextureHandle::INVALID`].
//! The shader checks for this and returns a magenta fallback color,
//! avoiding crashes from reading unbound descriptors — critical on mobile
//! where async resource loading is common.
//!
//! ## Flags
//!
//! `PARTIALLY_BOUND` | `UPDATE_AFTER_BIND` | `VARIABLE_DESCRIPTOR_COUNT`
//! | `RUNTIME_DESCRIPTOR_ARRAY` — see [`required_features`].

use anyhow::Context as _;
use ash::vk;
use ash::vk::Handle as _;

/// Opaque handle into the bindless SRV array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureHandle(pub u32);

impl TextureHandle {
    /// Invalid slot — shaders return fallback color when they see this.
    pub const INVALID: TextureHandle = TextureHandle(u32::MAX);
}

/// Fixed sampler types — the only sampling modes the engine needs.
/// Each maps to one entry in `global_samplers[]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SamplerType {
    /// Bilinear filtering, repeat addressing — general-purpose albedo textures.
    LinearWrap = 0,
    /// Bilinear filtering, clamp-to-edge — cubemaps, LUTs, UI.
    LinearClamp = 1,
    /// Nearest filtering — pixel art, debug visualizations.
    Nearest = 2,
    /// PCF shadow comparison sampler — shadow maps.
    Shadow = 3,
}

impl SamplerType {
    /// Number of sampler slots in `global_samplers[]`.
    pub const COUNT: u32 = 4;

    /// Create the Vulkan sampler create-info for this sampler type.
    fn create_info(self) -> vk::SamplerCreateInfo<'static> {
        match self {
            SamplerType::LinearWrap => vk::SamplerCreateInfo {
                mag_filter: vk::Filter::LINEAR,
                min_filter: vk::Filter::LINEAR,
                mipmap_mode: vk::SamplerMipmapMode::LINEAR,
                address_mode_u: vk::SamplerAddressMode::REPEAT,
                address_mode_v: vk::SamplerAddressMode::REPEAT,
                address_mode_w: vk::SamplerAddressMode::REPEAT,
                max_lod: vk::LOD_CLAMP_NONE,
                ..Default::default()
            },
            SamplerType::LinearClamp => vk::SamplerCreateInfo {
                mag_filter: vk::Filter::LINEAR,
                min_filter: vk::Filter::LINEAR,
                mipmap_mode: vk::SamplerMipmapMode::LINEAR,
                address_mode_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                address_mode_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                address_mode_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                max_lod: vk::LOD_CLAMP_NONE,
                ..Default::default()
            },
            SamplerType::Nearest => vk::SamplerCreateInfo {
                mag_filter: vk::Filter::NEAREST,
                min_filter: vk::Filter::NEAREST,
                mipmap_mode: vk::SamplerMipmapMode::NEAREST,
                address_mode_u: vk::SamplerAddressMode::REPEAT,
                address_mode_v: vk::SamplerAddressMode::REPEAT,
                address_mode_w: vk::SamplerAddressMode::REPEAT,
                ..Default::default()
            },
            SamplerType::Shadow => vk::SamplerCreateInfo {
                mag_filter: vk::Filter::LINEAR,
                min_filter: vk::Filter::LINEAR,
                mipmap_mode: vk::SamplerMipmapMode::LINEAR,
                address_mode_u: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                address_mode_v: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                address_mode_w: vk::SamplerAddressMode::CLAMP_TO_EDGE,
                compare_enable: vk::TRUE,
                compare_op: vk::CompareOp::LESS_OR_EQUAL,
                ..Default::default()
            },
        }
    }
}

/// The descriptor-indexing sub-features this table needs.
pub fn required_features() -> vk::PhysicalDeviceVulkan12Features<'static> {
    vk::PhysicalDeviceVulkan12Features::default()
        .descriptor_indexing(true)
        .runtime_descriptor_array(true)
        .descriptor_binding_partially_bound(true)
        .descriptor_binding_sampled_image_update_after_bind(true)
        .descriptor_binding_variable_descriptor_count(true)
        .shader_sampled_image_array_non_uniform_indexing(true)
}

/// Pure SRV-slot allocator for [`BindlessTextureTable`] (bump pointer +
/// free-list).
///
/// Kept free of Vulkan handles on purpose so slot bookkeeping is unit
/// testable without a device: `next` is the high-water mark, `free` holds
/// released slots that a later [`claim`](Self::claim) reuses (LIFO).
#[derive(Debug, Clone)]
pub struct SlotAllocator {
    capacity: u32,
    /// 下一个 never-used 槽 (bump pointer / high-water mark).
    next: u32,
    /// 释放过、可复用的 槽 (release → claim LIFO).
    free: Vec<u32>,
    /// claimed[i] == true iff 槽 i is currently live (claimed and not
    /// released). Needed because `next` counts gaps left by `claim_at`.
    claimed: Vec<bool>,
    /// Number of currently live in-use 槽.
    live: u32,
}

impl SlotAllocator {
    /// Empty allocator with 容量 槽 (0..容量
    pub fn new(capacity: u32) -> Self {
        Self {
            capacity,
            next: 0,
            free: Vec::new(),
            claimed: vec![false; capacity as usize],
            live: 0,
        }
    }

    /// Claim a 槽 freed 槽 are reused first (LIFO), otherwise the
    /// bump pointer advances. Errors when every 槽 is in use.
    pub fn claim(&mut self) -> anyhow::Result<u32> {
        if let Some(slot) = self.free.pop() {
            self.claimed[slot as usize] = true;
            self.live += 1;
            return Ok(slot);
        }
        anyhow::ensure!(
            self.next < self.capacity,
            "bindless SRV table full ({} / {})",
            self.live,
            self.capacity
        );
        let slot = self.next;
        self.claimed[slot as usize] = true;
        self.next += 1;
        self.live += 1;
        Ok(slot)
    }

    /// Claim a specific 槽 The 调用者 must know the 槽 is not
    /// live in use otherwise the previous view is silently overwritten
    /// by the subsequent descriptor 写入 This is what
    /// `register_with_handle` uses for the magenta 回退 槽
    ///
    /// If the 槽 was at or past 下一个 下一个 becomes 槽 + 1` so a later
    /// `claim` cannot hand the same 槽 out twice.
    pub fn claim_at(&mut self, slot: u32) -> anyhow::Result<u32> {
        anyhow::ensure!(
            slot < self.capacity,
            "register_with_handle: slot {slot} >= capacity {}",
            self.capacity
        );
        if let Some(pos) = self.free.iter().position(|free| *free == slot) {
            self.free.swap_remove(pos);
        }
        self.claimed[slot as usize] = true;
        self.live += 1;
        if slot >= self.next {
            self.next = slot + 1;
        }
        Ok(slot)
    }

    /// Return a 槽 to the free-list so a later `claim` reuses it.
    ///
    /// Returns `false` (and changes nothing) when the 槽 was never
    /// claimed or is already free — a double release must not hand the
    /// same 槽 out twice.
    pub fn release(&mut self, slot: u32) -> bool {
        if slot >= self.capacity || !self.claimed[slot as usize] {
            return false;
        }
        self.claimed[slot as usize] = false;
        self.live -= 1;
        self.free.push(slot);
        true
    }

    /// Whether 槽 is currently live in use). A 槽 counts as used once
    /// it has been claimed and until it is released.
    pub fn is_used(&self, slot: u32) -> bool {
        self.claimed.get(slot as usize).copied().unwrap_or(false)
    }

    /// Number of live in-use 槽 (released slots do not count).
    pub fn live_len(&self) -> u32 {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// 槽 released but not yet reused (diagnostics / tests).
    pub fn free_slot_count(&self) -> u32 {
        self.free.len() as u32
    }
}

/// Bindless texture table with separated SRV + global samplers.
///
/// Two bindings in one descriptor set:
/// - binding 0: `bindless_srvs[]` — SAMPLED_IMAGE array (texture views)
/// - binding 1: `global_samplers[4]` — SAMPLER array (fixed sampling modes)
pub struct BindlessTextureTable {
    device: ash::Device,
    pub layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    pub set: vk::DescriptorSet,
    /// SRV 槽 bookkeeping (bump pointer + free-list, see [`SlotAllocator`]).
    slots: SlotAllocator,
    /// The 4 global sampler objects owned by this table.
    samplers: [vk::Sampler; SamplerType::COUNT as usize],
}

/// Descriptor set binding indices.
///
/// `SAMPLERS` is binding 0 (fixed-size array) and `SRV` is binding 1 (the
/// runtime-sized, variable-count array). Vulkan requires
/// `VARIABLE_DESCRIPTOR_COUNT` to be on the binding with the highest number, so
/// the variable-count SRV array must come *after* the fixed sampler array.
pub mod bindings {
    /// `global_samplers[4]` - fixed SAMPLER array.
    pub const SAMPLERS: u32 = 0;
    /// `bindless_srvs[]` - runtime-sized SAMPLED_IMAGE array.
    pub const SRV: u32 = 1;
}

impl BindlessTextureTable {
    /// Create the table with room for `capacity` texture views.
    pub fn new(device: &ash::Device, capacity: u32) -> anyhow::Result<Self> {
        // --- Create the 4 global samplers ---
        let mut samplers = [vk::Sampler::null(); SamplerType::COUNT as usize];
        for (i, st) in [
            SamplerType::LinearWrap,
            SamplerType::LinearClamp,
            SamplerType::Nearest,
            SamplerType::Shadow,
        ]
        .iter()
        .enumerate()
        {
            samplers[i] = unsafe { device.create_sampler(&st.create_info(), None) }
                .context("create global sampler")?;
        }

        // --- Descriptor set layout: two bindings ---
        // Bindings are listed in ascending binding-number order so the binding
        // flags (which must be in the same order as the bindings array) line up
        // correctly. The variable-count SRV array is the highest binding.
        let bindings = [
            // binding 0: global samplers (fixed count)
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::SAMPLERS)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(SamplerType::COUNT)
                .stage_flags(
                    vk::ShaderStageFlags::VERTEX
                        | vk::ShaderStageFlags::FRAGMENT
                        | vk::ShaderStageFlags::COMPUTE,
                ),
            // binding 1: SRV array (textures without samplers)
            vk::DescriptorSetLayoutBinding::default()
                .binding(bindings::SRV)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(capacity)
                .stage_flags(
                    vk::ShaderStageFlags::VERTEX
                        | vk::ShaderStageFlags::FRAGMENT
                        | vk::ShaderStageFlags::COMPUTE,
                ),
        ];

        // Binding flags: samplers are immutable; the SRV array gets the bindless
        // flags. VARIABLE_DESCRIPTOR_COUNT must be on the highest-numbered binding
        // (binding 1 = SRV), which it now is.
        let binding_flags = [
            vk::DescriptorBindingFlags::empty(),
            vk::DescriptorBindingFlags::PARTIALLY_BOUND
                | vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::VARIABLE_DESCRIPTOR_COUNT,
        ];
        let mut flags_info =
            vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);

        let layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
            .bindings(&bindings)
            .push_next(&mut flags_info);
        let layout = unsafe { device.create_descriptor_set_layout(&layout_info, None) }
            .context("create bindless descriptor set layout")?;

        // --- Pool ---
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: capacity,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: SamplerType::COUNT,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .context("create bindless descriptor pool")?;

        // --- Allocate the set with variable descriptor count ---
        let counts = [capacity];
        let mut count_info = vk::DescriptorSetVariableDescriptorCountAllocateInfo::default()
            .descriptor_counts(&counts);
        let set_layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&set_layouts)
            .push_next(&mut count_info);
        let set = unsafe { device.allocate_descriptor_sets(&alloc_info) }
            .context("allocate bindless descriptor set")?[0];

        // Write the global samplers into binding 1 immediately (they never change).
        let sampler_infos: Vec<_> = samplers
            .iter()
            .map(|&s| {
                vk::DescriptorImageInfo::default()
                    .sampler(s)
                    .image_layout(vk::ImageLayout::UNDEFINED)
            })
            .collect();
        let sampler_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(bindings::SAMPLERS)
            .dst_array_element(0)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(&sampler_infos);
        unsafe { device.update_descriptor_sets(&[sampler_write], &[]) };

        Ok(Self {
            device: device.clone(),
            layout,
            pool,
            set,
            slots: SlotAllocator::new(capacity),
            samplers,
        })
    }

    /// Register a texture view (without sampler — the shader picks a
    /// [`SamplerType`] at sample time). Returns a handle for shader indexing.
    ///
    /// The image must already be in `SHADER_READ_ONLY_OPTIMAL` layout.
    pub fn register(&mut self, image_view: vk::ImageView) -> anyhow::Result<TextureHandle> {
        // 释放过的 槽 are reused before the bump pointer advances.
        let slot = self.slots.claim()?;
        self.write_srv(slot, image_view);
        Ok(TextureHandle(slot))
    }

    /// Register a texture view at a specific slot. The caller must
    /// ensure the slot is currently free (otherwise the previous view is
    /// silently overwritten, which is what `write_srv` does). This is
    /// the path `RenderTextureManager` uses to claim slot 0 for the
    /// magenta fallback at construction time.
    ///
    /// If the requested slot is at or past `next`, `next` is bumped to
    /// `slot + 1` so a subsequent `register` call will not overwrite
    /// the slot we just placed. This is what makes the "slot 0 is the
    /// fallback" convention safe.
    pub fn register_with_handle(
        &mut self,
        slot: u32,
        image_view: vk::ImageView,
    ) -> anyhow::Result<TextureHandle> {
        self.slots.claim_at(slot)?;
        self.write_srv(slot, image_view);
        Ok(TextureHandle(slot))
    }

    /// Return a 槽 to the free-list so a later [`register`](Self::register)
    /// reuses it.
    ///
    /// This only reclaims the *slot number*: the SRV descriptor still
    /// points at whatever view was last written into it. Callers that
    /// destroyed that view must first [`write_srv`](Self::write_srv) a
    /// live view (e.g. the magenta fallback) so a shader holding the old
    /// handle cannot sample a freed view.
    ///
    /// Returns `false` when the 槽 was never claimed or is already free.
    pub fn release_slot(&mut self, handle: TextureHandle) -> bool {
        self.slots.release(handle.0)
    }

    /// Whether 槽 is currently in use (claimed and not released).
    pub fn is_slot_used(&self, slot: u32) -> bool {
        self.slots.is_used(slot)
    }

    /// Overwrite an existing SRV slot (e.g. to swap a texture without reallocating).
    pub fn write_srv(&self, slot: u32, image_view: vk::ImageView) {
        let image_info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(image_view)];
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.set)
            .dst_binding(bindings::SRV)
            .dst_array_element(slot)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&image_info);
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };
    }

    /// Get the raw Vulkan sampler handle for a [`SamplerType`].
    /// Useful for code paths that still need a combined descriptor.
    pub fn sampler(&self, ty: SamplerType) -> vk::Sampler {
        self.samplers[ty as usize]
    }

    /// Borrow the `ash::Device` this table was created with. Used by owners
    /// that need to free GPU resources (images/views) referenced by the
    /// table's descriptors.
    pub fn device(&self) -> &ash::Device {
        &self.device
    }

    /// Number of live in-use 纹理 views (released 槽 excluded).
    pub fn len(&self) -> u32 {
        self.slots.live_len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn capacity(&self) -> u32 {
        self.slots.capacity()
    }
}

impl Drop for BindlessTextureTable {
    fn drop(&mut self) {
        unsafe {
            for &s in &self.samplers {
                if !s.is_null() {
                    self.device.destroy_sampler(s, None);
                }
            }
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.layout, None);
        }
    }
}

#[cfg(test)]
#[path = "bindless_tests.rs"]
mod tests;
