//! 昼夜系统（#983）：推进 [`TimeOfDay`]，把结果写进同一实体上的环境光。
//!
//! 一个工程只需要放一个实体：
//!
//! ```json
//! "prism_engine::scene::TimeOfDay": { "hours": 8.0, "day_length_seconds": 600.0 },
//! "prism_engine::scene::Light2D": { "kind": "Ambient", "color": [1, 1, 1], "intensity": 1 }
//! ```
//!
//! 系统每帧做两件事，顺序固定：
//!
//! 1. 按 `dt` 推进 `hours`（`paused` / `day_length_seconds <= 0` 时不动）；
//! 2. 用 [`TimeOfDay::ambient`] 得到的颜色覆写**同实体**上是
//!    `Light2DKind::Ambient` 的 [`Light2D`]。
//!
//! 它只碰环境光，点光（灯笼、篝火）保持作者写的强度 —— 天黑后点光自然突出。
//! 同一帧里 `sprite_light_entries` 读到的是推进后的值，所以画面没有一帧延迟。

use crate::ecs::World;
use prism_scene::components::{Light2D, Light2DKind, TimeOfDay};

/// 推进昼夜并刷新环境光。
pub fn time_of_day_system(world: &mut World, dt: f32) {
    // 先收集，避免在借用 `TimeOfDay` 的同时再借用 `Light2D`。
    let mut ambient: Vec<(crate::ecs::Entity, [f32; 3], f32)> = Vec::new();
    for (entity, clock) in world.query_mut::<TimeOfDay>() {
        clock.advance(dt);
        let (color, intensity) = clock.ambient();
        ambient.push((entity, color, intensity));
    }

    for (entity, color, intensity) in ambient {
        let Some(light) = world.get_mut::<Light2D>(entity) else {
            continue;
        };
        // 点光由作者控制，只有环境光跟着时间走。
        if light.kind != Light2DKind::Ambient {
            continue;
        }
        light.color = color;
        light.intensity = intensity;
        light.enabled = true;
    }
}

#[cfg(test)]
#[path = "time_of_day_tests.rs"]
mod time_of_day_tests;
