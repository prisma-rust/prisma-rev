//! Physics 线程 — owns the Rapier simulation 世界
//!
//! The main 线程 sends [`PhysicsStep`] (spawn/despawn/set-transform commands)
//! and receives [`PhysicsResult`] 动力学 body transforms) each 帧
//!
//! # Scope
//!
//! * 刚体：动力学 / 运动学 / 静态，支持父子层级
//! * 碰撞体：球 / 盒 / 胶囊（trimesh 尚未接 cook 产物）
//! * 无关节、无 CCD；射线查询在 `prism_engine::physics::raycast`
//!
//! # 接线（#522）
//!
//! [`PhysicsRuntime`] 由 `App::with_subsystem(Subsystem::Physics)` 创建：主线程每个
//! 固定步发 [`PhysicsStep`]，线程积分后回 [`PhysicsResult`]，结果在
//! `Engine::update` 的 `hierarchy_system` 之前写回 `LocalTransform`。
//!
//! 范围：
//!
//! * 支持带 `Parent` 的刚体：主线程沿父链累乘 `LocalTransform` 得到世界姿势，
//!   结果写回时再用父级世界矩阵的逆变换转回 `LocalTransform`。
//! * 刚体 transform 由 `LocalTransform` 驱动；`Collider.center` 与实体缩放会烘进
//!   rapier 形状。
//! * 没有速度组件：初速度恒为 0，代码改 `LocalTransform` 的动力学实体会在下一步
//!   被物理结果覆盖（Kinematic 例外，每步同步）。

use std::collections::HashMap;
use std::thread::JoinHandle;

use prism_engine::ecs::{Entity, World};
use prism_engine::scene::components::{
    BodyType, Collider, ColliderShape, LocalTransform, Parent, Rigidbody,
};
use rapier3d::dynamics::{
    CCDSolver, IntegrationParameters, IslandManager, RigidBodyBuilder, RigidBodyHandle,
    RigidBodySet, RigidBodyType,
};
use rapier3d::geometry::{BroadPhaseBvh, ColliderBuilder, ColliderSet, NarrowPhase};
use rapier3d::math::{Pose, Rotation, Vector};
use rapier3d::pipeline::PhysicsPipeline;

use flume::{Receiver, Sender};

/// 不透明 实体 identifier shared with the ECS 世界 on the main 线程
pub type EntityId = u64;

// ── Commands (main → physics) ─────────────────────────────────────────

pub struct PhysicsStep {
    pub commands: Vec<PhysicsCommand>,
}

pub enum PhysicsCommand {
    SpawnBody {
        entity: EntityId,
        position: [f32; 3],
        rotation: [f32; 4],
        body_status: PhysicsBodyStatus,
        settings: BodySettings,
        shape: ColliderDesc,
        /// `Collider.is_trigger` → rapier `sensor`：参与查询但不阻挡刚体。
        sensor: bool,
    },
    DespawnBody {
        entity: EntityId,
    },
    SetTransform {
        entity: EntityId,
        position: [f32; 3],
        rotation: [f32; 4],
    },
}

/// 作者可调的刚体参数（从 `Rigidbody` 组件烘进来）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodySettings {
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub use_gravity: bool,
}

impl Default for BodySettings {
    fn default() -> Self {
        Self {
            mass: 1.0,
            linear_damping: 0.0,
            angular_damping: 0.05,
            use_gravity: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicsBodyStatus {
    Dynamic,
    KinematicPosition,
    Static,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColliderDesc {
    Sphere {
        radius: f32,
        center: [f32; 3],
    },
    Box {
        half_extents: [f32; 3],
        center: [f32; 3],
    },
    Capsule {
        half_height: f32,
        radius: f32,
        center: [f32; 3],
    },
}

// ── Results (physics → main) ──────────────────────────────────────────

pub struct PhysicsResult {
    pub transforms: Vec<BodyTransform>,
}

pub struct BodyTransform {
    pub entity: EntityId,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
}

// ── helpers ───────────────────────────────────────────────────────────

fn array_to_pose(position: [f32; 3], rotation: [f32; 4]) -> Pose {
    Pose::from_parts(
        Vector::new(position[0], position[1], position[2]),
        Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]),
    )
}

fn rapier_status(status: PhysicsBodyStatus) -> RigidBodyType {
    match status {
        PhysicsBodyStatus::Dynamic => RigidBodyType::Dynamic,
        PhysicsBodyStatus::KinematicPosition => RigidBodyType::KinematicPositionBased,
        PhysicsBodyStatus::Static => RigidBodyType::Fixed,
    }
}

// ── 线程 entry point ────────────────────────────────────────────────

pub fn physics_thread_main(step_rx: Receiver<PhysicsStep>, result_tx: Sender<PhysicsResult>) {
    log::info!("Physics thread started");

    let mut rigid_body_set = RigidBodySet::new();
    let mut collider_set = ColliderSet::new();
    let mut island_manager = IslandManager::new();
    let mut broad_phase = BroadPhaseBvh::new();
    let mut narrow_phase = NarrowPhase::new();
    let mut impulse_joint_set = rapier3d::dynamics::ImpulseJointSet::new();
    let mut multibody_joint_set = rapier3d::dynamics::MultibodyJointSet::new();
    let mut ccd_solver = CCDSolver;
    let gravity = Vector::new(0.0, -9.81, 0.0);
    let integration_params = IntegrationParameters::default();
    let mut physics_pipeline = PhysicsPipeline::new();

    // 实体 → Rapier handle 映射表
    let mut entity_map: HashMap<EntityId, RigidBodyHandle> = HashMap::new();

    while let Ok(step) = step_rx.recv() {
        // 1. Apply commands.
        for cmd in step.commands {
            match cmd {
                PhysicsCommand::SpawnBody {
                    entity,
                    position,
                    rotation,
                    body_status,
                    settings,
                    shape,
                    sensor,
                } => {
                    let mut body_builder = RigidBodyBuilder::new(rapier_status(body_status))
                        .pose(array_to_pose(position, rotation))
                        .linear_damping(settings.linear_damping.max(0.0))
                        .angular_damping(settings.angular_damping.max(0.0))
                        .gravity_scale(if settings.use_gravity { 1.0 } else { 0.0 });
                    if body_status == PhysicsBodyStatus::Dynamic {
                        // Collider density is 0 for dynamic bodies, so the authored
                        // mass is the whole mass (instead of volume + mass).
                        body_builder = body_builder.additional_mass(settings.mass.max(1.0e-4));
                    }
                    let body = body_builder.build();
                    let body_handle = rigid_body_set.insert(body);
                    entity_map.insert(entity, body_handle);

                    let collider_builder = match shape {
                        ColliderDesc::Sphere { radius, center } => {
                            ColliderBuilder::ball(radius.max(1.0e-4))
                                .translation(Vector::from(center))
                        }
                        ColliderDesc::Box {
                            half_extents,
                            center,
                        } => ColliderBuilder::cuboid(
                            half_extents[0].max(1.0e-4),
                            half_extents[1].max(1.0e-4),
                            half_extents[2].max(1.0e-4),
                        )
                        .translation(Vector::from(center)),
                        ColliderDesc::Capsule {
                            half_height,
                            radius,
                            center,
                        } => ColliderBuilder::capsule_y(half_height.max(0.0), radius.max(1.0e-4))
                            .translation(Vector::from(center)),
                    };
                    let density = if body_status == PhysicsBodyStatus::Dynamic {
                        0.0
                    } else {
                        1.0
                    };
                    let collider = collider_builder.density(density).sensor(sensor).build();
                    collider_set.insert_with_parent(collider, body_handle, &mut rigid_body_set);
                }
                PhysicsCommand::DespawnBody { entity } => {
                    if let Some(handle) = entity_map.remove(&entity) {
                        rigid_body_set.remove(
                            handle,
                            &mut island_manager,
                            &mut collider_set,
                            &mut impulse_joint_set,
                            &mut multibody_joint_set,
                            true,
                        );
                    }
                }
                PhysicsCommand::SetTransform {
                    entity,
                    position,
                    rotation,
                } => {
                    if let Some(handle) = entity_map.get(&entity) {
                        if let Some(body) = rigid_body_set.get_mut(*handle) {
                            body.set_position(array_to_pose(position, rotation), true);
                        }
                    }
                }
            }
        }

        // 2. Step simulation.
        physics_pipeline.step(
            gravity,
            &integration_params,
            &mut island_manager,
            &mut broad_phase,
            &mut narrow_phase,
            &mut rigid_body_set,
            &mut collider_set,
            &mut impulse_joint_set,
            &mut multibody_joint_set,
            &mut ccd_solver,
            &(), // PhysicsHooks
            &(), // EventHandler
        );

        // 3. Collect 动力学 body transforms.
        let transforms: Vec<BodyTransform> = entity_map
            .iter()
            .filter_map(|(entity, handle)| {
                let body = rigid_body_set.get(*handle)?;
                if !body.is_dynamic() {
                    return None; // only send back dynamic body positions
                }
                let pose = body.position();
                Some(BodyTransform {
                    entity: *entity,
                    position: [pose.translation.x, pose.translation.y, pose.translation.z],
                    rotation: [
                        pose.rotation.x,
                        pose.rotation.y,
                        pose.rotation.z,
                        pose.rotation.w,
                    ],
                })
            })
            .collect();

        let _ = result_tx.send(PhysicsResult { transforms });
    }

    log::info!("Physics thread exiting");
}

// ── Main-thread runtime（App 持有）────────────────────────────────────

/// 主线程侧的物理运行时：拥有 step/result 通道、线程句柄与实体记账。
///
/// 每个固定步 `step(world)` 一次；`Drop` 关闭 step 通道并 join 线程。
pub struct PhysicsRuntime {
    step_tx: Option<Sender<PhysicsStep>>,
    result_rx: Receiver<PhysicsResult>,
    thread: Option<JoinHandle<()>>,
    bodies: HashMap<EntityId, TrackedBody>,
    /// 线程已退出 / 通道已断：之后 `step` 直接返回，不再刷日志。
    failed: bool,
}

#[derive(Clone, PartialEq)]
struct TrackedBody {
    status: PhysicsBodyStatus,
    settings: BodySettings,
    shape: ColliderDesc,
    sensor: bool,
}

impl PhysicsRuntime {
    /// 启动物理线程。
    pub fn spawn() -> Self {
        let (step_tx, step_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        let thread = std::thread::Builder::new()
            .name("prism-physics".to_string())
            .spawn(move || physics_thread_main(step_rx, result_tx))
            .map_err(|error| {
                log::error!("physics: failed to spawn thread: {error}");
                error
            })
            .ok();
        Self {
            step_tx: Some(step_tx),
            result_rx,
            thread,
            bodies: HashMap::new(),
            failed: false,
        }
    }

    /// 推进一个固定步：同步 spawn/despawn/kinematic → 等线程积分 → 写回
    /// `LocalTransform`。返回 false 表示物理线程已经退出（之后不再尝试）。
    pub fn step(&mut self, world: &mut World) -> bool {
        if self.failed {
            return false;
        }
        let commands = self.collect_commands(world);
        let Some(step_tx) = self.step_tx.as_ref() else {
            self.failed = true;
            return false;
        };
        if let Err(error) = step_tx.send(PhysicsStep { commands }) {
            log::warn!("physics: step channel closed ({error}); physics disabled");
            self.step_tx = None;
            self.failed = true;
            return false;
        }
        match self.result_rx.recv() {
            Ok(result) => {
                apply_result(world, &result);
                true
            }
            Err(error) => {
                log::warn!("physics: thread stopped ({error}); physics disabled");
                self.step_tx = None;
                self.failed = true;
                false
            }
        }
    }

    fn collect_commands(&mut self, world: &World) -> Vec<PhysicsCommand> {
        // 先收集实体，避免在 query 的借用里再 get 组件。
        let mut current: HashMap<EntityId, Entity> = HashMap::new();
        for (entity, _) in world.query::<Rigidbody>() {
            current.insert(entity_key(entity), entity);
        }
        for (entity, _) in world.query::<Collider>() {
            current.insert(entity_key(entity), entity);
        }

        let mut commands = Vec::new();

        // 已经消失的实体先 despawn（含被换场卸载的）。
        let removed: Vec<EntityId> = self
            .bodies
            .keys()
            .filter(|key| !current.contains_key(key))
            .copied()
            .collect();
        for key in removed {
            self.bodies.remove(&key);
            commands.push(PhysicsCommand::DespawnBody { entity: key });
        }

        for (key, entity) in current {
            if world.get::<LocalTransform>(entity).is_none() {
                continue;
            }
            let Some(world_matrix) = world_matrix(world, entity) else {
                continue;
            };
            let body = world.get::<Rigidbody>(entity);
            let collider = world.get::<Collider>(entity);

            let status = body.map(body_status).unwrap_or(PhysicsBodyStatus::Static);
            let settings = body.map(body_settings).unwrap_or_default();
            // 父级缩放也要进碰撞形状；没有父级时等价于原来的 `transform.scale`。
            let (world_scale, world_rotation, world_position) =
                world_matrix.to_scale_rotation_translation();
            let shape = collider
                .map(|collider| collider_desc(collider, world_scale))
                .unwrap_or_else(|| default_box(world_scale));
            let sensor = collider.is_some_and(|collider| collider.is_trigger);
            let desired = TrackedBody {
                status,
                settings,
                shape: shape.clone(),
                sensor,
            };

            let position = world_position.to_array();
            let rotation = world_rotation.to_array();
            match self.bodies.get(&key) {
                None => {
                    commands.push(PhysicsCommand::SpawnBody {
                        entity: key,
                        position,
                        rotation,
                        body_status: status,
                        settings,
                        shape,
                        sensor,
                    });
                }
                Some(previous) if *previous != desired => {
                    // rapier 不支持原地换 shape / 类型；despawn + spawn 最省事。
                    commands.push(PhysicsCommand::DespawnBody { entity: key });
                    commands.push(PhysicsCommand::SpawnBody {
                        entity: key,
                        position,
                        rotation,
                        body_status: status,
                        settings,
                        shape,
                        sensor,
                    });
                }
                Some(previous) if previous.status == PhysicsBodyStatus::KinematicPosition => {
                    // 动画 / 代码移动的实体由 ECS 驱动，每个固定步把新姿势送过去。
                    commands.push(PhysicsCommand::SetTransform {
                        entity: key,
                        position,
                        rotation,
                    });
                }
                Some(_) => {}
            }
            self.bodies.insert(key, desired);
        }

        commands
    }
}

impl Drop for PhysicsRuntime {
    fn drop(&mut self) {
        // 关掉 step 通道 → 线程的 recv 循环结束。
        self.step_tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 沿父链累乘 `LocalTransform`，得到实体当前的世界矩阵。
///
/// 不依赖 `LocalToWorld`：物理固定步可能跑在 `hierarchy_system` 之前，且这里
/// 只需要一棵很浅的父链。遇到断链 / 环时停在最后一个有效父级。
fn world_matrix(world: &World, entity: Entity) -> Option<glam::Mat4> {
    let local = world.get::<LocalTransform>(entity)?;
    let mut matrix = local.to_model_matrix();
    let mut cursor = entity;
    let mut visited = std::collections::HashSet::new();
    visited.insert(entity);
    while let Some(parent) = world.get::<Parent>(cursor).map(|parent| parent.0) {
        if !world.is_alive(parent) || !visited.insert(parent) {
            break;
        }
        let Some(parent_local) = world.get::<LocalTransform>(parent) else {
            break;
        };
        matrix = parent_local.to_model_matrix() * matrix;
        cursor = parent;
    }
    Some(matrix)
}

fn apply_result(world: &mut World, result: &PhysicsResult) {
    for body in &result.transforms {
        let entity = entity_from_key(body.entity);
        if !world.is_alive(entity) {
            continue;
        }
        let mut world_position = glam::Vec3::from_array(body.position);
        let mut world_rotation = glam::Quat::from_array(body.rotation);
        if body.rotation.iter().all(|value| value.is_finite())
            && world_rotation.length_squared() > 1e-8
        {
            world_rotation = world_rotation.normalize();
        } else {
            continue;
        }
        if !world_position.is_finite() {
            continue;
        }

        // rapier 返回的是世界姿势；带 Parent 的实体要转回父级局部空间，
        // 否则 hierarchy_system 下一帧会把父变换再乘一次。
        if let Some(parent) = world.get::<Parent>(entity).map(|parent| parent.0) {
            let Some(parent_matrix) = world_matrix(world, parent) else {
                continue;
            };
            let (_, parent_rotation, _) = parent_matrix.to_scale_rotation_translation();
            world_position = parent_matrix.inverse().transform_point3(world_position);
            world_rotation = parent_rotation.inverse() * world_rotation;
        }

        let Some(transform) = world.get_mut::<LocalTransform>(entity) else {
            continue;
        };
        transform.position = world_position;
        transform.rotation = world_rotation;
    }
}

fn entity_key(entity: Entity) -> EntityId {
    ((entity.id() as u64) << 32) | entity.generation() as u64
}

fn entity_from_key(key: EntityId) -> Entity {
    Entity::from_raw((key >> 32) as u32, key as u32)
}

fn body_status(body: &Rigidbody) -> PhysicsBodyStatus {
    match body.body_type {
        BodyType::Dynamic => PhysicsBodyStatus::Dynamic,
        BodyType::Kinematic => PhysicsBodyStatus::KinematicPosition,
        BodyType::Static => PhysicsBodyStatus::Static,
    }
}

fn body_settings(body: &Rigidbody) -> BodySettings {
    BodySettings {
        mass: body.mass,
        linear_damping: body.linear_damping,
        angular_damping: body.angular_damping,
        use_gravity: body.use_gravity,
    }
}

fn collider_desc(collider: &Collider, scale: glam::Vec3) -> ColliderDesc {
    let scale = scale.abs().max(glam::Vec3::splat(1.0e-4));
    let center = [
        collider.center[0] * scale.x,
        collider.center[1] * scale.y,
        collider.center[2] * scale.z,
    ];
    match collider.shape {
        ColliderShape::Box => ColliderDesc::Box {
            half_extents: [
                collider.half_extents[0] * scale.x,
                collider.half_extents[1] * scale.y,
                collider.half_extents[2] * scale.z,
            ],
            center,
        },
        ColliderShape::Sphere => ColliderDesc::Sphere {
            radius: collider.radius * scale.max_element(),
            center,
        },
        ColliderShape::Capsule => ColliderDesc::Capsule {
            half_height: collider.half_height * scale.y,
            radius: collider.radius * scale.x.max(scale.z),
            center,
        },
    }
}

/// `Rigidbody` 没有 `Collider` 时的兜底形状（与编辑器单位立方体代理一致）。
fn default_box(scale: glam::Vec3) -> ColliderDesc {
    let scale = scale.abs().max(glam::Vec3::splat(1.0e-4));
    ColliderDesc::Box {
        half_extents: [0.5 * scale.x, 0.5 * scale.y, 0.5 * scale.z],
        center: [0.0; 3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// #522：真实 rapier 线程——动态球落到静态地板上停住，而不是穿过去或一直下落。
    #[test]
    fn dynamic_sphere_falls_and_rests_on_the_static_floor() {
        let (step_tx, step_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        let thread = std::thread::spawn(move || physics_thread_main(step_rx, result_tx));

        let dynamic = 0x0000_0001_0000_0001u64;
        let floor = 0x0000_0002_0000_0001u64;
        step_tx
            .send(PhysicsStep {
                commands: vec![
                    PhysicsCommand::SpawnBody {
                        entity: dynamic,
                        position: [0.0, 5.0, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        body_status: PhysicsBodyStatus::Dynamic,
                        settings: BodySettings::default(),
                        shape: ColliderDesc::Sphere {
                            radius: 0.5,
                            center: [0.0; 3],
                        },
                        sensor: false,
                    },
                    PhysicsCommand::SpawnBody {
                        entity: floor,
                        position: [0.0, -0.5, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        body_status: PhysicsBodyStatus::Static,
                        settings: BodySettings::default(),
                        shape: ColliderDesc::Box {
                            half_extents: [5.0, 0.5, 5.0],
                            center: [0.0; 3],
                        },
                        sensor: false,
                    },
                ],
            })
            .unwrap();

        let mut y = 5.0;
        for _ in 0..240 {
            let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            for body in result.transforms {
                if body.entity == dynamic {
                    y = body.position[1];
                }
            }
            step_tx
                .send(PhysicsStep {
                    commands: Vec::new(),
                })
                .unwrap();
        }
        drop(step_tx);
        thread.join().unwrap();

        assert!(
            (0.45..0.55).contains(&y),
            "sphere should rest on the floor (center y≈0.5), got {y}"
        );
    }

    /// 作者的 `center` 与实体缩放都要烘进 rapier 形状。
    #[test]
    fn collider_desc_bakes_center_and_scale() {
        let mut collider = Collider {
            half_extents: [1.0, 2.0, 3.0],
            center: [0.5, 1.0, -0.5],
            ..Default::default()
        };
        match collider_desc(&collider, glam::Vec3::new(2.0, 3.0, 4.0)) {
            ColliderDesc::Box {
                half_extents,
                center,
            } => {
                assert_eq!(half_extents, [2.0, 6.0, 12.0]);
                assert_eq!(center, [1.0, 3.0, -2.0]);
            }
            other => panic!("expected a box, got {other:?}"),
        }

        collider.shape = ColliderShape::Sphere;
        collider.radius = 2.0;
        match collider_desc(&collider, glam::Vec3::new(1.0, 2.0, 3.0)) {
            ColliderDesc::Sphere { radius, .. } => assert_eq!(radius, 6.0),
            other => panic!("expected a sphere, got {other:?}"),
        }

        collider.shape = ColliderShape::Capsule;
        collider.half_height = 1.5;
        match collider_desc(&collider, glam::Vec3::new(2.0, 3.0, 4.0)) {
            ColliderDesc::Capsule {
                half_height,
                radius,
                ..
            } => {
                assert_eq!(half_height, 4.5);
                // 半径只跟水平缩放走，不受拉长的高度影响。
                assert_eq!(radius, 8.0);
            }
            other => panic!("expected a capsule, got {other:?}"),
        }
    }

    /// #522：`PhysicsRuntime` 从 ECS 自动 spawn，并把积分结果写回 `LocalTransform`；
    /// 实体被销毁后下一步安全 despawn。
    #[test]
    fn runtime_syncs_ecs_bodies_and_writes_back_transforms() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(entity, Rigidbody::default());
        world.insert(entity, Collider::default());
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::new(0.0, 5.0, 0.0),
                ..Default::default()
            },
        );

        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..90 {
            assert!(runtime.step(&mut world));
        }
        let y = world
            .get::<LocalTransform>(entity)
            .expect("transform")
            .position
            .y;
        assert!(y < 4.0, "dynamic body should fall through +Y, got y={y}");

        // 删掉实体：下一次 step 发 DespawnBody，不能 panic / 卡住。
        world.despawn(entity);
        assert!(runtime.step(&mut world));
    }

    #[test]
    fn world_matrix_accumulates_parent_rotation_and_scale() {
        let mut world = World::new();
        let parent = world.spawn();
        world.insert(
            parent,
            LocalTransform {
                position: glam::Vec3::new(3.0, 0.0, 0.0),
                rotation: glam::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                scale: glam::Vec3::splat(2.0),
            },
        );
        let child = world.spawn();
        world.insert(
            child,
            LocalTransform {
                position: glam::Vec3::new(1.0, 0.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(child, Parent(parent));

        let world_position = world_matrix(&world, child)
            .expect("world matrix")
            .transform_point3(glam::Vec3::ZERO);
        let expected = glam::Vec3::new(3.0, 0.0, -2.0);
        assert!(
            (world_position - expected).length() < 1.0e-5,
            "expected {expected:?}, got {world_position:?}"
        );
    }

    /// #522：带 `Parent` 的刚体用父链算世界姿势；积分结果再转回局部空间。
    /// 静态父级（地板）在 x=10，子球从局部 y=5 落下，最终局部 y≈0.5、
    /// 世界 x 仍为 10。
    #[test]
    fn parented_dynamic_body_uses_parent_world_transform() {
        use prism_engine::scene::helpers::HierarchyHelper;
        use prism_engine::scene::systems::hierarchy::hierarchy_system;

        let mut world = World::new();
        let parent = world.spawn();
        world.insert(
            parent,
            LocalTransform {
                position: glam::Vec3::new(10.0, 0.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(
            parent,
            Rigidbody {
                body_type: BodyType::Static,
                ..Default::default()
            },
        );
        world.insert(
            parent,
            Collider {
                half_extents: [5.0, 0.5, 5.0],
                center: [0.0, -0.5, 0.0],
                ..Default::default()
            },
        );

        let child = world.spawn();
        world.insert(
            child,
            LocalTransform {
                position: glam::Vec3::new(0.0, 5.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(child, Rigidbody::default());
        world.insert(
            child,
            Collider {
                shape: ColliderShape::Sphere,
                radius: 0.5,
                ..Default::default()
            },
        );
        HierarchyHelper::reparent(&mut world, child, Some(parent));

        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..240 {
            assert!(runtime.step(&mut world));
        }
        hierarchy_system(&mut world);

        let local = world
            .get::<LocalTransform>(child)
            .expect("child transform")
            .position;
        assert!(
            (0.45..0.55).contains(&local.y),
            "child should rest at local y≈0.5, got {local:?}"
        );
        assert!(
            local.x.abs() < 1.0e-3 && local.z.abs() < 1.0e-3,
            "world-space result must be converted back to parent-local space: {local:?}"
        );

        let world_position = world
            .get::<prism_engine::scene::components::LocalToWorld>(child)
            .expect("child world transform")
            .0
            .transform_point3(glam::Vec3::ZERO);
        assert!(
            (world_position.x - 10.0).abs() < 1.0e-3,
            "child should stay under its parent at world x=10, got {world_position:?}"
        );
    }

    /// #522：`Collider.is_trigger` 必须变成 rapier sensor——球穿过触发器，
    /// 落在更下面的真实地板上，而不是被 trigger 挡住。
    #[test]
    fn trigger_collider_does_not_block_a_dynamic_body() {
        let mut world = World::new();
        let trigger = world.spawn();
        world.insert(
            trigger,
            Rigidbody {
                body_type: BodyType::Static,
                ..Default::default()
            },
        );
        world.insert(
            trigger,
            Collider {
                half_extents: [5.0, 0.5, 5.0],
                is_trigger: true,
                ..Default::default()
            },
        );
        let floor = world.spawn();
        world.insert(
            floor,
            LocalTransform {
                position: glam::Vec3::new(0.0, -2.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(
            floor,
            Rigidbody {
                body_type: BodyType::Static,
                ..Default::default()
            },
        );
        world.insert(
            floor,
            Collider {
                half_extents: [5.0, 0.5, 5.0],
                ..Default::default()
            },
        );
        let ball = world.spawn();
        world.insert(
            ball,
            LocalTransform {
                position: glam::Vec3::new(0.0, 5.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(ball, Rigidbody::default());
        world.insert(
            ball,
            Collider {
                shape: ColliderShape::Sphere,
                radius: 0.5,
                ..Default::default()
            },
        );

        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..240 {
            assert!(runtime.step(&mut world));
        }
        let y = world
            .get::<LocalTransform>(ball)
            .expect("ball transform")
            .position
            .y;
        assert!(
            (-1.05..-0.95).contains(&y),
            "ball should pass through the sensor and rest at y≈-1.0, got {y}"
        );
    }

    #[test]
    fn entity_keys_round_trip() {
        let entity = Entity::from_raw(42, 7);
        assert_eq!(entity_from_key(entity_key(entity)), entity);
    }
}
