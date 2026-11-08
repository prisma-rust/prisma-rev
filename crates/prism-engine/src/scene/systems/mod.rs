//! Scene systems — transform hierarchy, rendering, lights, and camera.
//!
//! Each module exposes a single public function that reads from the ECS
//! [`World`] (or writes, in the case of `hierarchy_system`):
//!
//! | Module      | Function | Read/Write | Purpose |
//! |-------------|----------|------------|---------|
//! | `hierarchy` | `hierarchy_system` | 写入 | Recompute `LocalToWorld`（`Engine::update` 结束时调用） |
//! | `render`    | `scene_render_system` | Read | Collect `DrawItem`s |
//! | `lights`    | `collect_*` | Read | Collect light components |
//! | `camera`    | `collect_camera` | Read | Collect camera component |

pub mod animation;
pub mod animator;
pub mod camera;
pub mod foot_ik;
pub mod hierarchy;
pub mod lights;
pub mod lod;
pub mod particles;
pub mod particles2d;
pub mod post_process;
pub mod render;
pub mod skinned_draw;
pub mod terrain;
pub mod tilemap;
pub mod time_of_day;
pub mod ui_particles;
