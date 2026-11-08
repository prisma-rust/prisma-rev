//! 玩法回归测试（host 后端，不需要 Vulkan / 窗口）。
//!
//! 这些测试跑的是**和 Play 里同一份** `game::*::tick` 代码，只是把宿主从
//! 「编辑器进程」换成普通的 `prism_ecs::World`：`GameWorld::host` 直接借用世界，
//! 所以断言到的组件状态就是 Play 时模块通过 ABI 写进去的状态。
//!
//! 运行（工程目录内）：`cargo test`
//!
//! `world::tick` 会读 `assets/lake_layers.json`，靠 `prism_engine::asset_paths`
//! 从 cwd 往上找 `assets/`，所以必须在 `projects/fishing` 下跑。

use prism_game_api::{keys, EntityHandle, GameWorld, InputSnapshot};
use prism_scene::components::{Camera, LineRenderer, LocalTransform, Name, SpriteRenderer};

/// 建一个最小世界：船 + 浮标 + 相机（与 `lake.scene.json` 的实体同名）。
fn scene_world() -> prism_ecs::World {
    let mut world = prism_ecs::World::new();
    let camera = world.spawn();
    world.insert(camera, Camera::default());

    let boat = world.spawn();
    world.insert(boat, Name("boat".to_string()));
    world.insert(boat, LocalTransform::default());

    let bobber = world.spawn();
    world.insert(bobber, Name("bobber".to_string()));
    let mut transform = LocalTransform::default();
    transform.position.x = 44.0;
    transform.position.y = 24.0;
    world.insert(bobber, transform);
    world.insert(bobber, SpriteRenderer::default());

    world
}

fn click() -> InputSnapshot {
    InputSnapshot {
        left_clicked: true,
        ..InputSnapshot::default()
    }
}

/// 8 个图层全部 spawn，并且每个都带变换和贴图路径。
///
/// 这条断言盯的是 `prism-runtime-host` 曾经丢掉 `LocalTransform` 的那个坑：
/// 图层建出来了但没变换，屏幕上就是空的。
#[test]
fn lake_world_spawns_every_layer_with_transform() {
    let mut world = scene_world();
    let mut api = GameWorld::host(&mut world);
    let mut state = None;
    fishing::game::world::tick(&mut state, &mut api, 0.016);

    let lake = state.expect("lake world should be created on the first tick");
    assert_eq!(
        lake.layer_count(),
        8,
        "expected the 8 layers from assets/lake_layers.json (cwd must be projects/fishing)"
    );

    let layers: Vec<EntityHandle> = api.query::<SpriteRenderer>();
    let background: Vec<EntityHandle> = layers
        .into_iter()
        .filter(|entity| {
            api.get::<SpriteRenderer>(*entity)
                .map(|sprite| sprite.sprite_path.starts_with("sprites/bg/"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        background.len() >= 8,
        "every background layer needs a SpriteRenderer, got {}",
        background.len()
    );
    for entity in background {
        assert!(
            api.get::<LocalTransform>(entity).is_some(),
            "layer entity without LocalTransform never renders"
        );
    }
}

/// 抛竿 → 落水 → 咬钩 → 收线 → 钓到一条鱼。
#[test]
fn fishing_state_machine_runs_cast_to_catch() {
    use fishing::game::fishing::Phase;

    let mut world = scene_world();
    let mut api = GameWorld::host(&mut world);
    let mut state = None;

    // 第一次 tick 会找 boat/bobber 并建鱼线。
    api.set_input(InputSnapshot::default());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    let state_ref = state.as_ref().expect("state created");
    assert_eq!(state_ref.phase(), Phase::Idle);
    assert_eq!(state_ref.caught(), 0);

    // 点击 = 抛竿。
    api.set_input(click());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Casting);

    // 浮标落水（CAST_SECONDS = 0.55）。
    api.set_input(InputSnapshot::default());
    fishing::game::fishing::tick(&mut state, &mut api, 0.6);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Waiting);

    // 等咬钩（`bite_at` 在 1.4~3.6s 之间）。
    let mut phase = Phase::Waiting;
    for _ in 0..300 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        phase = state.as_ref().unwrap().phase();
        if phase == Phase::Biting {
            break;
        }
    }
    assert_eq!(phase, Phase::Biting, "bite should arrive within ~5s");

    // 咬钩窗口内再点一次 = 扬竿。
    api.set_input(click());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Reeling);

    // 收线（REEL_SECONDS = 0.9）。
    api.set_input(InputSnapshot::default());
    for _ in 0..120 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        if state.as_ref().unwrap().phase() == Phase::Caught {
            break;
        }
    }
    let state_ref = state.as_ref().unwrap();
    assert_eq!(state_ref.phase(), Phase::Caught);
    assert_eq!(state_ref.caught(), 1);
}

/// Holding Space counts as one input: cast and hook-set are press edges, not held state.
#[test]
fn holding_space_does_not_repeat_the_cast_or_set_the_hook() {
    use fishing::game::fishing::Phase;

    /// Space held down, without a fresh press edge this frame.
    fn space_held() -> InputSnapshot {
        let mut snapshot = InputSnapshot::default();
        snapshot.set_key(keys::SPACE, true);
        snapshot
    }

    /// Space held down **and** pressed this frame.
    fn space_pressed() -> InputSnapshot {
        let mut snapshot = space_held();
        snapshot.set_key_edge(keys::SPACE, true);
        snapshot
    }

    let mut world = scene_world();
    let mut api = GameWorld::host(&mut world);
    let mut state = None;
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);

    // Held without a new press edge: no cast.
    api.set_input(space_held());
    for _ in 0..10 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    }
    assert_eq!(
        state.as_ref().unwrap().phase(),
        Phase::Idle,
        "a held Space must not start a cast"
    );

    // A real press edge this frame: cast once.
    api.set_input(space_pressed());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Casting);

    // Keep holding (no new edge): the bobber lands and waits for a bite.
    api.set_input(space_held());
    fishing::game::fishing::tick(&mut state, &mut api, 0.6);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Waiting);

    let mut phase = Phase::Waiting;
    for _ in 0..300 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        phase = state.as_ref().unwrap().phase();
        if phase == Phase::Biting {
            break;
        }
    }
    assert_eq!(phase, Phase::Biting, "bite should arrive within ~5s");

    // Still only held during the bite window: the bite is missed instead of auto-hooked.
    for _ in 0..200 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        phase = state.as_ref().unwrap().phase();
        assert_ne!(phase, Phase::Reeling, "a held Space must not set the hook");
        if phase == Phase::Waiting {
            break;
        }
    }
    assert_eq!(
        phase,
        Phase::Waiting,
        "missed bite should return to Waiting"
    );

    // Wait for the next bite and hook it with a real press edge.
    for _ in 0..300 {
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        if state.as_ref().unwrap().phase() == Phase::Biting {
            break;
        }
    }
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Biting);
    api.set_input(space_pressed());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Reeling);
}

/// 鱼线在非 Idle/Caught 阶段可见，且终点跟着浮标走。
#[test]
fn rod_line_follows_the_bobber() {
    use fishing::game::fishing::Phase;

    let mut world = scene_world();
    let mut api = GameWorld::host(&mut world);
    let mut state = None;
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);

    let line = api
        .query::<LineRenderer>()
        .first()
        .copied()
        .expect("fishing spawns its own LineRenderer");
    let idle_points = api.get::<LineRenderer>(line).unwrap().points.len();
    assert!(
        idle_points <= 1,
        "idle line should be hidden, got {idle_points} points"
    );

    api.set_input(click());
    fishing::game::fishing::tick(&mut state, &mut api, 0.016);
    assert_eq!(state.as_ref().unwrap().phase(), Phase::Casting);
    fishing::game::fishing::tick(&mut state, &mut api, 0.6);

    let points = api.get::<LineRenderer>(line).unwrap().points;
    assert!(
        points.len() >= 3,
        "an active cast draws a polyline, got {} points",
        points.len()
    );
    assert!(
        state.as_ref().unwrap().phase() == Phase::Waiting,
        "cast should have landed"
    );
}

/// The reel reaches the same point at the same elapsed time whatever the frame rate is.
///
/// Drives the state machine up to `Reeling` with a shared step sequence, then
/// advances half of `REEL_SECONDS` at 30 FPS and at 60 FPS and compares the
/// bobber the engine ends up rendering.
#[test]
fn reel_position_does_not_depend_on_the_frame_rate() {
    use fishing::game::fishing::Phase;

    fn bobber_after_reeling(dt: f32, steps: usize) -> [f32; 2] {
        let mut world = scene_world();
        let mut api = GameWorld::host(&mut world);
        let mut state = None;

        // Identical lead-in for both frame rates: same ticks, same inputs.
        api.set_input(InputSnapshot::default());
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        api.set_input(click());
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        api.set_input(InputSnapshot::default());
        fishing::game::fishing::tick(&mut state, &mut api, 0.6);
        for _ in 0..300 {
            fishing::game::fishing::tick(&mut state, &mut api, 0.016);
            if state.as_ref().unwrap().phase() == Phase::Biting {
                break;
            }
        }
        assert_eq!(state.as_ref().unwrap().phase(), Phase::Biting);

        // Hook set, then reel for `steps` frames.
        api.set_input(click());
        fishing::game::fishing::tick(&mut state, &mut api, 0.016);
        assert_eq!(state.as_ref().unwrap().phase(), Phase::Reeling);
        api.set_input(InputSnapshot::default());
        for _ in 0..steps {
            fishing::game::fishing::tick(&mut state, &mut api, dt);
        }

        let bobber = api
            .query::<Name>()
            .into_iter()
            .find(|entity| {
                api.get::<Name>(*entity)
                    .map(|name| name.0 == "bobber")
                    .unwrap_or(false)
            })
            .expect("scene fixture has a bobber");
        let position = api.get::<LocalTransform>(bobber).unwrap().position;
        [position.x, position.y]
    }

    // 0.5 s of reeling: 15 frames at 30 FPS, 30 frames at 60 FPS.
    let slow = bobber_after_reeling(1.0 / 30.0, 15);
    let fast = bobber_after_reeling(1.0 / 60.0, 30);

    assert!(
        (slow[0] - fast[0]).abs() < 0.5 && (slow[1] - fast[1]).abs() < 0.5,
        "reel position depends on the frame rate: 30 FPS {slow:?} vs 60 FPS {fast:?}"
    );
}
