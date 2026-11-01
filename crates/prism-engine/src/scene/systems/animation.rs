//! 关键帧动画：播放头与写回 `Transform`。
//!
//! 数据侧（轨道 / 采样 / 端点保持）在 [`prism_scene::animation`]，组件是
//! [`KeyframeAnimation`]。本文件只负责「这一帧播放到哪」：
//!
//! - 播放头是运行时组件 [`ClipPlayhead`]，**不进** `.scene.json`：同一份场景
//!   每次加载都从头播，序列化状态里也不会夹带上一次运行的进度。
//! - 采样只写**有关键帧**的通道；没关键帧的通道保留作者写进 `transform` 的
//!   值（只动画 position 不会把旋转 / 缩放清零）。
//! - 系统跑在 `Engine::update` 的调度里、`hierarchy_system` 之前，所以同一帧
//!   里 `LocalToWorld` / 渲染提取 / 相机跟随看到的都是动画后的姿势。
//!
//! 注意：`save_scene_state` 读的是 `LocalToWorld`，播到一半存档会存下**当前
//! 姿势**而不是作者姿势。这与其他每帧改写 `LocalTransform` 的系统一致。

use prism_ecs::{Entity, World};

use crate::scene::components::{
    KeyframeAnimation, LocalTransform, SpriteAnimation, SpriteRef, SpriteRenderer,
};

/// 单个精灵的播放头（秒）。运行时组件，**不写进** `.scene.json`。
///
/// 1. 每个实体有自己的起点，不再用「进程启动以来的全局时间」；
/// 2. `playing` 从 `false` 变 `true` 时重置为 0，所以一次性动画可以重播；
/// 3. `was_playing` 只用来检测这次 false→true 的边沿，暂停期间保留当前帧。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SpriteAnimationState {
    /// 已播放的时间（不含 `phase`）。
    pub time: f32,
    /// 上一帧的 `SpriteAnimation.playing`，用于检测重播边沿。
    pub was_playing: bool,
    /// 是否已经初始化过：首次观察到实体时不要误判成「重播」。
    pub initialized: bool,
}

/// 每帧推进所有精灵动画。
///
/// 帧数优先级：`SpriteAnimation.frames`（>0）→ 纹理宽度 / 单帧宽度
/// → 1（无法确定时静止在第一帧）。
pub fn sprite_animation_system(world: &mut World, dt: f32) {
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
    let entities: Vec<prism_ecs::Entity> = world
        .query3::<SpriteRenderer, SpriteAnimation, SpriteRef>()
        .filter(|(_, _, animation, _)| animation.fps > 0.0)
        .map(|(entity, _, _, _)| entity)
        .collect();

    for entity in entities {
        let Some(animation) = world.get::<SpriteAnimation>(entity).cloned() else {
            continue;
        };
        let total = {
            let Some(sprite) = world.get::<SpriteRenderer>(entity) else {
                continue;
            };
            let Some(sprite_ref) = world.get::<SpriteRef>(entity) else {
                continue;
            };
            total_frames(&animation, sprite, sprite_ref.size)
        };
        if total <= 1 {
            continue;
        }

        let mut state = world
            .remove::<SpriteAnimationState>(entity)
            .unwrap_or_default();
        if !state.initialized {
            state.initialized = true;
            state.was_playing = animation.playing;
        }
        if animation.playing {
            // 一次性动画 / 手动暂停后重新播放：从第 0 帧开始。
            if !state.was_playing {
                state.time = 0.0;
            }
            state.time += dt;
            state.was_playing = true;
        } else {
            // 暂停时保留播放头；下一次 playing=true 会重置。
            state.was_playing = false;
        }

        let duration = total as f32 / animation.fps;
        let raw_time = if animation.looped {
            state.time
        } else {
            state.time.min(duration)
        };
        let raw = (raw_time + animation.phase).max(0.0) * animation.fps;
        let index = if animation.looped {
            raw as u32 % total
        } else {
            raw.min(total - 1) as u32
        };

        world.insert(entity, state);
        if let Some(sprite) = world.get_mut::<SpriteRenderer>(entity) {
            sprite.frame_index = index;
        }
    }
}

/// 动画总帧数（至少 1）。
fn total_frames(animation: &SpriteAnimation, sprite: &SpriteRenderer, texture: [u32; 2]) -> u32 {
    if animation.frames > 0 {
        return animation.frames;
    }
    let frame_width = if sprite.frame_size[0] > 0 {
        sprite.frame_size[0]
    } else {
        texture[0]
    };
    if frame_width == 0 {
        return 1;
    }
    (texture[0] / frame_width).max(1)
}

/// 一个实体的关键帧播放头（秒）。运行时组件，不写进场景。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ClipPlayhead {
    /// 已经播放的时间（不含 [`KeyframeAnimation::phase`]）。
    pub time: f32,
    /// 单次播放（`looped == false`）是否已播到末尾。
    ///
    /// 每帧重算，不是粘滞标志：把 `duration` 调长或多播一轮后会自然变回
    /// `false`，不需要额外的 reset。
    pub finished: bool,
}

/// 每帧推进所有 [`KeyframeAnimation`]，把采样结果写进实体的 `LocalTransform`。
///
/// 空剪辑（三条轨道都没关键帧）不挂播放头，避免给纯静态实体留垃圾组件。
pub fn keyframe_animation_system(world: &mut World, dt: f32) {
    let entities: Vec<Entity> = world
        .query::<KeyframeAnimation>()
        .map(|(entity, _)| entity)
        .collect();

    for entity in entities {
        // 采样借用块：算完就把 `&World` 的借用放掉，后面才能写组件。
        let computed = world.get::<KeyframeAnimation>(entity).and_then(|clip| {
            if clip.is_empty() {
                return None;
            }
            let mut head = world
                .get::<ClipPlayhead>(entity)
                .copied()
                .unwrap_or_default();
            let duration = clip.clip_duration();
            let speed = if clip.speed.is_finite() {
                clip.speed
            } else {
                1.0
            };
            if clip.playing && dt.is_finite() {
                head.time += dt * speed;
                // 单次播放停在末尾：播放头不再越过 `duration - phase`，
                // 否则把 `duration` 调长后会从中间莫名其妙地继续。
                let last = duration - clip.phase;
                if !clip.looped && last > 0.0 {
                    head.time = head.time.min(last);
                }
            }
            let raw = head.time + clip.phase;
            head.finished = duration > 0.0 && !clip.looped && raw >= duration;
            let local = if duration <= 0.0 || !duration.is_finite() {
                0.0
            } else if clip.looped {
                raw.rem_euclid(duration)
            } else {
                raw.clamp(0.0, duration)
            };
            Some((head, clip.sample(local)))
        });

        let Some((head, sample)) = computed else {
            continue;
        };
        let Some(transform) = world.get_mut::<LocalTransform>(entity) else {
            // 没有 `LocalTransform` 的实体（理论上不该存在）不挂播放头。
            continue;
        };
        if let Some(position) = sample.position {
            transform.position = glam::Vec3::from_array(position);
        }
        if let Some(euler) = sample.euler_xyz {
            let [x, y, z] = euler.map(f32::to_radians);
            transform.rotation = glam::Quat::from_euler(glam::EulerRot::XYZ, x, y, z);
        }
        if let Some(scale) = sample.scale {
            transform.scale = glam::Vec3::from_array(scale);
        }
        world.insert(entity, head);
    }
}

#[cfg(test)]
#[path = "animation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "keyframe_animation_tests.rs"]
mod keyframe_tests;
