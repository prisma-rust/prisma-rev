//! Scene systems — 变换 hierarchy, 渲染 lights, and 相机
//!
//! Each 模块 exposes a single 公开 函数 that reads from the ECS
//! 世界 (or writes, in the case of `hierarchy_system`):
//!
//! | 模块 | 函数 | Read/Write | Purpose |
//! |-------------|----------|------------|---------|
//! | `hierarchy` | `hierarchy_system` | 写入 | Recompute `LocalToWorld`（`Engine::update` 结束时调用） |
//! | 渲染 | `scene_render_system` | 读取 | Collect `DrawItem`s |
//! | `lights` | `collect_*` | 读取 | Collect 光源 components |
//! | 相机 | `collect_camera` | 读取 | Collect 相机 分量 |

pub mod animation;
pub mod camera;
pub mod hierarchy;
pub mod lights;
pub mod particles;
pub mod particles2d;
pub mod render;
pub mod terrain;
pub mod tilemap;
