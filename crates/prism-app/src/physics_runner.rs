//! Physics 线程 — owns the Rapier simulation 世界
//!
//! The main 线程 sends [`PhysicsStep`] (spawn/despawn/set-transform commands)
//! and receives [`PhysicsResult`] 动力学 body transforms) each 帧
//!
//! # Initial scope
//!
//! * 刚体 bodies only 动力学 / 运动学 / 静态
//! * 球体 / 盒 / capsule / trimesh colliders
//! * No joints, no CCD, no 查询 管线
//!
//! # 接线（#522）
//!
//! [`PhysicsRuntime`] 由 `App::with_subsystem(Subsystem::Physics)` 创建：主线程每个
//! 固定步发 [`PhysicsStep`]，线程积分后回 [`PhysicsResult`]，结果在
//! `Engine::update` 的 `hierarchy_system` 之前写回 `LocalTransform`。
//!
//! 第一版范围（刻意收窄）：
//!
//! * 只处理**无父级**的实体；有 `Parent` 的刚体跳过并 warn 一次（世界矩阵 ↔ 局部
//!   变换的换算留到后续）。
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
                    let collider = collider_builder.density(density).build();
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
    /// 有 `Parent` 的实体只 warn 一次（第一版不处理父子层级）。
    warned_parented: std::collections::HashSet<EntityId>,
    /// 线程已退出 / 通道已断：之后 `step` 直接返回，不再刷日志。
    failed: bool,
}

#[derive(Clone, PartialEq)]
struct TrackedBody {
    status: PhysicsBodyStatus,
    settings: BodySettings,
    shape: ColliderDesc,
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
            warned_parented: std::collections::HashSet::new(),
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
            self.warned_parented.remove(&key);
            commands.push(PhysicsCommand::DespawnBody { entity: key });
        }

        for (key, entity) in current {
            if world.get::<Parent>(entity).is_some() {
                if self.warned_parented.insert(key) {
                    log::warn!(
                        "physics: entity {key:#x} has a Parent; hierarchical rigid bodies are \
                         not supported yet and will be skipped"
                    );
                }
                continue;
            }
            let Some(transform) = world.get::<LocalTransform>(entity) else {
                continue;
            };
            let body = world.get::<Rigidbody>(entity);
            let collider = world.get::<Collider>(entity);

            let status = body.map(body_status).unwrap_or(PhysicsBodyStatus::Static);
            let settings = body.map(body_settings).unwrap_or_default();
            let shape = collider
                .map(|collider| collider_desc(collider, transform.scale))
                .unwrap_or_else(|| default_box(transform.scale));
            let desired = TrackedBody {
                status,
                settings,
                shape: shape.clone(),
            };

            let position = transform.position.to_array();
            let rotation = transform.rotation.to_array();
            match self.bodies.get(&key) {
                None => {
                    commands.push(PhysicsCommand::SpawnBody {
                        entity: key,
                        position,
                        rotation,
                        body_status: status,
                        settings,
                        shape,
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

fn apply_result(world: &mut World, result: &PhysicsResult) {
    for body in &result.transforms {
        let entity = entity_from_key(body.entity);
        if !world.is_alive(entity) {
            continue;
        }
        let Some(transform) = world.get_mut::<LocalTransform>(entity) else {
            continue;
        };
        if body.position.iter().all(|value| value.is_finite()) {
            transform.position = glam::Vec3::from_array(body.position);
        }
        let rotation = glam::Quat::from_array(body.rotation);
        if body.rotation.iter().all(|value| value.is_finite()) && rotation.length_squared() > 1e-8 {
            transform.rotation = rotation.normalize();
        }
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
    fn entity_keys_round_trip() {
        let entity = Entity::from_raw(42, 7);
        assert_eq!(entity_from_key(entity_key(entity)), entity);
    }
}
