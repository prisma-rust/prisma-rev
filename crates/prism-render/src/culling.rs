//! 包围盒与视锥剔除（纯数学，不含 Vulkan 句柄，也不依赖 glam）。
//!
//! 提取阶段（`prism-engine` 的 `scene_render_system`）在提交绘制项之前做一次
//! 保守剔除：实体在**主相机视锥**外就整帧不提交。这是 #491 建议里的第一步 ——
//! 以前每个实体每帧都是一次 draw，一半还在相机背后。
//!
//! 约定：
//!
//! - 坐标与矩阵遵循 `docs/DESIGN.md` 的约定：右手系、相机看 −Z、列主序
//!   `mat[col][row]`、`clip = projection * view * model`、Vulkan 深度范围
//!   **[0, 1]**。近平面因此是 `z_clip >= 0`（第 2 行），远平面是
//!   `w - z >= 0`（第 3 行 − 第 2 行），不是 OpenGL 的 `w + z`。
//! - 剔除是**保守**的：包围盒与视锥有任何交叠就保留。宁可多画一个，也不要把半个
//!   物体裁掉。
//! - 没有包围盒（程序化网格、句柄还没解析）或矩阵非有限时**不剔除**：数据未知时
//!   保持旧行为，比让物体凭空消失好。

/// 轴对齐包围盒（局部或世界空间，由调用方决定）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    pub fn new(min: [f32; 3], max: [f32; 3]) -> Self {
        Self { min, max }
    }

    /// 由顶点位置计算包围盒；空集合或非有限坐标返回 `None`。
    pub fn from_positions<I: IntoIterator<Item = [f32; 3]>>(positions: I) -> Option<Self> {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut any = false;
        for position in positions {
            for axis in 0..3 {
                let value = position[axis];
                if !value.is_finite() {
                    return None;
                }
                min[axis] = min[axis].min(value);
                max[axis] = max[axis].max(value);
            }
            any = true;
        }
        any.then_some(Self { min, max })
    }

    /// 两个包围盒的并集（分量取 min/max）。任一方无效时返回另一方。
    ///
    /// 蒙皮网格用它把「每个骨骼变换后的 bind 包围盒」合成一个保守包围盒
    /// （#560 P0-3c）：蒙皮后的顶点是这些盒子的凸包，所以并集不会漏掉。
    pub fn union(&self, other: &Self) -> Option<Self> {
        match (self.is_valid(), other.is_valid()) {
            (true, true) => {
                let mut bounds = *self;
                for axis in 0..3 {
                    bounds.min[axis] = self.min[axis].min(other.min[axis]);
                    bounds.max[axis] = self.max[axis].max(other.max[axis]);
                }
                Some(bounds)
            }
            (true, false) => Some(*self),
            (false, true) => Some(*other),
            (false, false) => None,
        }
    }

    /// `min <= max` 且分量有限。空/损坏的包围盒不参与剔除。
    pub fn is_valid(&self) -> bool {
        (0..3).all(|axis| {
            let (min, max) = (self.min[axis], self.max[axis]);
            min.is_finite() && max.is_finite() && min <= max
        })
    }

    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// 外接球半径（半个对角线长度）。
    pub fn radius(&self) -> f32 {
        let dx = (self.max[0] - self.min[0]) * 0.5;
        let dy = (self.max[1] - self.min[1]) * 0.5;
        let dz = (self.max[2] - self.min[2]) * 0.5;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// 8 个角点（顺序固定，测试与拾取都依赖这一点）。
    pub fn corners(&self) -> [[f32; 3]; 8] {
        let [x0, y0, z0] = self.min;
        let [x1, y1, z1] = self.max;
        [
            [x0, y0, z0],
            [x1, y0, z0],
            [x0, y1, z0],
            [x1, y1, z0],
            [x0, y0, z1],
            [x1, y0, z1],
            [x0, y1, z1],
            [x1, y1, z1],
        ]
    }

    /// 用模型矩阵（列主序 `mat[col][row]`）变换成世界空间的轴对齐包围盒。
    ///
    /// 非有限变换或无效包围盒返回 `None` —— 调用方据此放弃剔除。
    pub fn transformed(&self, model: &[[f32; 4]; 4]) -> Option<Self> {
        if !self.is_valid() {
            return None;
        }
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for corner in self.corners() {
            let world = transform_point3(model, corner);
            for axis in 0..3 {
                let value = world[axis];
                if !value.is_finite() {
                    return None;
                }
                min[axis] = min[axis].min(value);
                max[axis] = max[axis].max(value);
            }
        }
        Some(Self { min, max })
    }
}

/// 列主序 `mat[col][row]` 变换一个点（`w = 1`，不做透视除法）。
fn transform_point3(model: &[[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
    let [x, y, z] = point;
    [
        model[0][0] * x + model[1][0] * y + model[2][0] * z + model[3][0],
        model[0][1] * x + model[1][1] * y + model[2][1] * z + model[3][1],
        model[0][2] * x + model[1][2] * y + model[2][2] * z + model[3][2],
    ]
}

/// 视锥的 6 个平面（`[nx, ny, nz, d]`：单位法线 + 距离）。
///
/// 平面顺序无关紧要（只用「是否全在某一侧之外」），但保持
/// 左 / 右 / 下 / 上 / 近 / 远 便于调试。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frustum {
    planes: [[f32; 4]; 6],
}

impl Frustum {
    /// 从 `view_proj`（列主序）提取视锥平面。
    ///
    /// 退化矩阵（全零 / 非有限 / 法线长度为 0，例如单位矩阵）返回 `None`：
    /// 这种输入下任何「剔除」都只是瞎裁，调用方应当跳过剔除。
    pub fn from_view_proj(view_proj: &[[f32; 4]; 4]) -> Option<Self> {
        let row = |index: usize| {
            [
                view_proj[0][index],
                view_proj[1][index],
                view_proj[2][index],
                view_proj[3][index],
            ]
        };
        let (r0, r1, r2, r3) = (row(0), row(1), row(2), row(3));
        let raw = [
            add4(r3, r0), // 左：  w + x >= 0
            sub4(r3, r0), // 右：  w - x >= 0
            add4(r3, r1), // 下：  w + y >= 0
            sub4(r3, r1), // 上：  w - y >= 0
            r2,           // 近：  z >= 0（Vulkan 深度范围 [0, 1]）
            sub4(r3, r2), // 远：  w - z >= 0
        ];
        let mut planes = [[0.0f32; 4]; 6];
        for (slot, plane) in planes.iter_mut().zip(raw) {
            let length = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
            if !length.is_finite() || length < 1e-6 {
                return None;
            }
            for axis in 0..4 {
                slot[axis] = plane[axis] / length;
            }
        }
        Some(Self { planes })
    }

    /// 包围盒是否与视锥相交（保守：贴边、包含、部分交叠都返回 `true`）。
    ///
    /// 无效包围盒一律返回 `true`（不剔除）。
    pub fn intersects_aabb(&self, aabb: &Aabb) -> bool {
        if !aabb.is_valid() {
            return true;
        }
        for plane in self.planes {
            // 取包围盒在该平面法线方向上「最远」的角点（p-vertex）：
            // 它都在平面外侧，整个盒子里就没有可见部分。
            let positive = [
                if plane[0] > 0.0 {
                    aabb.max[0]
                } else {
                    aabb.min[0]
                },
                if plane[1] > 0.0 {
                    aabb.max[1]
                } else {
                    aabb.min[1]
                },
                if plane[2] > 0.0 {
                    aabb.max[2]
                } else {
                    aabb.min[2]
                },
            ];
            let distance =
                plane[0] * positive[0] + plane[1] * positive[1] + plane[2] * positive[2] + plane[3];
            if distance < 0.0 {
                return false;
            }
        }
        true
    }

    /// 6 个平面（调试 / 测试用）。
    pub fn planes(&self) -> &[[f32; 4]; 6] {
        &self.planes
    }
}

/// 屏幕空间粗深度（“保守遮挡剔除”用的第一级深度缓存）。
///
/// 每个 `tile_size × tile_size` 的屏幕块保存块内**最大**深度（最远的片元），
/// 背景保持清屏深度 `1.0`。这样「整个包围盒都被挡住」的判据可以写成：
/// 盒子投影矩形覆盖的每一块都满足 `tile_max < 盒子最近点深度` —— 只要块里
/// 还有背景（1.0）或更远的表面，就不会误剔。
///
/// 深度来自**上一帧**：渲染器把主通道的深度图异步回读、下采样后交给这里。
/// 延迟一帧只会让快速移动的物体偶尔晚一帧被剔除，不会影响正确性。
#[derive(Debug, Clone, PartialEq)]
pub struct DepthTiles {
    width: u32,
    height: u32,
    tile_size: u32,
    /// 行主序，长度 `width * height`。
    max_depth: Vec<f32>,
}

impl DepthTiles {
    /// 从一张行主序的 `D32_SFLOAT` 深度图构建粗深度。
    ///
    /// `samples.len()` 必须至少是 `extent[0] * extent[1]`；非有限深度按
    /// `1.0`（最远）处理，宁可少剔。参数非法时返回 `None`。
    pub fn from_samples(extent: [u32; 2], tile_size: u32, samples: &[f32]) -> Option<Self> {
        let (width, height) = (extent[0], extent[1]);
        if width == 0 || height == 0 || tile_size == 0 {
            return None;
        }
        if (samples.len() as u64) < u64::from(width) * u64::from(height) {
            return None;
        }
        let tiles_x = width.div_ceil(tile_size);
        let tiles_y = height.div_ceil(tile_size);
        let mut max_depth = vec![f32::NEG_INFINITY; (tiles_x * tiles_y) as usize];
        for y in 0..height {
            let tile_row = (y / tile_size) * tiles_x;
            let row = (y * width) as usize;
            for x in 0..width {
                let depth = samples[row + x as usize];
                if !depth.is_finite() {
                    continue;
                }
                let slot = &mut max_depth[(tile_row + x / tile_size) as usize];
                if depth > *slot {
                    *slot = depth;
                }
            }
        }
        // 没有任何有限样本的块保持清屏深度（最远），不会误剔。
        for depth in &mut max_depth {
            if !depth.is_finite() {
                *depth = 1.0;
            }
        }
        Some(Self {
            width: tiles_x,
            height: tiles_y,
            tile_size,
            max_depth,
        })
    }

    pub fn tiles(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn tile_size(&self) -> u32 {
        self.tile_size
    }

    /// 包围盒是否被**完全**挡住。
    ///
    /// 判据是保守的：只有 8 个角点都在屏幕内、深度都在 `[0, 1]`，并且投影
    /// 矩形覆盖的每个深度块都严格近于盒子的最近点时才返回 `true`。任何角点
    /// 在屏幕外、盒子跨过近平面、或块里有背景，都保持可见。
    ///
    /// `view_proj` 必须是**生成这张深度图那一帧**的矩阵；`margin_tiles` 用来
    /// 向外扩一圈，抵消一帧延迟带来的边缘误差。
    pub fn is_box_occluded(
        &self,
        view_proj: &[[f32; 4]; 4],
        aabb: &Aabb,
        margin_tiles: u32,
    ) -> bool {
        if !aabb.is_valid() || self.max_depth.is_empty() {
            return false;
        }
        const BIAS: f32 = 1e-4;
        let mut nearest = f32::INFINITY;
        let (mut min_uv, mut max_uv) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for corner in aabb.corners() {
            let [x, y, z] = corner;
            let clip = [
                view_proj[0][0] * x + view_proj[1][0] * y + view_proj[2][0] * z + view_proj[3][0],
                view_proj[0][1] * x + view_proj[1][1] * y + view_proj[2][1] * z + view_proj[3][1],
                view_proj[0][2] * x + view_proj[1][2] * y + view_proj[2][2] * z + view_proj[3][2],
                view_proj[0][3] * x + view_proj[1][3] * y + view_proj[2][3] * z + view_proj[3][3],
            ];
            // 角点在相机平面上或背后：没有可靠的屏幕投影，保守保留。
            if !clip[3].is_finite() || clip[3] <= 1e-6 {
                return false;
            }
            let inv_w = 1.0 / clip[3];
            let ndc = [clip[0] * inv_w, clip[1] * inv_w, clip[2] * inv_w];
            if !ndc.iter().all(|value| value.is_finite()) {
                return false;
            }
            if !(0.0..=1.0).contains(&ndc[2]) {
                return false;
            }
            let uv = [ndc[0] * 0.5 + 0.5, ndc[1] * 0.5 + 0.5];
            if uv.iter().any(|value| !(-0.001..=1.001).contains(value)) {
                return false;
            }
            let uv = [uv[0].clamp(0.0, 1.0), uv[1].clamp(0.0, 1.0)];
            min_uv = [min_uv[0].min(uv[0]), min_uv[1].min(uv[1])];
            max_uv = [max_uv[0].max(uv[0]), max_uv[1].max(uv[1])];
            nearest = nearest.min(ndc[2]);
        }
        if !nearest.is_finite() {
            return false;
        }
        let tile = |uv: f32, tiles: u32| -> u32 {
            ((uv * tiles as f32).floor() as u32).min(tiles.saturating_sub(1))
        };
        let x0 = tile(min_uv[0], self.width).saturating_sub(margin_tiles);
        let y0 = tile(min_uv[1], self.height).saturating_sub(margin_tiles);
        let x1 = tile(max_uv[0], self.width)
            .saturating_add(margin_tiles)
            .min(self.width - 1);
        let y1 = tile(max_uv[1], self.height)
            .saturating_add(margin_tiles)
            .min(self.height - 1);
        let mut farthest = 0.0f32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                farthest = farthest.max(self.max_depth[(y * self.width + x) as usize]);
            }
        }
        farthest < nearest - BIAS
    }
}

/// 一帧的遮挡剔除结果（报告 / 测试用）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OcclusionCounts {
    /// 参与测试的绘制项数（没有世界包围盒的不算）。
    pub tested: usize,
    /// 被判定为完全遮挡、主通道不再提交的数量。
    pub culled: usize,
}

/// 把上一帧粗深度判定为完全遮挡的绘制项标记成「主通道不可见」。
///
/// 只改 `visible_in_main_pass`：阴影通道不读这个字段，所以被挡住的物体仍然
/// 会投下可见的影子（这是正确的 —— 遮挡关系只对当前相机成立）。
pub fn mark_occluded_draws(
    items: &mut [crate::render_graph::DrawItem],
    tiles: &DepthTiles,
    view_proj: &[[f32; 4]; 4],
    margin_tiles: u32,
) -> OcclusionCounts {
    let mut counts = OcclusionCounts::default();
    for item in items {
        if !item.visible_in_main_pass {
            continue;
        }
        let Some(bounds) = item.world_bounds.as_ref() else {
            continue;
        };
        counts.tested += 1;
        if tiles.is_box_occluded(view_proj, bounds, margin_tiles) {
            item.visible_in_main_pass = false;
            counts.culled += 1;
        }
    }
    counts
}

fn add4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}

fn sub4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 手写小矩阵工具（本 crate 不依赖 glam） ─────────────────────────

    fn mul4(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
        let mut out = [[0.0f32; 4]; 4];
        for col in 0..4 {
            for row in 0..4 {
                out[col][row] = (0..4).map(|k| a[k][row] * b[col][k]).sum();
            }
        }
        out
    }

    fn translation(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
        let mut m = identity();
        m[3] = [x, y, z, 1.0];
        m
    }

    fn scale(s: f32) -> [[f32; 4]; 4] {
        let mut m = identity();
        m[0][0] = s;
        m[1][1] = s;
        m[2][2] = s;
        m
    }

    fn identity() -> [[f32; 4]; 4] {
        let mut m = [[0.0f32; 4]; 4];
        for (axis, column) in m.iter_mut().enumerate() {
            column[axis] = 1.0;
        }
        m
    }

    /// Vulkan 风格透视（深度 0..1，y 向下翻转），fov 45°，近 0.1 / 远 100。
    fn perspective() -> [[f32; 4]; 4] {
        let fov = 45f32.to_radians();
        let (near, far) = (0.1f32, 100.0f32);
        let f = 1.0 / (fov * 0.5).tan();
        let mut m = [[0.0f32; 4]; 4];
        m[0][0] = f;
        m[1][1] = -f; // Vulkan y-flip
        m[2][2] = far / (near - far);
        m[2][3] = -1.0;
        m[3][2] = (near * far) / (near - far);
        m
    }

    /// 相机在原点、看向 −Z 的 `view_proj`。
    fn camera() -> Frustum {
        Frustum::from_view_proj(&perspective()).expect("camera frustum must be valid")
    }

    fn unit_box_at(x: f32, y: f32, z: f32) -> Aabb {
        Aabb::new([x - 0.5, y - 0.5, z - 0.5], [x + 0.5, y + 0.5, z + 0.5])
    }

    #[test]
    fn a_box_in_front_of_the_camera_is_kept() {
        assert!(camera().intersects_aabb(&unit_box_at(0.0, 0.0, -5.0)));
    }

    #[test]
    fn a_box_behind_the_camera_is_culled() {
        assert!(!camera().intersects_aabb(&unit_box_at(0.0, 0.0, 5.0)));
    }

    #[test]
    fn a_box_far_to_the_side_is_culled() {
        assert!(!camera().intersects_aabb(&unit_box_at(100.0, 0.0, -5.0)));
    }

    #[test]
    fn a_box_past_the_far_plane_is_culled() {
        assert!(!camera().intersects_aabb(&unit_box_at(0.0, 0.0, -200.0)));
    }

    #[test]
    fn a_box_containing_the_camera_is_kept() {
        assert!(camera().intersects_aabb(&unit_box_at(0.0, 0.0, 0.0)));
    }

    #[test]
    fn a_box_straddling_the_left_plane_is_kept() {
        // 在 z = −5 处半个画面宽度约 2.07（45° 视场），这个盒子一半在外。
        assert!(camera().intersects_aabb(&Aabb::new([-2.5, -0.2, -5.5], [-2.0, 0.2, -4.5],)));
    }

    #[test]
    fn an_invalid_box_is_never_culled() {
        let inverted = Aabb::new([1.0; 3], [0.0; 3]);
        assert!(!inverted.is_valid());
        assert!(camera().intersects_aabb(&inverted));

        let nan = Aabb::new([f32::NAN; 3], [1.0; 3]);
        assert!(camera().intersects_aabb(&nan));
    }

    #[test]
    fn a_degenerate_matrix_produces_no_frustum() {
        // 全零 / 归一化后长度仍为 0 的矩阵：没有可用的平面。
        assert!(Frustum::from_view_proj(&[[0.0; 4]; 4]).is_none());
        assert!(Frustum::from_view_proj(&scale(0.0)).is_none());
    }

    #[test]
    fn the_planes_are_normalised() {
        for plane in camera().planes() {
            let length = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
            assert!(
                (length - 1.0).abs() < 1e-5,
                "plane {plane:?} is not unit length"
            );
        }
    }

    #[test]
    fn a_box_spanning_the_camera_plane_is_kept() {
        // z 从 +1 跨到 −1：既有相机背后的部分，也有近平面外的部分。
        let straddling = Aabb::new([-0.1, -0.1, -1.0], [0.1, 0.1, 1.0]);
        assert!(camera().intersects_aabb(&straddling));
        // 同一个盒子整体推到相机背后（z 1..2）就看不见了。
        let behind = Aabb::new([-0.1, -0.1, 1.0], [0.1, 0.1, 2.0]);
        assert!(!camera().intersects_aabb(&behind));
    }

    #[test]
    fn transforming_a_box_keeps_its_world_extent() {
        let local = Aabb::new([0.0, 0.0, 0.0], [1.0, 2.0, 3.0]);
        let world = local.transformed(&translation(4.0, 5.0, 6.0)).unwrap();
        assert_eq!(world.min, [4.0, 5.0, 6.0]);
        assert_eq!(world.max, [5.0, 7.0, 9.0]);
    }

    #[test]
    fn a_non_finite_transform_has_no_world_box() {
        let local = Aabb::new([0.0; 3], [1.0; 3]);
        assert!(local
            .transformed(&translation(f32::NAN, 0.0, 0.0))
            .is_none());
    }

    #[test]
    fn a_moved_camera_keeps_its_own_box_visible() {
        // 相机平移到 (0, 0, -10) 后看向 −Z：(0,0,-15) 在视锥内，(0,0,-5) 在背后。
        let view_proj = mul4(&perspective(), &translation(0.0, 0.0, 10.0));
        let frustum = Frustum::from_view_proj(&view_proj).unwrap();
        assert!(frustum.intersects_aabb(&unit_box_at(0.0, 0.0, -15.0)));
        assert!(!frustum.intersects_aabb(&unit_box_at(0.0, 0.0, -5.0)));
    }

    #[test]
    fn union_wraps_both_boxes_and_ignores_broken_ones() {
        let left = Aabb::new([-1.0, 0.0, -1.0], [0.0, 1.0, 1.0]);
        let right = Aabb::new([0.5, -2.0, -1.0], [2.0, 0.0, 1.0]);
        let merged = left.union(&right).expect("both valid");
        assert_eq!(merged.min, [-1.0, -2.0, -1.0]);
        assert_eq!(merged.max, [2.0, 1.0, 1.0]);
        // 无效的一方被忽略，两个都无效则没有结果。
        let broken = Aabb::new([f32::NAN, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert_eq!(left.union(&broken), Some(left));
        assert_eq!(broken.union(&broken), None);
    }

    fn uniform_tiles(depth: f32) -> DepthTiles {
        DepthTiles::from_samples([128, 128], 4, &vec![depth; 128 * 128]).expect("valid samples")
    }

    #[test]
    fn a_box_behind_a_full_screen_surface_is_occluded() {
        // 墙的深度 0.99（约 z = −10），盒子在 z = −50（深度 ≈ 0.999）。
        let tiles = uniform_tiles(0.99);
        let camera = perspective();
        assert!(tiles.is_box_occluded(&camera, &unit_box_at(0.0, 0.0, -50.0), 1));
    }

    #[test]
    fn a_box_in_front_of_the_surface_stays_visible() {
        // z = −5 的深度 ≈ 0.981 < 0.99：盒子比墙近，不能剔。
        let tiles = uniform_tiles(0.99);
        let camera = perspective();
        assert!(!tiles.is_box_occluded(&camera, &unit_box_at(0.0, 0.0, -5.0), 1));
    }

    #[test]
    fn a_background_tile_inside_the_rect_keeps_the_box_visible() {
        // 右半边是背景（1.0）：盒子投影覆盖到它就说明那里没有遮挡物。
        let mut samples = vec![0.99f32; 128 * 128];
        for y in 0..128 {
            for x in 64..128 {
                samples[y * 128 + x] = 1.0;
            }
        }
        let tiles = DepthTiles::from_samples([128, 128], 4, &samples).unwrap();
        let camera = perspective();
        // 盒子在 z = −50、x = 6：投影落在右半边。
        assert!(!tiles.is_box_occluded(&camera, &unit_box_at(6.0, 0.0, -50.0), 1));
    }

    #[test]
    fn boxes_touching_the_screen_edge_or_camera_plane_stay_visible() {
        let tiles = uniform_tiles(0.99);
        let camera = perspective();
        // 屏幕外（x = 100）与跨近平面（z 从 −0.05 到 +0.05）都保守保留。
        assert!(!tiles.is_box_occluded(&camera, &unit_box_at(100.0, 0.0, -50.0), 1));
        assert!(!tiles.is_box_occluded(
            &camera,
            &Aabb::new([-0.1, -0.1, -0.05], [0.1, 0.1, 0.05]),
            1
        ));
        // 无效包围盒同样不剔。
        assert!(!tiles.is_box_occluded(&camera, &Aabb::new([f32::NAN; 3], [1.0; 3]), 1));
    }

    #[test]
    fn marking_draws_only_touches_the_main_pass_flag() {
        use crate::render_graph::DrawItem;
        fn item(bounds: Option<Aabb>) -> DrawItem {
            DrawItem {
                mesh: Default::default(),
                entity_id: u32::MAX,
                model: identity(),
                material: None,
                cast_shadow: true,
                visible_in_main_pass: true,
                cast_shadow_two_sided: false,
                bone_offset: None,
                splat_slots: [u32::MAX; 4],
                splat_tiling: 0.0,
                state: Default::default(),
                world_bounds: bounds,
                index_start: 0,
                index_count: None,
            }
        }
        let tiles = uniform_tiles(0.99);
        let hidden = item(Some(unit_box_at(0.0, 0.0, -50.0)));
        let visible = item(Some(unit_box_at(0.0, 0.0, -5.0)));
        let unknown = item(None);
        let mut items = vec![hidden, visible, unknown];
        let counts = mark_occluded_draws(&mut items, &tiles, &perspective(), 1);
        assert_eq!(
            counts,
            OcclusionCounts {
                tested: 2,
                culled: 1
            }
        );
        assert!(!items[0].visible_in_main_pass);
        assert!(items[1].visible_in_main_pass);
        assert!(items[2].visible_in_main_pass);
        // 阴影仍然由 `cast_shadow` 决定，剔除不碰它。
        assert!(items[0].cast_shadow);
    }

    #[test]
    fn a_depth_grid_rejects_bad_input() {
        assert!(DepthTiles::from_samples([0, 4], 4, &[]).is_none());
        assert!(DepthTiles::from_samples([4, 4], 0, &[0.0; 16]).is_none());
        assert!(DepthTiles::from_samples([4, 4], 4, &[0.0; 15]).is_none());
        let tiles = DepthTiles::from_samples([5, 3], 4, &[f32::NAN; 15]).unwrap();
        assert_eq!(tiles.tiles(), (2, 1));
        // 非有限深度按最远处理，绝不剔除。
        assert!(!tiles.is_box_occluded(&perspective(), &unit_box_at(0.0, 0.0, -50.0), 0));
    }

    #[test]
    fn box_bounds_come_from_positions() {
        let bounds = Aabb::from_positions([[1.0, -2.0, 3.0], [-1.0, 2.0, 0.0], [0.5, 0.0, 6.0]])
            .expect("finite positions give bounds");
        assert_eq!(bounds.min, [-1.0, -2.0, 0.0]);
        assert_eq!(bounds.max, [1.0, 2.0, 6.0]);
        assert!(Aabb::from_positions([]).is_none());
        assert!(Aabb::from_positions([[f32::NAN, 0.0, 0.0]]).is_none());
    }
}
