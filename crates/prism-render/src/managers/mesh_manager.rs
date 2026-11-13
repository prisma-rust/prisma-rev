//! `RenderMeshManager` — device-local vertex/index buffers keyed by
//! an opaque handle type.
//!
//! The manager owns a `Mesh` per handle and exposes the underlying
//! `vk::Buffer` / `vk::DeviceAddress` to the renderer for the draw loop. A
//! `MeshHandle` that returns `None` from `get()` is treated as "not on the
//! GPU yet" by the renderer (it falls back to the magenta-fallback path
//! used for textures).
//!
//! P0 scope: synchronous upload via the existing `buffer::create_buffer` +
//! `buffer::upload_to_buffer` helpers. No timeline semaphore, no per-FIF
//! staging — the renderer waits on the implicit queue submit. A future
//! pass replaces this with a timeline-driven async path.

use anyhow::Context as _;
use ash::vk;
use slotmap::{new_key_type, SlotMap};

use crate::context::VulkanContext;
use crate::mesh::{Mesh, SkinVertex, Vertex};

// Local handle. The engine layer translates asset mesh handles into
// this when it calls `RenderMeshManager::register` so the render
// crate stays decoupled from the asset pipeline.
new_key_type! {
    /// Slotmap handle into [`RenderMeshManager`].
    pub struct MeshHandle;
}

/// Plain-data mesh description used at the manager boundary. The
/// engine layer translates asset mesh data into this so
/// the render crate stays decoupled from the asset pipeline.
#[derive(Debug, Clone)]
pub struct MeshUploadInput {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex color (the legacy `Vertex` format has an RGB color slot;
    /// procedural meshes can use it as albedo fallback when no texture is
    /// bound). Empty vector means "all white".
    pub colors: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// Per-vertex tangents (xyz = direction, w = handedness sign +1/-1).
    pub tangents: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

impl MeshUploadInput {
    /// 顶点位置的局部包围盒（#491：视锥剔除 / 阴影通道剔除 / 编辑器精确拾取
    /// 都要它）。空网格或非有限坐标返回 `None`，调用方据此跳过剔除。
    pub fn bounds(&self) -> Option<crate::culling::Aabb> {
        crate::culling::Aabb::from_positions(self.positions.iter().copied())
    }
}

/// 蒙皮属性（#560 P0-3）：每顶点最多 4 个关节下标 + 权重。
///
/// 单独于 [`MeshUploadInput`] 存在是有意的：绝大多数网格没有蒙皮，把它们
/// 塞进同一个结构会让每个构造点都要写两个空 `Vec`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkinAttributes {
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
}

impl SkinAttributes {
    pub fn is_empty(&self) -> bool {
        self.joints.is_empty() || self.weights.is_empty()
    }

    /// 顶点数是否和网格对齐（不一致时调用方必须放弃蒙皮，而不是读越界）。
    pub fn matches_vertex_count(&self, vertex_count: usize) -> bool {
        self.joints.len() == vertex_count && self.weights.len() == vertex_count
    }
}

/// One GPU-uploaded mesh plus the data the renderer needs to draw it.
pub struct UploadedMesh {
    /// The underlying Vulkan buffer + memory, owned here.
    pub mesh: Mesh,
    /// Optional per-vertex skinning buffer (binding 1, #560 P0-3).
    ///
    /// Only skinned meshes allocate it: the static-mesh pipeline does not
    /// declare binding 1, so unskinned meshes keep their old memory profile.
    pub skin: Option<SkinBuffer>,
    /// CPU-side interleaved vertices preserved for acceleration structure
    /// builds and ray-query geometry queries.
    pub cpu_vertices: Vec<Vertex>,
    /// CPU-side indices (or empty if unindexed).
    pub cpu_indices: Vec<u32>,
}

/// Device-local `SkinVertex` array owned by an [`UploadedMesh`].
pub struct SkinBuffer {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub vertex_count: u32,
}

impl SkinBuffer {
    fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

impl UploadedMesh {
    /// Convenience: number of triangles to feed `cmd_draw_indexed`. For
    /// non-indexed meshes this returns 0; the renderer detects that case
    /// and uses `cmd_draw` with `vertex_count` instead.
    pub fn index_count(&self) -> u32 {
        self.mesh.index_count
    }

    /// Convenience: vertex count.
    pub fn vertex_count(&self) -> u32 {
        self.mesh.vertex_count
    }

    pub fn is_indexed(&self) -> bool {
        self.mesh.index_buffer.is_some()
    }
}

/// Manager of GPU meshes. Constructed once per renderer and shared via
/// `&mut`. All public methods are `&mut self` because descriptor writes and
/// buffer creation are inherently mutating.
pub struct RenderMeshManager {
    meshes: SlotMap<MeshHandle, UploadedMesh>,
    /// Whether `destroy()` has run. The `Drop` impl asserts this.
    destroyed: bool,
}

impl Default for RenderMeshManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderMeshManager {
    pub fn new() -> Self {
        Self {
            meshes: SlotMap::with_key(),
            destroyed: false,
        }
    }

    /// Number of registered meshes.
    pub fn len(&self) -> usize {
        self.meshes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }

    /// 驻留网格数与估算字节数（标准顶点布局 + 4 字节索引；#518 Phase 2）。
    pub fn stats(&self) -> (usize, u64) {
        let vertex_size = std::mem::size_of::<crate::mesh::Vertex>() as u64;
        let bytes = self
            .meshes
            .values()
            .map(|uploaded| {
                let mesh = &uploaded.mesh;
                mesh.vertex_count as u64 * vertex_size + mesh.index_count as u64 * 4
            })
            .sum();
        (self.meshes.len(), bytes)
    }

    /// Translate `input` into the legacy interleaved `Vertex` layout, then
    /// upload vertex + (optional) index buffers through a staging buffer.
    /// Returns the handle the renderer uses to look the mesh up later.
    ///
    /// `command_pool` / `graphics_queue` are the same ones `Mesh::new` takes
    /// today; using the graphics queue keeps the upload path identical to
    /// the legacy code (the transfer-queue async path lands in a later
    /// pass
    pub fn register(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        input: &MeshUploadInput,
    ) -> anyhow::Result<MeshHandle> {
        let vertices = build_vertices(input);
        let indices_opt: Option<&[u32]> = if input.indices.is_empty() {
            None
        } else {
            Some(&input.indices)
        };
        let mesh = Mesh::new(
            context,
            command_pool,
            graphics_queue,
            &vertices,
            indices_opt,
        )
        .context("RenderMeshManager::register: Mesh::new failed")?;
        let handle = self.meshes.insert(UploadedMesh {
            mesh,
            skin: None,
            cpu_vertices: vertices,
            cpu_indices: input.indices.clone(),
        });
        Ok(handle)
    }

    /// Like [`register`](Self::register) but records into a shared
    /// [`BatchUploader`](crate::batch::BatchUploader) so many meshes can be
    /// uploaded with a single submit + fence. The caller must finish the
    /// uploader before drawing.
    /// `register` 的蒙皮版本（#560 P0-3）：顶点数据完全相同，另外把
    /// `SkinAttributes` 传成第二个顶点缓冲（binding 1）。
    pub fn register_skinned(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        input: &MeshUploadInput,
        skin: &SkinAttributes,
    ) -> anyhow::Result<MeshHandle> {
        let vertices = build_vertices(input);
        let indices_opt: Option<&[u32]> = if input.indices.is_empty() {
            None
        } else {
            Some(&input.indices)
        };
        let mesh = crate::mesh::Mesh::new(
            context,
            command_pool,
            graphics_queue,
            &vertices,
            indices_opt,
        )
        .context("RenderMeshManager::register_skinned: Mesh::new failed")?;
        let skin_buffer =
            upload_skin_vertices(context, command_pool, graphics_queue, skin, vertices.len())?;
        Ok(self.meshes.insert(UploadedMesh {
            mesh,
            skin: skin_buffer,
            cpu_vertices: vertices,
            cpu_indices: input.indices.clone(),
        }))
    }

    pub fn register_into(
        &mut self,
        context: &VulkanContext,
        uploader: &mut crate::batch::BatchUploader<'_>,
        input: &MeshUploadInput,
    ) -> anyhow::Result<MeshHandle> {
        self.register_skinned_into(context, uploader, input, None)
    }

    /// [`Self::register_into`] 的蒙皮版本：`None` = 普通网格。
    pub fn register_skinned_into(
        &mut self,
        context: &VulkanContext,
        uploader: &mut crate::batch::BatchUploader<'_>,
        input: &MeshUploadInput,
        skin: Option<&SkinAttributes>,
    ) -> anyhow::Result<MeshHandle> {
        let vertices = build_vertices(input);
        let indices_opt: Option<&[u32]> = if input.indices.is_empty() {
            None
        } else {
            Some(&input.indices)
        };
        let mesh = Mesh::new_into(context, uploader, &vertices, indices_opt)
            .context("RenderMeshManager::register_into: Mesh::new_into failed")?;
        // 蒙皮缓冲走与顶点缓冲相同的共享 uploader（一次 submit/围栏）。
        let skin_buffer = match skin {
            Some(skin) if !skin.is_empty() => {
                upload_skin_vertices_into(context, uploader, skin, vertices.len())?
            }
            _ => None,
        };
        let handle = self.meshes.insert(UploadedMesh {
            mesh,
            skin: skin_buffer,
            cpu_vertices: vertices,
            cpu_indices: input.indices.clone(),
        });
        Ok(handle)
    }

    /// Read-only access to a registered mesh.
    pub fn get(&self, handle: MeshHandle) -> Option<&UploadedMesh> {
        self.meshes.get(handle)
    }

    /// Get CPU-side geometry (interleaved vertices and indices) for ray-tracing
    /// acceleration structure builds.
    pub fn get_cpu_geometry(&self, handle: MeshHandle) -> Option<(&[crate::mesh::Vertex], &[u32])> {
        self.meshes
            .get(handle)
            .map(|u| (u.cpu_vertices.as_slice(), u.cpu_indices.as_slice()))
    }

    /// Drop a single mesh and release its GPU resources. Subsequent calls
    /// to `get` with the same handle return `None`.
    pub fn unregister(&mut self, device: &ash::Device, handle: MeshHandle) {
        if let Some(mut uploaded) = self.meshes.remove(handle) {
            unsafe { uploaded.mesh.destroy(device) };
        }
    }

    /// Release every GPU resource. The caller is responsible for ensuring
    /// no in-flight command buffer still references these buffers. After
    /// this call the manager is empty.
    pub fn destroy(&mut self, device: &ash::Device) {
        for (_, mut uploaded) in self.meshes.drain() {
            if let Some(skin) = uploaded.skin.as_ref() {
                skin.destroy(device);
            }
            unsafe { uploaded.mesh.destroy(device) };
        }
        self.destroyed = true;
    }
}

impl Drop for RenderMeshManager {
    fn drop(&mut self) {
        debug_assert!(
            self.destroyed || self.meshes.is_empty(),
            "RenderMeshManager dropped without explicit destroy()"
        );
    }
}

/// Translate `MeshUploadInput` into the interleaved `Vertex` layout. Missing
/// UVs / colors / tangents are filled with safe defaults so the GPU vertex
/// format is always well-defined (the shader treats "no UVs" as
/// "sample the magenta fallback" via the same INVALID-handle path used for
/// textures).
fn build_vertices(input: &MeshUploadInput) -> Vec<crate::mesh::Vertex> {
    let n = input.positions.len();
    // 导入网格的切线是常量占位（中间格式没有切线槽位，#515），这里按 UV 现算一份；
    // 自己带了真切线的网格（图元、手写网格）原样使用。
    let generated = if crate::tangents::are_placeholders(&input.tangents, n) {
        crate::tangents::generate(&input.positions, &input.normals, &input.uvs, &input.indices)
    } else {
        None
    };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let pos = input.positions.get(i).copied().unwrap_or([0.0, 0.0, 0.0]);
        let normal = input.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
        let color = input.colors.get(i).copied().unwrap_or([1.0, 1.0, 1.0]);
        let uv = input.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
        let tangent = generated
            .as_ref()
            .and_then(|tangents| tangents.get(i).copied())
            .or_else(|| input.tangents.get(i).copied())
            .unwrap_or([1.0, 0.0, 0.0, 1.0]);
        out.push(crate::mesh::Vertex {
            position: pos,
            normal,
            color,
            uv,
            tangent,
        });
    }
    out
}

/// 把 `SkinAttributes` 展开成 `SkinVertex` 交错数组并上传（device-local）。
///
/// 顶点数不匹配时返回 `None`：宁可不蒙皮，也不要读到别的顶点的权重。
fn skin_vertices(
    skin: &SkinAttributes,
    vertex_count: usize,
) -> Option<Vec<crate::mesh::SkinVertex>> {
    if !skin.matches_vertex_count(vertex_count) {
        log::warn!(
            "mesh_manager: skin attributes cover {} vertices, mesh has {vertex_count}; \
             dropping skinning",
            skin.joints.len()
        );
        return None;
    }
    Some(
        skin.joints
            .iter()
            .zip(skin.weights.iter())
            .map(|(joints, weights)| SkinVertex {
                joints: *joints,
                weights: *weights,
            })
            .collect(),
    )
}

fn upload_skin_vertices(
    context: &VulkanContext,
    command_pool: vk::CommandPool,
    graphics_queue: vk::Queue,
    skin: &SkinAttributes,
    vertex_count: usize,
) -> anyhow::Result<Option<SkinBuffer>> {
    let Some(vertices) = skin_vertices(skin, vertex_count) else {
        return Ok(None);
    };
    let bytes: &[u8] = bytemuck::cast_slice(&vertices);
    let (buffer, memory) = crate::buffer::create_buffer(
        context,
        bytes.len() as vk::DeviceSize,
        crate::buffer::BufferUsage::VERTEX_BUFFER,
        crate::buffer::MemoryProperties::DEVICE_LOCAL,
    )
    .context("mesh_manager: create skin buffer")?;
    unsafe {
        crate::buffer::upload_to_buffer(
            context,
            command_pool,
            graphics_queue,
            buffer,
            bytes.len() as u64,
            bytes,
        )
    }
    .context("mesh_manager: upload skin buffer")?;
    Ok(Some(SkinBuffer {
        buffer,
        memory,
        vertex_count: vertices.len() as u32,
    }))
}

fn upload_skin_vertices_into(
    context: &VulkanContext,
    uploader: &mut crate::batch::BatchUploader<'_>,
    skin: &SkinAttributes,
    vertex_count: usize,
) -> anyhow::Result<Option<SkinBuffer>> {
    let Some(vertices) = skin_vertices(skin, vertex_count) else {
        return Ok(None);
    };
    let bytes: &[u8] = bytemuck::cast_slice(&vertices);
    let (buffer, memory) = crate::buffer::create_buffer(
        context,
        bytes.len() as vk::DeviceSize,
        crate::buffer::BufferUsage::VERTEX_BUFFER,
        crate::buffer::MemoryProperties::DEVICE_LOCAL,
    )
    .context("mesh_manager: create skin buffer")?;
    uploader
        .upload_buffer(buffer, bytes.len() as vk::DeviceSize, bytes)
        .context("mesh_manager: stage skin buffer")?;
    Ok(Some(SkinBuffer {
        buffer,
        memory,
        vertex_count: vertices.len() as u32,
    }))
}

#[cfg(test)]
#[path = "mesh_manager_tests.rs"]
mod tests;

#[cfg(test)]
mod bounds_tests {
    use super::*;

    fn input(positions: Vec<[f32; 3]>) -> MeshUploadInput {
        MeshUploadInput {
            positions,
            normals: Vec::new(),
            colors: Vec::new(),
            uvs: Vec::new(),
            tangents: Vec::new(),
            indices: Vec::new(),
        }
    }

    #[test]
    fn upload_input_reports_its_local_bounds() {
        let bounds = input(vec![[0.0, 0.0, 0.0], [2.0, -1.0, 4.0]])
            .bounds()
            .expect("finite vertices give bounds");
        assert_eq!(bounds.min, [0.0, -1.0, 0.0]);
        assert_eq!(bounds.max, [2.0, 0.0, 4.0]);
    }

    #[test]
    fn an_empty_or_broken_mesh_has_no_bounds() {
        assert!(input(Vec::new()).bounds().is_none());
        assert!(input(vec![[f32::INFINITY, 0.0, 0.0]]).bounds().is_none());
    }

    /// #515：导入路径给每个顶点写 `[1,0,0,1]`，上传时要按 UV 换成真切线。
    #[test]
    fn placeholder_tangents_are_regenerated_from_the_uvs() {
        let mut mesh = MeshUploadInput {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            colors: Vec::new(),
            // U 轴沿 +Y、V 轴沿 −X：真切线是 +Y。
            uvs: vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
            tangents: vec![[1.0, 0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let vertices = build_vertices(&mesh);
        for vertex in &vertices {
            assert!(
                (vertex.tangent[1] - 1.0).abs() < 1e-5,
                "expected the UV-derived +Y tangent, got {:?}",
                vertex.tangent
            );
        }

        // 自己带了真切线（不统一）的网格不被覆盖。
        mesh.tangents = vec![
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
            [1.0, 0.0, 0.0, -1.0],
        ];
        let vertices = build_vertices(&mesh);
        assert_eq!(vertices[2].tangent, [0.0, 0.0, 1.0, 1.0]);
    }
}
