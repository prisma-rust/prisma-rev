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
//! 还没做（见 issue 的 Phase 2）：多 chunk + LOD、分层 splat、植被、
//! rapier 地形碰撞体（现在是解析查询，不参与物理积分）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::scene::components::{LocalToWorld, MeshRef, MeshRenderer, Terrain};
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

    /// 生成 `MeshUploadInput`：沿 `size` 铺开，`y = height * height_scale`。
    pub fn build_mesh(&self, size: [f32; 2], height_scale: f32) -> MeshUploadInput {
        let cells = self.cells;
        let [size_x, size_z] = [size[0].max(0.001), size[1].max(0.001)];
        let mut positions = Vec::with_capacity(cells * cells);
        let mut normals = Vec::with_capacity(cells * cells);
        let mut uvs = Vec::with_capacity(cells * cells);
        for y in 0..cells {
            for x in 0..cells {
                let (u, v) = (x as f32 / (cells - 1) as f32, y as f32 / (cells - 1) as f32);
                positions.push([
                    (u - 0.5) * size_x,
                    self.at(x, y) * height_scale,
                    (v - 0.5) * size_z,
                ]);
                normals.push(self.sample_normal(u, v, height_scale));
                uvs.push([u, 1.0 - v]);
            }
        }
        let mut indices = Vec::with_capacity((cells - 1) * (cells - 1) * 6);
        for y in 0..cells - 1 {
            for x in 0..cells - 1 {
                let top_left = (y * cells + x) as u32;
                let top_right = top_left + 1;
                let bottom_left = ((y + 1) * cells + x) as u32;
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
    pub mesh_path: String,
    pub size: [f32; 2],
    pub height_scale: f32,
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
        "{entity_id}|{}|{}x{}|{}|{}",
        terrain.heightmap,
        terrain.size[0],
        terrain.size[1],
        terrain.height_scale,
        terrain.cells()
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
    if pending.is_empty() {
        return;
    }

    for (entity, key, terrain) in pending {
        let Some(field) = load_heightfield(&terrain) else {
            continue;
        };
        let mesh = field.build_mesh(terrain.size, terrain.height_scale);
        let path = format!(
            "{PROCEDURAL_PREFIX}terrain/{}-{:x}",
            entity.id(),
            fnv1a64(&key)
        );

        // 旧的那份网格没有实体引用了，顺手回收。
        if let Some(previous) = world.get::<TerrainData>(entity) {
            let previous_path = previous.mesh_path.clone();
            if previous_path != path {
                if let Some(meshes) = world.get_resource_mut::<ProceduralMeshes>() {
                    meshes.remove(&previous_path);
                }
            }
        }
        if world.get_resource_mut::<ProceduralMeshes>().is_none() {
            world.insert_resource(ProceduralMeshes::default());
        }
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
        world.insert(
            entity,
            TerrainData {
                key,
                field: Arc::new(field),
                mesh_path: path,
                size: terrain.size,
                height_scale: terrain.height_scale,
            },
        );
    }
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
