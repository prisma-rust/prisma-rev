//! 2D 世界空间 CPU 粒子（#594）。
//!
//! 作者侧只有 [`ParticleEmitter2D`]；活着的粒子保存在运行时组件
//! [`ParticleEmitter2DState`] 里，**不进** `.scene.json` / RSCN（和 [`SpriteRef`]、
//! 3D 的 `ParticleEmitter` 一样）。
//!
//! 设计取舍（与 3D 的 `particles` 模块保持一致）：
//!
//! - **CPU 模拟 + 精灵通道绘制**：粒子在世界 XY 平面上积分，每个粒子是一个
//!   纯色四边形，作为 [`SpriteDrawItem`](prism_render::SpriteDrawItem) 和精灵
//!   一起排序 —— 不需要新 pass / 新着色器，也不受屏幕分辨率影响；
//! - 发射器移动不会拖走已经飞出的粒子；
//! - 初速度方向按发射器当前的 Z 轴旋转（2D 只关心绕 Z），出生点偏移也走同一
//!   个旋转；
//! - 随机源是每个发射器独立的 `xorshift32`（种子 = 实体 id），同一场景每次
//!   加载的粒子分布一致，单测可复现；
//! - `dt` 上限复用 3D 的 [`MAX_STEP`]，编辑器卡一下不会让粒子瞬移。
//!
//! 系统跑在 `Engine::update` 的调度里、`hierarchy_system` 之前；提取阶段
//! （`scene_2d_system_into`）读的是本帧推进后的粒子。

use prism_ecs::{Entity, World};

use crate::scene::components::{LocalToWorld, ParticleEmitter2D, ParticleShape2D};
use crate::scene::systems::hierarchy::is_active_in_hierarchy;
use crate::scene::systems::particles::MAX_STEP;

/// 单个存活粒子（世界 XY 平面）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Particle2DInstance {
    /// 世界空间位置（XY；Z 取发射器的 Z）。
    pub position: glam::Vec2,
    /// 世界空间速度（单位 / 秒）。
    pub velocity: glam::Vec2,
    /// 已经存活的时间（秒）。
    pub age: f32,
    /// 总寿命（秒）；`age >= lifetime` 时被回收。
    pub lifetime: f32,
    /// 四边形边长（世界单位）。
    pub size: f32,
}

/// 发射器的运行时状态（活粒子 + 发射计时 + 随机源）。
///
/// 每次场景加载都重置；不进作者格式。
#[derive(Debug, Default, Clone)]
pub struct ParticleEmitter2DState {
    /// 存活粒子（容量复用，不每帧重建）。
    pub particles: Vec<Particle2DInstance>,
    /// 发射累加器（不足一个粒子时保留小数）。
    pub accumulator: f32,
    /// 当前发射循环内的时间（秒）。
    pub cycle_time: f32,
    /// 本循环的 `burst` 是否已经发射过。
    pub emitted_burst: bool,
    /// `xorshift32` 状态；0 表示还没播种。
    pub rng: u32,
}

impl ParticleEmitter2DState {
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
    let (lo, hi) = if range[0].is_finite() && range[1].is_finite() {
        if range[0] <= range[1] {
            (range[0], range[1])
        } else {
            (range[1], range[0])
        }
    } else {
        (fallback, fallback)
    };
    lo + (hi - lo) * t
}

/// 出生点偏移（发射器本地 XY）与初速度方向。
fn spawn_local(
    shape: ParticleShape2D,
    size: [f32; 2],
    radius: f32,
    direction: [f32; 2],
    spread_degrees: f32,
    rng: &mut u32,
) -> (glam::Vec2, glam::Vec2) {
    let half_x = if size[0].is_finite() {
        size[0].abs()
    } else {
        0.0
    };
    let half_y = if size[1].is_finite() {
        size[1].abs()
    } else {
        0.0
    };
    let radius = if radius.is_finite() && radius > 0.0 {
        radius
    } else {
        0.0
    };

    let offset = match shape {
        ParticleShape2D::Point => glam::Vec2::ZERO,
        ParticleShape2D::Line => glam::Vec2::new((next_unit(rng) * 2.0 - 1.0) * half_x, 0.0),
        ParticleShape2D::Box => glam::Vec2::new(
            (next_unit(rng) * 2.0 - 1.0) * half_x,
            (next_unit(rng) * 2.0 - 1.0) * half_y,
        ),
        ParticleShape2D::Circle => {
            let angle = next_unit(rng) * std::f32::consts::TAU;
            // sqrt 让圆盘内均匀分布，而不是挤在圆心。
            glam::Vec2::new(angle.cos(), angle.sin()) * radius * next_unit(rng).sqrt()
        }
    };

    // 2D 锥形：基准方向绕 Z 旋转 `[-spread, +spread]`。
    let half_angle = if spread_degrees.is_finite() {
        spread_degrees.clamp(0.0, 180.0).to_radians()
    } else {
        0.0
    };
    let jitter = (next_unit(rng) * 2.0 - 1.0) * half_angle;
    let (sin, cos) = jitter.sin_cos();
    let dir = glam::Vec2::new(
        direction[0] * cos - direction[1] * sin,
        direction[0] * sin + direction[1] * cos,
    );
    (offset, dir)
}

fn spawn_particle(
    emitter_def: &ParticleEmitter2D,
    state: &mut ParticleEmitter2DState,
    origin: glam::Vec2,
    z: f32,
    rotation: f32,
) {
    let (offset, direction) = spawn_local(
        emitter_def.shape,
        emitter_def.shape_size,
        emitter_def.shape_radius,
        emitter_def.direction_or_down(),
        emitter_def.spread_degrees,
        &mut state.rng,
    );
    let (sin, cos) = rotation.sin_cos();
    let world_offset = glam::Vec2::new(
        offset.x * cos - offset.y * sin,
        offset.x * sin + offset.y * cos,
    );
    let world_dir = glam::Vec2::new(
        direction.x * cos - direction.y * sin,
        direction.x * sin + direction.y * cos,
    );

    let speed = range(emitter_def.start_speed, next_unit(&mut state.rng), 1.0).max(0.0);
    let size = range(emitter_def.start_size, next_unit(&mut state.rng), 0.1).max(0.0);
    let lifetime = range(emitter_def.lifetime, next_unit(&mut state.rng), 1.0).max(0.0);
    let _ = z;
    state.particles.push(Particle2DInstance {
        position: origin + world_offset,
        velocity: world_dir * speed,
        age: 0.0,
        lifetime,
        size,
    });
}

/// 推进所有 `ParticleEmitter2D` 实体：积分已有粒子、按 emission 规则发射新粒子。
///
/// 返回本帧结束时存活的粒子总数（编辑器 / 测试用）。
pub fn particle_emitter_2d_system(world: &mut World, dt: f32) -> usize {
    if !dt.is_finite() || dt < 0.0 {
        return 0;
    }
    let dt = dt.min(MAX_STEP);

    let entities: Vec<Entity> = world
        .query::<ParticleEmitter2D>()
        .filter(|(entity, _)| is_active_in_hierarchy(world, *entity))
        .map(|(entity, _)| entity)
        .collect();

    let mut alive = 0usize;
    for entity in entities {
        // 作者组件可能刚被删除；先拷一份，避免借用冲突。
        let Some(emitter_def) = world.get::<ParticleEmitter2D>(entity).cloned() else {
            continue;
        };
        let transform = world
            .get::<LocalToWorld>(entity)
            .map(|t| t.0)
            .unwrap_or(glam::Mat4::IDENTITY);
        let (scale, rotation, translation) = transform.to_scale_rotation_translation();
        // 2D 只看绕 Z 的旋转；非法矩阵按单位变换处理。
        let rotation = if rotation.is_finite() {
            rotation.to_euler(glam::EulerRot::XYZ).2
        } else {
            0.0
        };
        let origin = if translation.is_finite() {
            glam::Vec2::new(translation.x, translation.y)
        } else {
            glam::Vec2::ZERO
        };
        let z = if translation.is_finite() {
            translation.z
        } else {
            0.0
        };
        let _ = scale;

        if world.get::<ParticleEmitter2DState>(entity).is_none() {
            world.insert(entity, ParticleEmitter2DState::seeded(entity));
        }
        let Some(mut state) = world.remove::<ParticleEmitter2DState>(entity) else {
            continue;
        };

        step(&emitter_def, &mut state, origin, z, rotation, dt);
        alive += state.particles.len();
        world.insert(entity, state);
    }

    // 作者删掉组件后，运行时状态也要跟着走，别留下不显示的孤儿粒子。
    let orphans: Vec<Entity> = world
        .query::<ParticleEmitter2DState>()
        .filter(|(entity, _)| world.get::<ParticleEmitter2D>(*entity).is_none())
        .map(|(entity, _)| entity)
        .collect();
    for entity in orphans {
        world.remove::<ParticleEmitter2DState>(entity);
    }

    alive
}

fn step(
    emitter_def: &ParticleEmitter2D,
    state: &mut ParticleEmitter2DState,
    origin: glam::Vec2,
    z: f32,
    rotation: f32,
    dt: f32,
) {
    if !emitter_def.playing {
        // 暂停：粒子停在原地，也不再积累发射计数。
        return;
    }

    // 1. 积分 + 回收。阻尼按「每秒衰减 drag 比例」处理，与帧率无关。
    let drag = if emitter_def.drag.is_finite() {
        emitter_def.drag.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let damping = (1.0 - drag).max(0.0).powf(dt.max(0.0));
    let gravity = glam::Vec2::new(
        if emitter_def.gravity[0].is_finite() {
            emitter_def.gravity[0]
        } else {
            0.0
        },
        if emitter_def.gravity[1].is_finite() {
            emitter_def.gravity[1]
        } else {
            0.0
        },
    );
    for particle in &mut state.particles {
        particle.age += dt;
        particle.velocity += gravity * dt;
        particle.velocity *= damping;
        particle.position += particle.velocity * dt;
    }
    state.particles.retain(|p| p.age < p.lifetime);

    // 2. 发射窗口（duration / looping）。`duration == 0` = 一直发射。
    let duration = emitter_def.emission_duration();
    let emitting = match duration {
        None => true,
        Some(duration) => emitter_def.looping || state.cycle_time < duration,
    };

    let cap = emitter_def.particle_cap();

    // 3. burst：每个循环开始时一次性发射。
    if !state.emitted_burst {
        let burst = emitter_def.burst.min(cap as u32);
        for _ in 0..burst {
            if state.particles.len() >= cap {
                break;
            }
            spawn_particle(emitter_def, state, origin, z, rotation);
        }
        state.emitted_burst = true;
    }

    // 4. 持续发射。
    if emitting {
        state.accumulator += emitter_def.rate() * dt;
        while state.accumulator >= 1.0 {
            state.accumulator -= 1.0;
            if state.particles.len() >= cap {
                state.accumulator = 0.0;
                break;
            }
            spawn_particle(emitter_def, state, origin, z, rotation);
        }
    } else {
        state.accumulator = 0.0;
    }

    if state.particles.len() > cap {
        state.particles.truncate(cap);
    }

    // 5. 推进发射窗口：到点后循环回 0（重新计 burst）或停在终点。
    if let Some(duration) = duration {
        state.cycle_time += dt;
        if state.cycle_time >= duration {
            if emitter_def.looping {
                state.cycle_time = state.cycle_time.rem_euclid(duration);
                state.emitted_burst = false;
            } else {
                state.cycle_time = duration;
            }
        }
    }
}

/// 粒子当前颜色：`start_color → end_color` 按年龄线性插值。
pub fn particle_2d_color(
    emitter_def: &ParticleEmitter2D,
    particle: &Particle2DInstance,
) -> [f32; 4] {
    let t = if particle.lifetime > 0.0 {
        (particle.age / particle.lifetime).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    [
        lerp(emitter_def.start_color[0], emitter_def.end_color[0]),
        lerp(emitter_def.start_color[1], emitter_def.end_color[1]),
        lerp(emitter_def.start_color[2], emitter_def.end_color[2]),
        lerp(emitter_def.start_color[3], emitter_def.end_color[3]),
    ]
}

/// 2D 粒子的排序键；与精灵 / 折线共用 `SortKey` 的字段顺序。
pub(crate) type Particle2DSortKey = (i32, i32, f32, u32);

/// 提取所有可见 2D 发射器的粒子为绘制项。
///
/// 只读；不修改模拟状态。深度取发射器自身的 Z，和精灵的排序规则一致
/// （`sorting_layer → order_in_layer → z → entity id`）。
pub(crate) fn particle_entries_2d(
    world: &World,
) -> Vec<(Particle2DSortKey, prism_render::SpriteDrawItem)> {
    let mut out = Vec::new();
    for (entity, state) in world.query::<ParticleEmitter2DState>() {
        let Some(emitter_def) = world.get::<ParticleEmitter2D>(entity) else {
            continue;
        };
        if state.particles.is_empty() || !is_active_in_hierarchy(world, entity) {
            continue;
        }
        let z = world
            .get::<LocalToWorld>(entity)
            .map(|t| t.0.w_axis.z)
            .unwrap_or(0.0);
        let key: Particle2DSortKey = (
            emitter_def.sorting_layer,
            emitter_def.order_in_layer,
            z,
            entity.id(),
        );
        for particle in &state.particles {
            let model = glam::Mat4::from_scale_rotation_translation(
                glam::vec3(particle.size, particle.size, 1.0),
                glam::Quat::IDENTITY,
                glam::vec3(particle.position.x, particle.position.y, z),
            );
            out.push((
                key,
                prism_render::SpriteDrawItem {
                    model: model.to_cols_array_2d(),
                    corners: None,
                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                    color: particle_2d_color(emitter_def, particle),
                    texture: crate::scene::systems::render::SOLID_TEXTURE,
                    // 纯色四边形：标准 alpha 混合，和折线一致。
                    src_blend: prism_scene::render_enums::BlendMode::SrcAlpha,
                    dst_blend: prism_scene::render_enums::BlendMode::OneMinusSrcAlpha,
                    blend_op: prism_scene::render_enums::BlendOp::Add,
                    clip: prism_render::NO_CLIP,
                },
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "particles2d_tests.rs"]
mod tests;
