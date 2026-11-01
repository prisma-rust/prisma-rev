//! UI 系统 —— 基于 ECS 的声明式 UI
//!
//! # 架构
//!
//! ```text
//! UI 实体 = Node + Style + (Text | Interaction)
//!
//! `ComputedLayout` 由 Layout System 按需插入并每帧重写，作者不写它。
//!
//! [Layout System]    读取 Style → 写入 ComputedLayout
//! [Render System]    读取 ComputedLayout + Text → 生成 UiDrawList
//! ```
//!
//! 每个 UI 元素是一个 ECS Entity，通过组件组合定义外观和行为。
//! Panel 基类封装了常用的 UI Entity 创建逻辑。
//!
//! # 集成到 Engine
//!
//! ```ignore
//! engine.schedule_mut().add_systems(
//!     crate::app::ScheduleLabel::Update,
//!     crate::ui::ui_layout_system,
//! );
//! engine.schedule_mut().add_systems(
//!     crate::app::ScheduleLabel::PostRender,
//!     crate::ui::ui_render_system,
//! );
//! ```

pub mod components {
    //! UI schema components now live in `prism-scene`; re-export for path compatibility.
    pub use prism_scene::ui::*;
}
mod input;
mod layout;
mod panel_base;
mod particles;
mod render;

pub use components::*;
pub use input::{ui_input_system, UiFocus, UiInputState, UiPressState};
pub use layout::{assign_draw_order, ui_layout_system, ScreenSize};
pub use panel_base::PanelBase;
pub use particles::{Particle2D, ParticleSystem2D, SpawnKind};
pub use render::{
    convert_ui_draw_list_to_overlay, convert_ui_draw_list_to_overlay_into, ui_render_system,
    ExternalUiQuads, UiDrawItem, UiDrawList, UiQuad, UiTextCmd, EXTERNAL_UI_ORDER,
};

/// 实体及其祖先链是否全部处于激活状态。
///
/// `Active` 缺失视为激活 —— 只有显式关闭的实体才带 `Active(false)`。
/// 父级关闭会连带隐藏整个子树，所以这里沿 `Parent` 往上查。
///
/// 渲染（`ui_render_system`）与命中测试（`ui_input_system`）都必须用它，
/// 否则「隐藏」只对层级面板的眼睛图标生效，画面和点击都不受影响。
pub(crate) fn is_ui_active(world: &prism_ecs::World, entity: prism_ecs::Entity) -> bool {
    let mut current = entity;
    // 脏数据成环时最多上溯 64 层（与 `ui::input` 的深度上限一致）。
    for _ in 0..64 {
        if matches!(
            world.get::<prism_scene::components::Active>(current),
            Some(prism_scene::components::Active(false))
        ) {
            return false;
        }
        match world.get::<prism_scene::components::Parent>(current) {
            Some(parent) => current = parent.0,
            None => return true,
        }
    }
    true
}

/// UI 面板接口（将逐步被 ECS 组件方案取代）。
pub trait Panel {
    /// 每帧更新。返回 `false` 表示面板请求关闭。
    fn on_update(&mut self, _dt: f32) -> bool {
        true
    }
    /// 处理输入事件。
    fn on_event(&mut self, _event: &()) {}
    /// 面板关闭时的清理回调。
    fn on_close(&mut self) {}
}
