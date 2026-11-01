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
//! - 剪辑事件（#490）：播放头穿过 `AnimationEvent.time` 时写进
//!   [`AnimationEventQueue`]；队列每帧重写，玩法系统在同一个调度里读取。
//!   只处理正向播放；循环剪辑每圈各触发一次。
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
/// 帧数优先级：`SpriteAnimation.frames`（>0）→ 本行列数 − `frame_start`
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
        let (total, columns) = {
            let Some(sprite) = world.get::<SpriteRenderer>(entity) else {
                continue;
            };
            let Some(sprite_ref) = world.get::<SpriteRef>(entity) else {
                continue;
            };
            (
                animation_frame_count(&animation, sprite, sprite_ref.size),
                sprite.sheet_columns_or(sprite_ref.size),
            )
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
            raw.min((total - 1) as f32) as u32
        };

        world.insert(entity, state);
        if let Some(sprite) = world.get_mut::<SpriteRenderer>(entity) {
            // 行内绝对帧号；夹在本行最后一列，避免动画越界滚到下一行。
            sprite.frame_index = animation
                .frame_start
                .saturating_add(index)
                .min(columns.saturating_sub(1));
        }
    }
}

/// 本次动画播放的帧数（至少 1）。
///
/// `frames > 0` 时是作者指定长度；否则从 `frame_start` 播到本行末尾（#584）。
fn animation_frame_count(
    animation: &SpriteAnimation,
    sprite: &SpriteRenderer,
    texture: [u32; 2],
) -> u32 {
    let available = sprite
        .sheet_columns_or(texture)
        .saturating_sub(animation.frame_start)
        .max(1);
    if animation.frames > 0 {
        return animation.frames.min(available);
    }
    available
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

/// 本帧触发的一条动画事件（#490）。
#[derive(Debug, Clone, PartialEq)]
pub struct FiredAnimationEvent {
    /// 播放这个剪辑的实体。
    pub entity: Entity,
    /// 作者写的事件名。
    pub name: String,
    /// 事件在剪辑里的时间（秒）。
    pub time: f32,
}

/// 动画事件队列：`keyframe_animation_system` **每帧重写**。
///
/// 玩法系统在 `Engine::update` 的调度里读这个资源（它在 `hierarchy_system`
/// 之前运行）。没有事件 / 没有动画实体时是空的，不会保留上一帧的旧事件。
#[derive(Debug, Clone, Default)]
pub struct AnimationEventQueue {
    pub events: Vec<FiredAnimationEvent>,
}

impl AnimationEventQueue {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FiredAnimationEvent> {
        self.events.iter()
    }
}

/// 把原始播放时间折算成 `[0, duration]` 内的剪辑本地时间。
fn local_time_at(raw: f32, duration: f32, looped: bool) -> f32 {
    if duration <= 0.0 || !duration.is_finite() || !raw.is_finite() {
        0.0
    } else if looped {
        raw.rem_euclid(duration)
    } else {
        raw.clamp(0.0, duration)
    }
}

/// 收集 `(from, to]` 内的事件；`include_from` 时把 `from` 本身也算上。
fn collect_events(
    clip: &KeyframeAnimation,
    from: f32,
    to: f32,
    include_from: bool,
    out: &mut Vec<(String, f32)>,
) {
    if !from.is_finite() || !to.is_finite() || to < from {
        return;
    }
    for event in &clip.events {
        if event.name.is_empty() || !event.time.is_finite() {
            continue;
        }
        let in_range = if include_from {
            event.time >= from && event.time <= to
        } else {
            event.time > from && event.time <= to
        };
        if in_range {
            out.push((event.name.clone(), event.time));
        }
    }
}

/// 每帧推进所有 [`KeyframeAnimation`]，把采样结果写进实体的 `LocalTransform`。
///
/// 空剪辑（三条轨道都没关键帧）不挂播放头，避免给纯静态实体留垃圾组件。
pub fn keyframe_animation_system(world: &mut World, dt: f32) {
    let entities: Vec<Entity> = world
        .query::<KeyframeAnimation>()
        .map(|(entity, _)| entity)
        .collect();

    let mut fired: Vec<FiredAnimationEvent> = Vec::new();

    for entity in entities {
        // 采样借用块：算完就把 `&World` 的借用放掉，后面才能写组件。
        let computed = world.get::<KeyframeAnimation>(entity).and_then(|clip| {
            if clip.is_empty() {
                return None;
            }
            let previous_head = world
                .get::<ClipPlayhead>(entity)
                .copied()
                .unwrap_or_default();
            let mut head = previous_head;
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
            let local = local_time_at(raw, duration, clip.looped);

            // #490：只处理正向播放；倒放 / 暂停不发射事件。
            let mut events = Vec::new();
            if clip.playing && speed >= 0.0 && duration > 0.0 && dt.is_finite() {
                let previous_raw = previous_head.time + clip.phase;
                let previous_local = local_time_at(previous_raw, duration, clip.looped);
                let cycles = (raw / duration).floor() - (previous_raw / duration).floor();
                if clip.looped && cycles >= 1.0 {
                    // 跨过了一次（或多次）循环：先扫到本圈末尾，再从 0 扫到新的播放头。
                    collect_events(clip, previous_local, duration, false, &mut events);
                    collect_events(clip, 0.0, local, true, &mut events);
                } else {
                    // 刚起步时把 0 秒事件也算进来（否则第一个事件永远差一个 dt）。
                    let include_start =
                        previous_head.time == 0.0 && previous_raw <= 0.0 && clip.phase == 0.0;
                    collect_events(clip, previous_local, local, include_start, &mut events);
                }
            }

            Some((head, clip.sample(local), events))
        });

        let Some((head, sample, events)) = computed else {
            continue;
        };
        for event in events {
            fired.push(FiredAnimationEvent {
                entity,
                name: event.0,
                time: event.1,
            });
        }
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

    // 每帧重写：没有动画实体时也会清空旧事件。
    world.insert_resource(AnimationEventQueue { events: fired });
}

#[cfg(test)]
#[path = "animation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "keyframe_animation_tests.rs"]
mod keyframe_tests;
