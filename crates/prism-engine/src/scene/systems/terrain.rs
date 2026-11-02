//! 高度场地形运行时（#517 Phase 1）。
//!
//! 场景里的 [`Terrain`] 只是数据：这里把它读成 [`Heightfield`]，生成一张
//! `MeshUploadInput`（positions / normals / uv0 / indices），登记进
//! [`ProceduralMeshes`]，再把同一实体的 `MeshRenderer.mesh_path` 指向这条虚构路径。
//! 资产解析器遇到 `__proc/` 前缀时直接从 [`ProceduralMeshes`] 取网格，不需要 cook。
//!
//! 查询接口：[`Heightfield::sample_height`] / [`Heightfield::sample_normal`] 是纯函数，
//! [`terrain_height`] / [`terrain_sample`] 在世界上按 `TerrainData` 找覆盖点，
//! [`terrain_raycast`] 用同样的采样做解析射线查询（玩法落地 / 拾取用）。
//!
//! 多 chunk + LOD 已实现（#517 Phase 2）：`Terrain.chunks > 1` 时每个块是
//! 独立子实体 / 独立网格，渲染与阴影可以逐块剔除；`Terrain.lod_distances`
//! 控制 1–3 级 LOD，整张地形按最近相机统一切级（避免跨块裂缝）。
//! 分层 splat、植被、编辑器视口笔刷仍待后续；[`Heightfield::sculpt`] 与
//! `prism-build-pipeline sculpt-heightmap` 已提供 raise/lower/smooth/flatten
//! 的基础雕刻层。rapier 地形碰撞体由 `prism-app` 的物理线程维护（见
//! `physics_runner`）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::scene::components::{
    Camera, Child, LocalToWorld, LocalTransform, MeshRef, MeshRenderer, Terrain,
};
use crate::scene::helpers::HierarchyHelper;
use prism_render::managers::mesh_manager::MeshHandle;
use prism_render::managers::mesh_manager::MeshUploadInput;
use prism_scene::components::SceneAssetId;

/// 进程内生成网格的路径前缀；不为它们 cook、不进 `.pak`。
pub const PROCEDURAL_PREFIX: &str = "__proc/";

/// 解析后的高度场：行优先，`cells × cells` 个高度。
///
/// 数值用的就是 `.raw` 里的单位 —— heightmap CLI 写的是**米**（绝对高程，可以为负），
/// 而手工/归一化[0,1] 的高度场再靠 `height_scale` 放大成米。
#[derive(Debug, Clone, PartialEq)]
pub struct Heightfield {
    cells: usize,
    heights: Vec<f32>,
}

/// 高度场笔刷模式（#517 Phase 2 雕刻基础层）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SculptMode {
    /// 在笔刷范围内加高：`h += strength * falloff`（strength 是高度场单位）。
    #[default]
    Raise,
    /// 在笔刷范围内降低：`h -= strength * falloff`。
    Lower,
    /// 向邻域平均高度靠拢；`strength` 是 0..1 的混合系数。
    Smooth,
    /// 向笔刷中心高度靠拢；`strength` 是 0..1 的混合系数。
    Flatten,
}

impl Heightfield {
    /// 从 `.raw`（f32 little-endian，行优先）解析。
    ///
    /// 长度必须正好是 `cells * cells * 4`；不足或多余都返回 `None`（`resolution`
    /// 和文件对不上时宁可报错，也不要画出一张错位的地形）。高度值不钳制范围 ——
    /// heightmap CLI 写出的是米（有负值），归一化高度场靠 `height_scale` 放大。
    pub fn from_raw(bytes: &[u8], cells: usize) -> Option<Self> {
        let expected = cells.checked_mul(cells)?.checked_mul(4)?;
        if cells < 2 || bytes.len() != expected {
            return None;
        }
        let heights = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .map(|value| if value.is_finite() { value } else { 0.0 })
            .collect();
        Some(Self { cells, heights })
    }

    /// 从 `.raw` 推断分辨率：样本数必须是完全平方数（heightmap CLI 默认 1024×1024）。
    pub fn from_raw_auto(bytes: &[u8]) -> Option<Self> {
        if bytes.len() % 4 != 0 {
            return None;
        }
        let samples = bytes.len() / 4;
        let cells = (samples as f64).sqrt() as usize;
        if cells * cells != samples || cells < 2 {
            return None;
        }
        Self::from_raw(bytes, cells)
    }

    /// 全零的高度场（没有高度图时的占位，仍然是平的可见地面）。
    pub fn flat(cells: usize) -> Self {
        let cells = cells.max(2);
        Self {
            cells,
            heights: vec![0.0; cells * cells],
        }
    }

    pub fn cells(&self) -> usize {
        self.cells
    }

    pub fn heights(&self) -> &[f32] {
        &self.heights
    }

    /// 网格坐标上的高度：`u` / `v` 是 `[0, 1]` 的格子坐标（越界夹到边界）。
    pub fn sample_height(&self, u: f32, v: f32) -> f32 {
        let last = (self.cells - 1) as f32;
        let u = (u.clamp(0.0, 1.0) * last).max(0.0);
        let v = (v.clamp(0.0, 1.0) * last).max(0.0);
        let (x0, y0) = (u.floor() as usize, v.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.cells - 1), (y0 + 1).min(self.cells - 1));
        let (tx, ty) = (u - x0 as f32, v - y0 as f32);
        let h00 = self.at(x0, y0);
        let h10 = self.at(x1, y0);
        let h01 = self.at(x0, y1);
        let h11 = self.at(x1, y1);
        let top = h00 + (h10 - h00) * tx;
        let bottom = h01 + (h11 - h01) * tx;
        top + (bottom - top) * ty
    }

    fn at(&self, x: usize, y: usize) -> f32 {
        self.heights[y * self.cells + x]
    }

    /// 网格坐标上的法线（中心差分，Y 已归一化）。
    pub fn sample_normal(&self, u: f32, v: f32, height_scale: f32) -> [f32; 3] {
        let step = 1.0 / (self.cells - 1) as f32;
        let left = self.sample_height(u - step, v) * height_scale;
        let right = self.sample_height(u + step, v) * height_scale;
        let down = self.sample_height(u, v - step) * height_scale;
        let up = self.sample_height(u, v + step) * height_scale;
        // 网格是世界尺寸上的等距采样，所以梯度只需按格距缩放。
        let dx = (right - left) / (2.0 * step).max(1e-6);
        let dz = (up - down) / (2.0 * step).max(1e-6);
        let normal = glam::Vec3::new(-dx, 1.0, -dz).normalize_or_zero();
        if normal == glam::Vec3::ZERO {
            [0.0, 1.0, 0.0]
        } else {
            [normal.x, normal.y, normal.z]
        }
    }

    /// 生成整张地形的网格（单 chunk）：沿 `size` 铺开，`y = height * height_scale`。
    pub fn build_mesh(&self, size: [f32; 2], height_scale: f32) -> MeshUploadInput {
        self.build_chunk_mesh(size, height_scale, 0, self.cells - 1, 0, self.cells - 1)
    }

    /// 生成 `[x0, x1] × [z0, z1]` 格子范围的网格（#517 Phase 2）。
    ///
    /// 顶点坐标相对**这一块的中心**，方便把块实体挂在父地形下；UV 仍用整张
    /// 地形的 `[0, 1]`，所以相邻块的材质不会在接缝处重置。范围会自动排序、
    /// 夹到 `0..=cells-1`，并至少保留一个 quad。
    pub fn build_chunk_mesh(
        &self,
        size: [f32; 2],
        height_scale: f32,
        x0: usize,
        x1: usize,
        z0: usize,
        z1: usize,
    ) -> MeshUploadInput {
        self.build_chunk_mesh_lod(size, height_scale, x0, x1, z0, z1, 1)
    }

    /// 带步长的 chunk 网格（#517 LOD）：`step = 1/2/4` 对应 LOD0/1/2。
    ///
    /// 两端格子始终保留，所以每一级的边界与相邻块 / 更细一级对齐；级内
    /// 只是减少中间的采样行。整张地形统一选级时不会出现跨块裂缝。
    pub fn build_chunk_mesh_lod(
        &self,
        size: [f32; 2],
        height_scale: f32,
        x0: usize,
        x1: usize,
        z0: usize,
        z1: usize,
        step: usize,
    ) -> MeshUploadInput {
        let cells = self.cells;
        let last = cells - 1;
        let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        let (z0, z1) = if z0 <= z1 { (z0, z1) } else { (z1, z0) };
        let x0 = x0.min(last.saturating_sub(1));
        let x1 = x1.min(last).max(x0 + 1);
        let z0 = z0.min(last.saturating_sub(1));
        let z1 = z1.min(last).max(z0 + 1);

        let [size_x, size_z] = [size[0].max(0.001), size[1].max(0.001)];
        let last_f = last as f32;
        // 块中心在整张地形局部空间里的位置；顶点再减掉它。
        let center_x = ((x0 + x1) as f32 * 0.5 / last_f - 0.5) * size_x;
        let center_z = ((z0 + z1) as f32 * 0.5 / last_f - 0.5) * size_z;
        let xs = sample_indices(x0, x1, step);
        let zs = sample_indices(z0, z1, step);
        let columns = xs.len();
        let rows = zs.len();

        let mut positions = Vec::with_capacity(columns * rows);
        let mut normals = Vec::with_capacity(columns * rows);
        let mut uvs = Vec::with_capacity(columns * rows);
        for &z in &zs {
            for &x in &xs {
                let (u, v) = (x as f32 / last_f, z as f32 / last_f);
                positions.push([
                    (u - 0.5) * size_x - center_x,
                    self.at(x, z) * height_scale,
                    (v - 0.5) * size_z - center_z,
                ]);
                normals.push(self.sample_normal(u, v, height_scale));
                uvs.push([u, 1.0 - v]);
            }
        }
        let mut indices = Vec::with_capacity((columns - 1) * (rows - 1) * 6);
        for z in 0..rows - 1 {
            for x in 0..columns - 1 {
                let top_left = (z * columns + x) as u32;
                let top_right = top_left + 1;
                let bottom_left = ((z + 1) * columns + x) as u32;
                let bottom_right = bottom_left + 1;
                indices.extend_from_slice(&[top_left, bottom_left, top_right]);
                indices.extend_from_slice(&[top_right, bottom_left, bottom_right]);
            }
        }
        MeshUploadInput {
            positions,
            normals,
            colors: Vec::new(),
            uvs,
            tangents: Vec::new(),
            indices,
        }
    }

    /// 在归一化坐标 `(center_u, center_v)` 处应用一次笔刷（#517 Phase 2）。
    ///
    /// `radius` 也是归一化到地形宽度的 `[0, 1]`；`strength` 对 Raise/Lower
    /// 是高度场单位，对 Smooth/Flatten 是 `0..1` 的混合系数。半径外的格子
    /// 完全不动，边缘用 `(1 - t²)²` 平滑衰减。返回受影响的格子数。
    ///
    /// 非有限 / 非正参数按 no-op 处理；高度不会产生 NaN。
    pub fn sculpt(
        &mut self,
        center_u: f32,
        center_v: f32,
        radius: f32,
        strength: f32,
        mode: SculptMode,
    ) -> usize {
        if !center_u.is_finite()
            || !center_v.is_finite()
            || !radius.is_finite()
            || !strength.is_finite()
            || radius <= 0.0
            || strength <= 0.0
        {
            return 0;
        }
        let cells = self.cells;
        let last = (cells - 1) as f32;
        let center_u = center_u.clamp(0.0, 1.0);
        let center_v = center_v.clamp(0.0, 1.0);
        let radius = radius.min(1.0) * last;
        if radius <= 0.0 {
            return 0;
        }
        let center_x = center_u * last;
        let center_z = center_v * last;
        let min_x = (center_x - radius).floor().max(0.0) as usize;
        let max_x = (center_x + radius).ceil().min(last) as usize;
        let min_z = (center_z - radius).floor().max(0.0) as usize;
        let max_z = (center_z + radius).ceil().min(last) as usize;
        let original = (mode == SculptMode::Smooth).then(|| self.heights.clone());
        let target = if mode == SculptMode::Flatten {
            self.sample_height(center_u, center_v)
        } else {
            0.0
        };
        let blend = strength.clamp(0.0, 1.0);

        let mut affected = 0;
        for z in min_z..=max_z {
            for x in min_x..=max_x {
                let dx = x as f32 - center_x;
                let dz = z as f32 - center_z;
                let distance = (dx * dx + dz * dz).sqrt() / radius;
                if distance > 1.0 {
                    continue;
                }
                let falloff = (1.0 - distance * distance).max(0.0).powi(2);
                if falloff <= 0.0 {
                    continue;
                }
                let index = z * cells + x;
                match mode {
                    SculptMode::Raise => self.heights[index] += strength * falloff,
                    SculptMode::Lower => self.heights[index] -= strength * falloff,
                    SculptMode::Smooth => {
                        let original = original.as_ref().expect("smooth keeps a snapshot");
                        let left = original[z * cells + x.saturating_sub(1)];
                        let right = original[z * cells + (x + 1).min(cells - 1)];
                        let up = original[z.saturating_sub(1) * cells + x];
                        let down = original[(z + 1).min(cells - 1) * cells + x];
                        let average = (left + right + up + down) * 0.25;
                        let value = original[index];
                        self.heights[index] = value + (average - value) * falloff * blend;
                    }
                    SculptMode::Flatten => {
                        let value = self.heights[index];
                        self.heights[index] = value + (target - value) * falloff * blend;
                    }
                }
                affected += 1;
            }
        }
        affected
    }

    /// 序列化回 `.raw`（f32 little-endian，行优先）——供 CLI / 编辑器写盘。
    pub fn to_raw_bytes(&self) -> Vec<u8> {
        self.heights
            .iter()
            .flat_map(|height| height.to_le_bytes())
            .collect()
    }
}

/// `[start, end]` 的采样下标：按 `step` 跳，但始终包含两端。
fn sample_indices(start: usize, end: usize, step: usize) -> Vec<usize> {
    let step = step.max(1);
    let mut indices = Vec::new();
    let mut index = start;
    while index < end {
        indices.push(index);
        index += step;
    }
    indices.push(end);
    indices
}

/// 进程内生成的网格表：路径 → 顶点数据（资产解析器据此绕过 `.pak`）。
#[derive(Debug, Default)]
pub struct ProceduralMeshes {
    meshes: HashMap<String, Arc<MeshUploadInput>>,
}

impl ProceduralMeshes {
    pub fn get(&self, path: &str) -> Option<MeshUploadInput> {
        self.meshes.get(path).map(|mesh| (**mesh).clone())
    }

    pub fn insert(&mut self, path: impl Into<String>, mesh: MeshUploadInput) {
        self.meshes.insert(path.into(), Arc::new(mesh));
    }

    pub fn remove(&mut self, path: &str) -> bool {
        self.meshes.remove(path).is_some()
    }

    pub fn len(&self) -> usize {
        self.meshes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String) -> bool) {
        self.meshes.retain(|path, _| keep(path));
    }
}

/// 地形实体的运行时状态：解析好的高度场 + 当前生成的网格路径。
///
/// `key` 变了（高度图 / 尺寸 / 高度倍数 / 分辨率 / 实体变了）就重建网格。
#[derive(Debug, Clone)]
pub struct TerrainData {
    pub key: String,
    pub field: Arc<Heightfield>,
    /// 单块模式是实体自己的网格；多块模式是第 0 块（用于调试 / 兼容旧测试）。
    pub mesh_path: String,
    pub size: [f32; 2],
    pub height_scale: f32,
}

/// 运行时生成的地形分块实体（#517 Phase 2）。
///
/// 每块是一个挂在 `owner` 下的普通实体，带自己的 `MeshRenderer` / `MeshRef` /
/// `LocalTransform`；主相机与阴影视锥因此能逐块剔除。它没有 `SceneEntityId`，
/// 不会被场景状态保存。
#[derive(Debug, Clone)]
pub struct TerrainChunk {
    pub owner: prism_ecs::Entity,
    pub mesh_path: String,
}

/// 某一级 LOD 下的一组地形块；`Child` 里是这一级的所有块实体。
///
/// 整张地形统一切级（而不是每块各自切），所以相邻块永远用同一个步长，
/// 不会因为边界顶点密度不同产生裂缝。
#[derive(Debug, Clone)]
pub struct TerrainLodSet {
    pub owner: prism_ecs::Entity,
    pub level: usize,
}

/// 世界坐标 `(x, z)` 上的地形高度（米）。没被任何地形覆盖时返回 `None`。
///
/// 只支持平移过的地形（旋转按轴向处理，不参与插值）；多个地形重叠时取最高的那个。
pub fn terrain_height(world: &prism_ecs::World, x: f32, z: f32) -> Option<f32> {
    terrain_sample(world, x, z).map(|(_, height, _)| height)
}

/// 世界坐标 `(x, z)` 上最高的地形：实体、高度（米）与世界法线。
///
/// 与 [`terrain_height`] 共用同一套定位规则：只看实体平移，重叠时取最高的那份。
pub fn terrain_sample(
    world: &prism_ecs::World,
    x: f32,
    z: f32,
) -> Option<(prism_ecs::Entity, f32, [f32; 3])> {
    let mut best: Option<(prism_ecs::Entity, f32, [f32; 3])> = None;
    for (entity, data) in world.query::<TerrainData>() {
        let origin = world
            .get::<LocalToWorld>(entity)
            .map(|transform| transform.0.w_axis.truncate())
            .unwrap_or(glam::Vec3::ZERO);
        let half = glam::Vec2::new(data.size[0] * 0.5, data.size[1] * 0.5);
        let local = glam::Vec2::new(x - origin.x, z - origin.z);
        if local.x.abs() > half.x || local.y.abs() > half.y {
            continue;
        }
        let u = local.x / (half.x * 2.0) + 0.5;
        let v = local.y / (half.y * 2.0) + 0.5;
        let height = data.field.sample_height(u, v) * data.height_scale + origin.y;
        if best.is_none_or(|(_, current, _)| height > current) {
            let normal = data.field.sample_normal(u, v, data.height_scale);
            best = Some((entity, height, normal));
        }
    }
    best
}

/// 一次地形射线命中。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainHit {
    pub entity: prism_ecs::Entity,
    /// 从射线起点算起的距离（世界单位）。
    pub distance: f32,
    /// 命中点的世界坐标。
    pub point: [f32; 3],
    /// 命中处的高度场法线（世界空间，Y 分量向上）。
    pub normal: [f32; 3],
}

/// 对高度场地形做射线查询（#517 Phase 2 第 4 条）。
///
/// 先沿射线采样 `(x, z)` 处的地形高度，找到第一次从地表上方穿到下方，再用
/// 二分细化；`max_distance` 之外、或射线起点已经在地表以下时不返回命中。
///
/// 和 [`terrain_height`] 一样只支持**平移**过的地形；多份地形重叠时按最高的
/// 那份求交（与查询接口一致）。这是玩法用的解析查询，不是物理引擎的连续碰撞。
pub fn terrain_raycast(
    world: &prism_ecs::World,
    origin: [f32; 3],
    direction: [f32; 3],
    max_distance: f32,
) -> Option<TerrainHit> {
    let dir = glam::Vec3::from_array(direction);
    let length = dir.length();
    if !length.is_finite() || length <= 1e-6 || !(max_distance > 0.0) || !max_distance.is_finite() {
        return None;
    }
    let dir = dir / length;
    let origin = glam::Vec3::from_array(origin);
    if !origin.is_finite() {
        return None;
    }

    let (_, start_height, _) = terrain_sample(world, origin.x, origin.z)?;
    if origin.y < start_height {
        // 起点已经在地表以下（例如相机埋进山里）：不做“从内部穿出”的命中。
        return None;
    }

    // 粗采样步长：短射线更细，长射线最多 512 次采样；随后二分 16 次。
    let step = (max_distance / 512.0).clamp(0.25, 8.0).min(max_distance);
    let samples = ((max_distance / step).ceil() as usize).clamp(1, 4096);
    let mut previous: Option<(f32, f32)> = if origin.y > start_height {
        Some((0.0, origin.y - start_height))
    } else {
        None
    };

    for index in 1..=samples {
        let t = index as f32 / samples as f32 * max_distance;
        let point = origin + dir * t;
        let Some((_, height, _)) = terrain_sample(world, point.x, point.z) else {
            previous = None;
            continue;
        };
        let gap = point.y - height;
        if gap <= 0.0 {
            let lo = previous.map_or(0.0, |(previous_t, _)| previous_t);
            let (hit_t, entity, normal) = refine_terrain_hit(world, origin, dir, lo, t);
            let point = origin + dir * hit_t;
            return Some(TerrainHit {
                entity,
                distance: hit_t,
                point: point.to_array(),
                normal,
            });
        }
        previous = Some((t, gap));
    }
    None
}

/// 在 `[lo, hi]` 之间二分出射线与地形的交点（`lo` 在地表上方，`hi` 在下方）。
fn refine_terrain_hit(
    world: &prism_ecs::World,
    origin: glam::Vec3,
    dir: glam::Vec3,
    mut lo: f32,
    mut hi: f32,
) -> (f32, prism_ecs::Entity, [f32; 3]) {
    let mut entity = none_entity();
    let mut normal = [0.0, 1.0, 0.0];
    for _ in 0..16 {
        let mid = (lo + hi) * 0.5;
        let point = origin + dir * mid;
        let Some((sample_entity, height, sample_normal)) = terrain_sample(world, point.x, point.z)
        else {
            break;
        };
        entity = sample_entity;
        normal = sample_normal;
        if point.y > height {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (hi, entity, normal)
}

/// 二分过程中还没采到样本时的占位实体（正常路径上一定会被真实样本覆盖）。
fn none_entity() -> prism_ecs::Entity {
    prism_ecs::Entity::from_raw(u32::MAX, u32::MAX)
}

/// `Terrain` 的网格参数指纹：变了就要重建网格。
pub fn terrain_key(entity_id: u32, terrain: &Terrain) -> String {
    format!(
        "{entity_id}|{}|{}x{}|{}|{}|{}|{:?}",
        terrain.heightmap,
        terrain.size[0],
        terrain.size[1],
        terrain.height_scale,
        terrain.cells(),
        terrain.chunks,
        terrain.lod_distances
    )
}

/// 读高度图（`.raw`，f32 LE）。路径解析与其它资产一致；失败时返回 `None` 并由调用方告警。
pub fn load_heightfield(terrain: &Terrain) -> Option<Heightfield> {
    if !terrain.has_heightmap() {
        return Some(Heightfield::flat(terrain.cells()));
    }
    let Some(path) = crate::asset_paths::asset_path(&terrain.heightmap) else {
        log::warn!(
            "terrain: cannot resolve asset root for heightmap {}; using a flat field",
            terrain.heightmap
        );
        return Some(Heightfield::flat(terrain.cells()));
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!(
                "terrain: cannot read heightmap {} ({error}); using a flat field",
                path.display()
            );
            return Some(Heightfield::flat(terrain.cells()));
        }
    };
    // `resolution = 0` = 跟随文件大小；否则必须严格匹配，免得画出错位的地形。
    let parsed = if terrain.resolution == 0 {
        Heightfield::from_raw_auto(&bytes)
    } else {
        Heightfield::from_raw(&bytes, terrain.cells())
    };
    match parsed {
        Some(field) => Some(field),
        None => {
            log::warn!(
                "terrain: heightmap {} is {} bytes, expected {} ({} cells); using a flat field",
                path.display(),
                bytes.len(),
                terrain.cells() * terrain.cells() * 4,
                terrain.cells()
            );
            Some(Heightfield::flat(terrain.cells()))
        }
    }
}

/// 把 `Terrain` 实体变成可渲染的网格（#517）。
///
/// 只在参数指纹变化时重建：（重新）生成 `MeshUploadInput` → 登记进
/// [`ProceduralMeshes`] → 让同一实体的 `MeshRenderer` 指向它，并把 `MeshRef`
/// 的 generation 清零，交给资产解析器上传。材质沿用 MeshRenderer 上作者设置的路径。
pub fn terrain_system(world: &mut prism_ecs::World) {
    cleanup_orphan_chunks(world);

    let mut pending = Vec::new();
    for (entity, terrain) in world.query::<Terrain>() {
        let key = terrain_key(entity.id(), terrain);
        let up_to_date = world
            .get::<TerrainData>(entity)
            .is_some_and(|data| data.key == key);
        if !up_to_date {
            pending.push((entity, key, terrain.clone()));
        }
    }
    for (entity, key, terrain) in pending {
        rebuild_terrain(world, entity, key, terrain);
    }
    apply_terrain_lod(world);
}

/// 每边块数对应的格子边界：长度 `chunks + 1`，至少每块一个 quad。
fn chunk_bounds(cells: usize, chunks: usize) -> Vec<usize> {
    let segments = cells.saturating_sub(1);
    (0..=chunks)
        .map(|index| index * segments / chunks)
        .collect()
}

/// 回收某个地形 owner 名下的所有分块实体与程序化网格。
fn despawn_chunks(world: &mut prism_ecs::World, owner: prism_ecs::Entity) {
    let chunks: Vec<(prism_ecs::Entity, String)> = world
        .query::<TerrainChunk>()
        .filter(|(_, chunk)| chunk.owner == owner)
        .map(|(entity, chunk)| (entity, chunk.mesh_path.clone()))
        .collect();
    let sets: Vec<prism_ecs::Entity> = world
        .query::<TerrainLodSet>()
        .filter(|(_, set)| set.owner == owner)
        .map(|(entity, _)| entity)
        .collect();
    if chunks.is_empty() && sets.is_empty() {
        return;
    }
    let mut stale_ids: HashSet<prism_ecs::Entity> =
        chunks.iter().map(|(entity, _)| *entity).collect();
    stale_ids.extend(sets.iter().copied());
    if let Some(meshes) = world.get_resource_mut::<ProceduralMeshes>() {
        for (_, path) in &chunks {
            meshes.remove(path);
        }
    }
    if world.is_alive(owner) {
        if let Some(children) = world.get_mut::<Child>(owner) {
            children.0.retain(|entity| !stale_ids.contains(entity));
        }
        if world
            .get::<Child>(owner)
            .is_some_and(|children| children.0.is_empty())
        {
            world.remove::<Child>(owner);
        }
    }
    for (entity, _) in chunks {
        world.despawn(entity);
    }
    for entity in sets {
        world.despawn(entity);
    }
}

/// 地形实体被删除后，清掉它遗留的分块（没有 owner 的块不会自己消失）。
fn cleanup_orphan_chunks(world: &mut prism_ecs::World) {
    let live: HashSet<prism_ecs::Entity> =
        world.query::<Terrain>().map(|(entity, _)| entity).collect();
    let stale: HashSet<prism_ecs::Entity> = world
        .query::<TerrainChunk>()
        .map(|(_, chunk)| chunk.owner)
        .chain(world.query::<TerrainLodSet>().map(|(_, set)| set.owner))
        .filter(|owner| !live.contains(owner))
        .collect();
    for owner in stale {
        despawn_chunks(world, owner);
    }
}

/// 按最近的启用相机到地形原点的距离选择 LOD 级，并整张地形统一切级。
fn apply_terrain_lod(world: &mut prism_ecs::World) {
    let eyes: Vec<glam::Vec3> = world
        .query2::<Camera, LocalToWorld>()
        .filter(|(_, camera, _)| camera.enabled)
        .map(|(_, _, world)| world.0.w_axis.truncate())
        .collect();

    let terrains: Vec<(prism_ecs::Entity, Vec<f32>, glam::Vec3)> = world
        .query2::<Terrain, LocalToWorld>()
        .map(|(entity, terrain, world)| {
            (
                entity,
                terrain.lod_distances.clone(),
                world.0.w_axis.truncate(),
            )
        })
        .collect();
    // 还没有世界矩阵的地形（刚 spawn）：按原点算距离。
    let terrains_without_transform: Vec<(prism_ecs::Entity, Vec<f32>)> = world
        .query::<Terrain>()
        .filter(|(entity, _)| world.get::<LocalToWorld>(*entity).is_none())
        .map(|(entity, terrain)| (entity, terrain.lod_distances.clone()))
        .collect();

    let mut levels: HashMap<prism_ecs::Entity, usize> = HashMap::new();
    for (entity, distances, position) in terrains {
        levels.insert(entity, lod_level(&distances, position, &eyes));
    }
    for (entity, distances) in terrains_without_transform {
        levels.insert(entity, lod_level(&distances, glam::Vec3::ZERO, &eyes));
    }
    if levels.is_empty() {
        return;
    }

    let sets: Vec<(prism_ecs::Entity, prism_ecs::Entity, usize)> = world
        .query::<TerrainLodSet>()
        .map(|(entity, set)| (entity, set.owner, set.level))
        .collect();
    for (set_entity, owner, set_level) in sets {
        let visible = levels.get(&owner).copied().unwrap_or(0) == set_level;
        let children: Vec<prism_ecs::Entity> = world
            .get::<Child>(set_entity)
            .map(|children| children.0.clone())
            .unwrap_or_default();
        for child in children {
            world.insert(child, crate::scene::components::LodHidden(!visible));
        }
    }
}

/// 距离 → LOD 级：取第一个大于等于距离的阈值；超过全部阈值时用最后一级。
fn lod_level(distances: &[f32], position: glam::Vec3, eyes: &[glam::Vec3]) -> usize {
    if distances.is_empty() {
        return 0;
    }
    let distance = eyes
        .iter()
        .map(|eye| (*eye - position).length())
        .fold(f32::INFINITY, f32::min);
    let distance = if distance.is_finite() {
        distance.max(0.0)
    } else {
        0.0
    };
    distances
        .iter()
        .position(|limit| {
            let limit = if limit.is_finite() {
                limit.max(0.0)
            } else {
                f32::INFINITY
            };
            distance <= limit
        })
        .unwrap_or(distances.len())
        .min(2)
}

/// 确保实体进入「等待资产解析」状态。
fn ensure_mesh_ref(world: &mut prism_ecs::World, entity: prism_ecs::Entity) {
    match world.get_mut::<MeshRef>(entity) {
        Some(reference) => reference.generation = 0,
        None => {
            world.insert(
                entity,
                MeshRef {
                    asset_id: SceneAssetId::generate(),
                    render_handle: MeshHandle::default(),
                    generation: 0,
                },
            );
        }
    }
}

/// 重建一个地形的网格：一个整块，或 `chunks × chunks` 个独立子实体。
fn rebuild_terrain(
    world: &mut prism_ecs::World,
    entity: prism_ecs::Entity,
    key: String,
    terrain: Terrain,
) {
    let Some(field) = load_heightfield(&terrain) else {
        return;
    };
    despawn_chunks(world, entity);

    // 旧的那份网格没有实体引用了，顺手回收。
    if let Some(previous) = world.get::<TerrainData>(entity) {
        let previous_path = previous.mesh_path.clone();
        if !previous_path.is_empty() {
            if let Some(meshes) = world.get_resource_mut::<ProceduralMeshes>() {
                meshes.remove(&previous_path);
            }
        }
    }
    if world.get_resource_mut::<ProceduralMeshes>().is_none() {
        world.insert_resource(ProceduralMeshes::default());
    }

    let size = terrain.size;
    let height_scale = terrain.height_scale;
    let hash = fnv1a64(&key);
    // 每块至少一个 quad；小网格（cells = 2）自动退化成单块。
    let chunks = terrain.chunk_count().min(field.cells() - 1).max(1);

    let levels = terrain.lod_level_count();
    if chunks == 1 && levels == 1 {
        let path = format!("{PROCEDURAL_PREFIX}terrain/{}-{hash:x}", entity.id());
        let mesh = field.build_mesh(size, height_scale);
        if let Some(meshes) = world.get_resource_mut::<ProceduralMeshes>() {
            meshes.insert(path.clone(), mesh);
        }

        // MeshRenderer 是作者组件：只改路径，保留材质与阴影设置。
        match world.get_mut::<MeshRenderer>(entity) {
            Some(renderer) => renderer.mesh_path = path.clone(),
            None => {
                world.insert(
                    entity,
                    MeshRenderer {
                        mesh_path: path.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        ensure_mesh_ref(world, entity);
        world.insert(
            entity,
            TerrainData {
                key,
                field: Arc::new(field),
                mesh_path: path,
                size,
                height_scale,
            },
        );
        return;
    }

    // 多块 / LOD：owner 只保留材质 / 阴影设置，不再直接绘制。
    // 每个 LOD 级是一个 set 实体，set 下挂这一级的全部 chunk。
    let renderer_template = world
        .get::<MeshRenderer>(entity)
        .cloned()
        .unwrap_or_default();
    let bounds = chunk_bounds(field.cells(), chunks);
    let last = (field.cells() - 1) as f32;
    let size_x = size[0].max(0.001);
    let size_z = size[1].max(0.001);
    let mut first_path = String::new();

    for level in 0..levels {
        let step = 1usize << level;
        let set_entity = world.spawn();
        world.insert(set_entity, LocalTransform::default());
        world.insert(
            set_entity,
            TerrainLodSet {
                owner: entity,
                level,
            },
        );
        HierarchyHelper::reparent(world, set_entity, Some(entity));

        for z in 0..chunks {
            for x in 0..chunks {
                let path = format!(
                    "{PROCEDURAL_PREFIX}terrain/{}-{hash:x}/l{level}/c{x}_{z}",
                    entity.id()
                );
                let mesh = field.build_chunk_mesh_lod(
                    size,
                    height_scale,
                    bounds[x],
                    bounds[x + 1],
                    bounds[z],
                    bounds[z + 1],
                    step,
                );
                if let Some(meshes) = world.get_resource_mut::<ProceduralMeshes>() {
                    meshes.insert(path.clone(), mesh);
                }
                if first_path.is_empty() {
                    first_path = path.clone();
                }

                let center_x = (((bounds[x] + bounds[x + 1]) as f32 * 0.5 / last) - 0.5) * size_x;
                let center_z = (((bounds[z] + bounds[z + 1]) as f32 * 0.5 / last) - 0.5) * size_z;
                let chunk_entity = world.spawn();
                world.insert(
                    chunk_entity,
                    LocalTransform {
                        position: glam::Vec3::new(center_x, 0.0, center_z),
                        ..Default::default()
                    },
                );
                let mut renderer = renderer_template.clone();
                renderer.mesh_path = path.clone();
                world.insert(chunk_entity, renderer);
                world.insert(
                    chunk_entity,
                    MeshRef {
                        asset_id: SceneAssetId::generate(),
                        render_handle: MeshHandle::default(),
                        generation: 0,
                    },
                );
                world.insert(
                    chunk_entity,
                    TerrainChunk {
                        owner: entity,
                        mesh_path: path,
                    },
                );
                HierarchyHelper::reparent(world, chunk_entity, Some(set_entity));
            }
        }
    }

    // owner 不再有 MeshRef，因此不会被主通道重复绘制。
    world.remove::<MeshRef>(entity);
    if let Some(renderer) = world.get_mut::<MeshRenderer>(entity) {
        renderer.mesh_path.clear();
    }
    world.insert(
        entity,
        TerrainData {
            key,
            field: Arc::new(field),
            mesh_path: first_path,
            size,
            height_scale,
        },
    );
}

/// FNV-1a：网格路径里的短指纹（不需要密码学强度，只要稳定）。
fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
#[path = "terrain_tests.rs"]
mod tests;
