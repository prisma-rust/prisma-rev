//! 2D 虚拟相机系统（#1160）。
//!
//! 提供针对 2D 横版卷轴/顶视/像素动作游戏的通用虚拟相机功能：
//! 1. 目标平滑跟随（Damping）
//! 2. 移动死区矩形（Deadzone）
//! 3. 关卡世界边界围栏约束（Confiner）
//! 4. 像素对齐吸附防抖（Pixel Snapping）

use glam::Vec2;
use prism_ecs::{Entity, World};
use prism_scene::components::{
    Camera, Camera2D, Child, LocalToWorld, LocalTransform, Name, Parent, VirtualCamera2D,
};
use prism_scene::is_active_in_hierarchy;

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
        let cam_info = world
            .get::<Camera>(entity)
            .map(|c| (c.orthographic, c.orthographic_size, c.aspect));

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

// ---------------------------------------------------------------------------
// Name-based virtual camera follow (EchoWell slice).
// ---------------------------------------------------------------------------

/// Update every configured [`VirtualCamera2D`] after gameplay writes its
/// target transforms and before render extraction.
pub fn virtual_camera2d_system(world: &mut World, dt: f32) {
    let cameras: Vec<(Entity, VirtualCamera2D)> = world
        .query::<VirtualCamera2D>()
        .map(|(entity, camera)| (entity, camera.clone()))
        .collect();

    for (camera_entity, config) in cameras {
        if world
            .get::<Camera>(camera_entity)
            .is_none_or(|camera| !camera.enabled || !camera.orthographic)
            || !is_active_in_hierarchy(world, camera_entity)
        {
            continue;
        }
        if config.target_name.is_empty() {
            continue;
        }
        let Some(target_entity) = world
            .query::<Name>()
            .find(|(_, name)| name.0 == config.target_name)
            .map(|(entity, _)| entity)
        else {
            continue;
        };
        if !is_active_in_hierarchy(world, target_entity) {
            continue;
        }
        let Some(target) = world_position(world, target_entity) else {
            continue;
        };
        let Some(local_transform) = world.get::<LocalTransform>(camera_entity).cloned() else {
            continue;
        };
        let Some(parent_world) = parent_world_matrix(world, camera_entity) else {
            continue;
        };
        let camera_world = parent_world * local_transform.to_model_matrix();
        let current = camera_world.transform_point3(glam::Vec3::ZERO);

        let desired = [
            deadzone_target(
                current.x,
                target[0] + config.follow_offset[0],
                config.deadzone[0],
            ),
            deadzone_target(
                current.y,
                target[1] + config.follow_offset[1],
                config.deadzone[1],
            ),
        ];
        let mut position = [
            damp(current.x, desired[0], config.damping[0], dt),
            damp(current.y, desired[1], config.damping[1], dt),
        ];

        if let Some(bounds) = config.confiner {
            let (half_width, half_height) = view_half_size(world, camera_entity, camera_world);
            let ppu = config
                .pixel_snap
                .then(|| finite_positive(config.pixels_per_unit).unwrap_or(1.0));
            position[0] = confine_axis(position[0], bounds[0], bounds[2], half_width, ppu);
            position[1] = confine_axis(position[1], bounds[1], bounds[3], half_height, ppu);
        } else if config.pixel_snap {
            let ppu = finite_positive(config.pixels_per_unit).unwrap_or(1.0);
            position[0] = snap(position[0], ppu);
            position[1] = snap(position[1], ppu);
        }

        let desired_world = glam::Vec3::new(position[0], position[1], current.z);
        let local_position = if parent_world == glam::Mat4::IDENTITY {
            desired_world
        } else {
            let determinant = parent_world.determinant();
            if !determinant.is_finite() || determinant.abs() <= f32::EPSILON {
                continue;
            }
            parent_world.inverse().transform_point3(desired_world)
        };
        if !local_position.is_finite() {
            continue;
        }
        if let Some(transform) = world.get_mut::<LocalTransform>(camera_entity) {
            transform.position = local_position;
        }
    }
}

fn world_position(world: &World, entity: Entity) -> Option<[f32; 2]> {
    if let Some(transform) = world.get::<LocalToWorld>(entity) {
        let translation = transform.0.col(3);
        return Some([translation.x, translation.y]);
    }
    world
        .get::<LocalTransform>(entity)
        .map(|transform| [transform.position.x, transform.position.y])
}

fn parent_world_matrix(world: &World, entity: Entity) -> Option<glam::Mat4> {
    let Some(parent) = valid_parent(world, entity) else {
        return Some(glam::Mat4::IDENTITY);
    };
    let mut cursor = parent;
    let mut ancestors = Vec::new();
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cursor) {
            return None;
        }
        ancestors.push(cursor);
        let Some(parent) = valid_parent(world, cursor) else {
            break;
        };
        cursor = parent;
    }

    let mut world_matrix = glam::Mat4::IDENTITY;
    for ancestor in ancestors.into_iter().rev() {
        if let Some(transform) = world.get::<LocalTransform>(ancestor) {
            world_matrix *= transform.to_model_matrix();
        }
    }
    Some(world_matrix)
}

fn valid_parent(world: &World, entity: Entity) -> Option<Entity> {
    let parent = world.get::<Parent>(entity)?.0;
    (world.is_alive(parent)
        && world
            .get::<Child>(parent)
            .is_some_and(|children| children.0.contains(&entity))
        && parent != entity)
        .then_some(parent)
}

fn view_half_size(world: &World, camera: Entity, camera_world: glam::Mat4) -> (f32, f32) {
    let Some(camera) = world.get::<Camera>(camera) else {
        return (0.0, 0.0);
    };
    if !camera.orthographic {
        return (0.0, 0.0);
    }
    let half_height = camera.orthographic_size.max(0.0);
    let aspect = finite_positive(camera.aspect).unwrap_or(1.0);
    let half_width = half_height * aspect;
    let world_x = camera_world.transform_vector3(glam::Vec3::X * half_width);
    let world_y = camera_world.transform_vector3(glam::Vec3::Y * half_height);
    (
        world_x.x.abs() + world_y.x.abs(),
        world_x.y.abs() + world_y.y.abs(),
    )
}

fn snap(position: f32, pixels_per_unit: f32) -> f32 {
    (position * pixels_per_unit).round() / pixels_per_unit
}

fn finite_positive(value: f32) -> Option<f32> {
    value
        .is_finite()
        .then_some(value)
        .filter(|value| *value > 0.0)
}

fn deadzone_target(current: f32, target: f32, deadzone: f32) -> f32 {
    let deadzone = if deadzone.is_finite() {
        deadzone.max(0.0)
    } else {
        0.0
    };
    let delta = target - current;
    if delta.abs() <= deadzone {
        current
    } else {
        target - delta.signum() * deadzone
    }
}

fn damp(current: f32, target: f32, rate: f32, dt: f32) -> f32 {
    if !current.is_finite() || !target.is_finite() {
        return current;
    }
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
    let rate = if rate.is_finite() { rate.max(0.0) } else { 0.0 };
    if rate == 0.0 {
        target
    } else if dt == 0.0 {
        current
    } else {
        current + (target - current) * (1.0 - (-rate * dt).exp())
    }
}

fn confine_axis(
    position: f32,
    min: f32,
    max: f32,
    half_view: f32,
    pixels_per_unit: Option<f32>,
) -> f32 {
    let (min, max) = if min.is_finite() && max.is_finite() {
        (min.min(max), min.max(max))
    } else {
        return position;
    };
    let half_view = if half_view.is_finite() {
        half_view.max(0.0)
    } else {
        0.0
    };
    let low = min + half_view;
    let high = max - half_view;
    if low <= high {
        let Some(ppu) = pixels_per_unit else {
            return position.clamp(low, high);
        };
        let grid_low = (low * ppu).ceil();
        let grid_high = (high * ppu).floor();
        if grid_low.is_finite() && grid_high.is_finite() && grid_low <= grid_high {
            (position * ppu).round().clamp(grid_low, grid_high) / ppu
        } else {
            position.clamp(low, high)
        }
    } else {
        (min + max) * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::components::{Active, Child};

    fn camera_world(target: [f32; 2], config: VirtualCamera2D) -> (World, Entity) {
        let mut world = World::new();
        let target_entity = world.spawn();
        world.insert(target_entity, Name("player".to_string()));
        world.insert(
            target_entity,
            LocalTransform {
                position: glam::Vec3::new(target[0], target[1], 0.0),
                ..LocalTransform::default()
            },
        );
        let camera_entity = world.spawn();
        world.insert(camera_entity, Active(true));
        world.insert(camera_entity, LocalTransform::default());
        world.insert(
            camera_entity,
            Camera {
                orthographic: true,
                orthographic_size: 5.0,
                aspect: 2.0,
                ..Camera::default()
            },
        );
        world.insert(camera_entity, config);
        (world, camera_entity)
    }

    #[test]
    fn deadzone_keeps_the_camera_until_target_leaves_it() {
        let (mut world, camera) = camera_world(
            [3.0, 0.0],
            VirtualCamera2D {
                target_name: "player".to_string(),
                deadzone: [2.0, 0.0],
                ..VirtualCamera2D::default()
            },
        );
        virtual_camera2d_system(&mut world, 1.0 / 60.0);
        assert_eq!(world.get::<LocalTransform>(camera).unwrap().position.x, 1.0);
    }

    #[test]
    fn damping_moves_part_way_to_target() {
        let (mut world, camera) = camera_world(
            [10.0, 0.0],
            VirtualCamera2D {
                target_name: "player".to_string(),
                damping: [1.0, 0.0],
                ..VirtualCamera2D::default()
            },
        );
        virtual_camera2d_system(&mut world, 1.0);
        let x = world.get::<LocalTransform>(camera).unwrap().position.x;
        assert!(x > 0.0 && x < 10.0);
    }

    #[test]
    fn confiner_accounts_for_orthographic_view_size() {
        let (mut world, camera) = camera_world(
            [100.0, 100.0],
            VirtualCamera2D {
                target_name: "player".to_string(),
                confiner: Some([0.0, 0.0, 20.0, 8.0]),
                ..VirtualCamera2D::default()
            },
        );
        virtual_camera2d_system(&mut world, 1.0 / 60.0);
        let position = world.get::<LocalTransform>(camera).unwrap().position;
        assert_eq!(position.x, 10.0);
        assert_eq!(position.y, 4.0);
    }

    #[test]
    fn pixel_snap_uses_pixels_per_unit() {
        let (mut world, camera) = camera_world(
            [1.24, 2.76],
            VirtualCamera2D {
                target_name: "player".to_string(),
                pixel_snap: true,
                pixels_per_unit: 2.0,
                ..VirtualCamera2D::default()
            },
        );
        virtual_camera2d_system(&mut world, 1.0 / 60.0);
        let position = world.get::<LocalTransform>(camera).unwrap().position;
        assert_eq!(position.x, 1.0);
        assert_eq!(position.y, 3.0);
    }

    #[test]
    fn parented_camera_follows_and_confines_in_world_space() {
        let (mut world, camera) = camera_world(
            [100.0, 100.0],
            VirtualCamera2D {
                target_name: "player".to_string(),
                confiner: Some([0.0, 0.0, 20.0, 20.0]),
                ..VirtualCamera2D::default()
            },
        );
        let parent = world.spawn();
        world.insert(
            parent,
            LocalTransform {
                position: glam::Vec3::new(30.0, 40.0, 0.0),
                rotation: glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                ..LocalTransform::default()
            },
        );
        world.insert(parent, Child(vec![camera]));
        world.insert(camera, Parent(parent));

        virtual_camera2d_system(&mut world, 1.0 / 60.0);
        super::super::hierarchy::hierarchy_system(&mut world);

        let world_position = world
            .get::<LocalToWorld>(camera)
            .unwrap()
            .0
            .w_axis
            .truncate();
        assert!((world_position.x - 15.0).abs() < 1e-5);
        assert!((world_position.y - 10.0).abs() < 1e-5);
        let local = world.get::<LocalTransform>(camera).unwrap().position;
        assert!((local.x + 30.0).abs() < 1e-5);
        assert!((local.y - 15.0).abs() < 1e-5);
    }

    #[test]
    fn confiner_preserves_pixel_snap_when_bounds_allow_a_grid_point() {
        let (mut world, camera) = camera_world(
            [100.0, 100.0],
            VirtualCamera2D {
                target_name: "player".to_string(),
                confiner: Some([0.2, 0.2, 30.4, 20.4]),
                pixel_snap: true,
                pixels_per_unit: 2.0,
                ..VirtualCamera2D::default()
            },
        );
        virtual_camera2d_system(&mut world, 1.0 / 60.0);
        let position = world.get::<LocalTransform>(camera).unwrap().position;
        assert_eq!(position.x, 20.0);
        assert_eq!(position.y, 15.0);
    }
}
