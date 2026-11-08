//! 钓鱼状态机（M0 垂直切片）。
//!
//! 这是**用户项目**的玩法层：引擎只提供通用的 `LineRenderer` /
//! `SpriteRenderer` 与输入快照，鱼竿、浮标、咬钩时序全在这里。
//!
//! 状态流转：
//!
//! ```text
//! Idle ──点击──> Casting ──落水──> Waiting ──计时──> Biting
//!   ^                                                  │
//!   └────── Caught <──收线成功── Reeling <──点击────────┘
//!                                                      └──超时──> Waiting
//! ```
//!
//! 状态与读写都走 [`prism_game_api::GameWorld`]：与 `boat` / `world` 同构，
//! 静态 Player 与编辑器动态模块共用同一份玩法代码。

use prism_game_api::{keys, EntityHandle, GameWorld};
use prism_scene::components::{LineRenderer, LocalTransform, Name, SpriteRenderer, TimeOfDay};
use prism_scene::ui::Text;

/// 鱼竿尖端相对船锚点的偏移（船体局部像素）。
const ROD_OFFSET: [f32; 2] = [44.0, 24.0];
/// 抛竿落点相对船的水平距离。
const CAST_DISTANCE: f32 = 150.0;
/// 浮标飞行时间。
const CAST_SECONDS: f32 = 0.55;
/// 咬钩提示持续时间（超过就脱钩）。
const BITE_WINDOW: f32 = 1.1;
/// 收线动画时间。
const REEL_SECONDS: f32 = 0.9;
/// 水面高度（世界 y）。
const WATER_LINE: f32 = 80.0;
/// 鱼线排序层。
const LINE_LAYER: i32 = 45;

/// 钓鱼阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Casting,
    Waiting,
    Biting,
    Reeling,
    Caught,
}

impl Default for Phase {
    fn default() -> Self {
        Self::Idle
    }
}

/// 钓鱼状态——**项目私有**，由调用方持有（Player 侧放在 ECS resource 里）。
#[derive(Default)]
pub struct FishingState {
    phase: Phase,
    /// 当前阶段已经过的时间（秒）。
    elapsed: f32,
    /// 本阶段的目标时长；`0` 表示按逻辑自行判定。
    duration: f32,
    /// 浮标当前世界位置。
    bobber_pos: [f32; 2],
    /// 抛竿起点（船锚点）。
    cast_origin: [f32; 2],
    /// 本次等待的咬钩时间（伪随机）。
    bite_at: f32,
    /// Bobber position when the hook was set; the reel interpolates from here.
    reel_origin: [f32; 2],
    /// 已钓到的鱼数（M0 只做计数）。
    caught: u32,
    /// HUD 顶栏的文本实体（`title`），每帧写入鱼数与时间。
    hud: Option<EntityHandle>,
    boat: Option<EntityHandle>,
    bobber: Option<EntityHandle>,
    line: Option<EntityHandle>,
}

impl FishingState {
    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn caught(&self) -> u32 {
        self.caught
    }
}

/// HUD 顶栏文本实体（场景里叫 `title`，由工程自己的场景摆放）。
fn find_hud_label(world: &mut GameWorld<'_>) -> Option<EntityHandle> {
    for entity in world.query::<Name>() {
        let Some(name) = world.get::<Name>(entity) else {
            continue;
        };
        if name.0 == "title" {
            return Some(entity);
        }
    }
    None
}

/// 每帧推进钓鱼状态机。
pub fn tick(state: &mut Option<FishingState>, world: &mut GameWorld<'_>, dt: f32) {
    if state.is_none() {
        let mut fresh = FishingState::default();
        for entity in world.query::<Name>() {
            let Some(name) = world.get::<Name>(entity) else {
                continue;
            };
            match name.0.as_str() {
                "boat" => fresh.boat = Some(entity),
                "bobber" => fresh.bobber = Some(entity),
                _ => {}
            }
        }
        // #984：进度从玩家存档里恢复（键就是 `caught`，工程自己定的 schema）。
        fresh.caught = world
            .load_data("caught")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        if fresh.caught > 0 {
            log::info!(
                "fishing: restored {} caught fish from the save",
                fresh.caught
            );
        }
        fresh.hud = find_hud_label(world);
        fresh.line = spawn_line(world);
        *state = Some(fresh);
    }

    let Some(current) = state.as_mut() else {
        return;
    };

    // --- 输入（宿主每帧下发的输入快照） ---
    let input = world.input();
    let clicked = input.left_clicked;
    // Cast and hook-set are edge-triggered: holding the key counts once, like `left_clicked`.
    let pressed_space = input.key_pressed(keys::SPACE);
    // `FISHING_AUTOPLAY=1` 让状态机自己推进：用于演示与自动化截图，
    // 正常游玩时不设这个变量，完全由输入驱动。
    let autoplay = std::env::var_os("FISHING_AUTOPLAY").is_some();
    let auto_fire =
        autoplay && matches!(current.phase, Phase::Idle | Phase::Biting) && current.elapsed > 1.0;
    let action = clicked || pressed_space || auto_fire;

    // --- 船锚点 ---
    let boat_pos = current
        .boat
        .and_then(|boat| world.get::<LocalTransform>(boat))
        .map(|transform| [transform.position.x, transform.position.y])
        .unwrap_or([0.0, WATER_LINE]);

    current.elapsed += dt;

    match current.phase {
        Phase::Idle => {
            if action {
                current.phase = Phase::Casting;
                current.elapsed = 0.0;
                current.duration = CAST_SECONDS;
                current.cast_origin = [boat_pos[0] + ROD_OFFSET[0], boat_pos[1] + ROD_OFFSET[1]];
                current.bobber_pos = current.cast_origin;
                log::info!("fishing: cast");
            }
        }
        Phase::Casting => {
            let t = (current.elapsed / CAST_SECONDS).clamp(0.0, 1.0);
            let target = [boat_pos[0] + CAST_DISTANCE, WATER_LINE];
            // 抛物线：水平线性插值，垂直方向加一个拱形。
            let arc = (t * std::f32::consts::PI).sin() * 60.0;
            current.bobber_pos = [
                current.cast_origin[0] + (target[0] - current.cast_origin[0]) * t,
                current.cast_origin[1] + (target[1] - current.cast_origin[1]) * t + arc,
            ];
            if t >= 1.0 {
                current.phase = Phase::Waiting;
                current.elapsed = 0.0;
                current.bite_at = 1.4 + pseudo_random(current.caught) * 2.2;
                log::info!("fishing: bobber landed, waiting {:.2}s", current.bite_at);
            }
        }
        Phase::Waiting => {
            current.bobber_pos = [
                boat_pos[0] + CAST_DISTANCE,
                WATER_LINE + (current.elapsed * 2.2).sin() * 1.5,
            ];
            if current.elapsed >= current.bite_at {
                current.phase = Phase::Biting;
                current.elapsed = 0.0;
                log::info!("fishing: bite!");
            }
        }
        Phase::Biting => {
            // 咬钩时浮标下沉抖动。
            current.bobber_pos = [
                boat_pos[0] + CAST_DISTANCE,
                WATER_LINE - 4.0 + (current.elapsed * 18.0).sin() * 2.5,
            ];
            if action {
                current.phase = Phase::Reeling;
                current.reel_origin = current.bobber_pos;
                current.elapsed = 0.0;
                log::info!("fishing: hook set");
            } else if current.elapsed >= BITE_WINDOW {
                current.phase = Phase::Waiting;
                current.elapsed = 0.0;
                current.bite_at = 1.2 + pseudo_random(current.caught + 7) * 2.0;
                log::info!("fishing: missed the bite");
            }
        }
        Phase::Reeling => {
            let t = (current.elapsed / REEL_SECONDS).clamp(0.0, 1.0);
            let target = [boat_pos[0] + ROD_OFFSET[0], boat_pos[1] + ROD_OFFSET[1]];
            // Pure function of `t` (smoothstep), so a given elapsed time yields the
            // same point whatever the frame rate is (see #1059).
            let ease = t * t * (3.0 - 2.0 * t);
            current.bobber_pos = [
                current.reel_origin[0] + (target[0] - current.reel_origin[0]) * ease,
                current.reel_origin[1] + (target[1] - current.reel_origin[1]) * ease,
            ];
            if t >= 1.0 {
                current.caught += 1;
                current.phase = Phase::Caught;
                current.elapsed = 0.0;
                log::info!("fishing: landed fish #{}", current.caught);
                // 上钩的水花声（#987）：玩家和编辑器 Play 都能听到。
                match world.play_sound("audio/splash.wav", 0.8, false) {
                    Some(id) => log::info!("fishing: splash sound id {id}"),
                    None => log::warn!("fishing: splash sound unavailable"),
                }
                // #984：落袋即存档，退出游戏不会丢进度。
                let saved = world.save_data("caught", &current.caught.to_string());
                log::info!("fishing: landed fish #{} (saved: {saved})", current.caught);
            }
        }
        Phase::Caught => {
            if current.elapsed >= 0.8 {
                current.phase = Phase::Idle;
                current.elapsed = 0.0;
                current.bobber_pos = [boat_pos[0] + ROD_OFFSET[0], boat_pos[1] + ROD_OFFSET[1]];
            }
        }
    }

    // --- HUD：鱼数 + 当前时刻 -------------------------------------------------
    //
    // 时间来自引擎的 `TimeOfDay`（#983），工程不再自己维护计时器。
    if let Some(hud) = current.hud {
        if let Some(mut text) = world.get::<Text>(hud) {
            let clock = world
                .query::<TimeOfDay>()
                .into_iter()
                .find_map(|entity| world.get::<TimeOfDay>(entity))
                .map(|clock| clock.clock_text())
                .unwrap_or_else(|| "--:--".to_string());
            let content = format!("CATCH LOG   x{}   {}", current.caught, clock);
            if text.content != content {
                text.content = content;
                world.set(hud, text);
            }
        }
    }

    // --- 写回浮标位置与表现 ---
    if let Some(bobber) = current.bobber {
        if let Some(mut transform) = world.get::<LocalTransform>(bobber) {
            transform.position = glam::Vec3::new(current.bobber_pos[0], current.bobber_pos[1], 0.0);
            world.set(bobber, transform);
        }
        if let Some(mut sprite) = world.get::<SpriteRenderer>(bobber) {
            // 咬钩时闪一下暖色，让玩家看清窗口。
            sprite.color = if current.phase == Phase::Biting {
                [1.0, 0.55, 0.35, 1.0]
            } else {
                [1.0, 1.0, 1.0, 1.0]
            };
            sprite.sorting_layer = if current.phase == Phase::Idle {
                -100
            } else {
                60
            };
            world.set(bobber, sprite);
        }
    }

    // --- 鱼线：竿尖 → 浮标（中间一点下垂） ---
    if let Some(line) = current.line {
        if let Some(mut renderer) = world.get::<LineRenderer>(line) {
            let tip = [boat_pos[0] + ROD_OFFSET[0], boat_pos[1] + ROD_OFFSET[1]];
            let visible = current.phase != Phase::Idle && current.phase != Phase::Caught;
            renderer.points = if visible {
                let mid = [
                    (tip[0] + current.bobber_pos[0]) * 0.5,
                    (tip[1] + current.bobber_pos[1]) * 0.5 - 6.0,
                    0.0,
                ];
                vec![
                    [tip[0], tip[1], 0.0],
                    mid,
                    [current.bobber_pos[0], current.bobber_pos[1], 0.0],
                ]
            } else {
                Vec::new()
            };
            world.set(line, renderer);
        }
    }
}

fn spawn_line(world: &mut GameWorld<'_>) -> Option<EntityHandle> {
    let entity = world.spawn();
    world.set(entity, Name("fishing-line".to_string()));
    world.set(entity, LocalTransform::default());
    world.set(
        entity,
        LineRenderer {
            points: Vec::new(),
            color: [0.92, 0.95, 1.0, 0.85],
            width: 0.6,
            sorting_layer: LINE_LAYER,
            order_in_layer: 0,
            ..LineRenderer::default()
        },
    );
    Some(entity)
}

/// 稳定的伪随机（0..1），避免为一个演示引入 rand 依赖。
fn pseudo_random(seed: u32) -> f32 {
    let mut x = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    x ^= x >> 15;
    x = x.wrapping_mul(2_246_822_519);
    x ^= x >> 13;
    (x & 0xFFFF) as f32 / 65535.0
}
