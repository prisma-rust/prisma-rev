//! 2D 虚拟相机系统（#1160）。
//!
//! 提供针对 2D 横版卷轴/顶视/像素动作游戏的通用虚拟相机功能：
//! 1. 目标平滑跟随（Damping）
//! 2. 移动死区矩形（Deadzone）
//! 3. 关卡世界边界围栏约束（Confiner）
//! 4. 像素对齐吸附防抖（Pixel Snapping）

use glam::Vec2;
use prism_ecs::{Entity, World};
use prism_scene::components::{Camera, Camera2D, LocalToWorld, LocalTransform};

/// 解析跟随目标的当前世界位置 (x, y)。
pub fn resolve_target_pos(world: &World, target_id: u64) -> Option<Vec2> {
    if target_id == u64::MAX {
        return None;
    }
    let gen = (target_id >> 32) as u32;
    let id = (target_id & 0xFFFF_FFFF) as u32;
    let candidate = Entity::from_raw(id, gen);
    let target = if world.is_alive(candidate) {
        Some(candidate)
    } else {
        // 若生成代为 0 或失配，尝试按实体 ID 查找
        world
            .query::<LocalTransform>()
            .find_map(|(e, _)| if e.id() == id { Some(e) } else { None })
    }?;

    if let Some(l2w) = world.get::<LocalToWorld>(target) {
        let col3 = l2w.0.col(3);
        Some(glam::vec2(col3.x, col3.y))
    } else if let Some(tf) = world.get::<LocalTransform>(target) {
        Some(glam::vec2(tf.position.x, tf.position.y))
    } else {
        None
    }
}

/// 执行单帧 2D 相机更新逻辑。
///
/// 遍历场景中所有附带 [`Camera2D`] 与 [`LocalTransform`] 的实体并应用平滑追踪。
pub fn camera_2d_system(world: &mut World, dt: f32) {
    let mut updates = Vec::new();

    for (entity, cam2d) in world.query::<Camera2D>() {
        if !cam2d.enabled {
            continue;
        }
        let Some(tf) = world.get::<LocalTransform>(entity) else {
            continue;
        };

        let current_pos = glam::vec2(tf.position.x, tf.position.y);
        let cam_info = world.get::<Camera>(entity).map(|c| (c.orthographic, c.orthographic_size, c.aspect));

        updates.push((entity, cam2d.clone(), current_pos, cam_info));
    }

    for (entity, cam2d, current_pos, cam_info) in updates {
        let target_pos = resolve_target_pos(world, cam2d.target_entity);

        let mut desired = current_pos;
        if let Some(target) = target_pos {
            let offset = glam::vec2(cam2d.offset[0], cam2d.offset[1]);
            let target_point = target + offset;
            let diff = target_point - current_pos;

            let dz_x = cam2d.deadzone[0].abs();
            let dz_y = cam2d.deadzone[1].abs();

            if diff.x > dz_x {
                desired.x = target_point.x - dz_x;
            } else if diff.x < -dz_x {
                desired.x = target_point.x + dz_x;
            }

            if diff.y > dz_y {
                desired.y = target_point.y - dz_y;
            } else if diff.y < -dz_y {
                desired.y = target_point.y + dz_y;
            }
        }

        let mut next_pos = if cam2d.damping > 0.0 && dt > 0.0 {
            let factor = (1.0 - (-cam2d.damping * dt).exp()).clamp(0.0, 1.0);
            current_pos.lerp(desired, factor)
        } else {
            desired
        };

        if cam2d.confiner_enabled {
            let [min_x, min_y, max_x, max_y] = cam2d.confiner_bounds;
            let (half_w, half_h) = if let Some((ortho, ortho_size, aspect)) = cam_info {
                if ortho {
                    let h = ortho_size;
                    let w = h * aspect.max(0.01);
                    (w, h)
                } else {
                    (0.0, 0.0)
                }
            } else {
                (0.0, 0.0)
            };

            if min_x + half_w >= max_x - half_w {
                next_pos.x = (min_x + max_x) * 0.5;
            } else {
                next_pos.x = next_pos.x.clamp(min_x + half_w, max_x - half_w);
            }

            if min_y + half_h >= max_y - half_h {
                next_pos.y = (min_y + max_y) * 0.5;
            } else {
                next_pos.y = next_pos.y.clamp(min_y + half_h, max_y - half_h);
            }
        }

        if cam2d.pixel_snapping && cam2d.pixels_per_unit > 0.0 {
            let ppu = cam2d.pixels_per_unit;
            next_pos.x = (next_pos.x * ppu).round() / ppu;
            next_pos.y = (next_pos.y * ppu).round() / ppu;
        }

        if let Some(tf) = world.get_mut::<LocalTransform>(entity) {
            tf.position.x = next_pos.x;
            tf.position.y = next_pos.y;
        }
    }
}
