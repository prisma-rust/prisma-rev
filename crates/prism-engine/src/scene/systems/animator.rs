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

use prism_anim::{palette_to_arrays, BlendSample, BlendSpace1D, Clip, Skeleton};
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

/// 播放头（运行时组件，不进场景）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimatorPlayhead {
    /// 当前播放内容的键：剪辑路径，或 [`BLEND_SPACE_KEY`]。换了就从头播。
    pub clip_path: String,
    pub time: f32,
    /// 单次播放且已经播完。
    pub finished: bool,
    /// 交叉淡入的来源剪辑。空 = 没有正在进行的过渡。
    ///
    /// 过渡期间**冻结**在这个剪辑的 `previous_time` 上：冻结比让旧剪辑继续跑
    /// 更容易预测（不会出现"刚切走的动画还在往前跑"），也是 UE 简单
    /// cross-fade 的语义。
    pub previous_clip_path: String,
    pub previous_time: f32,
    /// 过渡已经进行的秒数（`>= blend_duration` 时清掉来源）。
    pub blend_elapsed: f32,
}

impl AnimatorPlayhead {
    /// 过渡权重：`0` = 完全是旧剪辑，`1` = 完全是新剪辑。
    pub fn blend_weight(&self, blend_duration: f32) -> Option<f32> {
        if self.previous_clip_path.is_empty() || !(blend_duration > 0.0) {
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
                // 换剪辑：从旧剪辑停住的那一刻淡出（#560 P1）。
                clip_path: playback_key.clone(),
                time: sanitize_time(animator.time),
                finished: false,
                previous_clip_path: head.clip_path.clone(),
                previous_time: head.time,
                blend_elapsed: 0.0,
            },
            None => AnimatorPlayhead {
                clip_path: playback_key.clone(),
                time: sanitize_time(animator.time),
                finished: false,
                previous_clip_path: String::new(),
                previous_time: 0.0,
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
        if animator.playing && dt.is_finite() {
            let speed = if animator.speed.is_finite() {
                animator.speed
            } else {
                0.0
            };
            playhead.time += dt * speed;
            if !animator.looped {
                playhead.time = playhead.time.clamp(0.0, duration);
            }
        }
        playhead.finished = !animator.looped && duration > 0.0 && playhead.time >= duration;

        // 循环与否由 `Animator` 组件说了算（剪辑里的 `looped` 只是默认值）。
        let local = if duration > 0.0 {
            if animator.looped {
                playhead.time.rem_euclid(duration)
            } else {
                playhead.time.clamp(0.0, duration)
            }
        } else {
            0.0
        };
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
            // 没有过渡时长 = 硬切：顺手清掉来源，否则以后把时长调回来的第一帧
            // 会突然淡入一个早已过期的姿态。
            playhead.previous_clip_path.clear();
            playhead.blend_elapsed = 0.0;
        } else if !playhead.previous_clip_path.is_empty() && animator.playing && dt.is_finite() {
            playhead.blend_elapsed += dt.abs();
        }
        if let Some(weight) = playhead.blend_weight(blend_duration) {
            match library.clip(&playhead.previous_clip_path) {
                Some(previous) => {
                    let mut fading = skeleton.rest_pose();
                    previous.sample_into(previous.wrap_time(playhead.previous_time), &mut fading);
                    // `pose` 是新剪辑、`fading` 是旧剪辑：`t = weight` 时 0 → 旧、1 → 新。
                    let mut blended = skeleton.rest_pose();
                    prism_anim::blend_into(&mut blended, &fading, &pose, weight);
                    pose = blended;
                    // 权重已经到 1：新姿态就是完整的新剪辑，来源可以丢了。
                    if weight >= 1.0 {
                        playhead.previous_clip_path.clear();
                        playhead.blend_elapsed = 0.0;
                    }
                }
                None => {
                    // 来源剪辑不在库里（被删了 / 路径写错）：直接切，别卡在过渡里。
                    playhead.previous_clip_path.clear();
                    playhead.blend_elapsed = 0.0;
                }
            }
        }

        let palette = BonePalette::from_matrices(&skeleton.bone_palette(&pose));
        updates.push((entity, palette, playhead));
    }

    let count = updates.len();
    for (entity, palette, playhead) in updates {
        world.insert(entity, palette);
        world.insert(entity, playhead);
    }
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
