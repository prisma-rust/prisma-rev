//! 二维混合空间（速度 × 方向，`#560 P1`）。
//!
//! 作者给一组样本 `(x, y) → clip`，运行时用参数平面上的一个点求姿态。
//! 与一维混合空间一样，这一层只做数学，不依赖 engine / render / ECS。
//!
//! 求值分两步：
//!
//! 1. 把样本点三角化（Delaunay）。三角化在 [`BlendSpace2D::new`] 里算一次，
//!    每帧只做「找三角形 + 重心插值」；
//! 2. 重心坐标落在凸包外时把负权重夹到 0 再归一化 —— 效果是投到最近的凸包
//!    边界上，而不是把角色弹回某个样本姿态。
//!
//! 样本少于 3 个、全部共线、或者位置非有限时没有可用三角形，`evaluate_into`
//! 返回 `false`，调用方退回一维 / 单剪辑。
//!
//! 坐标约定：`x` 通常对应速度（走 / 跑），`y` 对应方向（前 / 后 / 左 / 右）；
//! 单位由作者决定，这里只比较相对位置。

use std::sync::Arc;

use crate::blend::{blend_into, sample_at_phase};
use crate::clip::Clip;
use crate::pose::Pose;
use crate::sync::SyncGroup;

/// 二维混合空间的一个采样点：`position` 是参数平面上的坐标，`clip` 是对应剪辑。
#[derive(Debug, Clone)]
pub struct BlendSample2D {
    pub position: [f32; 2],
    pub clip: Arc<Clip>,
}

/// 二维混合空间。
///
/// 与 [`BlendSpace1D`](crate::blend::BlendSpace1D) 一样，每个剪辑采样前先把姿态
/// 重置成 `reference`：剪辑只覆盖部分轨道时，没覆盖的关节保持 reference，不会
/// 塌成单位变换。
#[derive(Debug, Clone, Default)]
pub struct BlendSpace2D {
    samples: Vec<BlendSample2D>,
    /// 三角化结果，元素是 `samples` 的下标，三角形按逆时针排列。
    triangles: Vec<[usize; 3]>,
    reference: Pose,
    /// 所有样本都有同步标记时，用第一个样本做领跑剪辑映射相位。
    sync: Option<SyncGroup>,
    /// 三个样本姿态 + 一个累加缓冲；每帧复用，避免求值时分配。
    scratch: [Pose; 4],
}

impl BlendSpace2D {
    /// 建一个二维混合空间。
    ///
    /// - 丢掉位置非有限（NaN / ∞）的样本；
    /// - 同一坐标只保留第一个样本（重复点会让三角化退化），保持作者顺序；
    /// - 少于 3 个不共线的点时不产生三角形。
    pub fn new(samples: Vec<BlendSample2D>, reference: Pose) -> Self {
        let mut unique: Vec<BlendSample2D> = Vec::with_capacity(samples.len());
        for sample in samples {
            if !sample.position[0].is_finite() || !sample.position[1].is_finite() {
                continue;
            }
            let duplicate = unique.iter().any(|kept| kept.position == sample.position);
            if !duplicate {
                unique.push(sample);
            }
        }
        let points: Vec<[f64; 2]> = unique
            .iter()
            .map(|sample| [sample.position[0] as f64, sample.position[1] as f64])
            .collect();
        let triangles = triangulate(&points);
        let sync = unique
            .iter()
            .map(|sample| sample.clip.sync_marker_set())
            .collect::<Option<Vec<_>>>()
            .and_then(SyncGroup::new)
            .filter(|group| group.len() == unique.len());
        Self {
            samples: unique,
            triangles,
            reference,
            sync,
            scratch: std::array::from_fn(|_| Pose::default()),
        }
    }

    /// 样本 `index` 实际使用的归一化相位（同 [`BlendSpace1D`](crate::blend::BlendSpace1D)）。
    fn phase_for(&self, index: usize, phase: f32) -> f32 {
        self.sync
            .as_ref()
            .map_or(phase, |group| group.map_phase(0, index, phase))
    }

    pub fn reference(&self) -> &Pose {
        &self.reference
    }

    pub fn samples(&self) -> &[BlendSample2D] {
        &self.samples
    }

    /// 三角化结果（`samples` 的下标，逆时针）。编辑器可以拿它画样本网格。
    pub fn triangles(&self) -> &[[usize; 3]] {
        &self.triangles
    }

    /// 当前参数下权重最大的样本下标（Notify 的 Dominant 策略用）。
    ///
    /// 权重相同取三角形里更靠前的顶点；没有可用三角形时返回 `None`。
    pub fn dominant_sample(&self, at: [f32; 2]) -> Option<usize> {
        let (vertices, weights) = self.weights_at(at)?;
        let (slot, _) = weights
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))?;
        Some(vertices[slot])
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty() || self.triangles.is_empty()
    }

    /// 在参数平面的 `at` 处求值到 `out`。
    ///
    /// 返回 `false` 时**不动** `out`（没有可用三角形 / 采样失败），调用方可以
    /// 保留上一帧的姿态。
    pub fn evaluate_into(&mut self, at: [f32; 2], phase: f32, out: &mut Pose) -> bool {
        let Some((vertices, weights)) = self.weights_at(at) else {
            return false;
        };

        // 先把三个样本姿态都采样到 scratch，再按权重合成。
        for (slot, vertex) in vertices.iter().enumerate() {
            self.scratch[slot].copy_from(&self.reference);
            let sample = &self.samples[*vertex];
            let phase = self.phase_for(*vertex, phase);
            if !sample_at_phase(&sample.clip, phase, &mut self.scratch[slot]) {
                return false;
            }
        }
        blend_weighted_into(out, &mut self.scratch, &weights);
        true
    }

    /// 查询点对应的三角形与三个重心权重。
    ///
    /// 点落在某个三角形内时直接用它的重心坐标；所有三角形都不包含时，取**离
    /// 查询点最近的三角形边界**上的点（边或顶点）作为结果，权重一定非负且和为 1。
    /// 没有可用三角形时返回 `None`。
    fn weights_at(&self, at: [f32; 2]) -> Option<([usize; 3], [f32; 3])> {
        if !at[0].is_finite() || !at[1].is_finite() || self.triangles.is_empty() {
            return None;
        }
        let point = [at[0] as f64, at[1] as f64];
        let mut nearest: Option<([usize; 3], [f32; 3], f64)> = None;
        for triangle in &self.triangles {
            let a = to_f64(self.samples[triangle[0]].position);
            let b = to_f64(self.samples[triangle[1]].position);
            let c = to_f64(self.samples[triangle[2]].position);
            let bary = barycentric(point, a, b, c);
            if bary.iter().all(|weight| *weight >= -1e-9) {
                let sum: f64 = bary.iter().map(|weight| weight.max(0.0)).sum();
                if sum <= 1e-12 {
                    continue;
                }
                return Some((
                    *triangle,
                    [
                        (bary[0].max(0.0) / sum) as f32,
                        (bary[1].max(0.0) / sum) as f32,
                        (bary[2].max(0.0) / sum) as f32,
                    ],
                ));
            }
            let (edge_weights, distance_sq) = closest_on_triangle(point, a, b, c);
            if nearest
                .map(|(_, _, best)| distance_sq < best)
                .unwrap_or(true)
            {
                nearest = Some((
                    *triangle,
                    [
                        edge_weights[0] as f32,
                        edge_weights[1] as f32,
                        edge_weights[2] as f32,
                    ],
                    distance_sq,
                ));
            }
        }
        nearest.map(|(triangle, weights, _)| (triangle, weights))
    }
}

/// 查询点到三角形边界的最近点，返回该点关于三个顶点的权重。
///
/// 三条边各投影一次取距离最小；顶点由线段的 `t = 0 / 1` 端点覆盖。
fn closest_on_triangle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> ([f64; 3], f64) {
    let mut best = ([0.0; 3], f64::INFINITY);
    for (weights, distance_sq) in [
        closest_on_edge(p, a, b, 0, 1),
        closest_on_edge(p, b, c, 1, 2),
        closest_on_edge(p, c, a, 2, 0),
    ] {
        if distance_sq < best.1 {
            best = (weights, distance_sq);
        }
    }
    best
}

/// 查询点到线段 `from → to` 的最近点，返回 `(a, b, c)` 顺序的权重与距离平方。
///
/// `from_slot` / `to_slot` 是线段两个端点在三角形里的下标。
fn closest_on_edge(
    p: [f64; 2],
    from: [f64; 2],
    to: [f64; 2],
    from_slot: usize,
    to_slot: usize,
) -> ([f64; 3], f64) {
    let edge = sub(to, from);
    let length_sq = edge[0] * edge[0] + edge[1] * edge[1];
    let t = if length_sq <= 1e-12 {
        0.0
    } else {
        let offset = sub(p, from);
        ((offset[0] * edge[0] + offset[1] * edge[1]) / length_sq).clamp(0.0, 1.0)
    };
    let closest = [from[0] + edge[0] * t, from[1] + edge[1] * t];
    let delta = sub(p, closest);
    let mut weights = [0.0; 3];
    weights[from_slot] = 1.0 - t;
    weights[to_slot] = t;
    (weights, delta[0] * delta[0] + delta[1] * delta[1])
}

fn to_f64(point: [f32; 2]) -> [f64; 2] {
    [point[0] as f64, point[1] as f64]
}

/// 三个样本姿态按权重合成到 `out`。
///
/// 从权重最大的样本开始，逐个用 `blend_into` 累加（旋转走 slerp，与一维混合
/// 空间一致）。`scratch[3]` 用来保存累加结果，因为 `blend_into` 不能把同一个
/// 姿态同时当输入和输出。
fn blend_weighted_into(out: &mut Pose, scratch: &mut [Pose; 4], weights: &[f32; 3]) {
    let mut order = [0usize, 1, 2];
    // 权重降序：先放最大的，避免 `t = w / total` 从 0 开始。
    order.sort_by(|a, b| weights[*b].total_cmp(&weights[*a]));
    let first = order[0];
    out.copy_from(&scratch[first]);
    let mut total = weights[first];
    for &slot in &order[1..] {
        let weight = weights[slot];
        if weight <= 0.0 {
            continue;
        }
        scratch[3].copy_from(out);
        let t = weight / (total + weight);
        blend_into(out, &scratch[3], &scratch[slot], t);
        total += weight;
    }
}

/// 三角形内一点的三个重心权重（与顶点顺序对应），顶点须逆时针。
///
/// 退化三角形返回全 0，让调用方把它当成「不包含查询点」。
fn barycentric(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> [f64; 3] {
    let area = cross(sub(b, a), sub(c, a));
    if area.abs() <= 1e-12 {
        return [0.0; 3];
    }
    let wa = cross(sub(b, p), sub(c, p)) / area;
    let wb = cross(sub(c, p), sub(a, p)) / area;
    let wc = 1.0 - wa - wb;
    [wa, wb, wc]
}

fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn cross(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

/// Bowyer–Watson 增量三角化。返回 `points` 的下标三元组（逆时针）。
///
/// 点数很少（作者手填的混合空间样本），所以这里直接 O(n²) 实现；不做空间索引。
fn triangulate(points: &[[f64; 2]]) -> Vec<[usize; 3]> {
    if points.len() < 3 {
        return Vec::new();
    }
    let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for point in points {
        min[0] = min[0].min(point[0]);
        min[1] = min[1].min(point[1]);
        max[0] = max[0].max(point[0]);
        max[1] = max[1].max(point[1]);
    }
    let span = (max[0] - min[0]).max(max[1] - min[1]).max(1.0);
    let mid = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
    // 超级三角形：把整个包围盒包住，最后再删掉引用它的三角形。
    let super_start = points.len();
    let mut all = points.to_vec();
    // 逆时针顺序：外接圆判定依赖三角形方向。
    all.push([mid[0] - 32.0 * span, mid[1] - span]);
    all.push([mid[0] + 32.0 * span, mid[1] - span]);
    all.push([mid[0], mid[1] + 32.0 * span]);

    let mut triangles: Vec<[usize; 3]> = vec![[super_start, super_start + 1, super_start + 2]];
    for index in 0..points.len() {
        let mut edges: Vec<(usize, usize)> = Vec::new();
        let mut kept: Vec<[usize; 3]> = Vec::new();
        for triangle in &triangles {
            if in_circumcircle(
                all[index],
                all[triangle[0]],
                all[triangle[1]],
                all[triangle[2]],
            ) {
                for edge in [
                    (triangle[0], triangle[1]),
                    (triangle[1], triangle[2]),
                    (triangle[2], triangle[0]),
                ] {
                    // 共享边会在相邻三角形里以相反方向出现，抵消掉；剩下的就是
                    // 新三角形的边界。
                    if let Some(position) = edges
                        .iter()
                        .position(|candidate| *candidate == (edge.1, edge.0))
                    {
                        edges.swap_remove(position);
                    } else {
                        edges.push(edge);
                    }
                }
            } else {
                kept.push(*triangle);
            }
        }
        for (a, b) in edges {
            let mut triangle = [a, b, index];
            if signed_area(all[triangle[0]], all[triangle[1]], all[triangle[2]]) < 0.0 {
                triangle.swap(1, 2);
            }
            kept.push(triangle);
        }
        triangles = kept;
    }
    // 删掉超级三角形的顶点，并保持确定性顺序（按最小下标 + 其余两个排序）。
    let mut result: Vec<[usize; 3]> = triangles
        .into_iter()
        .filter(|triangle| triangle.iter().all(|index| *index < super_start))
        .map(|mut triangle| {
            if signed_area(all[triangle[0]], all[triangle[1]], all[triangle[2]]) < 0.0 {
                triangle.swap(1, 2);
            }
            triangle
        })
        .collect();
    result.sort_by_key(|triangle| {
        let mut sorted = *triangle;
        sorted.sort_unstable();
        sorted
    });
    result
}

/// 点是否严格落在三角形的外接圆内（三角形须逆时针）。
fn in_circumcircle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    // 平移到 p，避免大坐标相减丢精度。
    let (ax, ay) = (a[0] - p[0], a[1] - p[1]);
    let (bx, by) = (b[0] - p[0], b[1] - p[1]);
    let (cx, cy) = (c[0] - p[0], c[1] - p[1]);
    let det = (ax * ax + ay * ay) * (bx * cy - by * cx) - (bx * bx + by * by) * (ax * cy - ay * cx)
        + (cx * cx + cy * cy) * (ax * by - ay * bx);
    // 逆时针三角形的行列式为正表示在外接圆内；共圆（≈0）不算，避免无穷递归。
    det > 1e-12
}

fn signed_area(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    cross(sub(b, a), sub(c, a)) * 0.5
}

#[cfg(test)]
#[path = "blend_space2d_tests.rs"]
mod tests;
