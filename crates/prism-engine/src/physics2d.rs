//! 2D 物理体系与空间查询（#1160）。
//!
//! 提供轻量级、确定性、零外部依赖的 2D 物理后端与空间查询系统：
//! 1. 刚体动力学积分（重力、阻尼、冲量响应）
//! 2. 窄相碰撞检测（盒-盒 SAT、圆-圆、盒-圆、凸多边形、线段）
//! 3. 接触求解器（弹性系数、库仑摩擦力、Baumgarte 穿透修正）
//! 4. 传感器（Sensor/Trigger）与碰撞事件（Started / Stopped）
//! 5. 2D 空间查询（`raycast_2d`、`overlap_circle_2d`、`overlap_box_2d`、`shapecast_2d`）

use glam::{Quat, Vec2};
use prism_ecs::{Entity, World};
use prism_scene::components::{
    Collider2D, Collider2DShape, LocalToWorld, LocalTransform, RigidBody2D, RigidBody2DType,
};

/// 2D 碰撞 / 传感器事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Physics2DEventKind {
    Started,
    Stopped,
}

/// 两个实体之间的 2D 碰撞或触发事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Physics2DEvent {
    pub kind: Physics2DEventKind,
    pub entity_a: Entity,
    pub entity_b: Entity,
    pub is_sensor: bool,
}

/// 主线程可见的 2D 物理事件缓冲资源。
#[derive(Debug, Clone, Default)]
pub struct Physics2DEvents {
    pub events: Vec<Physics2DEvent>,
}

impl Physics2DEvents {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn drain(&mut self) -> Vec<Physics2DEvent> {
        std::mem::take(&mut self.events)
    }
}

/// 内部保留的活跃接触对资源，用于检测 Started / Stopped 转换。
#[derive(Debug, Clone, Default)]
struct ActiveContactPairs {
    pairs: std::collections::HashSet<(Entity, Entity)>,
}

/// 2D 射线检测命中结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit2D {
    pub entity: Entity,
    /// 射线起点到命中点的距离（世界单位）。
    pub distance: f32,
    /// 命中点的世界坐标 [x, y]。
    pub point: [f32; 2],
    /// 命中表面的外法线 [nx, ny]（单位向量）。
    pub normal: [f32; 2],
    /// 命中的碰撞体是否是传感器。
    pub is_sensor: bool,
}

/// 内部世界空间 2D 形状。
#[derive(Debug, Clone)]
enum WorldShape2D {
    Circle {
        center: Vec2,
        radius: f32,
    },
    Box {
        center: Vec2,
        half_extents: Vec2,
        axes: [Vec2; 2],
    },
    Capsule {
        a: Vec2,
        b: Vec2,
        radius: f32,
    },
    Polygon {
        vertices: Vec<Vec2>,
    },
    Segment {
        a: Vec2,
        b: Vec2,
    },
}

impl WorldShape2D {
    fn from_collider(collider: &Collider2D, pos: Vec2, rot_angle: f32) -> Self {
        let rot_cos = rot_angle.cos();
        let rot_sin = rot_angle.sin();
        let rot_mat = |v: Vec2| -> Vec2 {
            Vec2::new(v.x * rot_cos - v.y * rot_sin, v.x * rot_sin + v.y * rot_cos)
        };

        let local_offset = Vec2::from_array(collider.offset);
        let center = pos + rot_mat(local_offset);

        match collider.shape {
            Collider2DShape::Circle => WorldShape2D::Circle {
                center,
                radius: collider.radius.max(1e-4),
            },
            Collider2DShape::Box => {
                let half_extents = Vec2::from_array(collider.half_extents)
                    .abs()
                    .max(Vec2::splat(1e-4));
                let u = rot_mat(Vec2::X);
                let v = rot_mat(Vec2::Y);
                WorldShape2D::Box {
                    center,
                    half_extents,
                    axes: [u, v],
                }
            }
            Collider2DShape::Capsule => {
                let hh = collider.half_height.max(0.0);
                let up = rot_mat(Vec2::new(0.0, hh));
                WorldShape2D::Capsule {
                    a: center - up,
                    b: center + up,
                    radius: collider.radius.max(1e-4),
                }
            }
            Collider2DShape::Polygon => {
                let mut verts = Vec::with_capacity(collider.vertices.len());
                for &v in &collider.vertices {
                    verts.push(pos + rot_mat(Vec2::from_array(v)));
                }
                if verts.len() < 3 {
                    // 退化为盒
                    let half = Vec2::from_array(collider.half_extents)
                        .abs()
                        .max(Vec2::splat(1e-4));
                    let u = rot_mat(Vec2::X);
                    let v = rot_mat(Vec2::Y);
                    WorldShape2D::Box {
                        center,
                        half_extents: half,
                        axes: [u, v],
                    }
                } else {
                    WorldShape2D::Polygon { vertices: verts }
                }
            }
            Collider2DShape::Segment => {
                let b_local = Vec2::from_array(collider.segment_b);
                WorldShape2D::Segment {
                    a: center,
                    b: pos + rot_mat(b_local),
                }
            }
        }
    }

    /// 射线与形状求交，返回 (距离, 法线)。
    fn intersect_ray(&self, origin: Vec2, dir: Vec2, max_dist: f32) -> Option<(f32, Vec2)> {
        match self {
            WorldShape2D::Circle { center, radius } => {
                let m = origin - *center;
                let b = m.dot(dir);
                let c = m.dot(m) - radius * radius;
                if c > 0.0 && b > 0.0 {
                    return None;
                }
                let discr = b * b - c;
                if discr < 0.0 {
                    return None;
                }
                let mut t = -b - discr.sqrt();
                if t < 0.0 {
                    t = 0.0;
                }
                if t <= max_dist {
                    let hit_point = origin + dir * t;
                    let mut norm = (hit_point - *center).normalize_or_zero();
                    if norm == Vec2::ZERO {
                        norm = -dir;
                    }
                    Some((t, norm))
                } else {
                    None
                }
            }
            WorldShape2D::Box {
                center,
                half_extents,
                axes,
                ..
            } => {
                let d = origin - *center;
                let mut t_min = 0.0f32;
                let mut t_max = max_dist;
                let mut normal = Vec2::ZERO;

                for (i, &axis) in axes.iter().enumerate() {
                    let e = axis.dot(d);
                    let f = axis.dot(dir);
                    let half = if i == 0 {
                        half_extents.x
                    } else {
                        half_extents.y
                    };

                    if f.abs() > 1e-6 {
                        let mut t1 = (-e - half) / f;
                        let mut t2 = (-e + half) / f;
                        let mut sign = -1.0;
                        if t1 > t2 {
                            std::mem::swap(&mut t1, &mut t2);
                            sign = 1.0;
                        }
                        if t1 > t_min {
                            t_min = t1;
                            normal = axis * sign;
                        }
                        t_max = t_max.min(t2);
                        if t_min > t_max {
                            return None;
                        }
                    } else if -e - half > 0.0 || -e + half < 0.0 {
                        return None;
                    }
                }
                if t_min <= max_dist {
                    Some((t_min, normal.normalize_or_zero()))
                } else {
                    None
                }
            }
            WorldShape2D::Segment { a, b } => {
                let seg = *b - *a;
                let seg_len = seg.length();
                if seg_len < 1e-6 {
                    return None;
                }
                let p = *a;
                let r = seg;
                let q = origin;
                let s = dir * max_dist;

                let r_cross_s = r.x * s.y - r.y * s.x;
                let q_minus_p = q - p;
                let q_minus_p_cross_r = q_minus_p.x * r.y - q_minus_p.y * r.x;

                if r_cross_s.abs() < 1e-6 {
                    return None;
                }

                let t = (q_minus_p.x * s.y - q_minus_p.y * s.x) / r_cross_s;
                let u = q_minus_p_cross_r / r_cross_s;

                if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
                    let dist = u * max_dist;
                    let norm = Vec2::new(-seg.y, seg.x).normalize_or_zero();
                    let n = if norm.dot(-dir) >= 0.0 { norm } else { -norm };
                    Some((dist, n))
                } else {
                    None
                }
            }
            WorldShape2D::Polygon { vertices } => {
                let count = vertices.len();
                let mut t_min = 0.0f32;
                let mut t_max = max_dist;
                let mut normal = Vec2::ZERO;

                for i in 0..count {
                    let p1 = vertices[i];
                    let p2 = vertices[(i + 1) % count];
                    let edge = p2 - p1;
                    let n = Vec2::new(-edge.y, edge.x).normalize_or_zero();

                    let num = n.dot(p1 - origin);
                    let den = n.dot(dir);

                    if den.abs() < 1e-6 {
                        if num < 0.0 {
                            return None;
                        }
                    } else {
                        let t = num / den;
                        if den < 0.0 {
                            if t > t_min {
                                t_min = t;
                                normal = n;
                            }
                        } else if t < t_max {
                            t_max = t;
                        }
                        if t_min > t_max {
                            return None;
                        }
                    }
                }
                if t_min <= max_dist {
                    Some((t_min, normal.normalize_or_zero()))
                } else {
                    None
                }
            }
            WorldShape2D::Capsule { a, b, radius } => {
                // 简化为线段求交后退化到圆
                let seg = *b - *a;
                let len = seg.length();
                if len < 1e-6 {
                    return WorldShape2D::Circle {
                        center: *a,
                        radius: *radius,
                    }
                    .intersect_ray(origin, dir, max_dist);
                }
                let mut best: Option<(f32, Vec2)> = None;
                for c in &[*a, *b] {
                    if let Some((d, n)) = (WorldShape2D::Circle {
                        center: *c,
                        radius: *radius,
                    })
                    .intersect_ray(origin, dir, max_dist)
                    {
                        if best.map_or(true, |(bd, _)| d < bd) {
                            best = Some((d, n));
                        }
                    }
                }
                // 胶囊身段
                let n = Vec2::new(-seg.y, seg.x).normalize_or_zero() * *radius;
                let quad = WorldShape2D::Polygon {
                    vertices: vec![*a + n, *b + n, *b - n, *a - n],
                };
                if let Some((d, norm)) = quad.intersect_ray(origin, dir, max_dist) {
                    if best.map_or(true, |(bd, _)| d < bd) {
                        best = Some((d, norm));
                    }
                }
                best
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 空间查询 API
// ---------------------------------------------------------------------------

/// 2D 射线投射检测最近碰撞体。
pub fn raycast_2d(
    world: &World,
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    layer_mask: u32,
) -> Option<RayHit2D> {
    raycast_2d_excluding(world, origin, direction, max_distance, layer_mask, None)
}

/// 2D 射线投射（跳过指定实体自身）。
pub fn raycast_2d_excluding(
    world: &World,
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    layer_mask: u32,
    excluded: Option<Entity>,
) -> Option<RayHit2D> {
    let dir = Vec2::from_array(direction);
    let len = dir.length();
    if !len.is_finite() || len < 1e-6 || max_distance <= 0.0 {
        return None;
    }
    let dir = dir / len;
    let origin = Vec2::from_array(origin);

    let mut best: Option<RayHit2D> = None;

    for (entity, collider) in world.query::<Collider2D>() {
        if collider.collision_groups & layer_mask == 0 || Some(entity) == excluded {
            continue;
        }

        let (pos, rot) = extract_2d_transform(world, entity);
        let shape = WorldShape2D::from_collider(collider, pos, rot);

        if let Some((dist, normal)) = shape.intersect_ray(origin, dir, max_distance) {
            if dist <= max_distance && best.map_or(true, |b| dist < b.distance) {
                let hit_pt = origin + dir * dist;
                best = Some(RayHit2D {
                    entity,
                    distance: dist,
                    point: [hit_pt.x, hit_pt.y],
                    normal: [normal.x, normal.y],
                    is_sensor: collider.is_sensor,
                });
            }
        }
    }

    best
}

/// 2D 射线投射检测所有命中的碰撞体，按距离升序排列。
pub fn raycast_2d_all(
    world: &World,
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    layer_mask: u32,
) -> Vec<RayHit2D> {
    let dir = Vec2::from_array(direction);
    let len = dir.length();
    if !len.is_finite() || len < 1e-6 || max_distance <= 0.0 {
        return Vec::new();
    }
    let dir = dir / len;
    let origin = Vec2::from_array(origin);

    let mut hits = Vec::new();

    for (entity, collider) in world.query::<Collider2D>() {
        if collider.collision_groups & layer_mask == 0 {
            continue;
        }

        let (pos, rot) = extract_2d_transform(world, entity);
        let shape = WorldShape2D::from_collider(collider, pos, rot);

        if let Some((dist, normal)) = shape.intersect_ray(origin, dir, max_distance) {
            if dist <= max_distance {
                let hit_pt = origin + dir * dist;
                hits.push(RayHit2D {
                    entity,
                    distance: dist,
                    point: [hit_pt.x, hit_pt.y],
                    normal: [normal.x, normal.y],
                    is_sensor: collider.is_sensor,
                });
            }
        }
    }

    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    hits
}

/// 2D 圆形重叠查询，返回落在此圆范围内的所有实体。
pub fn overlap_circle_2d(
    world: &World,
    center: [f32; 2],
    radius: f32,
    layer_mask: u32,
) -> Vec<Entity> {
    let c = Vec2::from_array(center);
    let mut result = Vec::new();

    for (entity, collider) in world.query::<Collider2D>() {
        if collider.collision_groups & layer_mask == 0 {
            continue;
        }
        let (pos, rot) = extract_2d_transform(world, entity);
        let shape = WorldShape2D::from_collider(collider, pos, rot);

        if check_circle_overlap(&shape, c, radius) {
            result.push(entity);
        }
    }
    result
}

/// 2D 矩形盒重叠查询。
pub fn overlap_box_2d(
    world: &World,
    center: [f32; 2],
    half_extents: [f32; 2],
    angle: f32,
    layer_mask: u32,
) -> Vec<Entity> {
    let box_shape = WorldShape2D::Box {
        center: Vec2::from_array(center),
        half_extents: Vec2::from_array(half_extents),
        axes: [
            Vec2::new(angle.cos(), angle.sin()),
            Vec2::new(-angle.sin(), angle.cos()),
        ],
    };

    let mut result = Vec::new();
    for (entity, collider) in world.query::<Collider2D>() {
        if collider.collision_groups & layer_mask == 0 {
            continue;
        }
        let (pos, rot) = extract_2d_transform(world, entity);
        let shape = WorldShape2D::from_collider(collider, pos, rot);

        if check_shapes_overlap(&box_shape, &shape) {
            result.push(entity);
        }
    }
    result
}

/// 2D 形状投射查询（Shapecast2D）。
pub fn shapecast_2d(
    world: &World,
    cast_shape: &Collider2D,
    origin: [f32; 2],
    direction: [f32; 2],
    max_distance: f32,
    layer_mask: u32,
) -> Option<RayHit2D> {
    // 按步长采样推进检测
    let dir = Vec2::from_array(direction).normalize_or_zero();
    if dir == Vec2::ZERO || max_distance <= 0.0 {
        return None;
    }

    let steps = 16;
    let step_dist = max_distance / steps as f32;
    let current_pos = Vec2::from_array(origin);

    for s in 0..=steps {
        let dist = s as f32 * step_dist;
        let test_pos = current_pos + dir * dist;
        let shape = WorldShape2D::from_collider(cast_shape, test_pos, 0.0);

        for (entity, target_col) in world.query::<Collider2D>() {
            if target_col.collision_groups & layer_mask == 0 {
                continue;
            }
            let (t_pos, t_rot) = extract_2d_transform(world, entity);
            let target_shape = WorldShape2D::from_collider(target_col, t_pos, t_rot);

            if check_shapes_overlap(&shape, &target_shape) {
                return Some(RayHit2D {
                    entity,
                    distance: dist,
                    point: [test_pos.x, test_pos.y],
                    normal: [-dir.x, -dir.y],
                    is_sensor: target_col.is_sensor,
                });
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// 碰撞与接触求解器
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ContactManifold {
    normal: Vec2,
    penetration: f32,
}

fn extract_2d_transform(world: &World, entity: Entity) -> (Vec2, f32) {
    if let Some(tf) = world.get::<LocalTransform>(entity) {
        let angle = 2.0 * tf.rotation.z.atan2(tf.rotation.w);
        (Vec2::new(tf.position.x, tf.position.y), angle)
    } else if let Some(l2w) = world.get::<LocalToWorld>(entity) {
        let col3 = l2w.0.col(3);
        let col0 = l2w.0.col(0);
        let angle = col0.y.atan2(col0.x);
        (Vec2::new(col3.x, col3.y), angle)
    } else {
        (Vec2::ZERO, 0.0)
    }
}

fn check_circle_overlap(shape: &WorldShape2D, c: Vec2, radius: f32) -> bool {
    match shape {
        WorldShape2D::Circle { center, radius: r } => c.distance(*center) <= (radius + r),
        WorldShape2D::Box {
            center,
            half_extents,
            axes,
            ..
        } => {
            let rel = c - *center;
            let local_x = rel.dot(axes[0]).abs();
            let local_y = rel.dot(axes[1]).abs();
            let clamped_x = local_x.min(half_extents.x);
            let clamped_y = local_y.min(half_extents.y);
            let diff = Vec2::new(local_x - clamped_x, local_y - clamped_y);
            diff.length_squared() <= radius * radius
        }
        _ => false,
    }
}

fn check_shapes_overlap(a: &WorldShape2D, b: &WorldShape2D) -> bool {
    collide_shapes(a, b).is_some()
}

fn collide_shapes(a: &WorldShape2D, b: &WorldShape2D) -> Option<ContactManifold> {
    match (a, b) {
        (
            WorldShape2D::Circle {
                center: c1,
                radius: r1,
            },
            WorldShape2D::Circle {
                center: c2,
                radius: r2,
            },
        ) => {
            let diff = *c2 - *c1;
            let dist_sq = diff.length_squared();
            let r_sum = *r1 + *r2;
            if dist_sq > r_sum * r_sum {
                return None;
            }
            let dist = dist_sq.sqrt();
            let (normal, penetration) = if dist > 1e-6 {
                (diff / dist, r_sum - dist)
            } else {
                (Vec2::Y, r_sum)
            };
            Some(ContactManifold {
                normal,
                penetration,
            })
        }
        (
            WorldShape2D::Circle { center, radius },
            WorldShape2D::Box {
                center: bc,
                half_extents,
                axes,
                ..
            },
        ) => {
            let rel = *center - *bc;
            let lx = rel.dot(axes[0]);
            let ly = rel.dot(axes[1]);

            let cx = lx.clamp(-half_extents.x, half_extents.x);
            let cy = ly.clamp(-half_extents.y, half_extents.y);

            let _closest_local = Vec2::new(cx, cy);
            let closest_world = *bc + axes[0] * cx + axes[1] * cy;
            let diff = closest_world - *center;
            let dist_sq = diff.length_squared();

            if dist_sq > radius * radius {
                return None;
            }

            let dist = dist_sq.sqrt();
            let (normal, penetration) = if dist > 1e-6 {
                (diff / dist, *radius - dist)
            } else {
                (axes[1], *radius)
            };
            Some(ContactManifold {
                normal,
                penetration,
            })
        }
        (b @ WorldShape2D::Box { .. }, c @ WorldShape2D::Circle { .. }) => {
            let mut m = collide_shapes(c, b)?;
            m.normal = -m.normal;
            Some(m)
        }
        (
            WorldShape2D::Box {
                center: c1,
                half_extents: h1,
                axes: a1,
                ..
            },
            WorldShape2D::Box {
                center: c2,
                half_extents: h2,
                axes: a2,
                ..
            },
        ) => {
            // SAT 检测盒-盒碰撞
            let d = *c2 - *c1;
            let mut min_overlap = f32::MAX;
            let mut best_axis = Vec2::ZERO;

            let test_axes = [a1[0], a1[1], a2[0], a2[1]];

            for axis in test_axes {
                let dist = d.dot(axis).abs();
                let r1 = (a1[0].dot(axis)).abs() * h1.x + (a1[1].dot(axis)).abs() * h1.y;
                let r2 = (a2[0].dot(axis)).abs() * h2.x + (a2[1].dot(axis)).abs() * h2.y;
                let overlap = r1 + r2 - dist;

                if overlap <= 0.0 {
                    return None;
                }
                if overlap < min_overlap {
                    min_overlap = overlap;
                    best_axis = if d.dot(axis) >= 0.0 { axis } else { -axis };
                }
            }

            Some(ContactManifold {
                normal: best_axis.normalize_or_zero(),
                penetration: min_overlap,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 物理模拟步进系统
// ---------------------------------------------------------------------------

/// 推进 2D 物理系统模拟单步（推荐在 `fixed_update` 中调用）。
///
/// 包括：
/// 1. 动态刚体受重力与阻尼积分
/// 2. 窄相碰撞与触发器检测
/// 3. 冲量求解与反弹修正
/// 4. 实体位置与朝向同步
/// 5. 碰撞 / 传感器事件发射
pub fn step_2d(world: &mut World, dt: f32, gravity: [f32; 2]) {
    if dt <= 0.0 {
        return;
    }

    let g = Vec2::from_array(gravity);

    // 1. 积分动态刚体速度
    for (_, rb, _) in world.query2_mut::<RigidBody2D, LocalTransform>() {
        if rb.body_type != RigidBody2DType::Dynamic {
            continue;
        }

        let mut vel = Vec2::from_array(rb.linear_velocity);
        vel += g * rb.gravity_scale * dt;

        let lin_damping = (1.0f32 / (1.0f32 + rb.linear_damping * dt)).clamp(0.0f32, 1.0f32);
        vel *= lin_damping;

        let ang_damping = (1.0f32 / (1.0f32 + rb.angular_damping * dt)).clamp(0.0f32, 1.0f32);
        rb.angular_velocity *= ang_damping;

        rb.linear_velocity = [vel.x, vel.y];
    }

    // 收集所有带 Collider2D 的实体
    let mut colliders = Vec::new();
    for (entity, col) in world.query::<Collider2D>() {
        let (pos, rot) = extract_2d_transform(world, entity);
        let rb = world.get::<RigidBody2D>(entity).cloned();
        let shape = WorldShape2D::from_collider(col, pos, rot);
        colliders.push((entity, col.clone(), rb, shape));
    }

    let mut current_contacts = std::collections::HashSet::new();
    let mut events = Vec::new();

    // 2. 窄相碰撞检测与求解
    let count = colliders.len();
    for i in 0..count {
        for j in (i + 1)..count {
            let (e_a, col_a, rb_a, shape_a) = &colliders[i];
            let (e_b, col_b, rb_b, shape_b) = &colliders[j];

            if (col_a.collision_groups & col_b.collision_mask == 0)
                || (col_b.collision_groups & col_a.collision_mask == 0)
            {
                continue;
            }

            if let Some(manifold) = collide_shapes(shape_a, shape_b) {
                let pair_key = if e_a.id() < e_b.id() {
                    (*e_a, *e_b)
                } else {
                    (*e_b, *e_a)
                };
                current_contacts.insert(pair_key);

                let is_sensor = col_a.is_sensor || col_b.is_sensor;

                if !is_sensor {
                    // 求解刚体冲量
                    let type_a = rb_a
                        .as_ref()
                        .map_or(RigidBody2DType::Static, |r| r.body_type);
                    let type_b = rb_b
                        .as_ref()
                        .map_or(RigidBody2DType::Static, |r| r.body_type);

                    let inv_mass_a = if type_a == RigidBody2DType::Dynamic {
                        1.0 / rb_a.as_ref().map_or(1.0, |r| r.mass.max(1e-4))
                    } else {
                        0.0
                    };
                    let inv_mass_b = if type_b == RigidBody2DType::Dynamic {
                        1.0 / rb_b.as_ref().map_or(1.0, |r| r.mass.max(1e-4))
                    } else {
                        0.0
                    };

                    let total_inv_mass = inv_mass_a + inv_mass_b;
                    if total_inv_mass > 0.0 {
                        let vel_a = rb_a
                            .as_ref()
                            .map_or(Vec2::ZERO, |r| Vec2::from_array(r.linear_velocity));
                        let vel_b = rb_b
                            .as_ref()
                            .map_or(Vec2::ZERO, |r| Vec2::from_array(r.linear_velocity));
                        let rel_vel = vel_b - vel_a;

                        let normal_vel = rel_vel.dot(manifold.normal);
                        if normal_vel < 0.0 {
                            let restitution = col_a.restitution.min(col_b.restitution);
                            let impulse_mag = -(1.0 + restitution) * normal_vel / total_inv_mass;
                            let impulse = manifold.normal * impulse_mag;

                            if inv_mass_a > 0.0 {
                                if let Some(rb) = world.get_mut::<RigidBody2D>(*e_a) {
                                    let v =
                                        Vec2::from_array(rb.linear_velocity) - impulse * inv_mass_a;
                                    rb.linear_velocity = [v.x, v.y];
                                }
                            }
                            if inv_mass_b > 0.0 {
                                if let Some(rb) = world.get_mut::<RigidBody2D>(*e_b) {
                                    let v =
                                        Vec2::from_array(rb.linear_velocity) + impulse * inv_mass_b;
                                    rb.linear_velocity = [v.x, v.y];
                                }
                            }
                        }

                        // Baumgarte 穿透修正
                        let slop = 0.005f32;
                        let percent = 0.4f32;
                        let correction =
                            (manifold.penetration - slop).max(0.0) / total_inv_mass * percent;
                        let corr_vec = manifold.normal * correction;

                        if inv_mass_a > 0.0 {
                            if let Some(tf) = world.get_mut::<LocalTransform>(*e_a) {
                                tf.position.x -= corr_vec.x * inv_mass_a;
                                tf.position.y -= corr_vec.y * inv_mass_a;
                            }
                        }
                        if inv_mass_b > 0.0 {
                            if let Some(tf) = world.get_mut::<LocalTransform>(*e_b) {
                                tf.position.x += corr_vec.x * inv_mass_b;
                                tf.position.y += corr_vec.y * inv_mass_b;
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. 状态转换对比并生成 Started / Stopped 事件
    let prev_contacts = world
        .get_resource::<ActiveContactPairs>()
        .cloned()
        .unwrap_or_default();

    for &pair in &current_contacts {
        if !prev_contacts.pairs.contains(&pair) {
            let is_sensor = world
                .get::<Collider2D>(pair.0)
                .map_or(false, |c| c.is_sensor)
                || world
                    .get::<Collider2D>(pair.1)
                    .map_or(false, |c| c.is_sensor);
            events.push(Physics2DEvent {
                kind: Physics2DEventKind::Started,
                entity_a: pair.0,
                entity_b: pair.1,
                is_sensor,
            });
        }
    }

    for &pair in &prev_contacts.pairs {
        if !current_contacts.contains(&pair) {
            let is_sensor = world
                .get::<Collider2D>(pair.0)
                .map_or(false, |c| c.is_sensor)
                || world
                    .get::<Collider2D>(pair.1)
                    .map_or(false, |c| c.is_sensor);
            events.push(Physics2DEvent {
                kind: Physics2DEventKind::Stopped,
                entity_a: pair.0,
                entity_b: pair.1,
                is_sensor,
            });
        }
    }

    world.insert_resource(ActiveContactPairs {
        pairs: current_contacts,
    });
    world.insert_resource(Physics2DEvents { events });

    // 4. 推进动态与运动学刚体的位置与旋转
    for (_, rb, tf) in world.query2_mut::<RigidBody2D, LocalTransform>() {
        if rb.body_type == RigidBody2DType::Static {
            continue;
        }

        tf.position.x += rb.linear_velocity[0] * dt;
        tf.position.y += rb.linear_velocity[1] * dt;

        if !rb.fixed_rotation && rb.angular_velocity.abs() > 1e-6 {
            let cur_angle = 2.0 * tf.rotation.z.atan2(tf.rotation.w);
            let next_angle = cur_angle + rb.angular_velocity * dt;
            tf.rotation = Quat::from_rotation_z(next_angle);
        }
    }
}
