//! 世界空间 CPU 粒子模拟（#501 Phase 1）。
//!
//! 数据侧只有作者组件 [`ParticleSystem`]；活着的粒子保存在运行时组件
//! [`ParticleEmitter`] 里，**不进** `.scene.json` / RSCN（和播放头、`SpriteRef`
//! 一样的约定）。
//!
//! 设计取舍：
//!
//! - **CPU 模拟 + 实例化 billboard**：先做与 ECS / 场景格式一致的版本，
//!   GPU（compute + indirect draw）留 Phase 2；
//! - 粒子在**世界空间**积分，发射器移动不会拖走已飞出的粒子；初速度方向按
//!   发射器当前的旋转换算；
//! - 出生时按 `start_speed` / `start_size` / `lifetime` 的区间各取一个随机值；
//!   颜色 / 尺寸 / 速度偏移都按年龄在作者曲线上求值（提取或积分时算，不在模拟里
//!   存曲线状态，这样改作者数据立刻生效）；
//! - 可选碰撞（#501）：`collision = true` 时，粒子按自身半径被推出带
//!   [`Collider`] 的实体表面（盒 / 球 / 胶囊），法向按 `collision_bounce`
//!   反弹、切向按 `collision_friction` 衰减；地形高度场还不是碰撞体；
//! - 随机源是每个发射器独立的 `xorshift32`（种子 = 实体 id），所以同一场景
//!   每次加载的粒子分布一致——比 `rand` 依赖更轻，也让单测可复现；
//! - `dt` 上限 0.1s：编辑器卡一下不会让粒子瞬移穿过整个场景。
//!
//! 系统跑在 `Engine::update` 的调度里、`hierarchy_system` 之前（和关键帧动画
//! 并排），所以同一帧的提取看到的已经是推进后的粒子。

use prism_ecs::{Entity, World};

use crate::physics::WorldShape;
use crate::scene::components::{Collider, LocalToWorld, ParticleShape, ParticleSystem};
use crate::scene::systems::hierarchy::is_active_in_hierarchy;

/// 单帧最大模拟步长（秒）。更大的 `dt` 被截断，避免卡顿后粒子瞬移。
pub const MAX_STEP: f32 = 0.1;

/// 单个存活粒子（世界空间）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Particle {
    /// 世界空间位置。
    pub position: glam::Vec3,
    /// 世界空间速度（单位 / 秒）。
    pub velocity: glam::Vec3,
    /// 已经存活的时间（秒）。
    pub age: f32,
    /// 总寿命（秒）；`age >= lifetime` 时被回收。
    pub lifetime: f32,
    /// 世界单位半径。
    pub size: f32,
}

/// 发射器的运行时状态（活粒子 + 发射计时 + 随机源）。
///
/// 每次场景加载都重置；不进作者格式。
#[derive(Debug, Default, Clone)]
pub struct ParticleEmitter {
    /// 存活粒子（容量复用，不每帧重建）。
    pub particles: Vec<Particle>,
    /// 发射累加器（不足一个粒子时保留小数）。
    pub accumulator: f32,
    /// 当前发射循环内的时间（秒）。
    pub cycle_time: f32,
    /// 本循环的 `burst` 是否已经发射过。
    pub emitted_burst: bool,
    /// `xorshift32` 状态；0 表示还没播种。
    pub rng: u32,
}

impl ParticleEmitter {
    /// 用实体 id 播种：同一场景每次加载的粒子分布一致。
    pub fn seeded(entity: Entity) -> Self {
        Self {
            particles: Vec::new(),
            accumulator: 0.0,
            cycle_time: 0.0,
            emitted_burst: false,
            rng: seed_from_entity(entity),
        }
    }
}

fn seed_from_entity(entity: Entity) -> u32 {
    // 0 是 xorshift 的非法状态，这里强制非 0。
    entity.id().wrapping_mul(2_654_435_761).max(1)
}

/// `xorshift32` → `[0, 1)`。
fn next_unit(rng: &mut u32) -> f32 {
    let mut x = *rng;
    if x == 0 {
        x = 1;
    }
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *rng = x;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// 区间取值 `[min, max]`（非有限 / 倒序时回落到 `fallback`）。
fn range(range: [f32; 2], t: f32, fallback: f32) -> f32 {
    ParticleSystem::random_range(range, t, fallback)
}

fn sanitize_vec3(v: [f32; 3]) -> glam::Vec3 {
    glam::Vec3::new(
        if v[0].is_finite() { v[0] } else { 0.0 },
        if v[1].is_finite() { v[1] } else { 0.0 },
        if v[2].is_finite() { v[2] } else { 0.0 },
    )
}

fn random_unit(rng: &mut u32) -> glam::Vec3 {
    // 拒绝采样：球内均匀随机点归一化；命中概率 ~52%，重试几次就够。
    for _ in 0..4 {
        let x = next_unit(rng) * 2.0 - 1.0;
        let y = next_unit(rng) * 2.0 - 1.0;
        let z = next_unit(rng) * 2.0 - 1.0;
        let v = glam::Vec3::new(x, y, z);
        let len_sq = v.length_squared();
        if len_sq > 1e-4 && len_sq <= 1.0 {
            return v / len_sq.sqrt();
        }
    }
    glam::Vec3::Y
}

/// 发射器局部空间的出生偏移与初速度方向。
fn spawn_local(
    shape: ParticleShape,
    radius: f32,
    cone_degrees: f32,
    rng: &mut u32,
) -> (glam::Vec3, glam::Vec3) {
    let radius = if radius.is_finite() && radius > 0.0 {
        radius
    } else {
        0.0
    };
    match shape {
        ParticleShape::Point => (glam::Vec3::ZERO, random_unit(rng)),
        ParticleShape::Sphere => {
            let dir = random_unit(rng);
            (dir * radius * next_unit(rng).sqrt(), dir)
        }
        ParticleShape::Box => {
            let offset = glam::Vec3::new(
                (next_unit(rng) * 2.0 - 1.0) * radius,
                (next_unit(rng) * 2.0 - 1.0) * radius,
                (next_unit(rng) * 2.0 - 1.0) * radius,
            );
            (offset, random_unit(rng))
        }
        ParticleShape::Cone => {
            // 圆锥绕发射器本地 +Y：先在球面上取一个点，再按 half-angle
            // 把极角压到 [0, cone]。
            let cos_max = cone_degrees.clamp(0.0, 180.0).to_radians().cos();
            let z = 1.0 - next_unit(rng) * (1.0 - cos_max);
            let phi = next_unit(rng) * std::f32::consts::TAU;
            let sin_theta = (1.0 - z * z).max(0.0).sqrt();
            let dir = glam::Vec3::new(sin_theta * phi.cos(), z, sin_theta * phi.sin());
            let offset = dir * radius * next_unit(rng).sqrt();
            (offset, dir)
        }
    }
}

fn spawn_particle(
    system: &ParticleSystem,
    emitter: &mut ParticleEmitter,
    origin: glam::Vec3,
    rotation: glam::Quat,
) {
    let (offset, direction) = spawn_local(
        system.shape,
        system.shape_radius,
        system.cone_angle_degrees,
        &mut emitter.rng,
    );
    let speed = range(system.start_speed, next_unit(&mut emitter.rng), 1.0).max(0.0);
    let size = range(system.start_size, next_unit(&mut emitter.rng), 0.2).max(0.0);
    let lifetime = range(system.lifetime, next_unit(&mut emitter.rng), 1.0).max(0.0);
    emitter.particles.push(Particle {
        position: origin + rotation * offset,
        velocity: rotation * direction * speed,
        age: 0.0,
        lifetime,
        size,
    });
}

/// 推进所有 `ParticleSystem` 实体：积分已有粒子、按 emission 规则发射新粒子。
///
/// 返回本帧结束时存活的粒子总数（编辑器 / 测试用）。
pub fn particle_system(world: &mut World, dt: f32) -> usize {
    if !dt.is_finite() || dt < 0.0 {
        return 0;
    }
    let dt = dt.min(MAX_STEP);

    let entities: Vec<Entity> = world
        .query::<ParticleSystem>()
        .filter(|(entity, _)| is_active_in_hierarchy(world, *entity))
        .map(|(entity, _)| entity)
        .collect();

    // #501：碰撞形状每帧快照一次；没有发射器开碰撞时也能接受这点开销
    // （通常场景只有个位数 collider）。
    let colliders: Vec<WorldShape> = world
        .query::<Collider>()
        .filter_map(|(entity, collider)| {
            world
                .get::<LocalToWorld>(entity)
                .map(|transform| WorldShape::new(collider, &transform.0))
        })
        .collect();

    let mut alive = 0usize;
    for entity in entities {
        // 作者组件可能刚被删除；先拷一份，避免借用冲突。
        let Some(system) = world.get::<ParticleSystem>(entity).cloned() else {
            continue;
        };
        let transform = world
            .get::<LocalToWorld>(entity)
            .map(|t| t.0)
            .unwrap_or(glam::Mat4::IDENTITY);
        let (scale, rotation, translation) = transform.to_scale_rotation_translation();
        // 无效矩阵（NaN / 零缩放）时按单位变换处理，别把粒子射到无穷远。
        let rotation = if rotation.is_finite() {
            rotation.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        let origin = if translation.is_finite() {
            translation
        } else {
            glam::Vec3::ZERO
        };
        let _ = scale;

        if world.get::<ParticleEmitter>(entity).is_none() {
            world.insert(entity, ParticleEmitter::seeded(entity));
        }
        let Some(mut emitter) = world.remove::<ParticleEmitter>(entity) else {
            continue;
        };

        step(&system, &mut emitter, origin, rotation, dt, &colliders);
        alive += emitter.particles.len();
        world.insert(entity, emitter);
    }
    alive
}

/// 把一个粒子推出所有相交的碰撞体，并按法向 / 切向分别处理弹跳与摩擦。
///
/// 多次推出是必要的：角落 / 凹槽里可能同时碰到两个形状；每次推出都会用新的
/// 位置继续测试，直到所有形状都满足（形状数很少，不做空间索引）。
fn collide_particle(system: &ParticleSystem, particle: &mut Particle, colliders: &[WorldShape]) {
    let bounce = if system.collision_bounce.is_finite() {
        system.collision_bounce.clamp(0.0, 1.0)
    } else {
        0.3
    };
    let friction = if system.collision_friction.is_finite() {
        system.collision_friction.clamp(0.0, 1.0)
    } else {
        0.2
    };
    for shape in colliders {
        let Some((position, normal)) = shape.push_out(particle.position, particle.size.max(0.0))
        else {
            continue;
        };
        particle.position = position;
        let normal_speed = particle.velocity.dot(normal);
        if normal_speed < 0.0 {
            let tangential = particle.velocity - normal * normal_speed;
            particle.velocity = tangential * (1.0 - friction) - normal * normal_speed * bounce;
        }
    }
}

fn step(
    system: &ParticleSystem,
    emitter: &mut ParticleEmitter,
    origin: glam::Vec3,
    rotation: glam::Quat,
    dt: f32,
    colliders: &[WorldShape],
) {
    if !system.playing {
        // 暂停：粒子停在原地，也不再积累发射计数。
        return;
    }

    // 1. 积分 + 回收。
    //    阻尼按「每秒衰减 drag 比例」处理：`(1 - drag)^dt`，与帧率无关。
    let drag = if system.drag.is_finite() {
        system.drag.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let damping = (1.0 - drag).max(0.0).powf(dt.max(0.0));
    let gravity = sanitize_vec3(system.gravity);
    for particle in &mut emitter.particles {
        particle.age += dt;
        particle.velocity += gravity * dt;
        particle.velocity *= damping;
        // 速度曲线只改变**位移**，不写回 `velocity`：否则会与重力 / 阻尼叠加，
        // 让“风速”变成永久加速度。
        let drift = particle_velocity_offset(system, particle);
        particle.position += (particle.velocity + drift) * dt;
        if system.collision && !colliders.is_empty() {
            collide_particle(system, particle, colliders);
        }
    }
    emitter.particles.retain(|p| p.age < p.lifetime);

    // 2. 发射窗口（duration / looping）。`duration == 0` = 一直发射。
    //
    // 先按「本步开始时」的窗口状态发射，再把 `cycle_time` 往前推：这样
    // `duration = 0.2` 的发射器能发满最后那一小段，而不是在恰好到点的那一帧
    // 提前停下。
    let duration = system.emission_duration();
    let emitting = match duration {
        None => true,
        Some(duration) => system.looping || emitter.cycle_time < duration,
    };

    let cap = system.particle_cap();

    // 3. burst：每个循环开始时一次性发射。
    if !emitter.emitted_burst {
        let burst = system.burst.min(cap as u32);
        for _ in 0..burst {
            if emitter.particles.len() >= cap {
                break;
            }
            spawn_particle(system, emitter, origin, rotation);
        }
        emitter.emitted_burst = true;
    }

    // 4. 持续发射。
    if emitting {
        emitter.accumulator += system.rate() * dt;
        while emitter.accumulator >= 1.0 {
            emitter.accumulator -= 1.0;
            if emitter.particles.len() >= cap {
                emitter.accumulator = 0.0;
                break;
            }
            spawn_particle(system, emitter, origin, rotation);
        }
    } else {
        emitter.accumulator = 0.0;
    }

    if emitter.particles.len() > cap {
        emitter.particles.truncate(cap);
    }

    // 5. 推进发射窗口：到点后循环回 0（重新计 burst）或停在终点。
    if let Some(duration) = duration {
        emitter.cycle_time += dt;
        if emitter.cycle_time >= duration {
            if system.looping {
                emitter.cycle_time = emitter.cycle_time.rem_euclid(duration);
                emitter.emitted_burst = false;
            } else {
                emitter.cycle_time = duration;
            }
        }
    }
}

/// 粒子在生命周期上的进度（`0` = 出生，`1` = 死亡）。
///
/// 寿命为 0 / 非有限值时按 `1`（已经死亡），避免除零产生 NaN。
fn age_fraction(particle: &Particle) -> f32 {
    if particle.lifetime.is_finite() && particle.lifetime > 0.0 {
        (particle.age / particle.lifetime).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// 在多段颜色渐变上取色；`stops` 为空时返回 `None`（调用方退回两色插值）。
///
/// 停靠点在 `[0, 1]` 上均匀分布，相邻两点之间线性插值；`t` 超出范围时钳到端点。
/// 只有一个停靠点时整条生命周期都是这个颜色。
pub fn sample_gradient(stops: &[[f32; 4]], t: f32) -> Option<[f32; 4]> {
    match stops.len() {
        0 => None,
        1 => Some(stops[0]),
        len => {
            let t = if t.is_finite() {
                t.clamp(0.0, 1.0)
            } else {
                1.0
            };
            let scaled = t * (len - 1) as f32;
            let index = (scaled.floor() as usize).min(len - 2);
            let local = scaled - index as f32;
            let a = stops[index];
            let b = stops[index + 1];
            let lerp = |a: f32, b: f32| a + (b - a) * local;
            Some([
                lerp(a[0], b[0]),
                lerp(a[1], b[1]),
                lerp(a[2], b[2]),
                lerp(a[3], b[3]),
            ])
        }
    }
}

/// 粒子当前颜色。
///
/// `color_stops` 非空时按生命周期在多段渐变上取色；否则退回
/// `start_color → end_color` 的两色线性插值（旧行为）。
pub fn particle_color(system: &ParticleSystem, particle: &Particle) -> [f32; 4] {
    let t = age_fraction(particle);
    if let Some(color) = sample_gradient(&system.color_stops, t) {
        return color;
    }
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    [
        lerp(system.start_color[0], system.end_color[0]),
        lerp(system.start_color[1], system.end_color[1]),
        lerp(system.start_color[2], system.end_color[2]),
        lerp(system.start_color[3], system.end_color[3]),
    ]
}

/// 粒子当前尺寸：出生尺寸 × `size_over_lifetime` 在生命周期上的插值。
///
/// 倍率非有限值时按 `1` 处理；结果钳到非负，避免 billboard 尺寸变成负 / NaN。
pub fn particle_size(system: &ParticleSystem, particle: &Particle) -> f32 {
    let t = age_fraction(particle);
    let start = system.size_over_lifetime[0];
    let end = system.size_over_lifetime[1];
    let start = if start.is_finite() { start } else { 1.0 };
    let end = if end.is_finite() { end } else { 1.0 };
    let scale = start + (end - start) * t;
    (particle.size * scale).max(0.0)
}

/// 粒子当前的世界空间速度偏移（`velocity_over_lifetime` 在寿命上的线性插值）。
///
/// 非有限值按 0 处理；结果只用于位移，不写回 [`Particle::velocity`]。
pub fn particle_velocity_offset(system: &ParticleSystem, particle: &Particle) -> glam::Vec3 {
    let start = sanitize_vec3(system.velocity_over_lifetime[0]);
    let end = sanitize_vec3(system.velocity_over_lifetime[1]);
    let t = age_fraction(particle);
    start + (end - start) * t
}

/// 提取所有可见发射器的粒子为 billboard 绘制项，按到相机的距离**远到近**排序。
///
/// 只读；不修改模拟状态。没有 `ParticleSystem` 的孤儿 `ParticleEmitter`
/// （作者删了组件）被跳过。
pub fn scene_particles_into(
    world: &World,
    eye: glam::Vec3,
    out: &mut Vec<prism_render::ParticleDrawItem>,
) {
    out.clear();
    for (entity, emitter) in world.query::<ParticleEmitter>() {
        let Some(system) = world.get::<ParticleSystem>(entity) else {
            continue;
        };
        if !is_active_in_hierarchy(world, entity) {
            continue;
        }
        out.extend(
            emitter
                .particles
                .iter()
                .map(|particle| prism_render::ParticleDrawItem {
                    center: particle.position.to_array(),
                    size: particle_size(system, particle),
                    color: particle_color(system, particle),
                }),
        );
    }

    out.sort_by(|a, b| {
        let da = glam::Vec3::from_array(a.center).distance_squared(eye);
        let db = glam::Vec3::from_array(b.center).distance_squared(eye);
        db.total_cmp(&da)
    });
}

#[cfg(test)]
#[path = "particle_tests.rs"]
mod particle_tests;
