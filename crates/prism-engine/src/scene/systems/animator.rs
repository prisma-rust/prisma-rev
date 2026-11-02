//! 骨骼动画求值（#560 P0-2）。
//!
//! 数据流：`SkeletonRoot.skeleton_path` + `Animator.clip_path` → 已解码的
//! [`prism_anim::Skeleton`] / [`prism_anim::Clip`]（放在 [`SkeletonLibrary`] 资源里）
//! → 采样成 `Pose` → `bone_palette` → [`BonePalette`]。
//!
//! 资源从 `.pak` 里读出来的那一步在 `asset_resolver::load_animation_assets`
//! （需要 `&mut World` 与 `ResourceManager`，所以不在调度系统里做）。
//!
//! 这一层仍然**不渲染**：GPU 蒙皮在 P0-3，届时 `BonePalette::matrices` 直接上传。

use std::collections::HashMap;
use std::sync::Arc;

use prism_anim::{palette_to_arrays, BlendSample, BlendSpace1D, Clip, Pose, Skeleton};
use prism_ecs::{Entity, World};
use prism_scene::components::{Animator, SkeletonRoot};

/// 已解码的骨架与剪辑，按资产路径索引（与 `MeshBounds` 一样是 World 资源）。
#[derive(Debug, Default)]
pub struct SkeletonLibrary {
    skeletons: HashMap<String, Arc<Skeleton>>,
    clips: HashMap<String, Arc<Clip>>,
}

impl SkeletonLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// 存入骨架；返回 `true` 表示这是新路径。
    pub fn insert_skeleton(&mut self, path: impl Into<String>, skeleton: Skeleton) -> bool {
        self.skeletons
            .insert(path.into(), Arc::new(skeleton))
            .is_none()
    }

    pub fn insert_clip(&mut self, path: impl Into<String>, clip: Clip) -> bool {
        self.clips.insert(path.into(), Arc::new(clip)).is_none()
    }

    pub fn skeleton(&self, path: &str) -> Option<Arc<Skeleton>> {
        self.skeletons.get(path).cloned()
    }

    pub fn clip(&self, path: &str) -> Option<Arc<Clip>> {
        self.clips.get(path).cloned()
    }

    /// 剪辑里出现的关键帧时间点（升序去重）；没加载过的路径返回空。
    pub fn clip_key_times(&self, path: &str) -> Vec<f32> {
        self.clips
            .get(path)
            .map(|clip| clip.key_times())
            .unwrap_or_default()
    }

    pub fn has_skeleton(&self, path: &str) -> bool {
        self.skeletons.contains_key(path)
    }

    pub fn has_clip(&self, path: &str) -> bool {
        self.clips.contains_key(path)
    }

    pub fn skeleton_count(&self) -> usize {
        self.skeletons.len()
    }

    pub fn clip_count(&self) -> usize {
        self.clips.len()
    }

    pub fn clear(&mut self) {
        self.skeletons.clear();
        self.clips.clear();
    }
}

/// 本帧算出来的骨骼矩阵（列主序，P0-3 直接写进 storage buffer）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BonePalette {
    pub matrices: Vec<[[f32; 4]; 4]>,
}

impl BonePalette {
    pub fn from_matrices(palette: &[glam::Mat4]) -> Self {
        Self {
            matrices: palette_to_arrays(palette),
        }
    }

    pub fn len(&self) -> usize {
        self.matrices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matrices.is_empty()
    }
}

/// 本帧触发的一个关键帧事件（#560 P1 Notify）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimEvent {
    pub entity: Entity,
    /// 事件名，由项目解释（`Footstep.L` / `Traversal.Window` / …）。
    pub name: String,
    /// 事件在剪辑里的时间（秒）。
    pub time: f32,
}

/// 一次 `animator_system` 调用里触发的事件（World 资源）。
///
/// 每个 tick **整体重建**（不是累加）：消费者在 `update` 之后读到的永远只是这一帧
/// 跨过的时间点，不会重复播上一帧的脚步声。要跨帧缓存就自己 `drain` 走。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimEvents {
    pub events: Vec<AnimEvent>,
}

impl AnimEvents {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// 取走这一帧的事件（资源本身留空）。
    pub fn drain(&mut self) -> Vec<AnimEvent> {
        std::mem::take(&mut self.events)
    }

    /// 某个实体这一帧触发的事件名。
    pub fn names_for(&self, entity: Entity) -> impl Iterator<Item = &str> {
        self.events
            .iter()
            .filter(move |event| event.entity == entity)
            .map(|event| event.name.as_str())
    }
}

/// 播放头（运行时组件，不进场景）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimatorPlayhead {
    /// 当前播放内容的键：剪辑路径，或 [`BLEND_SPACE_KEY`]。换了就从头播。
    pub clip_path: String,
    pub time: f32,
    /// 单次播放且已经播完。
    pub finished: bool,
    /// 交叉淡入的淡出源姿态（`None` = 当前没有过渡），过渡开始时冻结、权重到 1 丢掉。
    ///
    /// 存姿态而不是「旧剪辑 + 时间」：混合空间的退出、来源剪辑被删、连续换剪辑这些
    /// 情况都没有（或不该再找）一个可采样的剪辑，快照让它们都能正常淡出。
    pub previous_pose: Option<Pose>,
    /// 上一帧**实际显示**的姿态：下一次过渡的淡出源就是它。
    pub pose: Pose,
    /// 过渡已经进行的秒数（`>= blend_duration` 时清掉来源）。
    pub blend_elapsed: f32,
}

impl AnimatorPlayhead {
    /// 过渡权重：`0` = 完全是旧剪辑，`1` = 完全是新剪辑。
    pub fn blend_weight(&self, blend_duration: f32) -> Option<f32> {
        if self.previous_pose.is_none() || !(blend_duration > 0.0) {
            return None;
        }
        Some((self.blend_elapsed / blend_duration).clamp(0.0, 1.0))
    }
}

/// 混合空间在播放头里的「剪辑路径」占位符。
///
/// 混合空间不是单个剪辑，但播放头需要**一个键**来判断「播放内容变了吗」。用这个
/// 不可能与真实资产路径冲突的标记，进出混合空间就自然地触发一次交叉淡入。
pub const BLEND_SPACE_KEY: &str = "<blend-space>";

/// 推进所有 `Animator` 并重算骨骼矩阵，返回更新过的实体数。
pub fn animator_system(world: &mut World, dt: f32) -> usize {
    let Some(library) = world.get_resource::<SkeletonLibrary>() else {
        return 0;
    };
    let library = LibraryView {
        skeletons: library.skeletons.clone(),
        clips: library.clips.clone(),
    };

    let mut updates: Vec<(Entity, BonePalette, AnimatorPlayhead)> = Vec::new();
    let mut events: Vec<AnimEvent> = Vec::new();
    for (entity, animator) in world.query::<Animator>() {
        let Some(root) = world.get::<SkeletonRoot>(entity) else {
            continue;
        };
        let Some(skeleton) = library.skeleton(&root.skeleton_path) else {
            continue;
        };
        let single_path = if animator.clip_path.is_empty() {
            root.default_clip_path.clone()
        } else {
            animator.clip_path.clone()
        };

        // 一维混合空间优先（#560 P1）：作者列出的剪辑里至少要有两个真的加载了，
        // 否则退回单剪辑 —— 少一个样本就少一个自由度，总比整个实体不动好。
        let mut samples: Vec<BlendSample> = Vec::new();
        if animator.uses_blend_space() {
            samples = animator
                .blend_clip_paths
                .iter()
                .zip(animator.blend_positions.iter())
                .filter_map(|(path, position)| {
                    library.clip(path).map(|clip| BlendSample {
                        position: *position,
                        clip,
                    })
                })
                .collect();
        }
        let blend_space = samples.len() >= 2;
        let clip = if blend_space {
            None
        } else {
            match library.clip(&single_path) {
                Some(clip) => Some(clip),
                None => continue,
            }
        };
        let playback_key = if blend_space {
            BLEND_SPACE_KEY.to_string()
        } else {
            single_path.clone()
        };

        // 播放头：换剪辑从作者给的起始时间重来，否则接着上一帧。
        let mut playhead = match world.get::<AnimatorPlayhead>(entity) {
            Some(head) if head.clip_path == playback_key => head.clone(),
            Some(head) => AnimatorPlayhead {
                // 换剪辑：把旧剪辑**停住那一刻**的姿态冻下来，作为淡出源（#560 P1）。
                // 旧剪辑已经不在库里（删资产 / 上一帧是混合空间）时沿用已有快照 ——
                // 连续换两次也不会丢掉正在淡出的那一份。
                clip_path: playback_key.clone(),
                time: sanitize_time(animator.time),
                finished: false,
                previous_pose: if head.pose.len() > 0 {
                    Some(head.pose.clone())
                } else {
                    head.previous_pose.clone()
                },
                pose: Pose::default(),
                blend_elapsed: 0.0,
            },
            None => AnimatorPlayhead {
                clip_path: playback_key.clone(),
                time: sanitize_time(animator.time),
                finished: false,
                previous_pose: None,
                pose: Pose::default(),
                blend_elapsed: 0.0,
            },
        };
        // 混合空间没有自己的时长：取最长的那条剪辑，短的自然会循环追上。
        let duration = match &clip {
            Some(clip) => clip.duration.max(0.0),
            None => samples
                .iter()
                .map(|sample| sample.clip.duration)
                .fold(0.0f32, f32::max)
                .max(0.0),
        };
        // 上一帧已经在这个播放内容上时的本地时间：只有它存在才触发 notify ——
        // 刚创建 / 刚换剪辑的第一帧不该把整段剪辑的事件一次性播出来。
        let previous_local = match world.get::<AnimatorPlayhead>(entity) {
            Some(head) if head.clip_path == playback_key => {
                Some(local_time(duration, animator.looped, head.time))
            }
            _ => None,
        };

        let speed = if animator.speed.is_finite() {
            animator.speed
        } else {
            0.0
        };
        if animator.playing && dt.is_finite() {
            playhead.time += dt * speed;
            if !animator.looped {
                playhead.time = playhead.time.clamp(0.0, duration);
            }
        }
        playhead.finished = !animator.looped && duration > 0.0 && playhead.time >= duration;

        // 循环与否由 `Animator` 组件说了算（剪辑里的 `looped` 只是默认值）。
        let local = local_time(duration, animator.looped, playhead.time);

        // 关键帧事件：只在前向播放、且上一帧确实在这个剪辑上时触发。
        // 倒放（`speed < 0`）需要反向扫区间，暂时不触发 —— 宁可少一次脚步声，
        // 也不要在倒放时把整段事件倒着播一遍。
        if let (Some(previous), Some(clip)) = (previous_local, clip.as_ref()) {
            if animator.playing && dt.is_finite() && speed > 0.0 {
                for notify in clip.notifies_between(previous, local) {
                    events.push(AnimEvent {
                        entity,
                        name: notify.name.clone(),
                        time: notify.time,
                    });
                }
            }
        }
        let mut pose = skeleton.rest_pose();
        match &clip {
            Some(clip) => {
                clip.sample_into(local, &mut pose);
            }
            None => {
                let phase = if duration > 0.0 {
                    local / duration
                } else {
                    0.0
                };
                let mut space = BlendSpace1D::new(samples.clone(), skeleton.rest_pose());
                space.evaluate_into(animator.blend_parameter, phase, &mut pose);
            }
        }

        // 交叉淡入：旧剪辑冻结在过渡开始的那一刻，按权重混进新姿态。
        //
        // 先推进再取权重：换剪辑的那一帧 `dt` 是"距上一帧实际过去的时间"，
        // 过渡就该从那一刻开始计时，否则最后一帧永远到不了权重 1。
        let blend_duration = sanitize_blend_duration(animator.blend_duration);
        if blend_duration <= 0.0 {
            // 没有过渡时长 = 硬切：顺手丢掉快照，否则以后把时长调回来的第一帧
            // 会突然淡入一个早已过期的姿态。
            playhead.previous_pose = None;
            playhead.blend_elapsed = 0.0;
        } else if playhead.previous_pose.is_some() && animator.playing && dt.is_finite() {
            playhead.blend_elapsed += dt.abs();
        }
        if let Some(weight) = playhead.blend_weight(blend_duration) {
            if let Some(fading) = playhead.previous_pose.as_ref() {
                // `fading` 是旧姿态、`pose` 是新姿态：`t = weight` 时 0 → 旧、1 → 新。
                let mut blended = skeleton.rest_pose();
                prism_anim::blend_into(&mut blended, fading, &pose, weight);
                pose = blended;
            }
            // 权重已经到 1：新姿态就是完整的新剪辑，快照可以丢了。
            if weight >= 1.0 {
                playhead.previous_pose = None;
                playhead.blend_elapsed = 0.0;
            }
        }

        // 这份姿态就是下一次过渡的淡出源（不需要 clone：上面已经用完 `pose`）。
        playhead.pose = pose;
        let palette = BonePalette::from_matrices(&skeleton.bone_palette(&playhead.pose));
        updates.push((entity, palette, playhead));
    }

    let count = updates.len();
    for (entity, palette, playhead) in updates {
        world.insert(entity, palette);
        world.insert(entity, playhead);
    }
    world.insert_resource(AnimEvents { events });
    count
}

/// 交叉淡入时长：负值 / NaN / 无穷都按 0（直接切换）处理。
fn sanitize_blend_duration(duration: f32) -> f32 {
    if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        0.0
    }
}

/// 把播放头时间折算成剪辑本地时间（循环取余、单次夹到 `0..=duration`）。
fn local_time(duration: f32, looped: bool, time: f32) -> f32 {
    if duration > 0.0 {
        if looped {
            time.rem_euclid(duration)
        } else {
            time.clamp(0.0, duration)
        }
    } else {
        0.0
    }
}

fn sanitize_time(time: f32) -> f32 {
    if time.is_finite() {
        time
    } else {
        0.0
    }
}

/// 借用一下、避免在遍历时继续持有 World 资源。
struct LibraryView {
    skeletons: HashMap<String, Arc<Skeleton>>,
    clips: HashMap<String, Arc<Clip>>,
}

impl LibraryView {
    fn skeleton(&self, path: &str) -> Option<Arc<Skeleton>> {
        self.skeletons.get(path).cloned()
    }

    fn clip(&self, path: &str) -> Option<Arc<Clip>> {
        self.clips.get(path).cloned()
    }
}

#[cfg(test)]
#[path = "animator_tests.rs"]
pub(crate) mod tests;
