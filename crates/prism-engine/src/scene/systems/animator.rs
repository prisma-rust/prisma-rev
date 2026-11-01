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

use prism_anim::{palette_to_arrays, Clip, Skeleton};
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
    /// 当前剪辑路径，换了剪辑就从头播。
    pub clip_path: String,
    pub time: f32,
    /// 单次播放且已经播完。
    pub finished: bool,
}

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
        let clip_path = if animator.clip_path.is_empty() {
            root.default_clip_path.clone()
        } else {
            animator.clip_path.clone()
        };
        let Some(clip) = library.clip(&clip_path) else {
            continue;
        };

        // 播放头：换剪辑从作者给的起始时间重来，否则接着上一帧。
        let mut playhead = match world.get::<AnimatorPlayhead>(entity) {
            Some(head) if head.clip_path == clip_path => head.clone(),
            _ => AnimatorPlayhead {
                clip_path: clip_path.clone(),
                time: sanitize_time(animator.time),
                finished: false,
            },
        };
        let duration = clip.duration.max(0.0);
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
        clip.sample_into(local, &mut pose);
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
