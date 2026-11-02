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
