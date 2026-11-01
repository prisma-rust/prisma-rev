//! 视口数学：世界 ↔ 屏幕投影、射线、AABB 拾取、gizmo 轴命中。
//!
//! 与引擎共享同一套坐标约定（右手系、相机看 −Z、Vulkan 深度 [0,1]）。
//! 拾取优先用实体真实局部包围盒（网格 AABB / 精灵尺寸），没有数据时退回
//! 单位立方体（`[-0.5,0.5]³` × scale）。

use crate::doc::{SceneDoc, TransformJson};
use crate::workspace::OrbitCamera;

pub const GIZMO_SCREEN_PIXELS: f32 = 90.0;
pub const GIZMO_PICK_RADIUS: f32 = 8.0;
/// 旋转 gizmo 圆环的命中带宽（像素）。
pub const GIZMO_RING_TOLERANCE: f32 = 10.0;

pub const IDENTITY_BASIS: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
pub const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// 实体在自身局部空间的包围盒，用于精确拾取（#600）。
///
/// 网格来自 cooked mesh 的 local AABB；精灵由 `frame_size / pixels_per_unit`
/// 和 pivot 推出。没有数据时退回原有的单位立方体代理。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickBounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl PickBounds {
    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    pub fn half(&self) -> [f32; 3] {
        [
            (self.max[0] - self.min[0]).abs() * 0.5,
            (self.max[1] - self.min[1]).abs() * 0.5,
            (self.max[2] - self.min[2]).abs() * 0.5,
        ]
    }
}

/// gizmo 的坐标空间：世界轴或实体自身轴。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GizmoSpace {
    #[default]
    World,
    Local,
}

impl GizmoSpace {
    pub fn label(self) -> &'static str {
        match self {
            GizmoSpace::World => "世界",
            GizmoSpace::Local => "局部",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            GizmoSpace::World => GizmoSpace::Local,
            GizmoSpace::Local => GizmoSpace::World,
        }
    }
}

/// 旋转 gizmo 的圆环在屏幕上的半径（每个轴一条）。
pub fn ring_screen_radius(
    frame: &CameraFrame,
    origin: [f32; 3],
    axis: usize,
    world_radius: f32,
    width: f32,
    height: f32,
) -> Option<(f32, f32, f32)> {
    ring_screen_radius_oriented(
        frame,
        origin,
        IDENTITY_BASIS,
        axis,
        world_radius,
        width,
        height,
    )
}

/// 局部坐标下的圆环半径：`basis` 的三列是实体局部轴在世界空间的方向。
pub fn ring_screen_radius_oriented(
    frame: &CameraFrame,
    origin: [f32; 3],
    basis: [[f32; 3]; 3],
    axis: usize,
    world_radius: f32,
    width: f32,
    height: f32,
) -> Option<(f32, f32, f32)> {
    // 圆环在垂直于轴的平面里；用两条正交半径的屏幕长度平均近似半径。
    let (u, v) = match axis {
        0 => (basis[1], basis[2]),
        1 => (basis[0], basis[2]),
        _ => (basis[0], basis[1]),
    };
    let start = project(origin, frame, width, height)?;
    let a = project(add(origin, scale(u, world_radius)), frame, width, height)?;
    let b = project(add(origin, scale(v, world_radius)), frame, width, height)?;
    let ra = ((a.0 - start.0).powi(2) + (a.1 - start.1).powi(2)).sqrt();
    let rb = ((b.0 - start.0).powi(2) + (b.1 - start.1).powi(2)).sqrt();
    Some((start.0, start.1, (ra + rb) * 0.5))
}

/// 命中旋转圆环：返回轴。屏幕距离与投影半径之差小于带宽即命中。
pub fn pick_rotation_axis(
    point: (f32, f32),
    frame: &CameraFrame,
    origin: [f32; 3],
    world_radius: f32,
    width: f32,
    height: f32,
) -> Option<usize> {
    pick_rotation_axis_oriented(
        point,
        frame,
        origin,
        IDENTITY_BASIS,
        world_radius,
        width,
        height,
    )
}

/// 局部坐标下的圆环命中。
pub fn pick_rotation_axis_oriented(
    point: (f32, f32),
    frame: &CameraFrame,
    origin: [f32; 3],
    basis: [[f32; 3]; 3],
    world_radius: f32,
    width: f32,
    height: f32,
) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for axis in 0..3 {
        let Some((cx, cy, radius)) =
            ring_screen_radius_oriented(frame, origin, basis, axis, world_radius, width, height)
        else {
            continue;
        };
        if radius < 12.0 {
            continue;
        }
        let distance = ((point.0 - cx).powi(2) + (point.1 - cy).powi(2)).sqrt();
        let error = (distance - radius).abs();
        if error <= GIZMO_RING_TOLERANCE && best.map(|(_, best)| error < best).unwrap_or(true) {
            best = Some((axis, error));
        }
    }
    best.map(|(axis, _)| axis)
}

/// 屏幕点相对圆心的角度（用于旋转拖动）。
pub fn screen_angle(point: (f32, f32), center: (f32, f32)) -> f32 {
    (point.1 - center.1).atan2(point.0 - center.0)
}

/// 把角度差规范到 `(-π, π]`。
pub fn wrap_angle(angle: f32) -> f32 {
    let two_pi = std::f32::consts::TAU;
    let mut wrapped = angle % two_pi;
    if wrapped > std::f32::consts::PI {
        wrapped -= two_pi;
    } else if wrapped <= -std::f32::consts::PI {
        wrapped += two_pi;
    }
    wrapped
}

/// 四元数乘法（`a * b`，与 glam 的 `Quat::mul_quat` 同序）。
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let (ax, ay, az, aw) = (a[0], a[1], a[2], a[3]);
    let (bx, by, bz, bw) = (b[0], b[1], b[2], b[3]);
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// 绕世界轴旋转 `angle` 弧度的四元数。
pub fn quat_axis_angle(axis: usize, angle: f32) -> [f32; 4] {
    let half = angle * 0.5;
    let (sin, cos) = half.sin_cos();
    match axis {
        0 => [sin, 0.0, 0.0, cos],
        1 => [0.0, sin, 0.0, cos],
        _ => [0.0, 0.0, sin, cos],
    }
}

/// 用四元数旋转向量（测试 / 旋转后的轴端点用）。
pub fn quat_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let qv = [q[0], q[1], q[2]];
    let uv = cross(qv, v);
    let uuv = cross(qv, uv);
    add(v, add(scale(uv, 2.0 * q[3]), scale(uuv, 2.0)))
}

pub fn quat_normalize(q: [f32; 4]) -> [f32; 4] {
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3])
        .sqrt()
        .max(1e-6);
    [q[0] / len, q[1] / len, q[2] / len, q[3] / len]
}

/// 单位四元数的共轭（= 逆）。
pub fn quat_conjugate(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// 四元数 → XYZ 欧拉角（度）。
///
/// 约定 `R = Rz * Ry * Rx`（glam 的 `EulerRot::XYZ`），检查器用它把
/// `rotation` 显示成可读的角度；`euler_degrees_to_quat` 是它的逆。
pub fn quat_to_euler_degrees(q: [f32; 4]) -> [f32; 3] {
    let length = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if length < 1e-8 {
        return [0.0; 3];
    }
    let [x, y, z, w] = [q[0] / length, q[1] / length, q[2] / length, q[3] / length];
    // 旋转矩阵 R = Rz Ry Rx 的元素（行主序）。
    let m00 = 1.0 - 2.0 * (y * y + z * z);
    let m10 = 2.0 * (x * y + z * w);
    let m20 = 2.0 * (x * z - y * w);
    let m21 = 2.0 * (y * z + x * w);
    let m22 = 1.0 - 2.0 * (x * x + y * y);
    let m11 = 1.0 - 2.0 * (x * x + z * z);
    let m12 = 2.0 * (y * z - x * w);

    let sin_y = (-m20).clamp(-1.0, 1.0);
    let ey = sin_y.asin();
    // 万向锁：Y 接近 ±90° 时 X/Z 耦合，把滚转挪到 Z 上（Unity 同样取 0）。
    let (ex, ez) = if sin_y.abs() < 0.999_99 {
        (m21.atan2(m22), m10.atan2(m00))
    } else {
        ((-m12).atan2(m11), 0.0)
    };
    [ex.to_degrees(), ey.to_degrees(), ez.to_degrees()]
}

/// XYZ 欧拉角（度）→ 四元数，[`quat_to_euler_degrees`] 的逆。
pub fn euler_degrees_to_quat(euler: [f32; 3]) -> [f32; 4] {
    let half = [
        euler[0].to_radians() * 0.5,
        euler[1].to_radians() * 0.5,
        euler[2].to_radians() * 0.5,
    ];
    let (sx, cx) = half[0].sin_cos();
    let (sy, cy) = half[1].sin_cos();
    let (sz, cz) = half[2].sin_cos();
    // q = qz * qy * qx
    [
        cz * cy * sx - sz * sy * cx,
        cz * sy * cx + sz * cy * sx,
        sz * cy * cx - cz * sy * sx,
        cz * cy * cx + sz * sy * sx,
    ]
}

/// 局部三轴在世界空间的方向（列向量）。
pub fn basis_from_quat(q: [f32; 4]) -> [[f32; 3]; 3] {
    [
        quat_rotate(q, [1.0, 0.0, 0.0]),
        quat_rotate(q, [0.0, 1.0, 0.0]),
        quat_rotate(q, [0.0, 0.0, 1.0]),
    ]
}

/// 实体沿父链累积的世界旋转（含自身；父级优先，遇环时截断）。
pub fn world_rotation(doc: &SceneDoc, index: usize) -> [f32; 4] {
    let mut chain = Vec::new();
    let mut current = Some(index);
    while let Some(current_index) = current {
        if chain.len() >= 64 || chain.contains(&current_index) {
            break;
        }
        chain.push(current_index);
        current = doc.entities[current_index]
            .parent
            .and_then(|parent_id| doc.index_of_id(parent_id));
    }
    let mut rotation = IDENTITY_ROTATION;
    for entity_index in chain.iter().rev() {
        rotation = quat_mul(rotation, doc.entities[*entity_index].transform.rotation);
    }
    quat_normalize(rotation)
}

/// 父级的世界旋转（不含自身；根实体的父级为单位旋转）。
pub fn parent_world_rotation(doc: &SceneDoc, index: usize) -> [f32; 4] {
    doc.entities[index]
        .parent
        .and_then(|parent_id| doc.index_of_id(parent_id))
        .map(|parent_index| world_rotation(doc, parent_index))
        .unwrap_or(IDENTITY_ROTATION)
}

/// 实体世界位置（父链累积：父旋转 + 父缩放作用在子局部位置上）。
pub fn world_position(doc: &SceneDoc, index: usize) -> [f32; 3] {
    fn walk(doc: &SceneDoc, index: usize, depth: usize) -> [f32; 3] {
        let Some(entity) = doc.entities.get(index) else {
            return [0.0; 3];
        };
        let local = entity.transform.position;
        if depth > 64 {
            return local;
        }
        let Some(parent_index) = entity.parent.and_then(|parent| doc.index_of_id(parent)) else {
            return local;
        };
        let parent_world = walk(doc, parent_index, depth + 1);
        let parent_rotation = world_rotation(doc, parent_index);
        let rotated = quat_rotate(parent_rotation, local);
        let parent_scale = doc.entities[parent_index].transform.scale;
        add(
            parent_world,
            [
                rotated[0] * parent_scale[0],
                rotated[1] * parent_scale[1],
                rotated[2] * parent_scale[2],
            ],
        )
    }
    walk(doc, index, 0)
}

/// 实体世界缩放（父链分量相乘）。
pub fn world_scale(doc: &SceneDoc, index: usize) -> [f32; 3] {
    let mut scale = doc
        .entities
        .get(index)
        .map(|entity| entity.transform.scale)
        .unwrap_or([1.0; 3]);
    let mut current = doc.entities.get(index).and_then(|entity| entity.parent);
    let mut depth = 0;
    while let Some(id) = current {
        if depth > 64 {
            break;
        }
        let Some(parent_index) = doc.index_of_id(id) else {
            break;
        };
        let parent_scale = doc.entities[parent_index].transform.scale;
        for axis in 0..3 {
            scale[axis] *= parent_scale[axis];
        }
        current = doc.entities[parent_index].parent;
        depth += 1;
    }
    scale
}

/// 一组实体在世界空间的**包围球** `(center, radius)`（`#505`）。
///
/// 编辑器文档里没有网格数据，所以每个实体用「单位立方体 × 世界缩放」当代理
/// （与选择描边 / gizmo 的代理一致）：8 个角点按世界旋转换算到世界空间，
/// 取并集 AABB 的中心与半对角线作为半径。父链的平移 / 旋转 / 缩放都通过
/// [`world_position`] / [`world_rotation`] / [`world_scale`] 累积，所以子物体框选
/// 得到的是它在**世界**里的位置（而不是局部 `transform.position`）。
///
/// 越界索引、非有限变换、空集合都只跳过；全空时返回 `None`。半径下限 `0.25`，
/// 免得零缩放 / 单点实体的取景距离塌到相机最小距离。
pub fn world_bounds(doc: &SceneDoc, indices: &[usize]) -> Option<([f32; 3], f32)> {
    let mut points: Vec<[f32; 3]> = Vec::new();
    for index in indices {
        if *index >= doc.entities.len() {
            continue;
        }
        let center = world_position(doc, *index);
        let scale = world_scale(doc, *index);
        let rotation = world_rotation(doc, *index);
        if !center.iter().all(|value| value.is_finite())
            || !scale.iter().all(|value| value.is_finite())
            || !rotation.iter().all(|value| value.is_finite())
        {
            continue;
        }
        let half = [
            scale[0].abs() * 0.5,
            scale[1].abs() * 0.5,
            scale[2].abs() * 0.5,
        ];
        for sx in [-1.0f32, 1.0] {
            for sy in [-1.0f32, 1.0] {
                for sz in [-1.0f32, 1.0] {
                    let corner = quat_rotate(rotation, [half[0] * sx, half[1] * sy, half[2] * sz]);
                    points.push(add(center, corner));
                }
            }
        }
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for point in &points {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    if points.is_empty() {
        return None;
    }
    let center = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    // 半径取「中心到最远角点」而不是 AABB 半对角线：后者会随旋转变大，
    // 同一个物体转一下就换了取景距离。
    let radius = points
        .iter()
        .map(|point| {
            ((point[0] - center[0]).powi(2)
                + (point[1] - center[1]).powi(2)
                + (point[2] - center[2]).powi(2))
            .sqrt()
        })
        .fold(0.0f32, f32::max);
    let radius = if radius.is_finite() {
        radius.max(0.25)
    } else {
        0.25
    };
    Some((center, radius))
}

/// 世界轴在当前相机里的屏幕方向（x 右、y 下）。
pub fn orientation_axis_direction(frame: &CameraFrame, axis: usize, sign: f32) -> [f32; 2] {
    let world = match axis {
        0 => [sign, 0.0, 0.0],
        1 => [0.0, sign, 0.0],
        _ => [0.0, 0.0, sign],
    };
    [dot(world, frame.right), -dot(world, frame.up)]
}

/// 方向 gizmo 某根轴的端点（屏幕坐标）。
pub fn orientation_axis_end(
    frame: &CameraFrame,
    axis: usize,
    sign: f32,
    center: (f32, f32),
    radius: f32,
) -> (f32, f32) {
    let direction = orientation_axis_direction(frame, axis, sign);
    (
        center.0 + direction[0] * radius,
        center.1 + direction[1] * radius,
    )
}

/// 点中方向 gizmo 的哪根轴（返回 `(axis, sign)`），容差 9px。
pub fn pick_orientation_axis(
    position: (f32, f32),
    center: (f32, f32),
    radius: f32,
    frame: &CameraFrame,
) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32, f32)> = None;
    for axis in 0..3 {
        for sign in [1.0_f32, -1.0] {
            let direction = orientation_axis_direction(frame, axis, sign);
            // 正对相机的轴投影长度为 0，无法点击。
            if (direction[0] * direction[0] + direction[1] * direction[1]).sqrt() < 0.2 {
                continue;
            }
            let end = orientation_axis_end(frame, axis, sign, center, radius);
            let dx = end.0 - position.0;
            let dy = end.1 - position.1;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance <= 9.0 && best.is_none_or(|(_, _, best)| distance < best) {
                best = Some((axis, sign, distance));
            }
        }
    }
    best.map(|(axis, sign, _)| (axis, sign))
}

#[derive(Debug, Clone, Copy)]
pub struct CameraFrame {
    pub eye: [f32; 3],
    pub right: [f32; 3],
    pub up: [f32; 3],
    pub forward: [f32; 3],
    /// 正交半高（世界单位）；`None` = 透视（沿用历史硬编码的 30° 半角）。
    ///
    /// [#641] Scene view 的 2D 模式是正交相机，投影 / 拾取射线都要跟着换公式，
    /// 否则屏幕上的位置和引擎里画出来的对不上。
    pub ortho_half_height: Option<f32>,
}

pub fn camera_frame(camera: &OrbitCamera) -> CameraFrame {
    let eye = camera.eye();
    let target = camera.target;
    let forward = normalize([target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]]);
    let right = normalize(cross(forward, [0.0, 1.0, 0.0]));
    let up = cross(right, forward);
    CameraFrame {
        eye,
        right,
        up,
        forward,
        ortho_half_height: camera.ortho_half_height(),
    }
}

pub fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / len, v[1] / len, v[2] / len]
}

pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn scale(a: [f32; 3], factor: f32) -> [f32; 3] {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}

/// 世界坐标 → 屏幕坐标（像素，相对视口左上角），返回 `(x, y, depth)`。
pub fn project(
    point: [f32; 3],
    frame: &CameraFrame,
    width: f32,
    height: f32,
) -> Option<(f32, f32, f32)> {
    let delta = sub(point, frame.eye);
    let depth = dot(delta, frame.forward);
    if depth <= 0.05 {
        return None;
    }
    let aspect = (width / height.max(1.0)).max(0.2);
    let (ndc_x, ndc_y) = match frame.ortho_half_height {
        // 正交：屏幕位置与深度无关，半高就是世界单位。
        Some(half_h) => {
            let half_h = half_h.max(1e-4);
            (
                dot(delta, frame.right) / (half_h * aspect),
                dot(delta, frame.up) / half_h,
            )
        }
        None => {
            let tan_half = 30.0f32.to_radians().tan();
            (
                dot(delta, frame.right) / depth / tan_half / aspect,
                dot(delta, frame.up) / depth / tan_half,
            )
        }
    };
    Some((
        (ndc_x * 0.5 + 0.5) * width,
        (1.0 - (ndc_y * 0.5 + 0.5)) * height,
        depth,
    ))
}

/// 屏幕像素 → 世界射线 `(origin, direction)`。
pub fn ray_from_screen(
    x: f32,
    y: f32,
    frame: &CameraFrame,
    width: f32,
    height: f32,
) -> ([f32; 3], [f32; 3]) {
    let aspect = (width / height.max(1.0)).max(0.2);
    let (ndc_x, ndc_y) = (
        2.0 * x / width.max(1.0) - 1.0,
        1.0 - 2.0 * y / height.max(1.0),
    );
    if let Some(half_h) = frame.ortho_half_height {
        // 正交：所有射线方向相同，起点在近平面上按半宽 / 半高铺开。
        let half_h = half_h.max(1e-4);
        let offset = add(
            scale(frame.right, ndc_x * half_h * aspect),
            scale(frame.up, ndc_y * half_h),
        );
        return (add(frame.eye, offset), frame.forward);
    }
    let tan_half = 30.0f32.to_radians().tan();
    let ndc_x = ndc_x * aspect * tan_half;
    let ndc_y = ndc_y * tan_half;
    let direction = normalize(add(
        frame.forward,
        add(scale(frame.right, ndc_x), scale(frame.up, ndc_y)),
    ));
    (frame.eye, direction)
}

/// 射线与轴对齐盒求交，返回最近命中距离。
pub fn ray_box(
    origin: [f32; 3],
    direction: [f32; 3],
    center: [f32; 3],
    half: [f32; 3],
) -> Option<f32> {
    let mut t_min = f32::NEG_INFINITY;
    let mut t_max = f32::INFINITY;
    for axis in 0..3 {
        let origin_axis = origin[axis];
        let dir = direction[axis];
        let min = center[axis] - half[axis];
        let max = center[axis] + half[axis];
        if dir.abs() < 1e-6 {
            if origin_axis < min || origin_axis > max {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dir;
        let mut t1 = (min - origin_axis) * inv;
        let mut t2 = (max - origin_axis) * inv;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
        }
        t_min = t_min.max(t1);
        t_max = t_max.min(t2);
        if t_min > t_max {
            return None;
        }
    }
    if t_max < 0.0 {
        None
    } else {
        Some(t_min.max(0.0))
    }
}

/// 屏幕空间点到线段的距离。
pub fn distance_point_segment(point: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let ap = (point.0 - a.0, point.1 - a.1);
    let length_sq = ab.0 * ab.0 + ab.1 * ab.1;
    let t = if length_sq <= 1e-6 {
        0.0
    } else {
        ((ap.0 * ab.0 + ap.1 * ab.1) / length_sq).clamp(0.0, 1.0)
    };
    let projection = (a.0 + ab.0 * t, a.1 + ab.1 * t);
    ((point.0 - projection.0).powi(2) + (point.1 - projection.1).powi(2)).sqrt()
}

/// gizmo 轴的世界长度：保持屏幕上的长度近似恒定。
pub fn gizmo_world_length(camera: &OrbitCamera, viewport_height: f32) -> f32 {
    let pixels = GIZMO_SCREEN_PIXELS / viewport_height.max(1.0);
    if camera.orthographic {
        // 正交：GIZMO_SCREEN_PIXELS 像素对应 distance（半高）×2 世界单位。
        return camera.distance.max(0.05) * pixels * 2.0;
    }
    let tan_half = 30.0f32.to_radians().tan();
    camera.distance * pixels * 2.0 * tan_half
}

/// 一条 gizmo 轴的屏幕端点 `(start, end, 每世界单位像素)`。
pub type ScreenSegment = ((f32, f32), (f32, f32), f32);

/// 一条 gizmo 轴的屏幕端点与「每世界单位多少像素」。
pub fn axis_screen_segment(
    frame: &CameraFrame,
    origin: [f32; 3],
    axis: usize,
    world_length: f32,
    width: f32,
    height: f32,
) -> Option<ScreenSegment> {
    let mut direction = [0.0; 3];
    direction[axis] = 1.0;
    axis_screen_segment_oriented(frame, origin, direction, world_length, width, height)
}

/// 指定方向的轴线段（局部 gizmo 用）。
pub fn axis_screen_segment_oriented(
    frame: &CameraFrame,
    origin: [f32; 3],
    direction: [f32; 3],
    world_length: f32,
    width: f32,
    height: f32,
) -> Option<ScreenSegment> {
    let end = add(origin, scale(direction, world_length));
    let start = project(origin, frame, width, height)?;
    let end = project(end, frame, width, height)?;
    let px = ((end.0 - start.0).powi(2) + (end.1 - start.1).powi(2)).sqrt();
    Some((
        (start.0, start.1),
        (end.0, end.1),
        px / world_length.max(1e-4),
    ))
}

/// 命中测试：返回 (轴, 屏幕方向单位向量, 每世界单位像素)。
pub fn pick_gizmo_axis(
    point: (f32, f32),
    frame: &CameraFrame,
    origin: [f32; 3],
    world_length: f32,
    width: f32,
    height: f32,
) -> Option<(usize, [f32; 2], f32)> {
    pick_gizmo_axis_oriented(
        point,
        frame,
        origin,
        IDENTITY_BASIS,
        world_length,
        width,
        height,
    )
}

/// 局部坐标下的轴命中。
pub fn pick_gizmo_axis_oriented(
    point: (f32, f32),
    frame: &CameraFrame,
    origin: [f32; 3],
    basis: [[f32; 3]; 3],
    world_length: f32,
    width: f32,
    height: f32,
) -> Option<(usize, [f32; 2], f32)> {
    let mut best: Option<(usize, [f32; 2], f32, f32)> = None;
    for (axis, direction) in basis.iter().enumerate() {
        let Some((start, end, px_per_unit)) =
            axis_screen_segment_oriented(frame, origin, *direction, world_length, width, height)
        else {
            continue;
        };
        let distance = distance_point_segment(point, start, end);
        if distance > GIZMO_PICK_RADIUS {
            continue;
        }
        let dir = normalize2((end.0 - start.0, end.1 - start.1));
        if best
            .map(|(_, _, _, best_distance)| distance < best_distance)
            .unwrap_or(true)
        {
            best = Some((axis, dir, px_per_unit, distance));
        }
    }
    best.map(|(axis, dir, px, _)| (axis, dir, px))
}

pub fn normalize2(v: (f32, f32)) -> [f32; 2] {
    let len = (v.0 * v.0 + v.1 * v.1).sqrt().max(1e-6);
    [v.0 / len, v.1 / len]
}

/// 实体局部单位立方体的 8 个角（世界坐标，含旋转与缩放）。
///
/// 代理里的 `transform` 已经是世界变换（`world_position` / `world_rotation` /
/// `world_scale`），所以这里不再沿父链推导。
fn transformed_corners(transform: &TransformJson) -> [[f32; 3]; 8] {
    let rotation = quat_normalize(transform.rotation);
    let half = [
        transform.scale[0].abs().max(0.05) * 0.5,
        transform.scale[1].abs().max(0.05) * 0.5,
        transform.scale[2].abs().max(0.05) * 0.5,
    ];
    let mut corners = [[0.0f32; 3]; 8];
    let mut next = 0;
    for dx in [-1.0f32, 1.0] {
        for dy in [-1.0f32, 1.0] {
            for dz in [-1.0f32, 1.0] {
                let local = [half[0] * dx, half[1] * dy, half[2] * dz];
                let rotated = quat_rotate(rotation, local);
                corners[next] = [
                    transform.position[0] + rotated[0],
                    transform.position[1] + rotated[1],
                    transform.position[2] + rotated[2],
                ];
                next += 1;
            }
        }
    }
    corners
}

/// 世界代理立方体的 12 条边投影到屏幕空间（#487 的选中描边）。
///
/// 返回 `[((x0, y0), (x1, y1)); 12]`，坐标是**视口像素**（左上原点，与
/// [`box_select`] / `pick_entity` 同一坐标系）。任何一个角落在相机背后时
/// 返回 `None`：那种情况下逐点裁剪只会画出穿透屏幕的乱线。
pub fn projected_box_edges(
    transform: &TransformJson,
    frame: &CameraFrame,
    width: f32,
    height: f32,
) -> Option<[((f32, f32), (f32, f32)); 12]> {
    let corners = transformed_corners(transform);
    let mut screen = [(0.0f32, 0.0f32); 8];
    for (slot, corner) in corners.iter().enumerate() {
        let (x, y, _) = project(*corner, frame, width, height)?;
        screen[slot] = (x, y);
    }
    // 角点下标按「dx / dy / dz 各占一位」排列：z 变化最快。
    const EDGES: [(usize, usize); 12] = [
        (0, 1),
        (2, 3),
        (4, 5),
        (6, 7), // 沿 z
        (0, 2),
        (1, 3),
        (4, 6),
        (5, 7), // 沿 y
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7), // 沿 x
    ];
    let mut out = [((0.0f32, 0.0f32), (0.0f32, 0.0f32)); 12];
    for (slot, (a, b)) in EDGES.iter().enumerate() {
        out[slot] = (screen[*a], screen[*b]);
    }
    Some(out)
}

/// 拖放落点：射线与「作者平面」的交点（#646）。
///
/// 3D 视图把资产扔到地面 `y = 0`；2D 视图（#641）是正视 XY，地面在那里是
/// 侧着的一条线，所以改打 `z = 0`（UI / sprite 所在平面）。射线与平面平行、
/// 交点在相机背后（`t <= 0`）或非有限时返回 `None`。
pub fn drop_point(camera: &OrbitCamera, ray: ([f32; 3], [f32; 3])) -> Option<[f32; 3]> {
    let (origin, direction) = ray;
    let axis = if camera.orthographic { 2 } else { 1 };
    let mut point = [0.0f32; 3];
    point[axis] = 0.0;
    if direction[axis].abs() <= 1e-4 {
        return None;
    }
    let t = -origin[axis] / direction[axis];
    if !t.is_finite() || t <= 0.0 {
        return None;
    }
    for slot in 0..3 {
        point[slot] = if slot == axis {
            0.0
        } else {
            origin[slot] + direction[slot] * t
        };
    }
    if point.iter().all(|value| value.is_finite()) {
        Some(point)
    } else {
        None
    }
}

/// 2D 矩形（sprite / rect）的 4 条边投影到屏幕空间（#643）。
///
/// `size` 是世界单位下的宽高，`pivot` 是 `0..1` 的轴心（左下角为原点，
/// 与 `SpriteRenderer::pivot` 同约定）。矩形在实体的局部 XY 平面上，
/// 跟着 `transform` 的旋转 / 缩放走。返回的是首尾相接的 4 条边，
/// 坐标同样是**视口像素**（左上原点）。
pub fn projected_rect_edges(
    transform: &TransformJson,
    size: [f32; 2],
    pivot: [f32; 2],
    frame: &CameraFrame,
    width: f32,
    height: f32,
) -> Option<[((f32, f32), (f32, f32)); 4]> {
    if !(size[0].is_finite() && size[1].is_finite()) || size[0] <= 0.0 || size[1] <= 0.0 {
        return None;
    }
    let (x0, x1) = (-pivot[0] * size[0], (1.0 - pivot[0]) * size[0]);
    let (y0, y1) = (-pivot[1] * size[1], (1.0 - pivot[1]) * size[1]);
    let local = [
        [x0 * transform.scale[0], y0 * transform.scale[1], 0.0],
        [x1 * transform.scale[0], y0 * transform.scale[1], 0.0],
        [x1 * transform.scale[0], y1 * transform.scale[1], 0.0],
        [x0 * transform.scale[0], y1 * transform.scale[1], 0.0],
    ];
    let mut screen = [(0.0f32, 0.0f32); 4];
    for (slot, corner) in local.iter().enumerate() {
        let rotated = quat_rotate(transform.rotation, *corner);
        let world = add(transform.position, rotated);
        let (x, y, _) = project(world, frame, width, height)?;
        screen[slot] = (x, y);
    }
    Some([
        (screen[0], screen[1]),
        (screen[1], screen[2]),
        (screen[2], screen[3]),
        (screen[3], screen[0]),
    ])
}

/// 把屏幕上的拖拽框换算成视口像素坐标 `(min_x, min_y, max_x, max_y)`。
///
/// [`box_select`] 和 `pick_entity` 都以**视口左上角**为原点，而 egui 给的是屏幕坐标：
/// 视口左边有层级面板时原点不为零。忘了减原点，框选就会恒从视口左上角开始，
/// 把光标上方的所有物体一起选中（#422）。反方向拖拽也会在这里归一化成 min/max。
pub fn local_box(start: (f32, f32), end: (f32, f32), origin: (f32, f32)) -> (f32, f32, f32, f32) {
    let start = (start.0 - origin.0, start.1 - origin.1);
    let end = (end.0 - origin.0, end.1 - origin.1);
    (
        start.0.min(end.0),
        start.1.min(end.1),
        start.0.max(end.0),
        start.1.max(end.1),
    )
}

/// 框选：实体旋转后的包围盒投影成屏幕 AABB，与矩形**相交**即选中。
///
/// 矩形是 `(min_x, min_y, max_x, max_y)`（视口像素坐标）；索引按文档顺序返回。
/// 用相交而不是「角点落在矩形内」，所以填满视口的大物体也能被小框选中。
pub fn box_select(
    rect: (f32, f32, f32, f32),
    frame: &CameraFrame,
    width: f32,
    height: f32,
    entities: &[(usize, TransformJson)],
) -> Vec<usize> {
    let (min_x, min_y, max_x, max_y) = rect;
    let mut selected = Vec::new();
    for (index, transform) in entities {
        let mut bounds: Option<(f32, f32, f32, f32)> = None;
        for corner in transformed_corners(transform) {
            let Some((x, y, _)) = project(corner, frame, width, height) else {
                continue;
            };
            bounds = Some(match bounds {
                None => (x, y, x, y),
                Some((min_px, min_py, max_px, max_py)) => {
                    (min_px.min(x), min_py.min(y), max_px.max(x), max_py.max(y))
                }
            });
        }
        // 全部角点都在相机背后 → 无法投影，跳过。
        let Some((min_px, min_py, max_px, max_py)) = bounds else {
            continue;
        };
        if max_px >= min_x && min_px <= max_x && max_py >= min_y && min_py <= max_y {
            selected.push(*index);
        }
    }
    selected
}

/// 射线与实体的**有向**包围盒求交，返回世界空间射线参数。
///
/// 把射线变换到实体局部空间（先反旋转、再除以缩放）后与单位立方体
/// `[-0.5, 0.5]³` 求交；`ray_box` 返回的参数与世界射线同一尺度，所以
/// 不同缩放的实体之间仍可直接比较远近。
fn ray_obb(origin: [f32; 3], direction: [f32; 3], transform: &TransformJson) -> Option<f32> {
    let inverse = quat_conjugate(quat_normalize(transform.rotation));
    let scale = [
        transform.scale[0].abs().max(0.05),
        transform.scale[1].abs().max(0.05),
        transform.scale[2].abs().max(0.05),
    ];
    let offset = [
        origin[0] - transform.position[0],
        origin[1] - transform.position[1],
        origin[2] - transform.position[2],
    ];
    let local_offset = quat_rotate(inverse, offset);
    let local_direction = quat_rotate(inverse, direction);
    ray_box(
        [
            local_offset[0] / scale[0],
            local_offset[1] / scale[1],
            local_offset[2] / scale[2],
        ],
        [
            local_direction[0] / scale[0],
            local_direction[1] / scale[1],
            local_direction[2] / scale[2],
        ],
        [0.0, 0.0, 0.0],
        [0.5, 0.5, 0.5],
    )
}

/// 用实体的真实局部 AABB 做 OBB 命中；`None` 时退回单位立方体。
fn ray_obb_with_bounds(
    origin: [f32; 3],
    direction: [f32; 3],
    transform: &TransformJson,
    bounds: Option<PickBounds>,
) -> Option<f32> {
    let Some(bounds) = bounds else {
        return ray_obb(origin, direction, transform);
    };
    let inverse = quat_conjugate(quat_normalize(transform.rotation));
    let scale = [
        transform.scale[0].abs().max(0.05),
        transform.scale[1].abs().max(0.05),
        transform.scale[2].abs().max(0.05),
    ];
    let offset = [
        origin[0] - transform.position[0],
        origin[1] - transform.position[1],
        origin[2] - transform.position[2],
    ];
    let local_offset = quat_rotate(inverse, offset);
    let local_direction = quat_rotate(inverse, direction);
    ray_box(
        [
            local_offset[0] / scale[0],
            local_offset[1] / scale[1],
            local_offset[2] / scale[2],
        ],
        [
            local_direction[0] / scale[0],
            local_direction[1] / scale[1],
            local_direction[2] / scale[2],
        ],
        bounds.center(),
        bounds.half(),
    )
}

/// 拾取离相机最近的实体（索引, 命中距离）。
///
/// 兼容旧调用：没有 bounds 数据时用单位立方体。
pub fn pick_entity(
    point: (f32, f32),
    frame: &CameraFrame,
    width: f32,
    height: f32,
    entities: &[(usize, TransformJson)],
) -> Option<usize> {
    let (origin, direction) = ray_from_screen(point.0, point.1, frame, width, height);
    let mut best: Option<(usize, f32)> = None;
    for (index, transform) in entities {
        if let Some(distance) = ray_obb(origin, direction, transform) {
            if best.map(|(_, best)| distance < best).unwrap_or(true) {
                best = Some((*index, distance));
            }
        }
    }
    best.map(|(index, _)| index)
}

/// 带真实局部包围盒的拾取（#600）。
pub fn pick_entity_with_bounds(
    point: (f32, f32),
    frame: &CameraFrame,
    width: f32,
    height: f32,
    entities: &[(usize, TransformJson, Option<PickBounds>)],
) -> Option<usize> {
    let (origin, direction) = ray_from_screen(point.0, point.1, frame, width, height);
    let mut best: Option<(usize, f32)> = None;
    for (index, transform, bounds) in entities {
        if let Some(distance) = ray_obb_with_bounds(origin, direction, transform, *bounds) {
            if best.map(|(_, best)| distance < best).unwrap_or(true) {
                best = Some((*index, distance));
            }
        }
    }
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> OrbitCamera {
        OrbitCamera {
            target: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            distance: 10.0,
            orthographic: false,
        }
    }

    fn camera_2d() -> OrbitCamera {
        let mut camera = camera();
        camera.set_2d(true);
        camera
    }

    /// #641：2D 视图是正交投影，屏幕位置与深度无关。
    #[test]
    fn ortho_projection_ignores_depth() {
        let camera = camera_2d();
        let frame = camera_frame(&camera);
        let near = project([1.0, 0.5, 0.0], &frame, 800.0, 600.0).unwrap();
        let far = project([1.0, 0.5, -6.0], &frame, 800.0, 600.0).unwrap();
        assert!((near.0 - far.0).abs() < 1e-4);
        assert!((near.1 - far.1).abs() < 1e-4);
        // 半高 = distance = 10 → 600px 高的视口里每世界单位 30px：
        // +1 X 右移 30px、+0.5 Y 上抬 15px。
        assert!((near.0 - 430.0).abs() < 1e-3);
        assert!((near.1 - 285.0).abs() < 1e-3);
    }

    /// 正交射线互相平行，起点按像素铺开；正中那条就从 eye 出发。
    #[test]
    fn ortho_rays_are_parallel() {
        let camera = camera_2d();
        let frame = camera_frame(&camera);
        let (o_center, d_center) = ray_from_screen(400.0, 300.0, &frame, 800.0, 600.0);
        let (o_side, d_side) = ray_from_screen(700.0, 100.0, &frame, 800.0, 600.0);
        for axis in 0..3 {
            assert!((d_center[axis] - d_side[axis]).abs() < 1e-5);
        }
        assert!((o_center[0] - frame.eye[0]).abs() < 1e-4);
        assert!((o_center[1] - frame.eye[1]).abs() < 1e-4);
        assert!(o_side[0] > o_center[0], "屏幕右移 → 起点沿 +X 平移");
        assert!(o_side[1] > o_center[1], "屏幕上移 → 起点沿 +Y 平移");
        // 中心射线命中盒子；横向平移 10 个世界单位的那条直接打空
        // （正交没有近大远小，不会因为「更靠近相机」而命中）。
        assert!(ray_box(o_center, d_center, [0.0, 0.0, 0.0], [0.5, 0.5, 0.5]).is_some());
        assert!(ray_box(o_side, d_side, [0.0, 0.0, 0.0], [0.5, 0.5, 0.5]).is_none());
    }

    fn rect_transform(position: [f32; 3], scale: [f32; 3]) -> TransformJson {
        TransformJson {
            position,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale,
        }
    }

    /// #643：sprite 矩形按 `size` + `pivot` 落在屏幕上的正确像素。
    #[test]
    fn sprite_rect_uses_size_and_pivot() {
        let camera = camera_2d();
        let frame = camera_frame(&camera);
        // 半高 10 → 600px 高的视口里每世界单位 30px。
        let centered = rect_transform([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let edges =
            projected_rect_edges(&centered, [2.0, 1.0], [0.5, 0.5], &frame, 800.0, 600.0).unwrap();
        let xs: Vec<f32> = edges.iter().map(|(a, _)| a.0).collect();
        let ys: Vec<f32> = edges.iter().map(|(a, _)| a.1).collect();
        assert!((xs.iter().cloned().fold(f32::MIN, f32::max) - 430.0).abs() < 1e-3);
        assert!((xs.iter().cloned().fold(f32::MAX, f32::min) - 370.0).abs() < 1e-3);
        assert!((ys.iter().cloned().fold(f32::MIN, f32::max) - 315.0).abs() < 1e-3);
        assert!((ys.iter().cloned().fold(f32::MAX, f32::min) - 285.0).abs() < 1e-3);

        // 底边轴心 `[0.5, 0.0]`：矩形整体在实体上方半个高度。
        let bottom =
            projected_rect_edges(&centered, [2.0, 1.0], [0.5, 0.0], &frame, 800.0, 600.0).unwrap();
        let min_y = bottom.iter().map(|(a, _)| a.1).fold(f32::MAX, f32::min);
        let max_y = bottom.iter().map(|(a, _)| a.1).fold(f32::MIN, f32::max);
        // 屏幕 y 向下：抬高半个世界单位 = 屏幕 y 变小 15px。
        assert!((max_y - 300.0).abs() < 1e-3);
        assert!((min_y - 270.0).abs() < 1e-3);
    }

    /// 退化尺寸（没填 `frame_size`）不画，避免出现零面积矩形。
    #[test]
    fn sprite_rect_rejects_degenerate_sizes() {
        let camera = camera_2d();
        let frame = camera_frame(&camera);
        let transform = rect_transform([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert!(
            projected_rect_edges(&transform, [0.0, 1.0], [0.5, 0.5], &frame, 800.0, 600.0)
                .is_none()
        );
        assert!(projected_rect_edges(
            &transform,
            [1.0, f32::NAN],
            [0.5, 0.5],
            &frame,
            800.0,
            600.0
        )
        .is_none());
    }

    /// #646：拖放落点 —— 3D 打地面 y = 0，2D 打 z = 0。
    #[test]
    fn drop_point_uses_the_authoring_plane_per_view() {
        // 2D：正视 XY，30px/世界单位，屏幕 (430, 285) = 世界 (+1, +0.5)。
        let mut camera = camera_2d();
        let frame = camera_frame(&camera);
        let ray = ray_from_screen(430.0, 285.0, &frame, 800.0, 600.0);
        let point = drop_point(&camera, ray).unwrap();
        assert!(point[2].abs() < 1e-4, "2D 落点必须在 z = 0 平面上");
        assert!((point[0] - 1.0).abs() < 1e-3);
        assert!((point[1] - 0.5).abs() < 1e-3);

        // 3D：俯视时中心射线落到原点（target）处的地面。
        camera.set_2d(false);
        camera.pitch = 0.6;
        let frame = camera_frame(&camera);
        let ray = ray_from_screen(400.0, 300.0, &frame, 800.0, 600.0);
        let point = drop_point(&camera, ray).unwrap();
        assert!(point[1].abs() < 1e-4, "3D 落点必须在地面上");
        assert!(point[0].abs() < 1e-3 && point[2].abs() < 1e-3);
    }

    #[test]
    fn drop_point_rejects_rays_parallel_to_the_plane() {
        let camera = camera();
        let frame = camera_frame(&camera);
        // 默认相机水平看向 −Z，与 y = 0 平行 → 没有落点。
        let ray = ray_from_screen(400.0, 300.0, &frame, 800.0, 600.0);
        assert!(drop_point(&camera, ray).is_none());
        // 平行射线 / 朝平面外飞的射线都拒绝；垂直向下的射线正常投影到地面。
        assert!(drop_point(&camera, ([0.0, 5.0, 0.0], [1.0, 0.0, 0.0])).is_none());
        assert!(drop_point(&camera, ([0.0, 5.0, 0.0], [0.0, 1.0, 0.0])).is_none());
        let point = drop_point(&camera, ([1.0, 5.0, 2.0], [0.0, -1.0, 0.0])).unwrap();
        assert_eq!(point, [1.0, 0.0, 2.0]);
    }

    #[test]
    fn ortho_gizmo_length_follows_the_ortho_size() {
        let camera = camera_2d();
        let length = gizmo_world_length(&camera, 600.0);
        let expected = camera.distance * GIZMO_SCREEN_PIXELS / 600.0 * 2.0;
        assert!((length - expected).abs() < 1e-4);
        assert!(length.is_finite());
    }

    #[test]
    fn projects_the_orbit_target_to_screen_center() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let (x, y, depth) = project([0.0, 0.0, 0.0], &frame, 800.0, 600.0).unwrap();
        assert!((x - 400.0).abs() < 1e-3);
        assert!((y - 300.0).abs() < 1e-3);
        assert!((depth - 10.0).abs() < 1e-3);
    }

    #[test]
    fn points_behind_the_camera_are_rejected() {
        let camera = camera();
        let frame = camera_frame(&camera);
        // eye 在 +Z 10；再往前是 +Z 20（相机背后）。
        assert!(project([0.0, 0.0, 20.0], &frame, 800.0, 600.0).is_none());
    }

    #[test]
    fn ray_through_center_hits_box_at_origin() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let (origin, direction) = ray_from_screen(400.0, 300.0, &frame, 800.0, 600.0);
        let distance = ray_box(origin, direction, [0.0, 0.0, 0.0], [0.5, 0.5, 0.5]).unwrap();
        assert!((distance - 9.5).abs() < 0.2, "distance = {distance}");

        // 偏到画面角落就不命中。
        let (origin, direction) = ray_from_screen(10.0, 10.0, &frame, 800.0, 600.0);
        assert!(ray_box(origin, direction, [0.0, 0.0, 0.0], [0.5, 0.5, 0.5]).is_none());
    }

    #[test]
    fn picks_nearest_entity_under_cursor() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let entities = vec![
            (
                0,
                TransformJson {
                    position: [0.0, 0.0, 0.0],
                    ..Default::default()
                },
            ),
            (
                1,
                TransformJson {
                    position: [3.0, 0.0, 0.0],
                    ..Default::default()
                },
            ),
        ];
        let picked = pick_entity((400.0, 300.0), &frame, 800.0, 600.0, &entities);
        assert_eq!(picked, Some(0));

        // 右移命中第二个。
        let (x, _, _) = project([3.0, 0.0, 0.0], &frame, 800.0, 600.0).unwrap();
        let picked = pick_entity((x, 300.0), &frame, 800.0, 600.0, &entities);
        assert_eq!(picked, Some(1));
    }

    #[test]
    fn picking_uses_the_local_bounds_when_provided() {
        let camera = camera_2d();
        let frame = camera_frame(&camera);
        let entities = vec![(
            0,
            TransformJson::default(),
            Some(PickBounds {
                min: [2.0, -1.0, -0.1],
                max: [8.0, 1.0, 0.1],
            }),
        )];
        let (x, y, _) = project([5.0, 0.0, 0.0], &frame, 800.0, 600.0).unwrap();
        assert_eq!(
            pick_entity_with_bounds((x, y), &frame, 800.0, 600.0, &entities),
            Some(0),
            "真实 AABB 内应命中"
        );
        assert_eq!(
            pick_entity_with_bounds((400.0, 300.0), &frame, 800.0, 600.0, &entities),
            None,
            "原点处的单位立方体不应再冒充整个精灵"
        );
    }

    #[test]
    fn picking_uses_the_rotated_box_not_its_axis_aligned_bounds() {
        // 薄板 2 × 0.1 × 2，绕 Z 转 45°：视觉上是斜的，轴对齐盒却是正放的。
        let plate = TransformJson {
            position: [0.0, 0.0, 0.0],
            rotation: quat_axis_angle(2, std::f32::consts::FRAC_PI_4),
            scale: [2.0, 0.1, 2.0],
        };
        let (origin, direction) = ([0.9f32, 0.0, 5.0], [0.0f32, 0.0, -1.0]);
        // 轴对齐盒里的空白点（旧实现会误选）现在必须落空。
        assert!(
            ray_obb(origin, direction, &plate).is_none(),
            "AABB 角落的空处不应命中旋转后的薄板"
        );
        // 薄板真实的一端（旧实现反而选不中）。
        assert!(
            ray_obb([0.636, 0.636, 5.0], direction, &plate).is_some(),
            "旋转后薄板本体应可点中"
        );
    }

    #[test]
    fn box_select_matches_a_wall_that_fills_the_viewport() {
        let camera = camera();
        let frame = camera_frame(&camera);
        // 一面 40 × 40 的墙，投影后覆盖整个视口，但四个角都在视口外。
        let wall = TransformJson {
            position: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [40.0, 40.0, 0.2],
        };
        let entities = vec![(7, wall)];
        let hits = box_select(
            (100.0, 100.0, 150.0, 150.0),
            &frame,
            800.0,
            600.0,
            &entities,
        );
        assert_eq!(hits, vec![7], "与屏幕矩形相交的大物体应被框选中");

        // 远离墙的框仍然什么都选不到。
        let misses = box_select(
            (4000.0, 4000.0, 4100.0, 4100.0),
            &frame,
            800.0,
            600.0,
            &entities,
        );
        assert!(misses.is_empty());
    }

    #[test]
    fn euler_round_trips_through_quaternions() {
        for euler in [
            [0.0f32, 0.0, 0.0],
            [0.0, 90.0, 0.0],
            [30.0, 45.0, -60.0],
            [-120.0, 10.0, 170.0],
        ] {
            let quat = euler_degrees_to_quat(euler);
            let back = quat_to_euler_degrees(quat);
            for axis in 0..3 {
                assert!(
                    (back[axis] - euler[axis]).abs() < 1e-4,
                    "euler {euler:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn euler_conversion_handles_the_gimbal_pole_and_degenerate_quat() {
        // Y = 90° 是万向锁：只能保证整体旋转一致，不保证分量一致。
        let quat = euler_degrees_to_quat([20.0, 90.0, 0.0]);
        let back = quat_to_euler_degrees(quat);
        assert!((back[1] - 90.0).abs() < 1e-3, "{back:?}");
        assert_eq!(back[2], 0.0);
        // 退化四元数不能出 NaN。
        assert_eq!(quat_to_euler_degrees([0.0; 4]), [0.0; 3]);
        // 未归一化的输入先归一化。
        let long = euler_degrees_to_quat([0.0, 90.0, 0.0]).map(|value| value * 3.0);
        let back = quat_to_euler_degrees(long);
        assert!((back[1] - 90.0).abs() < 1e-4, "{back:?}");
    }

    #[test]
    fn gizmo_world_length_follows_the_viewport_height() {
        let camera = camera();
        let short = gizmo_world_length(&camera, 600.0);
        let tall = gizmo_world_length(&camera, 1200.0);
        // 屏幕像素长度固定，视口越高，同样的像素对应的世界长度越小。
        assert!((short - tall * 2.0).abs() < 1e-4);
        assert!(
            gizmo_world_length(&camera, 0.0).is_finite(),
            "退化高度不能除零"
        );
    }

    #[test]
    fn gizmo_axis_pick_and_pixel_scale() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let length = gizmo_world_length(&camera, 600.0);
        let (start, end, px_per_unit) =
            axis_screen_segment(&frame, [0.0, 0.0, 0.0], 0, length, 800.0, 600.0).unwrap();
        assert!(px_per_unit > 1.0);
        let middle = ((start.0 + end.0) * 0.5, (start.1 + end.1) * 0.5);
        let hit = pick_gizmo_axis(middle, &frame, [0.0, 0.0, 0.0], length, 800.0, 600.0);
        let (axis, _, _) = hit.expect("X 轴命中");
        assert_eq!(axis, 0, "水平方向最接近 X 轴");

        // 远离所有轴 → 不命中。
        assert!(
            pick_gizmo_axis((10.0, 590.0), &frame, [0.0, 0.0, 0.0], length, 800.0, 600.0).is_none()
        );
    }

    #[test]
    fn quaternion_rotation_about_world_axes() {
        let q = quat_axis_angle(0, std::f32::consts::FRAC_PI_2);
        let rotated = quat_rotate(q, [0.0, 1.0, 0.0]);
        assert!((rotated[0]).abs() < 1e-4);
        assert!((rotated[1]).abs() < 1e-4);
        assert!((rotated[2] - 1.0).abs() < 1e-4, "+Y → +Z");

        // 组合：绕 X 90° 再绕 Y 90°。
        let qx = quat_axis_angle(0, std::f32::consts::FRAC_PI_2);
        let qy = quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        let combined = quat_normalize(quat_mul(qy, qx));
        let v = quat_rotate(combined, [0.0, 1.0, 0.0]);
        assert!((v[0] - 1.0).abs() < 1e-4, "+Y → +Z → +X: {v:?}");
    }

    #[test]
    fn ring_pick_and_angle_helpers() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let radius = gizmo_world_length(&camera, 600.0);
        let (cx, cy, screen_radius) =
            ring_screen_radius(&frame, [0.0, 0.0, 0.0], 1, radius, 800.0, 600.0).unwrap();
        assert!(screen_radius > 10.0);
        // 圆环右侧一点（Y 轴圆环在 XZ 平面，屏幕上是一个椭圆）。
        let hit = pick_rotation_axis(
            (cx + screen_radius, cy),
            &frame,
            [0.0, 0.0, 0.0],
            radius,
            800.0,
            600.0,
        );
        assert_eq!(hit, Some(1), "最外圈是 Y 轴圆环");

        assert!((wrap_angle(3.0 * std::f32::consts::PI) - std::f32::consts::PI).abs() < 1e-4);
        assert!((wrap_angle(-3.0 * std::f32::consts::PI) + std::f32::consts::PI).abs() < 1e-4);
        let a = screen_angle((10.0, 0.0), (0.0, 0.0));
        assert!(a.abs() < 1e-5);
    }

    #[test]
    fn quaternion_conjugate_inverts_rotation() {
        let q = quat_normalize(quat_axis_angle(1, 0.7));
        let v = [0.3, -0.5, 0.8];
        let round_trip = quat_rotate(quat_conjugate(q), quat_rotate(q, v));
        for axis in 0..3 {
            assert!((round_trip[axis] - v[axis]).abs() < 1e-4);
        }
    }

    #[test]
    fn basis_from_quat_rotates_axes() {
        let basis = basis_from_quat(quat_axis_angle(1, std::f32::consts::FRAC_PI_2));
        // 绕 Y 90°：X → −Z，Y 不变，Z → X。
        assert!((basis[0][2] + 1.0).abs() < 1e-4);
        assert!((basis[1][1] - 1.0).abs() < 1e-4);
        assert!((basis[2][0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn world_rotation_composes_parent_chain() {
        let mut doc = SceneDoc::default();
        let root = doc.add_named_child(None, "Root");
        doc.entities[root].transform.rotation = quat_axis_angle(1, std::f32::consts::FRAC_PI_2);
        let child = doc.add_named_child(Some(root), "Child");
        doc.entities[child].transform.rotation = quat_axis_angle(1, std::f32::consts::FRAC_PI_2);

        let x = quat_rotate(world_rotation(&doc, child), [1.0, 0.0, 0.0]);
        assert!((x[0] + 1.0).abs() < 1e-4, "两级 90° = 180°，X → −X");

        let parent_x = quat_rotate(parent_world_rotation(&doc, child), [1.0, 0.0, 0.0]);
        assert!((parent_x[2] + 1.0).abs() < 1e-4, "父级只含 Root 自身旋转");
    }

    #[test]
    fn oriented_gizmo_pick_uses_local_axis() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let basis = basis_from_quat(quat_axis_angle(2, 0.6));
        let length = gizmo_world_length(&camera, 600.0);
        let (start, end, _) =
            axis_screen_segment_oriented(&frame, [0.0, 0.0, 0.0], basis[0], length, 800.0, 600.0)
                .unwrap();
        let middle = ((start.0 + end.0) * 0.5, (start.1 + end.1) * 0.5);
        let hit =
            pick_gizmo_axis_oriented(middle, &frame, [0.0, 0.0, 0.0], basis, length, 800.0, 600.0);
        assert_eq!(hit.map(|(axis, _, _)| axis), Some(0));
    }

    #[test]
    fn box_select_uses_projected_bounds() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let entities = vec![
            (0, TransformJson::default()),
            (
                1,
                TransformJson {
                    position: [3.0, 0.0, 0.0],
                    ..Default::default()
                },
            ),
        ];
        let (center_x, center_y, _) = project([0.0, 0.0, 0.0], &frame, 800.0, 600.0).unwrap();
        let small = (
            center_x - 20.0,
            center_y - 20.0,
            center_x + 20.0,
            center_y + 20.0,
        );
        assert_eq!(box_select(small, &frame, 800.0, 600.0, &entities), vec![0]);
        assert_eq!(
            box_select((0.0, 0.0, 800.0, 600.0), &frame, 800.0, 600.0, &entities),
            vec![0, 1]
        );
        assert!(box_select((0.0, 0.0, 4.0, 4.0), &frame, 800.0, 600.0, &entities).is_empty());
    }

    #[test]
    fn box_select_starts_where_the_drag_started_not_at_the_viewport_origin() {
        // 视口左边有层级面板时原点不为零（#422）：屏幕坐标必须先减去它。
        let origin = (300.0, 120.0);
        assert_eq!(
            local_box((700.0, 500.0), (720.0, 520.0), origin),
            (400.0, 380.0, 420.0, 400.0)
        );
    }

    #[test]
    fn a_world_box_projects_twelve_edges() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let transform = TransformJson::default();
        let edges = projected_box_edges(&transform, &frame, 800.0, 600.0).expect("in front");
        assert_eq!(edges.len(), 12);
        // 单位立方体在原点、相机沿 +Z 后退 10：中心应该落在屏幕中心附近。
        let center_x: f32 = edges.iter().map(|(a, b)| (a.0 + b.0) * 0.5).sum::<f32>() / 12.0;
        let center_y: f32 = edges.iter().map(|(a, b)| (a.1 + b.1) * 0.5).sum::<f32>() / 12.0;
        assert!((center_x - 400.0).abs() < 1.0, "{center_x}");
        assert!((center_y - 300.0).abs() < 1.0, "{center_y}");

        // 相机背后的盒子不画（否则会画出穿透屏幕的线）。
        let behind = TransformJson {
            position: [0.0, 0.0, 50.0],
            ..TransformJson::default()
        };
        assert!(projected_box_edges(&behind, &frame, 800.0, 600.0).is_none());
    }

    #[test]
    fn local_box_normalizes_a_reverse_drag() {
        assert_eq!(
            local_box((720.0, 520.0), (700.0, 500.0), (300.0, 120.0)),
            (400.0, 380.0, 420.0, 400.0)
        );
    }

    #[test]
    fn world_position_and_scale_follow_parent_chain() {
        let mut doc = SceneDoc::default();
        let parent = doc.add_named_child(None, "Parent");
        doc.entities[parent].transform.position = [1.0, 2.0, 3.0];
        doc.entities[parent].transform.scale = [2.0, 2.0, 2.0];
        let child = doc.add_named_child(Some(parent), "Child");
        doc.entities[child].transform.position = [1.0, 0.0, 0.0];
        let world = world_position(&doc, child);
        assert!(
            (world[0] - 3.0).abs() < 1e-4,
            "父缩放作用在子局部位置上: {world:?}"
        );
        assert!((world[1] - 2.0).abs() < 1e-4);
        assert!((world[2] - 3.0).abs() < 1e-4);
        let scale = world_scale(&doc, child);
        assert_eq!(scale, [2.0, 2.0, 2.0]);
    }

    #[test]
    fn world_bounds_wraps_the_unit_cube_and_follows_the_parent_chain() {
        let mut doc = SceneDoc::default();
        let parent = doc.add_named_child(None, "Parent");
        doc.entities[parent].transform.position = [100.0, 0.0, 0.0];
        let child = doc.add_named_child(Some(parent), "Child");

        // 单位立方体：半对角线 = sqrt(3)/2 ≈ 0.866。
        let (center, radius) = world_bounds(&doc, &[child]).expect("bounds");
        assert!(
            (center[0] - 100.0).abs() < 1e-4,
            "子物体应在世界 x=100: {center:?}"
        );
        assert!((radius - 0.866_025).abs() < 1e-3, "radius {radius}");

        // 父缩放放大包围盒。
        doc.entities[parent].transform.scale = [10.0, 1.0, 1.0];
        let (_, scaled_radius) = world_bounds(&doc, &[child]).expect("bounds");
        assert!(
            scaled_radius > radius * 5.0,
            "缩放后半径应变大: {scaled_radius}"
        );

        // 旋转不改变立方体的包围球半径（只是换了朝向）。
        doc.entities[parent].transform.scale = [1.0; 3];
        doc.entities[parent].transform.rotation = euler_degrees_to_quat([30.0, 45.0, 10.0]);
        let (_, rotated_radius) = world_bounds(&doc, &[child]).expect("bounds");
        assert!((rotated_radius - radius).abs() < 1e-3, "旋转不该改变半径");
    }

    #[test]
    fn world_bounds_handles_empty_and_broken_input() {
        let doc = SceneDoc::default();
        assert!(world_bounds(&doc, &[]).is_none());
        assert!(world_bounds(&doc, &[7]).is_none(), "越界索引跳过");

        let mut doc = SceneDoc::default();
        let entity = doc.add_named_child(None, "Tiny");
        doc.entities[entity].transform.scale = [0.0; 3];
        let (_, radius) = world_bounds(&doc, &[entity]).expect("零缩放仍要有一个半径");
        assert_eq!(radius, 0.25, "半径下限让取景距离不会塌到 0");
    }

    #[test]
    fn orientation_axes_project_and_pick() {
        let camera = camera();
        let frame = camera_frame(&camera);
        let center = (100.0, 100.0);
        let x_end = orientation_axis_end(&frame, 0, 1.0, center, 40.0);
        assert!(x_end.0 > center.0);
        assert_eq!(
            pick_orientation_axis(x_end, center, 40.0, &frame),
            Some((0, 1.0))
        );
        assert_eq!(
            pick_orientation_axis((center.0, center.1), center, 40.0, &frame),
            None
        );
    }

    #[test]
    fn scaled_boxes_report_world_space_distance() {
        // 回归 #403：旧 Tauri 实现把 slab 参数又除以一次 |M⁻¹·dir|，
        // 导致返回距离跟实体缩放成正比（scale 4 时距离翻 4 倍）。
        let origin = [0.0f32, 0.0, 5.0];
        let direction = [0.0f32, 0.0, -1.0];
        for scale in [1.0f32, 2.0, 4.0] {
            let entity = TransformJson {
                position: [0.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [scale; 3],
            };
            let distance = ray_obb(origin, direction, &entity).expect("center ray must hit");
            let expected = 5.0 - 0.5 * scale;
            assert!(
                (distance - expected).abs() < 1e-4,
                "scale {scale}: distance {distance} != {expected}"
            );
        }
    }

    #[test]
    fn pick_prefers_a_near_scaled_box_over_a_far_unit_box() {
        // 同一个屏幕点上重叠两个盒子：近处的大盒子（scale 4，前表面 z = 2）
        // 必须压过远处的小盒子（scale 1，前表面 z = -9.5）。
        let camera = camera();
        let frame = camera_frame(&camera);
        let entities = vec![
            (
                0,
                TransformJson {
                    position: [0.0, 0.0, 0.0],
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    scale: [4.0, 4.0, 4.0],
                },
            ),
            (
                1,
                TransformJson {
                    position: [0.0, 0.0, -10.0],
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    scale: [1.0, 1.0, 1.0],
                },
            ),
        ];
        assert_eq!(
            pick_entity((400.0, 300.0), &frame, 800.0, 600.0, &entities),
            Some(0),
            "近处缩放实体应该赢，而不是被放大后的距离挤到后面"
        );
    }

    #[test]
    fn point_segment_distance_is_clamped() {
        assert!((distance_point_segment((0.0, 5.0), (0.0, 0.0), (10.0, 0.0)) - 5.0).abs() < 1e-4);
        assert!((distance_point_segment((-5.0, 0.0), (0.0, 0.0), (10.0, 0.0)) - 5.0).abs() < 1e-4);
        assert!(distance_point_segment((5.0, 0.0), (0.0, 0.0), (10.0, 0.0)) < 1e-4);
    }
}
