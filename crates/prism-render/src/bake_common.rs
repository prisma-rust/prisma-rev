//! Shared BLAS / TLAS building and per-instance geometry types reused by
//! the real-time path tracer (`PathTracePass`).
//!
//! Provides per-instance geometry flattening, combined vertex/index buffers,
//! BLAS/TLAS building, and emissive-triangle extraction.  Scene loading
//! (RSCN / glTF) is handled exclusively by the engine crate; this module
//! only consumes the resulting [`PtGeometryInstance`] arrays.

use anyhow::{Context, Result};
use ash::vk;

use crate::context::VulkanContext;
use crate::descriptor::{PtEmissiveTri, PT_EMISSIVE_MAX};
use crate::mesh::Vertex;

// -------------------------------------------------------------------
// Per-instance PT geometry types
// -------------------------------------------------------------------

/// One ray-traceable scene instance: its own world-space vertex/index data
/// and the material SSBO slot the path tracer looks up via the TLAS
/// `instanceCustomIndex` at hit time.
///
/// Used by `PathTracePass::set_geometry`, which builds a per-instance BLAS and
/// a single TLAS carrying the instance index as the custom index (which then
/// looks up `material_slot`). Keeping the material identity separate is what
/// lets the path tracer sample the correct albedo texture per surface (Sponza
/// has many materials). Vertices are already in world space (the instance
/// transform is baked in), so the TLAS transform is identity.
#[derive(Clone)]
pub struct PtGeometryInstance {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Index into the `GpuMaterial[]` SSBO (`RenderMaterialManager`).
    pub material_slot: u32,
}

/// Per-instance metadata mirroring `PtInstanceMeta` in `pt_pass.rs` /
/// `path_integrator.slang` (16 bytes, repr(C)). Written into the
/// `instance_meta` SSBO and looked up in the shader by
/// `q.CommittedInstanceID()`.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PtInstanceMeta {
    pub material_slot: u32,
    pub index_base: u32,
    pub vertex_base: u32,
    pub _pad: u32,
}

/// A fully-built ray-traceable scene: combined vertex/index buffers,
/// per-instance metadata + materials SSBOs, per-instance BLAS, and one TLAS.
///
/// Built by [`build_pt_scene`] from a list of [`PtGeometryInstance`] + a
/// materials SSBO byte buffer. Owns all GPU resources; drops them on drop.
/// Both `PathTracePass::set_geometry` and the offline bakers consume this so
/// the per-instance BLAS/TLAS/meta/materials setup stays in one place.
pub struct PtScene {
    pub vertex_buffer: vk::Buffer,
    pub vertex_memory: vk::DeviceMemory,
    pub vertex_address: vk::DeviceAddress,
    pub index_buffer: vk::Buffer,
    pub index_memory: vk::DeviceMemory,
    pub instance_meta_buffer: vk::Buffer,
    pub instance_meta_memory: vk::DeviceMemory,
    pub materials_buffer: vk::Buffer,
    pub materials_memory: vk::DeviceMemory,
    pub blas_entries: Vec<crate::acceleration_structure::BlasEntry>,
    pub tlas: Option<crate::acceleration_structure::Tlas>,
    pub instance_count: u32,
    pub device: Option<ash::Device>,
}

impl PtScene {
    /// Destroy all GPU resources. Safe to call once; `Drop` is a no-op after.
    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.vertex_buffer, None);
            device.free_memory(self.vertex_memory, None);
            device.destroy_buffer(self.index_buffer, None);
            device.free_memory(self.index_memory, None);
            device.destroy_buffer(self.instance_meta_buffer, None);
            device.free_memory(self.instance_meta_memory, None);
            device.destroy_buffer(self.materials_buffer, None);
            device.free_memory(self.materials_memory, None);
        }
        self.blas_entries.clear();
        self.tlas = None;
        self.device = None;
    }
}

impl Drop for PtScene {
    fn drop(&mut self) {
        if let Some(d) = self.device.take() {
            self.destroy(&d);
        }
    }
}

// -------------------------------------------------------------------
// Emissive triangle extraction
// -------------------------------------------------------------------

/// Extract emissive triangles from scene instances + materials bytes.
///
/// Iterates over all instances, checks each instance's material for emissive
/// radiance > 0, and collects the world-space triangles into a flat array
/// suitable for a `StructuredBuffer<PtEmissiveTri>`.
///
/// `materials_bytes` is the raw `GpuMaterial[]` from `build_pt_scene`'s
/// caller. Each `GpuMaterial` is 96 bytes; the emissive radiance is at
/// byte offset 24 (z of `metallic_roughness_emissive`) and strength at
/// offset 28 (w). Radiance = emissive * strength.
pub fn build_emissive_triangles(
    instances: &[PtGeometryInstance],
    materials_bytes: &[u8],
) -> Vec<PtEmissiveTri> {
    const MAT_SIZE: usize = 96; // GpuMaterial is 96 bytes
    let mut out: Vec<PtEmissiveTri> = Vec::new();
    for inst in instances {
        let mat_offset = (inst.material_slot as usize) * MAT_SIZE;
        if mat_offset + 32 > materials_bytes.len() {
            continue;
        }
        let slice = &materials_bytes[mat_offset..mat_offset + 32];
        let emissive = f32::from_ne_bytes([slice[24], slice[25], slice[26], slice[27]]);
        let strength = f32::from_ne_bytes([slice[28], slice[29], slice[30], slice[31]]);
        let rad = emissive * strength;
        if rad <= 0.0 {
            continue;
        }
        let tri_count = inst.indices.len() / 3;
        for ti in 0..tri_count {
            if out.len() >= PT_EMISSIVE_MAX as usize {
                return out;
            }
            let i0 = inst.indices[ti * 3] as usize;
            let i1 = inst.indices[ti * 3 + 1] as usize;
            let i2 = inst.indices[ti * 3 + 2] as usize;
            let v0 = inst.vertices[i0].position;
            let v1 = inst.vertices[i1].position;
            let v2 = inst.vertices[i2].position;
            let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
            let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
            let nx = e1[1] * e2[2] - e1[2] * e2[1];
            let ny = e1[2] * e2[0] - e1[0] * e2[2];
            let nz = e1[0] * e2[1] - e1[1] * e2[0];
            let nl = (nx * nx + ny * ny + nz * nz).sqrt().max(1e-8);
            let tri_area = 0.5 * nl;
            out.push(PtEmissiveTri {
                v0: [v0[0], v0[1], v0[2], 0.0],
                v1: [v1[0], v1[1], v1[2], 0.0],
                v2: [v2[0], v2[1], v2[2], 0.0],
                normal: [nx / nl, ny / nl, nz / nl, 0.0],
                radiance: [rad, rad, rad, 0.0],
                area: tri_area,
            });
        }
    }
    out
}

/// Create a device-local storage buffer containing [`PtEmissiveTri`] entries for
/// all emissive triangles in the given instances/materials. Returns
/// `(buffer, memory, count)` — all zero/null if no emissive geometry.
///
/// This is separate from `build_pt_scene` because the real-time PT pass builds
/// its BLAS/TLAS with placeholder materials (before the material manager is
/// ready), but can call this later once actual material bytes are available.
pub fn create_emissive_buffer(
    context: &VulkanContext,
    instances: &[PtGeometryInstance],
    materials_bytes: &[u8],
) -> (Option<vk::Buffer>, Option<vk::DeviceMemory>, u32) {
    let tris = build_emissive_triangles(instances, materials_bytes);
    if tris.is_empty() {
        return (None, None, 0);
    }
    let bytes: Vec<u8> = {
        let mut b = Vec::with_capacity(tris.len() * size_of::<PtEmissiveTri>());
        for tri in &tris {
            b.extend_from_slice(bytemuck::bytes_of(tri));
        }
        b
    };
    match create_storage_buffer(context, &bytes) {
        Ok((buf, mem)) => (Some(buf), Some(mem), tris.len() as u32),
        Err(e) => {
            log::warn!("create_emissive_buffer failed: {e}");
            (None, None, 0)
        }
    }
}

// -------------------------------------------------------------------
// BLAS / TLAS build
// -------------------------------------------------------------------

/// Build a [`PtScene`] from per-instance geometry + a materials SSBO byte
/// buffer. Creates a combined vertex/index buffer, one BLAS per instance
/// (pointing at its slice of the combined buffers), a TLAS whose
/// `instanceCustomIndex` carries the instance index, and the
/// `instance_meta` + `materials` SSBOs.
pub fn build_pt_scene(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    instances: &[PtGeometryInstance],
    materials_bytes: &[u8],
) -> Result<PtScene> {
    use crate::acceleration_structure::{BlasBuildParams, BlasEntry, Tlas, TlasInstance};

    let device = &context.device;
    if instances.is_empty() {
        anyhow::bail!("build_pt_scene: no instances");
    }

    // ---- 1. Concatenate all instances into one combined vertex/index buffer.
    let mut all_verts: Vec<Vertex> = Vec::new();
    let mut all_indices: Vec<u32> = Vec::new();
    let mut meta: Vec<PtInstanceMeta> = Vec::with_capacity(instances.len());
    for inst in instances {
        let vertex_base = all_verts.len() as u32;
        let index_base = all_indices.len() as u32;
        all_verts.extend_from_slice(&inst.vertices);
        for &ix in &inst.indices {
            all_indices.push(ix + vertex_base);
        }
        meta.push(PtInstanceMeta {
            material_slot: inst.material_slot,
            index_base,
            vertex_base,
            _pad: 0,
        });
    }

    let (vbuf, vmem) = create_storage_buffer(context, vertex_bytes(&all_verts))
        .context("build_pt_scene: vertex buffer")?;
    let vbase_addr = unsafe {
        device.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(vbuf))
    };

    let (ibuf, imem) = create_storage_buffer(context, index_bytes(&all_indices))
        .context("build_pt_scene: index buffer")?;
    let ibase_addr = unsafe {
        device.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(ibuf))
    };

    let meta_bytes: &[u8] = bytemuck::cast_slice(&meta);
    let (mbuf, mmem) = create_storage_buffer(context, meta_bytes)
        .context("build_pt_scene: instance meta buffer")?;

    let (matbuf, matmem) = create_storage_buffer(context, materials_bytes)
        .context("build_pt_scene: materials buffer")?;

    // ---- 2. Build all BLAS in one batch (single submit + wait).
    let index_stride = 4u32 as vk::DeviceAddress;
    let total_verts = all_verts.len() as u32;

    let build_params: Vec<BlasBuildParams> = instances
        .iter()
        .zip(meta.iter())
        .map(|(inst, m)| {
            let tri_count = if !inst.indices.is_empty() {
                inst.indices.len() as u32 / 3
            } else {
                inst.vertices.len() as u32 / 3
            };
            BlasBuildParams {
                vertex_addr: vbase_addr,
                index_addr: ibase_addr + (m.index_base as vk::DeviceAddress) * index_stride,
                vertex_count: total_verts,
                tri_count,
            }
        })
        .collect();

    let blas_entries = BlasEntry::build_batch(context, command_pool, &build_params)
        .context("build_pt_scene: batch BLAS build")?;

    let mut blas_addrs: Vec<vk::DeviceAddress> = Vec::with_capacity(instances.len());
    let mut tlas_instances: Vec<TlasInstance> = Vec::with_capacity(instances.len());
    for (i, blas) in blas_entries.iter().enumerate() {
        blas_addrs.push(blas.device_address);
        tlas_instances.push(TlasInstance {
            transform: [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            custom_index: i as u32,
            mask: 0xFF,
            instance_shader_binding_table_record_offset: 0,
            flags: vk::GeometryInstanceFlagsKHR::TRIANGLE_FACING_CULL_DISABLE,
        });
    }

    let tlas = Tlas::build(context, command_pool, &tlas_instances, &blas_addrs)
        .context("build_pt_scene: TLAS")?;

    log::info!(
        "build_pt_scene: {} instances, {} verts, {} indices",
        instances.len(),
        all_verts.len(),
        all_indices.len()
    );

    Ok(PtScene {
        vertex_buffer: vbuf,
        vertex_memory: vmem,
        vertex_address: vbase_addr,
        index_buffer: ibuf,
        index_memory: imem,
        instance_meta_buffer: mbuf,
        instance_meta_memory: mmem,
        materials_buffer: matbuf,
        materials_memory: matmem,
        blas_entries,
        tlas: Some(tlas),
        instance_count: instances.len() as u32,
        device: Some(device.clone()),
    })
}

// -------------------------------------------------------------------
// Buffer upload helpers
// -------------------------------------------------------------------

pub fn vertex_bytes(vertices: &[Vertex]) -> &[u8] {
    bytemuck::cast_slice(vertices)
}

pub fn index_bytes(indices: &[u32]) -> &[u8] {
    bytemuck::cast_slice(indices)
}

/// Host-visible storage buffer (also usable as a BLAS build input), initialized
/// with `data`.
pub fn create_storage_buffer(
    context: &VulkanContext,
    data: &[u8],
) -> Result<(ash::vk::Buffer, ash::vk::DeviceMemory)> {
    use crate::buffer::{self, BufferUsage, MemoryProperties};

    let size = data.len() as ash::vk::DeviceSize;
    let (buffer, memory) = buffer::create_buffer(
        context,
        size,
        BufferUsage::STORAGE_BUFFER
            | BufferUsage::SHADER_DEVICE_ADDRESS
            | BufferUsage::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR,
        MemoryProperties::HOST_VISIBLE | MemoryProperties::HOST_COHERENT,
    )?;
    unsafe {
        let ptr = context
            .device
            .map_memory(memory, 0, size, ash::vk::MemoryMapFlags::empty())?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        context.device.unmap_memory(memory);
    }
    Ok((buffer, memory))
}
