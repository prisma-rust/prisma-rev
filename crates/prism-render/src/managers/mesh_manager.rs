//! `RenderMeshManager` — device-local vertex/index buffers keyed by
//! an 不透明 handle 类型
//!
//! The 管理器 owns a 网格 per handle and exposes the underlying
//! `vk::Buffer` / `vk::DeviceAddress` to the 渲染器 for the 绘制 循环 A
//! `MeshHandle` that returns `None` from `get()` is treated as "not on the
//! GPU yet" by the 渲染器 (it falls 后 to the magenta-fallback path
//! used for textures).
//!
//! P0 scope: 同步 upload via the existing `buffer::create_buffer` +
//! `buffer::upload_to_buffer` helpers. No 时间线 信号量 no per-FIF
//! staging — the 渲染器 waits on the implicit 队列 submit. A future
//! pass replaces this with a timeline-driven 异步 path.

use anyhow::Context as _;
use ash::vk;
use slotmap::{new_key_type, SlotMap};

use crate::context::VulkanContext;
use crate::mesh::{Mesh, SkinVertex};

// 局部 handle. The engine 层 translates 资源 网格 handles into
// this when it calls `RenderMeshManager::register` so the 渲染
// crate stays decoupled from the 资源 管线
new_key_type! {
    /// Slotmap handle into [`RenderMeshManager`].
    pub struct MeshHandle;
}

/// Plain-data 网格 描述 used at the 管理器 boundary. The
/// engine 层 translates 资源 网格 data into this so
/// the 渲染 crate stays decoupled from the 资源 管线
#[derive(Debug, Clone)]
pub struct MeshUploadInput {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex 颜色 (the legacy 顶点 格式 has an RGB 颜色 槽
    /// procedural meshes can use it as albedo 回退 when no 纹理 is
    /// bound). 空 向量 means "all white".
    pub colors: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// Per-vertex tangents (xyz = direction, w = handedness 符号 +1/-1).
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

/// One GPU-uploaded 网格 plus the data the 渲染器 needs to 绘制 it.
pub struct UploadedMesh {
    /// The underlying Vulkan 缓冲区 + 内存 owned here.
    pub mesh: Mesh,
    /// Optional per-vertex skinning buffer (binding 1, #560 P0-3).
    ///
    /// Only skinned meshes allocate it: the static-mesh pipeline does not
    /// declare binding 1, so unskinned meshes keep their old memory profile.
    pub skin: Option<SkinBuffer>,
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
    /// non-indexed meshes this returns 0; the 渲染器 detects that case
    /// and uses `cmd_draw` with `vertex_count` instead.
    pub fn index_count(&self) -> u32 {
        self.mesh.index_count
    }

    /// Convenience: 顶点 count.
    pub fn vertex_count(&self) -> u32 {
        self.mesh.vertex_count
    }

    pub fn is_indexed(&self) -> bool {
        self.mesh.index_buffer.is_some()
    }
}

/// 管理器 of GPU meshes. Constructed once per 渲染器 and shared via
/// `&mut`. All 公开 methods are `&mut self` because 描述符 writes and
/// 缓冲区 creation are inherently mutating.
pub struct RenderMeshManager {
    meshes: SlotMap<MeshHandle, UploadedMesh>,
    /// Whether 销毁 has run. The 放置 impl asserts this.
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

    /// Translate 输入 into the legacy interleaved 顶点 布局 then
    /// upload 顶点 + (optional) 索引 buffers through a staging 缓冲区
    /// Returns the handle the 渲染器 uses to look the 网格 上 later.
    ///
    /// `command_pool` / `graphics_queue` are the same ones `Mesh::new` takes
    /// today; using the graphics 队列 keeps the upload path 相同 to
    /// the legacy 代码 (the transfer-queue 异步 path lands in a later
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
        let handle = self.meshes.insert(UploadedMesh { mesh, skin: None });
        Ok(handle)
    }

    /// Like [`register`](Self::register) but records into a shared
    /// [`BatchUploader`](crate::batch::BatchUploader) so many meshes can be
    /// uploaded with a single submit + 围栏 The 调用者 must finish the
    /// uploader before 绘制
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
        });
        Ok(handle)
    }

    /// Read-only 访问 to a registered 网格
    pub fn get(&self, handle: MeshHandle) -> Option<&UploadedMesh> {
        self.meshes.get(handle)
    }

    /// 放置 a single 网格 and 释放 its GPU resources. Subsequent calls
    /// to `get` with the same handle return `None`.
    pub fn unregister(&mut self, device: &ash::Device, handle: MeshHandle) {
        if let Some(mut uploaded) = self.meshes.remove(handle) {
            unsafe { uploaded.mesh.destroy(device) };
        }
    }

    /// 释放 every GPU 资源 The 调用者 is responsible for ensuring
    /// no in-flight 命令 缓冲区 still references these buffers. After
    /// this 调用 the 管理器 is 空
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

/// Translate `MeshUploadInput` into the interleaved 顶点 布局 缺少
/// UVs / colors / tangents are filled with safe defaults so the GPU 顶点
/// 格式 is always well-defined (the 着色器 treats "no UVs" as
/// 样本 the magenta 回退 via the same INVALID-handle path used for
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
    let bytes = unsafe {
        std::slice::from_raw_parts(
            vertices.as_ptr() as *const u8,
            std::mem::size_of_val(vertices.as_slice()),
        )
    };
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
    let bytes = unsafe {
        std::slice::from_raw_parts(
            vertices.as_ptr() as *const u8,
            std::mem::size_of_val(vertices.as_slice()),
        )
    };
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
