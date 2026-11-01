//! 帧间脏数据追踪，用于 [`SceneChanges`]（PR-S2）。
//!
//! [`DirtyRouter`] 存储上一帧的 [`SceneChanges`]，
//! 并在更新时比较每个字段以生成 [`DirtyFlags`]。
//! 下游消费者（PR-S3 SceneReadView / PR-S4 上传阶段）使用这些标志跳过
//! 冗余的 GPU 上传——例如仅在 `POINT_LIGHTS` 变脏时重新上传光源缓冲区，
//! 仅在相机变脏时重新绑定相机 UBO 等。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use prism_render::DrawItem;

use crate::render_system::SceneChanges;

// ---------------------------------------------------------------------------
// DirtyFlags
// ---------------------------------------------------------------------------

/// 连续帧之间发生变化的场景字段集合。
///
/// 零延迟：在 [`DirtyRouter::update`] 期间同步计算，早于任何渲染工作开始，
/// 因此准备/渲染阶段可以立即作用于当前帧的脏数据（无帧延迟）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtyFlags {
    /// 相机（view-proj、eye、视图投影或任何派生值）。
    pub camera: bool,
    /// 方向光的方向、颜色或强度。
    pub directional_light: bool,
    /// 点光源列表（数量、位置、颜色、范围）。
    pub point_lights: bool,
    /// 曝光/相机存在性（影响 tonemap）
    pub exposure: bool,
}

impl DirtyFlags {
    pub const fn all() -> Self {
        Self {
            camera: true,
            directional_light: true,
            point_lights: true,
            exposure: true,
        }
    }

    pub fn any(&self) -> bool {
        self.camera || self.directional_light || self.point_lights || self.exposure
    }

    pub fn none(&self) -> bool {
        !self.any()
    }
}

// ---------------------------------------------------------------------------
// DirtyRouter
// ---------------------------------------------------------------------------

/// Per-frame change detector for [`SceneChanges`].
///
/// On the 第一个 调用 to [`update`](Self::update) (no 上一个 快照 every
/// field is reported dirty.  Subsequent calls return only the fields whose
/// values actually changed.
pub struct DirtyRouter {
    prev: Option<Box<SceneChanges>>,
    prev_draw_signature: Option<u64>,
    prev_pt_bounces: Option<u32>,
}

impl DirtyRouter {
    pub fn new() -> Self {
        Self {
            prev: None,
            prev_draw_signature: None,
            prev_pt_bounces: None,
        }
    }

    /// 追踪 draw items 的内容变化（矩阵 / mesh / material / 阴影标志），
    /// 不再只看数量：数量不变但物体移动或换网格同样算脏。
    pub fn draw_items_dirty(&mut self, signature: u64) -> bool {
        let dirty = self.prev_draw_signature != Some(signature);
        self.prev_draw_signature = Some(signature);
        dirty
    }

    /// 追踪路径追踪的回弹次数变化：变化后已有累积样本的反弹路径失效，
    /// 调用方必须重置累积。
    pub fn pt_bounces_dirty(&mut self, max_bounces: u32) -> bool {
        let dirty = self.prev_pt_bounces != Some(max_bounces);
        self.prev_pt_bounces = Some(max_bounces);
        dirty
    }

    /// 比较 `new` against the 上一个 快照 and return [`DirtyFlags`].
    ///
    /// The 上一个 快照 is **replaced** with a clone of `new` after the
    /// 比较 so the 下一个 帧 can diff against the 当前 状态
    pub fn update(&mut self, new: &SceneChanges) -> DirtyFlags {
        let Some(ref prev) = self.prev else {
            // 第一个 帧 everything is dirty.
            self.prev = Some(Box::new(new.clone()));
            return DirtyFlags::all();
        };

        let flags = DirtyFlags {
            camera: prev.view_proj != new.view_proj
                || prev.eye != new.eye
                || prev.view != new.view
                || prev.projection != new.projection
                || prev.inv_projection != new.inv_projection
                || prev.proj22 != new.proj22
                || prev.proj32 != new.proj32,
            directional_light: prev.light_direction != new.light_direction
                || prev.light_color != new.light_color
                || prev.light_view_proj != new.light_view_proj,
            point_lights: prev.lights != new.lights || prev.pt_lights.len() != new.pt_lights.len(),
            exposure: (prev.exposure - new.exposure).abs() > f32::EPSILON
                || prev.has_camera != new.has_camera,
        };

        self.prev = Some(Box::new(new.clone()));
        flags
    }
}

impl Default for DirtyRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// 绘制列表的内容签名：模型矩阵、mesh、material 槽与阴影标志。
///
/// 数量不变但物体移动 / 换了网格时签名也会变化，供路径追踪判断是否需要
/// 丢弃已有累积样本。
pub fn draw_items_signature(items: &[DrawItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    items.len().hash(&mut hasher);
    for item in items {
        item.mesh.hash(&mut hasher);
        for column in &item.model {
            for value in column {
                value.to_bits().hash(&mut hasher);
            }
        }
        item.material.hash(&mut hasher);
        item.cast_shadow.hash(&mut hasher);
        item.visible_in_main_pass.hash(&mut hasher);
        item.cast_shadow_two_sided.hash(&mut hasher);
        item.index_start.hash(&mut hasher);
        item.index_count.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_item(x: f32, material: Option<u32>) -> DrawItem {
        DrawItem {
            mesh: Default::default(),
            entity_id: u32::MAX,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [x, 0.0, 0.0, 1.0],
            ],
            material,
            cast_shadow: true,
            visible_in_main_pass: true,
            cast_shadow_two_sided: false,
            bone_offset: None,
            state: Default::default(),
            world_bounds: None,
            index_start: 0,
            index_count: None,
        }
    }

    #[test]
    fn draw_signature_tracks_content_not_just_count() {
        let a = vec![draw_item(0.0, None)];
        let b = vec![draw_item(1.0, None)];
        assert_eq!(draw_items_signature(&a), draw_items_signature(&a));
        assert_ne!(
            draw_items_signature(&a),
            draw_items_signature(&b),
            "moving a draw item must change the signature"
        );
    }

    #[test]
    fn draw_signature_tracks_material_slot() {
        let a = vec![draw_item(0.0, Some(1))];
        let b = vec![draw_item(0.0, Some(2))];
        assert_ne!(draw_items_signature(&a), draw_items_signature(&b));
    }

    #[test]
    fn draw_items_dirty_reports_changes_once() {
        let mut router = DirtyRouter::new();
        assert!(router.draw_items_dirty(1));
        assert!(!router.draw_items_dirty(1));
        assert!(router.draw_items_dirty(2));
    }

    #[test]
    fn pt_bounces_dirty_reports_changes_once() {
        let mut router = DirtyRouter::new();
        assert!(router.pt_bounces_dirty(3));
        assert!(!router.pt_bounces_dirty(3));
        assert!(router.pt_bounces_dirty(5));
    }
}
