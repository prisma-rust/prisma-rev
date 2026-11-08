//! Physics thread — owns the Rapier simulation world.
//!
//! The main thread sends [`PhysicsStep`] (spawn/despawn/set-transform commands)
//! and receives [`PhysicsResult`] (dynamic body transforms) each frame.
//!
//! # Scope
//!
//! * 刚体：动力学 / 运动学 / 静态，支持父子层级
//! * 碰撞体：球 / 盒 / 胶囊 + `TerrainData` 自动生成的静态 heightfield（#517）
//! * Started/Stopped 碰撞与 sensor 事件：每步写入 ECS `PhysicsEvents` 资源
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

use std::collections::{HashMap, HashSet};
use std::thread::JoinHandle;

use prism_engine::ecs::{Entity, World};
use prism_engine::physics::{PhysicsEvent, PhysicsEventKind, PhysicsEvents};
use prism_engine::scene::components::{
    BodyType, CharacterController, Collider, ColliderShape, LocalTransform, Parent, Rigidbody,
};
use prism_engine::scene::systems::terrain::TerrainData;
use rapier3d::control::{CharacterLength, KinematicCharacterController};
use rapier3d::dynamics::{
    CCDSolver, ImpulseJointSet, IntegrationParameters, IslandManager, MultibodyJointSet,
    RigidBodyBuilder, RigidBodyHandle, RigidBodySet, RigidBodyType,
};
use rapier3d::geometry::{
    BroadPhaseBvh, ColliderBuilder, ColliderHandle, ColliderSet, CollisionEvent,
    CollisionEventFlags, NarrowPhase,
};
use rapier3d::math::{Pose, Rotation, Vector};
use rapier3d::pipeline::{ActiveEvents, ChannelEventCollector, PhysicsPipeline, QueryFilter};

use flume::{Receiver, Sender};

/// Opaque entity identifier shared with the ECS world on the main thread.
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
    /// 地形高度场（#517）：`TerrainData` 自动生成的静态 rapier heightfield。
    ///
    /// 地形没有 `Rigidbody` / `Collider` 组件，所以不和 `SpawnBody` 共用路径：
    /// 物理线程只建一个 fixed body + heightfield collider，不向 ECS 回写姿势。
    SpawnTerrain {
        entity: EntityId,
        position: [f32; 3],
        /// 行优先 `cells × cells`，已经乘过 `height_scale`（米）。
        heights: Vec<f32>,
        cells: usize,
        /// 世界尺寸 `[x, z]`（米）。
        size: [f32; 2],
    },
    DespawnTerrain {
        entity: EntityId,
    },
    /// 运动学角色移动（#560 P2）：在 `step` 之后用最新 broad phase 求解，
    /// 结果经 `PhysicsResult::character_moves` 回传。
    MoveCharacter {
        request: CharacterMoveRequest,
    },
}

/// 作者可调的刚体参数（从 `Rigidbody` 组件烘进来）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodySettings {
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub use_gravity: bool,
    pub ccd: bool,
}

impl Default for BodySettings {
    fn default() -> Self {
        Self {
            mass: 1.0,
            linear_damping: 0.0,
            angular_damping: 0.05,
            use_gravity: true,
            ccd: false,
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

/// 一次运动学角色移动的参数（#560 P2）。
///
/// 形状固定是竖直胶囊：脚下的射线、台阶、坡度和贴地都由 rapier 的
/// `KinematicCharacterController` 处理，调用方只提供「想走多远」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterMoveSettings {
    /// 胶囊圆柱段半高（不含两端半球）。
    pub half_height: f32,
    pub radius: f32,
    /// 与环境的间隙，必须为非零小数才数值稳定。
    pub offset: f32,
    /// 撞到斜面时是否沿表面滑动。
    pub slide: bool,
    /// 能爬的最大坡度（度）。
    pub max_slope_deg: f32,
    /// 超过这个坡度就开始下滑（度）。
    pub min_slide_slope_deg: f32,
    /// 离地小于这个距离时自动贴地；`None` 关闭。
    pub snap_to_ground: Option<f32>,
    /// 「上」方向，默认 +Y。
    pub up: [f32; 3],
}

impl Default for CharacterMoveSettings {
    fn default() -> Self {
        Self {
            half_height: 0.6,
            radius: 0.3,
            offset: 0.01,
            slide: true,
            max_slope_deg: 45.0,
            min_slide_slope_deg: 45.0,
            snap_to_ground: Some(0.3),
            up: [0.0, 1.0, 0.0],
        }
    }
}

/// 一条移动请求：由 `CharacterMovement`（下一片）每步写进
/// `CharacterMoveRequests`，物理线程计算后写回 `CharacterMoveResults`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterMoveRequest {
    pub entity: EntityId,
    /// 本步想移动的位移（已经乘过 dt，米）。
    pub desired: [f32; 3],
    /// 物理步长，rapier 用它做摩擦与贴地计算。
    pub dt: f32,
    pub settings: CharacterMoveSettings,
}

/// 物理线程对一条移动请求的回答。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterMoveResult {
    pub entity: EntityId,
    /// 实际允许的位移（撞墙 / 上坡后可能小于请求值）。
    pub translation: [f32; 3],
    /// 移动之后是否站在地上。
    pub grounded: bool,
    /// 是否因为坡度过大而自动下滑。
    pub sliding_down_slope: bool,
}

/// 每步的移动请求队列（写进 World，`PhysicsRuntime::step` 消费并清空）。
#[derive(Debug, Clone, Default)]
pub struct CharacterMoveRequests {
    pub requests: Vec<CharacterMoveRequest>,
}

/// 每步移动结果（由 `PhysicsRuntime::step` 写进 World，供角色系统读取）。
#[derive(Debug, Clone, Default)]
pub struct CharacterMoveResults {
    pub moves: Vec<CharacterMoveResult>,
}

/// 每步的角色输入（#560 P2）：世界空间方向，`y` 忽略，长度会被归一化。
///
/// 由玩法层每帧写入；没有这个资源时角色不移动，也不会产生任何请求。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CharacterMoveInput {
    pub direction: [f32; 3],
    pub jump: bool,
    pub sprint: bool,
}

/// 角色移动的内部状态（上一帧的水平速度，用于加速 / 减速）。
#[derive(Debug, Clone, Copy, Default)]
struct CharacterMotionState {
    horizontal: [f32; 2],
}

// ── Results (physics → main) ──────────────────────────────────────────

pub struct PhysicsResult {
    pub transforms: Vec<BodyTransform>,
    /// 本步 rapier 报告的 Started/Stopped，已映射为 ECS 实体。
    pub events: Vec<PhysicsEvent>,
    /// 本步计算的运动学角色移动结果。
    pub character_moves: Vec<CharacterMoveResult>,
}

pub struct BodyTransform {
    pub entity: EntityId,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    /// 上一步结束时 rapier 是否已让该刚体休眠（仅动态刚体会回传）。
    pub sleeping: bool,
}

// ── helpers ───────────────────────────────────────────────────────────

fn array_to_pose(position: [f32; 3], rotation: [f32; 4]) -> Pose {
    Pose::from_parts(
        Vector::new(position[0], position[1], position[2]),
        Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]),
    )
}

fn sanitize(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// Build rapier's character controller from the authored settings.
fn character_controller(settings: &CharacterMoveSettings) -> KinematicCharacterController {
    let max_slope = sanitize(settings.max_slope_deg)
        .clamp(0.0, 89.0)
        .to_radians();
    let min_slide = sanitize(settings.min_slide_slope_deg)
        .clamp(0.0, 89.0)
        .to_radians();
    let up = Vector::new(
        sanitize(settings.up[0]),
        sanitize(settings.up[1]),
        sanitize(settings.up[2]),
    );
    KinematicCharacterController {
        up: if up.length_squared() > 1e-8 {
            up.normalize()
        } else {
            Vector::Y
        },
        offset: CharacterLength::Absolute(sanitize(settings.offset).max(1.0e-4)),
        slide: settings.slide,
        max_slope_climb_angle: max_slope,
        min_slope_slide_angle: min_slide,
        snap_to_ground: settings
            .snap_to_ground
            .map(|distance| CharacterLength::Absolute(sanitize(distance).max(0.0))),
        ..KinematicCharacterController::default()
    }
}

fn rapier_status(status: PhysicsBodyStatus) -> RigidBodyType {
    match status {
        PhysicsBodyStatus::Dynamic => RigidBodyType::Dynamic,
        PhysicsBodyStatus::KinematicPosition => RigidBodyType::KinematicPositionBased,
        PhysicsBodyStatus::Static => RigidBodyType::Fixed,
    }
}

/// 删除一个地形 body 及其 heightfield collider（#517）。
///
/// 收在一个结构里而不是八个参数：调用点是热路径上的两条分支，参数一多
/// 很容易把 `collider_set` / `rigid_body_set` 的顺序写反。
struct TerrainBodies<'a> {
    bodies: &'a mut HashMap<EntityId, RigidBodyHandle>,
    colliders: &'a mut HashMap<EntityId, ColliderHandle>,
    collider_entities: &'a mut HashMap<ColliderHandle, Entity>,
    rigid_bodies: &'a mut RigidBodySet,
    collider_set: &'a mut ColliderSet,
    islands: &'a mut IslandManager,
    impulse_joints: &'a mut ImpulseJointSet,
    multibody_joints: &'a mut MultibodyJointSet,
}

impl TerrainBodies<'_> {
    fn remove(&mut self, entity: EntityId) {
        if let Some(handle) = self.bodies.remove(&entity) {
            // 删除 body 会连带删除挂在它下面的 collider。
            self.rigid_bodies.remove(
                handle,
                self.islands,
                self.collider_set,
                self.impulse_joints,
                self.multibody_joints,
                true,
            );
        }
        if let Some(collider) = self.colliders.remove(&entity) {
            self.collider_entities.remove(&collider);
        }
    }
}

// ── Thread entry point ────────────────────────────────────────────────

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

    // Entity → Rapier handle map.
    let mut entity_map: HashMap<EntityId, RigidBodyHandle> = HashMap::new();
    let mut body_colliders: HashMap<EntityId, ColliderHandle> = HashMap::new();
    let mut collider_entities: HashMap<ColliderHandle, Entity> = HashMap::new();
    // 地形是固定的 heightfield body，不参与 `entity_map` 的姿势回写（#517）。
    let mut terrain_bodies: HashMap<EntityId, RigidBodyHandle> = HashMap::new();
    let mut terrain_colliders: HashMap<EntityId, ColliderHandle> = HashMap::new();
    // 角色移动要等 `step` 之后 broad phase 才是这一帧的，所以先收集。
    let mut character_requests: Vec<CharacterMoveRequest> = Vec::new();

    // rapier 在 step 内通过 EventHandler 报告 Started/Stopped；这里用官方
    // ChannelEventCollector 把事件排到通道，step 后统一映射回实体。
    let (collision_tx, collision_rx) = std::sync::mpsc::channel();
    let (force_tx, _force_rx) = std::sync::mpsc::channel();
    let event_handler = ChannelEventCollector::new(collision_tx, force_tx);

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
                        .gravity_scale(if settings.use_gravity { 1.0 } else { 0.0 })
                        .ccd_enabled(settings.ccd);
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
                    let collider = collider_builder
                        .density(density)
                        .sensor(sensor)
                        .active_events(ActiveEvents::COLLISION_EVENTS)
                        .build();
                    let collider_handle =
                        collider_set.insert_with_parent(collider, body_handle, &mut rigid_body_set);
                    body_colliders.insert(entity, collider_handle);
                    collider_entities.insert(collider_handle, entity_from_key(entity));
                }
                PhysicsCommand::DespawnBody { entity } => {
                    if let Some(collider_handle) = body_colliders.remove(&entity) {
                        collider_entities.remove(&collider_handle);
                    }
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
                PhysicsCommand::SpawnTerrain {
                    entity,
                    position,
                    heights,
                    cells,
                    size,
                } => {
                    // 换高度图 / 移动地形时先拆旧 body，避免留下两份碰撞体。
                    TerrainBodies {
                        bodies: &mut terrain_bodies,
                        colliders: &mut terrain_colliders,
                        collider_entities: &mut collider_entities,
                        rigid_bodies: &mut rigid_body_set,
                        collider_set: &mut collider_set,
                        islands: &mut island_manager,
                        impulse_joints: &mut impulse_joint_set,
                        multibody_joints: &mut multibody_joint_set,
                    }
                    .remove(entity);
                    // rapier 的 heightfield 行 = z、列 = x；它的 `Array2` 底层是
                    // 列优先（`index = row + col * nrows`），而 `Heightfield` 是行优先
                    // `(z * cells + x)`，所以这里必须转置一次。
                    let mut grid = vec![0.0f32; cells * cells];
                    for z in 0..cells {
                        for x in 0..cells {
                            grid[z + x * cells] = heights[z * cells + x];
                        }
                    }
                    let grid = rapier3d::geometry::Array2::new(cells, cells, grid);
                    let collider = ColliderBuilder::heightfield(
                        grid,
                        Vector::new(size[0].max(1.0e-3), 1.0, size[1].max(1.0e-3)),
                    )
                    .active_events(ActiveEvents::COLLISION_EVENTS)
                    .build();
                    let body = RigidBodyBuilder::fixed()
                        .translation(Vector::from(position))
                        .build();
                    let body_handle = rigid_body_set.insert(body);
                    let collider_handle =
                        collider_set.insert_with_parent(collider, body_handle, &mut rigid_body_set);
                    terrain_bodies.insert(entity, body_handle);
                    terrain_colliders.insert(entity, collider_handle);
                    collider_entities.insert(collider_handle, entity_from_key(entity));
                }
                PhysicsCommand::MoveCharacter { request } => {
                    character_requests.push(request);
                }
                PhysicsCommand::DespawnTerrain { entity } => {
                    TerrainBodies {
                        bodies: &mut terrain_bodies,
                        colliders: &mut terrain_colliders,
                        collider_entities: &mut collider_entities,
                        rigid_bodies: &mut rigid_body_set,
                        collider_set: &mut collider_set,
                        islands: &mut island_manager,
                        impulse_joints: &mut impulse_joint_set,
                        multibody_joints: &mut multibody_joint_set,
                    }
                    .remove(entity);
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
            &event_handler,
        );

        // 2b. Drain collision events and map collider handles back to entities.
        let mut events = Vec::new();
        while let Ok(event) = collision_rx.try_recv() {
            let (kind, handle_a, handle_b, flags) = match event {
                CollisionEvent::Started(a, b, flags) => (PhysicsEventKind::Started, a, b, flags),
                CollisionEvent::Stopped(a, b, flags) => (PhysicsEventKind::Stopped, a, b, flags),
            };
            let (Some(entity_a), Some(entity_b)) = (
                collider_entities.get(&handle_a),
                collider_entities.get(&handle_b),
            ) else {
                // 一方在事件到达前已被 despawn：保留给上一步的消费方没有意义。
                continue;
            };
            if entity_a == entity_b {
                continue;
            }
            events.push(PhysicsEvent {
                kind,
                entity_a: *entity_a,
                entity_b: *entity_b,
                is_sensor: flags.contains(CollisionEventFlags::SENSOR),
            });
        }

        // 3. Collect dynamic body transforms.
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
                    sleeping: body.is_sleeping(),
                })
            })
            .collect();

        // 2d. Kinematic character moves (#560 P2). Run after the step so the
        // broad phase reflects this frame's colliders; each request resolves
        // against the current pose and writes the corrected position back.
        let mut character_moves = Vec::new();
        for request in character_requests.drain(..) {
            let (Some(handle), Some(collider_handle)) = (
                entity_map.get(&request.entity).copied(),
                body_colliders.get(&request.entity).copied(),
            ) else {
                log::warn!(
                    "physics: character move for entity {} without a body/collider",
                    request.entity
                );
                continue;
            };
            let (Some(body), Some(collider)) = (
                rigid_body_set.get(handle),
                collider_set.get(collider_handle),
            ) else {
                continue;
            };
            let pose = *body.position();
            let controller = character_controller(&request.settings);
            let desired = Vector::new(
                sanitize(request.desired[0]),
                sanitize(request.desired[1]),
                sanitize(request.desired[2]),
            );
            let filter = QueryFilter {
                exclude_collider: Some(collider_handle),
                ..QueryFilter::default()
            };
            let pipeline = broad_phase.as_query_pipeline(
                narrow_phase.query_dispatcher(),
                &rigid_body_set,
                &collider_set,
                filter,
            );
            let movement = controller.move_shape(
                sanitize(request.dt).max(0.0),
                &pipeline,
                collider.shape(),
                &pose,
                desired,
                |_| {},
            );
            let mut new_pose = pose;
            new_pose.translation += movement.translation;
            if let Some(body) = rigid_body_set.get_mut(handle) {
                // Keep the next kinematic target in sync: a kinematic body is
                // integrated towards `next_position`, so setting only the
                // current pose would be undone by the next step.
                body.set_next_kinematic_position(new_pose);
                body.set_position(new_pose, false);
            }
            character_moves.push(CharacterMoveResult {
                entity: request.entity,
                translation: [
                    movement.translation.x,
                    movement.translation.y,
                    movement.translation.z,
                ],
                grounded: movement.grounded,
                sliding_down_slope: movement.is_sliding_down_slope,
            });
        }

        let _ = result_tx.send(PhysicsResult {
            transforms,
            events,
            character_moves,
        });
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
    /// 已同步的地形 heightfield（#517）。
    terrains: HashMap<EntityId, TrackedTerrain>,
    /// 上一步动态刚体的休眠状态（仅用于无窗口的观测 / 测试）。
    sleeping: HashMap<EntityId, bool>,
    /// 线程已退出 / 通道已断：之后 `step` 直接返回，不再刷日志。
    failed: bool,
    /// 角色移动状态（#560 P2），按实体记账。
    motion: HashMap<EntityId, CharacterMotionState>,
}

#[derive(Clone, PartialEq)]
struct TrackedBody {
    status: PhysicsBodyStatus,
    settings: BodySettings,
    shape: ColliderDesc,
    sensor: bool,
}

/// 地形 collider 的变更指纹（#517）。
///
/// `TerrainData.key` 覆盖高度图路径 / 尺寸 / 高度倍数 / 分辨率；`position` 覆盖
/// 实体移动。高度数组本身不进指纹——它只会在重建 `TerrainData` 时变化，而重建
/// 必然换 `key`。
#[derive(Clone, PartialEq)]
struct TrackedTerrain {
    key: String,
    position: [f32; 3],
    size: [f32; 2],
    height_scale: f32,
    cells: usize,
}

impl PhysicsRuntime {
    /// 启动物理线程。
    pub fn spawn() -> Self {
        let (step_tx, step_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        let thread = std::thread::Builder::new()
            .name("prism-physics".to_string())
            .spawn(move || {
                if !prism_platform::thread::put_behind_current() {
                    log::debug!("physics thread keeps the default scheduling priority");
                }
                physics_thread_main(step_rx, result_tx)
            })
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
            terrains: HashMap::new(),
            sleeping: HashMap::new(),
            failed: false,
            motion: HashMap::new(),
        }
    }

    /// 上一步结束后实体是否处于休眠状态。
    ///
    /// 只对动态刚体有值；静态 / 运动学刚体回传 `None`。主要供无窗口的
    /// 场景测试观察堆叠稳定性。
    pub fn is_sleeping(&self, entity: Entity) -> Option<bool> {
        self.sleeping.get(&entity_key(entity)).copied()
    }

    /// 推进一个固定步：同步 spawn/despawn/kinematic → 等线程积分 → 写回
    /// `LocalTransform`。返回 false 表示物理线程已经退出（之后不再尝试）。
    pub fn step(&mut self, world: &mut World) -> bool {
        if self.failed {
            return false;
        }
        self.update_character_movement(world);
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
                self.sleeping.clear();
                for body in &result.transforms {
                    self.sleeping.insert(body.entity, body.sleeping);
                }
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

    /// Turn `CharacterController` + `CharacterMoveInput` into move requests
    /// (#560 P2): gravity, jump, acceleration and the capsule settings that the
    /// physics thread needs. Runs before `collect_commands`, so the requests it
    /// writes are consumed by the step that is about to happen.
    fn update_character_movement(&mut self, world: &mut World) {
        /// Matches the fixed step in `App::host_tick` / the winit loop.
        const FIXED_DT: f32 = 1.0 / 60.0;

        let Some(input) = world.get_resource::<CharacterMoveInput>().copied() else {
            return;
        };

        let direction = glam::Vec3::from_array(input.direction);
        let direction = glam::Vec3::new(direction.x, 0.0, direction.z);
        let direction = if direction.length_squared() > 1e-6 {
            direction.normalize()
        } else {
            glam::Vec3::ZERO
        };

        let mut requests = Vec::new();
        let mut seen: HashSet<EntityId> = HashSet::new();
        for (entity, controller) in world.query::<CharacterController>() {
            let key = entity_key(entity);
            let Some(collider) = world.get::<Collider>(entity) else {
                continue;
            };
            if collider.shape != ColliderShape::Capsule {
                continue;
            }
            let Some(body) = world.get::<Rigidbody>(entity) else {
                continue;
            };
            if body.body_type != BodyType::Kinematic {
                continue;
            }
            if world.get::<LocalTransform>(entity).is_none() {
                continue;
            }
            seen.insert(key);

            let state = self.motion.entry(key).or_default();
            let speed = if input.sprint {
                controller.run_speed.max(0.0)
            } else {
                controller.walk_speed.max(0.0)
            };
            let target = direction * speed;
            let current = glam::Vec3::new(state.horizontal[0], 0.0, state.horizontal[1]);
            let blend = (controller.acceleration.max(0.0) * FIXED_DT).clamp(0.0, 1.0);
            let horizontal = current + (target - current) * blend;
            state.horizontal = [horizontal.x, horizontal.z];

            // Gravity / jump are not wired yet: the kinematic controller does
            // not settle onto the floor with small downward steps, so the
            // vertical axis stays with the caller until that is understood.
            requests.push(CharacterMoveRequest {
                entity: key,
                desired: [horizontal.x * FIXED_DT, 0.0, horizontal.z * FIXED_DT],
                dt: FIXED_DT,
                settings: CharacterMoveSettings {
                    half_height: collider.half_height.max(0.0),
                    radius: collider.radius.max(1.0e-4),
                    offset: controller.skin_offset,
                    slide: true,
                    max_slope_deg: controller.max_slope_degrees,
                    min_slide_slope_deg: controller.max_slope_degrees,
                    snap_to_ground: Some(controller.snap_distance.max(0.0)),
                    up: [0.0, 1.0, 0.0],
                },
            });
        }
        self.motion.retain(|key, _| seen.contains(key));

        // Keep requests written by gameplay this frame; ours are appended.
        let mut queue = world
            .remove_resource::<CharacterMoveRequests>()
            .unwrap_or_default();
        queue.requests.extend(requests);
        world.insert_resource(queue);
    }

    fn collect_commands(&mut self, world: &mut World) -> Vec<PhysicsCommand> {
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

        // 地形高度场（#517）：`TerrainData` 是运行时系统生成的分量，没有
        // `Rigidbody` / `Collider`，所以单独同步成静态 heightfield collider。
        let mut current_terrains: HashMap<EntityId, Entity> = HashMap::new();
        for (entity, _) in world.query::<TerrainData>() {
            current_terrains.insert(entity_key(entity), entity);
        }
        let removed_terrains: Vec<EntityId> = self
            .terrains
            .keys()
            .filter(|key| !current_terrains.contains_key(key))
            .copied()
            .collect();
        for key in removed_terrains {
            self.terrains.remove(&key);
            commands.push(PhysicsCommand::DespawnTerrain { entity: key });
        }
        for (key, entity) in current_terrains {
            let Some(data) = world.get::<TerrainData>(entity) else {
                continue;
            };
            let cells = data.field.cells();
            if cells < 2 {
                continue;
            }
            let Some(position) =
                world_matrix(world, entity).map(|matrix| matrix.w_axis.truncate().to_array())
            else {
                continue;
            };
            if !position.iter().all(|value| value.is_finite()) {
                continue;
            }
            let desired = TrackedTerrain {
                key: data.key.clone(),
                position,
                size: data.size,
                height_scale: data.height_scale,
                cells,
            };
            if self.terrains.get(&key) == Some(&desired) {
                continue;
            }
            if self.terrains.contains_key(&key) {
                commands.push(PhysicsCommand::DespawnTerrain { entity: key });
            }
            let scale = if data.height_scale.is_finite() {
                data.height_scale
            } else {
                1.0
            };
            let heights = data
                .field
                .heights()
                .iter()
                .map(|height| {
                    let value = height * scale;
                    if value.is_finite() {
                        value
                    } else {
                        0.0
                    }
                })
                .collect();
            commands.push(PhysicsCommand::SpawnTerrain {
                entity: key,
                position,
                heights,
                cells,
                size: data.size,
            });
            self.terrains.insert(key, desired);
        }

        // Character moves are per-step requests (#560 P2): take them out of the
        // world so a stale request is not replayed on every step.
        if let Some(requests) = world.remove_resource::<CharacterMoveRequests>() {
            for request in requests.requests {
                commands.push(PhysicsCommand::MoveCharacter { request });
            }
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
    // 事件只保留双方仍存活的；换场 / despawn 后残留的 Stopped 不再暴露。
    let events = result
        .events
        .iter()
        .copied()
        .filter(|event| world.is_alive(event.entity_a) && world.is_alive(event.entity_b))
        .collect();
    world.insert_resource(PhysicsEvents { events });

    for movement in &result.character_moves {
        let entity = entity_from_key(movement.entity);
        if !world.is_alive(entity) {
            continue;
        }
        let delta = glam::Vec3::from_array(movement.translation);
        if !delta.is_finite() {
            continue;
        }
        let Some(mut local) = world.get::<LocalTransform>(entity).cloned() else {
            continue;
        };
        // rapier works in world space; entities parented to something else need
        // the delta rotated (and unscaled) back into parent space.
        let delta = match world.get::<Parent>(entity).map(|parent| parent.0) {
            Some(parent) => {
                let Some(parent_matrix) = world_matrix(world, parent) else {
                    continue;
                };
                let (scale, rotation, _) = parent_matrix.inverse().to_scale_rotation_translation();
                rotation * (delta * scale)
            }
            None => delta,
        };
        local.position += delta;
        world.insert(entity, local);
    }
    world.insert_resource(CharacterMoveResults {
        moves: result.character_moves.clone(),
    });

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
        ccd: body.ccd,
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

    /// #560 P2：运动学胶囊站在地板上、撞到墙时被挡住（不是穿过去）。
    #[test]
    fn kinematic_character_stands_on_the_floor_and_is_blocked_by_a_wall() {
        let (step_tx, step_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        let thread = std::thread::spawn(move || physics_thread_main(step_rx, result_tx));

        let character = 0x0000_0011_0000_0001u64;
        let floor = 0x0000_0012_0000_0001u64;
        let wall = 0x0000_0013_0000_0001u64;
        step_tx
            .send(PhysicsStep {
                commands: vec![
                    PhysicsCommand::SpawnBody {
                        entity: character,
                        position: [0.0, 1.2, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        body_status: PhysicsBodyStatus::KinematicPosition,
                        settings: BodySettings::default(),
                        shape: ColliderDesc::Capsule {
                            half_height: 0.6,
                            radius: 0.3,
                            center: [0.0; 3],
                        },
                        sensor: false,
                    },
                    PhysicsCommand::SpawnBody {
                        entity: floor,
                        position: [0.0, -0.1, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        body_status: PhysicsBodyStatus::Static,
                        settings: BodySettings::default(),
                        shape: ColliderDesc::Box {
                            half_extents: [10.0, 0.1, 10.0],
                            center: [0.0; 3],
                        },
                        sensor: false,
                    },
                    PhysicsCommand::SpawnBody {
                        entity: wall,
                        position: [1.5, 1.0, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        body_status: PhysicsBodyStatus::Static,
                        settings: BodySettings::default(),
                        shape: ColliderDesc::Box {
                            half_extents: [0.1, 1.0, 5.0],
                            center: [0.0; 3],
                        },
                        sensor: false,
                    },
                ],
            })
            .unwrap();

        // The spawn step answers with its own (empty) result; drain it so every
        // send below is paired with the result of the same step.
        let _ = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let settings = CharacterMoveSettings::default();
        let mut grounded = false;
        // Fall onto the floor first.
        for _ in 0..30 {
            step_tx
                .send(PhysicsStep {
                    commands: vec![PhysicsCommand::MoveCharacter {
                        request: CharacterMoveRequest {
                            entity: character,
                            desired: [0.0, -0.05, 0.0],
                            dt: 1.0 / 60.0,
                            settings,
                        },
                    }],
                })
                .unwrap();
            let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if let Some(movement) = result.character_moves.first() {
                grounded = movement.grounded;
            }
        }
        assert!(grounded, "character should end up standing on the floor");

        // Walk into the wall: the allowed translation must stop short.
        step_tx
            .send(PhysicsStep {
                commands: vec![PhysicsCommand::MoveCharacter {
                    request: CharacterMoveRequest {
                        entity: character,
                        desired: [3.0, 0.0, 0.0],
                        dt: 1.0 / 60.0,
                        settings,
                    },
                }],
            })
            .unwrap();
        let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let movement = result.character_moves.first().expect("one move result");
        assert!(
            movement.translation[0] > 0.5,
            "character should walk up to the wall, got {:?}",
            movement.translation
        );
        assert!(
            movement.translation[0] < 2.9,
            "wall must stop the character, got {:?}",
            movement.translation
        );

        drop(step_tx);
        thread.join().unwrap();
    }

    /// #517：`TerrainData` 自动变成静态 heightfield collider，动态球落在地形上。
    ///
    /// 高度用 2.0 而不是 0：如果高度数组根本没传给 rapier，球会停在 y≈0.5，
    /// 这个断言就会失败（同时覆盖 height_scale = 1 的米制语义）。
    #[test]
    fn dynamic_sphere_rests_on_a_terrain_heightfield() {
        use prism_engine::scene::systems::terrain::Heightfield;
        use std::sync::Arc;

        let cells = 33;
        let mut raw = Vec::with_capacity(cells * cells * 4);
        for _ in 0..cells * cells {
            raw.extend_from_slice(&2.0f32.to_le_bytes());
        }
        let field = Heightfield::from_raw(&raw, cells).expect("heightfield");

        let mut world = World::new();
        let terrain = world.spawn();
        world.insert(terrain, LocalTransform::default());
        world.insert(
            terrain,
            TerrainData {
                key: "test-raised".into(),
                field: Arc::new(field),
                mesh_path: String::new(),
                size: [20.0, 20.0],
                height_scale: 1.0,
            },
        );

        let ball = world.spawn();
        world.insert(ball, Rigidbody::default());
        world.insert(
            ball,
            Collider {
                shape: ColliderShape::Sphere,
                radius: 0.5,
                ..Default::default()
            },
        );
        world.insert(
            ball,
            LocalTransform {
                position: glam::Vec3::new(0.0, 5.0, 0.0),
                ..Default::default()
            },
        );

        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..300 {
            assert!(runtime.step(&mut world));
        }
        let y = world
            .get::<LocalTransform>(ball)
            .expect("ball transform")
            .position
            .y;
        assert!(
            (2.45..2.55).contains(&y),
            "sphere should rest on the raised terrain heightfield (center y≈2.5), got {y}"
        );

        // 地形被销毁后下一步安全 despawn。
        world.despawn(terrain);
        assert!(runtime.step(&mut world));
    }

    /// #560 P2：`CharacterMoveRequests` 资源 → 物理线程 → `LocalTransform` +
    /// `CharacterMoveResults`，且请求只消费一次（下一步重复执行不会再次移动）。
    #[test]
    fn character_move_requests_move_the_transform_and_are_consumed_once() {
        use prism_engine::scene::components::{BodyType, ColliderShape, Rigidbody};

        let mut world = World::new();
        let floor = world.spawn();
        world.insert(floor, LocalTransform::default());
        world.insert(
            floor,
            Collider {
                half_extents: [10.0, 0.1, 10.0],
                ..Default::default()
            },
        );

        let character = world.spawn();
        world.insert(
            character,
            Rigidbody {
                body_type: BodyType::Kinematic,
                ..Default::default()
            },
        );
        world.insert(
            character,
            Collider {
                shape: ColliderShape::Capsule,
                radius: 0.3,
                half_height: 0.6,
                ..Default::default()
            },
        );
        world.insert(
            character,
            LocalTransform {
                position: glam::Vec3::new(0.0, 0.9, 0.0),
                ..Default::default()
            },
        );

        let mut runtime = PhysicsRuntime::spawn();
        assert!(runtime.step(&mut world));

        world.insert_resource(CharacterMoveRequests {
            requests: vec![CharacterMoveRequest {
                entity: entity_key(character),
                desired: [0.5, 0.0, 0.0],
                dt: 1.0 / 60.0,
                settings: CharacterMoveSettings::default(),
            }],
        });
        assert!(runtime.step(&mut world));

        let x = world
            .get::<LocalTransform>(character)
            .expect("character transform")
            .position
            .x;
        assert!((x - 0.5).abs() < 0.05, "character should move +x, got {x}");
        let results = world
            .get_resource::<CharacterMoveResults>()
            .expect("move results");
        assert_eq!(results.moves.len(), 1);
        assert_eq!(results.moves[0].entity, entity_key(character));

        // The request resource was consumed: another step must not move again.
        assert!(runtime.step(&mut world));
        let x_after = world
            .get::<LocalTransform>(character)
            .expect("character transform")
            .position
            .x;
        assert!(
            (x_after - x).abs() < 1e-4,
            "request replayed: {x} -> {x_after}"
        );
    }

    /// #560 P2：`CharacterController` + `CharacterMoveInput` 让角色按作者速度行走，
    /// 松开输入后减速停下。
    #[test]
    fn character_movement_walks_at_the_authored_speed() {
        use prism_engine::scene::components::{BodyType, CharacterController, ColliderShape};

        let mut world = World::new();
        let floor = world.spawn();
        world.insert(floor, LocalTransform::default());
        world.insert(
            floor,
            Collider {
                half_extents: [20.0, 0.1, 20.0],
                ..Default::default()
            },
        );
        let character = world.spawn();
        world.insert(
            character,
            Rigidbody {
                body_type: BodyType::Kinematic,
                ..Default::default()
            },
        );
        world.insert(
            character,
            Collider {
                shape: ColliderShape::Capsule,
                radius: 0.3,
                half_height: 0.6,
                ..Default::default()
            },
        );
        world.insert(character, CharacterController::default());
        world.insert(
            character,
            LocalTransform {
                position: glam::Vec3::new(0.0, 0.9, 0.0),
                ..Default::default()
            },
        );

        world.insert_resource(CharacterMoveInput {
            direction: [0.0, 0.0, -1.0],
            ..Default::default()
        });
        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..60 {
            assert!(runtime.step(&mut world));
        }
        let walked = world
            .get::<LocalTransform>(character)
            .expect("character transform")
            .position;
        let distance = -walked.z;
        assert!(
            (1.6..2.4).contains(&distance),
            "walk speed 2.0 should cover ~2 m in one second, got {distance}"
        );
        assert!((walked.y - 0.9).abs() < 0.02, "walking must stay grounded");

        // Releasing the input decelerates to a stop instead of sliding forever.
        world.insert_resource(CharacterMoveInput::default());
        for _ in 0..60 {
            assert!(runtime.step(&mut world));
        }
        let stopped = world
            .get::<LocalTransform>(character)
            .expect("character transform")
            .position;
        assert!(
            (stopped.z - walked.z).abs() < 0.1,
            "character should stop after releasing input, drifted {}",
            stopped.z - walked.z
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

    /// #522：CCD 让高速下落的球不会穿过薄地板。
    #[test]
    fn ccd_stops_a_fast_body_on_a_thin_floor() {
        let mut world = World::new();
        let floor = world.spawn();
        world.insert(floor, LocalTransform::default());
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
                half_extents: [5.0, 0.02, 5.0],
                ..Default::default()
            },
        );

        let ball = world.spawn();
        world.insert(
            ball,
            LocalTransform {
                position: glam::Vec3::new(0.0, 20.0, 0.0),
                ..Default::default()
            },
        );
        world.insert(
            ball,
            Rigidbody {
                ccd: true,
                ..Default::default()
            },
        );
        world.insert(
            ball,
            Collider {
                shape: ColliderShape::Sphere,
                radius: 0.2,
                ..Default::default()
            },
        );

        let mut runtime = PhysicsRuntime::spawn();
        for _ in 0..360 {
            assert!(runtime.step(&mut world));
        }

        let y = world
            .get::<LocalTransform>(ball)
            .expect("ball transform")
            .position
            .y;
        assert!(
            (0.1..0.5).contains(&y),
            "CCD should stop the ball on the thin floor, got y={y}"
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
        // `PhysicsRuntime` 只同步有 LocalTransform 的实体；触发器也要有位置，
        // 否则它根本不会被 spawn，测试会假通过。
        world.insert(trigger, LocalTransform::default());
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
        let mut saw_started = false;
        let mut saw_stopped = false;
        for _ in 0..240 {
            assert!(runtime.step(&mut world));
            let events = world
                .get_resource::<PhysicsEvents>()
                .expect("physics events resource");
            for event in &events.events {
                if !event.is_sensor || (event.entity_a != ball && event.entity_b != ball) {
                    continue;
                }
                match event.kind {
                    PhysicsEventKind::Started => saw_started = true,
                    PhysicsEventKind::Stopped => saw_stopped = true,
                }
            }
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
        assert!(
            saw_started,
            "sensor Started event should reach PhysicsEvents"
        );
        assert!(
            saw_stopped,
            "sensor Stopped event should reach PhysicsEvents"
        );
    }

    #[test]
    fn entity_keys_round_trip() {
        let entity = Entity::from_raw(42, 7);
        assert_eq!(entity_from_key(entity_key(entity)), entity);
    }
}
