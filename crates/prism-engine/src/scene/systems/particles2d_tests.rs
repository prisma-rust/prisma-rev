// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;
use crate::scene::components::ParticleEmitter2D;

fn spawn_emitter(world: &mut World, emitter_def: ParticleEmitter2D) -> Entity {
    let entity = world.spawn();
    world.insert(entity, emitter_def);
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_translation(glam::vec3(3.0, 5.0, -2.0))),
    );
    entity
}

fn state_of(world: &World, entity: Entity) -> &ParticleEmitter2DState {
    world
        .get::<ParticleEmitter2DState>(entity)
        .expect("runtime state must exist after one step")
}

#[test]
fn emission_rate_accumulates_across_frames() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 10.0,
            lifetime: [10.0, 10.0],
            ..ParticleEmitter2D::default()
        },
    );

    // 10/s，每帧 0.05s → 0.5 个/帧，两帧才凑够一个。
    assert_eq!(particle_emitter_2d_system(&mut world, 0.05), 0);
    assert_eq!(particle_emitter_2d_system(&mut world, 0.05), 1);
    // 单步上限 `MAX_STEP = 0.1s`（卡顿保护），所以 0.5s 只推进 0.1s。
    assert_eq!(particle_emitter_2d_system(&mut world, 0.5), 2);
    assert_eq!(particle_emitter_2d_system(&mut world, 0.1), 3);
    assert_eq!(state_of(&world, entity).particles.len(), 3);
}

#[test]
fn the_particle_cap_is_respected() {
    let mut world = World::new();
    spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 1000.0,
            max_particles: 4,
            lifetime: [10.0, 10.0],
            ..ParticleEmitter2D::default()
        },
    );

    let alive = particle_emitter_2d_system(&mut world, 0.5);
    assert_eq!(alive, 4, "超过上限就停止发射");
}

#[test]
fn gravity_pulls_particles_down_the_y_axis() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 100.0,
            lifetime: [10.0, 10.0],
            start_speed: [0.0, 0.0],
            gravity: [0.0, -4.0],
            drag: 0.0,
            direction: [1.0, 0.0],
            spread_degrees: 0.0,
            ..ParticleEmitter2D::default()
        },
    );

    particle_emitter_2d_system(&mut world, 0.1);
    let before = state_of(&world, entity).particles[0].position;
    particle_emitter_2d_system(&mut world, 0.1);
    let after = state_of(&world, entity).particles[0].position;

    assert!(
        after.y < before.y,
        "重力必须把粒子往下拉：{before:?} → {after:?}"
    );
    assert!((after.x - before.x).abs() < 1e-4, "没有水平风时 x 不应漂移");
}

#[test]
fn particles_expire_after_their_lifetime() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 0.0,
            burst: 2,
            lifetime: [0.2, 0.2],
            ..ParticleEmitter2D::default()
        },
    );

    // 出生发生在积分之后，所以第一帧结束时 age 仍是 0。
    particle_emitter_2d_system(&mut world, 0.1);
    assert_eq!(
        state_of(&world, entity).particles.len(),
        2,
        "burst 先发两个"
    );
    particle_emitter_2d_system(&mut world, 0.1);
    assert_eq!(
        state_of(&world, entity).particles.len(),
        2,
        "age 0.1 还没到 0.2 的寿命"
    );
    particle_emitter_2d_system(&mut world, 0.1);
    assert_eq!(
        state_of(&world, entity).particles.len(),
        0,
        "寿命到点后回收"
    );
}

#[test]
fn pausing_freezes_existing_particles_and_stops_emission() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 100.0,
            lifetime: [10.0, 10.0],
            gravity: [0.0, -10.0],
            ..ParticleEmitter2D::default()
        },
    );
    particle_emitter_2d_system(&mut world, 0.1);
    let before = state_of(&world, entity).particles.clone();
    assert!(!before.is_empty());

    world.get_mut::<ParticleEmitter2D>(entity).unwrap().playing = false;
    particle_emitter_2d_system(&mut world, 0.1);
    let after = state_of(&world, entity).particles.clone();
    assert_eq!(before, after, "暂停时既不发射也不推进");
}

#[test]
fn a_non_looping_duration_stops_emitting() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 100.0,
            duration: 0.1,
            looping: false,
            lifetime: [10.0, 10.0],
            ..ParticleEmitter2D::default()
        },
    );

    particle_emitter_2d_system(&mut world, 0.1);
    let emitted = state_of(&world, entity).particles.len();
    assert_eq!(emitted, 10, "0.1s × 100/s");

    // 窗口结束后不再发射，已有粒子继续存活。
    particle_emitter_2d_system(&mut world, 0.5);
    assert_eq!(
        state_of(&world, entity).particles.len(),
        emitted,
        "一次性发射器到点后停止"
    );
}

#[test]
fn removing_the_component_drops_the_runtime_state() {
    let mut world = World::new();
    let entity = spawn_emitter(&mut world, ParticleEmitter2D::default());
    particle_emitter_2d_system(&mut world, 0.1);
    assert!(world.get::<ParticleEmitter2DState>(entity).is_some());

    world.remove::<ParticleEmitter2D>(entity);
    particle_emitter_2d_system(&mut world, 0.1);
    assert!(
        world.get::<ParticleEmitter2DState>(entity).is_none(),
        "作者删掉组件后不该留下孤儿粒子"
    );
}

#[test]
fn spread_zero_uses_the_authored_direction() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 100.0,
            lifetime: [10.0, 10.0],
            start_speed: [2.0, 2.0],
            gravity: [0.0, 0.0],
            drag: 0.0,
            direction: [1.0, 0.0],
            spread_degrees: 0.0,
            ..ParticleEmitter2D::default()
        },
    );

    particle_emitter_2d_system(&mut world, 0.1);
    let particle = state_of(&world, entity).particles[0];
    assert!(
        (particle.velocity.x - 2.0).abs() < 1e-4,
        "{:?}",
        particle.velocity
    );
    assert!(particle.velocity.y.abs() < 1e-4, "{:?}", particle.velocity);
}

#[test]
fn emitters_are_transformed_into_world_space() {
    let mut world = World::new();
    // 30° 绕 Z 旋转 + (1, 2, 0) 平移的发射器。
    let entity = world.spawn();
    world.insert(
        entity,
        ParticleEmitter2D {
            emission_rate: 100.0,
            lifetime: [10.0, 10.0],
            start_speed: [0.0, 0.0],
            gravity: [0.0, 0.0],
            direction: [1.0, 0.0],
            spread_degrees: 0.0,
            ..ParticleEmitter2D::default()
        },
    );
    world.insert(
        entity,
        LocalToWorld(glam::Mat4::from_scale_rotation_translation(
            glam::Vec3::ONE,
            glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_6),
            glam::vec3(1.0, 2.0, 0.0),
        )),
    );

    particle_emitter_2d_system(&mut world, 0.1);
    let particle = state_of(&world, entity).particles[0];
    assert!(
        (particle.position - glam::vec2(1.0, 2.0)).length() < 1e-4,
        "Point 形状从发射器原点出生：{:?}",
        particle.position
    );
}

#[test]
fn particle_quads_reach_the_2d_draw_list_with_sorting_and_fade() {
    let mut world = World::new();
    let entity = spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 0.0,
            burst: 1,
            lifetime: [1.0, 1.0],
            start_speed: [0.0, 0.0],
            gravity: [0.0, 0.0],
            start_size: [0.5, 0.5],
            start_color: [1.0, 1.0, 1.0, 1.0],
            end_color: [1.0, 1.0, 1.0, 0.0],
            sorting_layer: 20,
            order_in_layer: 3,
            ..ParticleEmitter2D::default()
        },
    );
    // 第一帧只发射（age 0），第二帧才推进到 0.1s。
    particle_emitter_2d_system(&mut world, 0.1);
    particle_emitter_2d_system(&mut world, 0.1);

    let items = crate::scene::systems::render::scene_2d_system(&world);
    assert_eq!(items.len(), 1, "无精灵 / 折线时只剩这一个粒子");

    let item = &items[0];
    // 四边形尺寸 = start_size，中心 = 发射器的世界位置。
    // `to_cols_array_2d`：model[0] / model[3] 分别是 X 轴与平移列。
    assert!((item.model[0][0] - 0.5).abs() < 1e-6, "{:?}", item.model);
    assert!((item.model[3][0] - 3.0).abs() < 1e-6);
    assert!((item.model[3][1] - 5.0).abs() < 1e-6);
    assert_eq!(item.texture, crate::scene::systems::render::SOLID_TEXTURE);
    // 存活 0.1 / 1.0 → alpha 0.9。
    assert!((item.color[3] - 0.9).abs() < 1e-4, "{:?}", item.color);
    let _ = entity;
}

#[test]
fn particles_sort_against_sprites_by_layer() {
    let mut world = World::new();
    // 精灵层 10，粒子层 20 → 粒子画在后面（更靠上层）。
    let sprite = world.spawn();
    world.insert(sprite, crate::scene::components::SpriteRenderer::default());
    world.insert(
        sprite,
        crate::scene::runtime_components::SpriteRef {
            asset_id: crate::scene::components::SceneAssetId::generate(),
            texture_slot: 1,
            size: [16, 16],
            generation: 1,
        },
    );
    world.insert(sprite, LocalToWorld(glam::Mat4::IDENTITY));
    world
        .get_mut::<crate::scene::components::SpriteRenderer>(sprite)
        .unwrap()
        .sprite_path = "sprites/a.png".into();
    world
        .get_mut::<crate::scene::components::SpriteRenderer>(sprite)
        .unwrap()
        .sorting_layer = 10;

    spawn_emitter(
        &mut world,
        ParticleEmitter2D {
            emission_rate: 0.0,
            burst: 1,
            lifetime: [5.0, 5.0],
            sorting_layer: 20,
            ..ParticleEmitter2D::default()
        },
    );
    particle_emitter_2d_system(&mut world, 0.1);

    let items = crate::scene::systems::render::scene_2d_system(&world);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].texture, 1, "精灵（层 10）先画");
    assert_eq!(
        items[1].texture,
        crate::scene::systems::render::SOLID_TEXTURE
    );
}

#[test]
fn an_authored_component_json_reaches_the_particle_system() {
    // 与 `SceneLoader` 同一条路：registry.apply 就是 `.scene.json` / RSCN
    // 把组件写进 World 的入口。
    let mut registry = prism_scene::registry::ComponentRegistry::new();
    prism_scene::registry::register_builtin_components(&mut registry);
    let authored = serde_json::json!({
        "emission_rate": 50.0,
        "burst": 1,
        "lifetime": [2.0, 2.0],
        "start_size": [0.25, 0.25],
        "shape": "Circle",
        "shape_radius": 1.0,
        "sorting_layer": 5,
        "start_color": [0.9, 0.95, 1.0, 0.8],
        "end_color": [0.9, 0.95, 1.0, 0.0]
    });

    let mut world = World::new();
    let entity = world.spawn();
    world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
    registry.apply(
        &mut world,
        entity,
        "prism_engine::scene::ParticleEmitter2D",
        &authored,
    );

    let emitter_def = world
        .get::<ParticleEmitter2D>(entity)
        .expect("authored component must deserialize");
    assert_eq!(emitter_def.shape, ParticleShape2D::Circle);
    assert_eq!(emitter_def.sorting_layer, 5);
    assert!((emitter_def.shape_radius - 1.0).abs() < 1e-6);

    particle_emitter_2d_system(&mut world, 0.1);
    let items = crate::scene::systems::render::scene_2d_system(&world);
    // burst 1 + 0.1s × 50/s = 6 个。
    assert_eq!(items.len(), 6);
    assert!((items[0].model[0][0] - 0.25).abs() < 1e-6);
}
