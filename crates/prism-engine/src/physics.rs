//! 物理查询（#522 Phase 1）。
//!
//! 这一层只做**纯几何**：把作者的 [`Collider`] 换成世界空间的解析形状，再做射线求交。
//! `prism-app` 的 physics feature 已经用 rapier 积分刚体；这里仍然不依赖
//! `rapier3d`，也不持有物理世界状态 —— 每次查询都直接读 ECS。rapier 的
//! Started/Stopped 事件由 `prism-app::physics_runner` 写进 [`PhysicsEvents`] 资源。
//!
//! 支持的形状：盒（OBB，按实体的旋转/缩放求交）、球、胶囊（沿本地 +Y）。
//! 胶囊按「盒 + 球」两段求交近似（圆柱段当盒，两端当球），足够做角色/子弹查询。

use prism_ecs::{Entity, World};
use prism_scene::components::{Collider, ColliderShape, LocalToWorld};

/// 一次碰撞 / 触发器事件的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicsEventKind {
    Started,
    Stopped,
}

/// rapier 在固定步内报告的碰撞事件，已映射回 ECS 实体。
///
/// `is_sensor == true` 表示至少一方是 `Collider.is_trigger`；普通刚体之间的
/// 接触也会产生 Started/Stopped，便于做落地 / 撞击逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicsEvent {
    pub kind: PhysicsEventKind,
    pub entity_a: Entity,
    pub entity_b: Entity,
    pub is_sensor: bool,
}

/// 主线程可见的物理事件缓冲。
///
/// 每个固定步覆盖一次：上一次 `step` 之后发生的 Started/Stopped 都放在这里。
/// 需要跨步累积的消费方应在每步后自行 drain 到自己的队列。
#[derive(Debug, Clone, Default)]
pub struct PhysicsEvents {
    pub events: Vec<PhysicsEvent>,
}

impl PhysicsEvents {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// 一次命中的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    pub entity: Entity,
    /// 从射线起点算起的距离（世界单位）。
    pub distance: f32,
    /// 命中点的世界坐标。
    pub point: [f32; 3],
    /// 命中面的世界空间法线（球/胶囊是径向法线）。
    pub normal: [f32; 3],
    /// 命中的碰撞体是不是触发器（触发器不阻挡移动，但查询能看到）。
    pub is_trigger: bool,
}

/// 射一条射线，返回最近的命中。
///
/// `layer_mask` 与碰撞体的 `layer` 相与非 0 才参与；`max_distance` 之外的不算命中。
/// 方向会被归一化，零向量直接返回 `None`。
pub fn raycast(
    world: &World,
    origin: [f32; 3],
    direction: [f32; 3],
    max_distance: f32,
    layer_mask: u32,
) -> Option<RayHit> {
    let dir = glam::Vec3::from_array(direction);
    let length = dir.length();
    if !length.is_finite() || length <= 1e-6 || !(max_distance > 0.0) {
        return None;
    }
    let dir = dir / length;
    let origin = glam::Vec3::from_array(origin);

    let mut best: Option<RayHit> = None;
    for (entity, collider) in world.query::<Collider>() {
        if collider.layer & layer_mask == 0 {
            continue;
        }
        let Some(transform) = world.get::<LocalToWorld>(entity) else {
            continue;
        };
        let shape = WorldShape::new(collider, &transform.0);
        let Some((distance, normal)) = shape.intersect(origin, dir, max_distance) else {
            continue;
        };
        if distance > max_distance {
            continue;
        }
        if best.is_some_and(|hit| hit.distance <= distance) {
            continue;
        }
        best = Some(RayHit {
            entity,
            distance,
            point: (origin + dir * distance).to_array(),
            normal: normal.to_array(),
            is_trigger: collider.is_trigger,
        });
    }
    best
}

/// 世界空间的碰撞形状（已把实体的旋转/缩放/中心偏移烘进去）。
#[derive(Debug, Clone, Copy)]
pub enum WorldShape {
    /// 定向盒：中心 + 三个半轴向量。
    Box {
        center: glam::Vec3,
        axes: [glam::Vec3; 3],
    },
    Sphere {
        center: glam::Vec3,
        radius: f32,
    },
}

impl WorldShape {
    /// 由作者的碰撞体 + 实体的世界矩阵构造。
    ///
    /// 胶囊退化成「以中心为心、覆盖两个端点的盒」，盒本身保留旋转（OBB）。
    /// 非有限/退化的尺寸返回球（半径 0）—— 求交时永远不会命中。
    pub fn new(collider: &Collider, world: &glam::Mat4) -> Self {
        let scale = world.to_scale_rotation_translation().0;
        let rotation = world.to_scale_rotation_translation().1;
        let center = world.transform_point3(glam::Vec3::from_array(collider.center));
        let axis = |local: glam::Vec3| rotation * (local * scale);
        let abs_scale = glam::Vec3::new(scale.x.abs(), scale.y.abs(), scale.z.abs());

        match collider.shape {
            ColliderShape::Box => Self::Box {
                center,
                axes: [
                    axis(glam::Vec3::X) * collider.half_extents[0],
                    axis(glam::Vec3::Y) * collider.half_extents[1],
                    axis(glam::Vec3::Z) * collider.half_extents[2],
                ],
            },
            ColliderShape::Sphere => Self::Sphere {
                center,
                radius: (collider.radius * abs_scale.max_element()).max(0.0),
            },
            ColliderShape::Capsule => {
                // 圆柱段 + 两端半球：用「盒 + 胶囊半径」的 OBB 覆盖，够查询用。
                let half = glam::Vec3::new(
                    collider.radius * abs_scale.x,
                    (collider.half_height + collider.radius) * abs_scale.y,
                    collider.radius * abs_scale.z,
                );
                Self::Box {
                    center,
                    axes: [
                        axis(glam::Vec3::X) * half.x,
                        axis(glam::Vec3::Y) * half.y,
                        axis(glam::Vec3::Z) * half.z,
                    ],
                }
            }
        }
    }

    /// 把一个半径为 `radius` 的点推出形状表面（#501 粒子碰撞用）。
    ///
    /// 返回 `(修正后的位置, 表面世界法线)`；点在形状外（且离表面超过半径）时
    /// 返回 `None`。盒内部用「穿透最浅的面」推出，球用径向法线。
    pub fn push_out(&self, point: glam::Vec3, radius: f32) -> Option<(glam::Vec3, glam::Vec3)> {
        if !point.is_finite() || !radius.is_finite() || radius < 0.0 {
            return None;
        }
        match *self {
            Self::Sphere {
                center,
                radius: shape,
            } => {
                let delta = point - center;
                let distance = delta.length();
                let minimum = (shape + radius).max(0.0);
                if distance >= minimum {
                    return None;
                }
                let normal = if distance > 1e-6 {
                    delta / distance
                } else {
                    glam::Vec3::Y
                };
                Some((center + normal * minimum, normal))
            }
            Self::Box { center, axes } => {
                let lengths = [axes[0].length(), axes[1].length(), axes[2].length()];
                if lengths
                    .iter()
                    .any(|length| !length.is_finite() || *length <= 1e-6)
                {
                    return None;
                }
                let basis = [
                    axes[0] / lengths[0],
                    axes[1] / lengths[1],
                    axes[2] / lengths[2],
                ];
                let local = point - center;
                let mut coordinates = [0.0f32; 3];
                let mut closest = center;
                for i in 0..3 {
                    let coordinate = local.dot(basis[i]);
                    coordinates[i] = coordinate;
                    let clamped = coordinate.clamp(-lengths[i], lengths[i]);
                    closest += basis[i] * clamped;
                }
                let delta = point - closest;
                let distance = delta.length();
                if distance > 1e-6 {
                    if distance > radius {
                        return None;
                    }
                    let normal = delta / distance;
                    return Some((closest + normal * radius, normal));
                }

                // 在盒内部：沿穿透最浅的轴推出去。
                let mut best_penetration = f32::INFINITY;
                let mut normal = glam::Vec3::Y;
                let mut face = center;
                for i in 0..3 {
                    let penetration = lengths[i] - coordinates[i].abs();
                    if penetration < best_penetration {
                        best_penetration = penetration;
                        let sign = if coordinates[i] >= 0.0 { 1.0 } else { -1.0 };
                        normal = basis[i] * sign;
                        face = center;
                        for j in 0..3 {
                            let coordinate = if i == j {
                                lengths[j] * sign
                            } else {
                                coordinates[j].clamp(-lengths[j], lengths[j])
                            };
                            face += basis[j] * coordinate;
                        }
                    }
                }
                Some((face + normal * radius, normal))
            }
        }
    }

    /// 射线求交：返回 `(距离, 世界法线)`。
    pub fn intersect(
        &self,
        origin: glam::Vec3,
        dir: glam::Vec3,
        max_distance: f32,
    ) -> Option<(f32, glam::Vec3)> {
        match *self {
            Self::Box { center, axes } => {
                // slab 法：把射线变换到盒的局部坐标系（轴向量可能带缩放，先归一化取长度）。
                let lengths = [axes[0].length(), axes[1].length(), axes[2].length()];
                if lengths.iter().any(|l| !l.is_finite() || *l <= 1e-6) {
                    return None;
                }
                let basis = [
                    axes[0] / lengths[0],
                    axes[1] / lengths[1],
                    axes[2] / lengths[2],
                ];
                let local_origin = origin - center;
                let origin_local = glam::Vec3::new(
                    local_origin.dot(basis[0]) / lengths[0],
                    local_origin.dot(basis[1]) / lengths[1],
                    local_origin.dot(basis[2]) / lengths[2],
                );
                let dir_local = glam::Vec3::new(
                    dir.dot(basis[0]) / lengths[0],
                    dir.dot(basis[1]) / lengths[1],
                    dir.dot(basis[2]) / lengths[2],
                );

                let mut t_min = 0.0f32;
                let mut t_max = max_distance;
                let mut axis_hit = 0usize;
                let mut sign = 1.0f32;
                for i in 0..3 {
                    let o = origin_local[i];
                    let d = dir_local[i];
                    if d.abs() < 1e-9 {
                        if o.abs() > 1.0 {
                            return None;
                        }
                        continue;
                    }
                    let inv = 1.0 / d;
                    let mut t1 = (-1.0 - o) * inv;
                    let mut t2 = (1.0 - o) * inv;
                    // 记录进入面：先进入的那个平面就是命中面。
                    let mut near_sign = -1.0;
                    if t1 > t2 {
                        std::mem::swap(&mut t1, &mut t2);
                        near_sign = 1.0;
                    }
                    if t1 > t_min {
                        t_min = t1;
                        axis_hit = i;
                        sign = near_sign;
                    }
                    t_max = t_max.min(t2);
                    if t_min > t_max {
                        return None;
                    }
                }
                if t_min > max_distance || t_max < 0.0 {
                    return None;
                }
                let normal = basis[axis_hit] * sign;
                Some((t_min, normal))
            }
            Self::Sphere { center, radius } => {
                let oc = origin - center;
                let b = oc.dot(dir);
                let c = oc.dot(oc) - radius * radius;
                let discriminant = b * b - c;
                if discriminant < 0.0 {
                    return None;
                }
                let sqrt = discriminant.sqrt();
                let mut t = -b - sqrt;
                if t < 0.0 {
                    // 起点在球内：从内部射出，用远交点。
                    t = -b + sqrt;
                }
                if t < 0.0 || t > max_distance {
                    return None;
                }
                let point = origin + dir * t;
                let normal = (point - center).normalize_or_zero();
                Some((t, normal))
            }
        }
    }
}

#[cfg(test)]
#[path = "physics_tests.rs"]
mod tests;
