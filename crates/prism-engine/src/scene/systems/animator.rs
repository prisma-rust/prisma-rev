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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use prism_anim::{
    palette_to_arrays, AnimGraphEvaluator, AnimGraphSource, AnimParameters, BlendLayer,
    BlendSample, BlendSample2D, BlendSpace1D, BlendSpace2D, BlendStack, BoneMask, Clip,
    GraphEvalError, GraphNodeSnapshot, Montage, Pose, Skeleton, StateMachine,
};
use prism_asset::formats_anim_graph::AnimGraphInfo;
use prism_ecs::{Entity, World};
use prism_scene::components::{Animator, Camera, LocalToWorld, LocalTransform, SkeletonRoot};

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

    /// 剪辑里的 Notify（时间 + 名字，按时间升序）；没加载过的路径返回空。
    pub fn clip_notifies(&self, path: &str) -> Vec<(f32, String)> {
        self.clips
            .get(path)
            .map(|clip| {
                clip.notifies
                    .iter()
                    .map(|notify| (notify.time, notify.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 剪辑里出现的关键帧时间点（升序去重）；没加载过的路径返回空。
    pub fn clip_key_times(&self, path: &str) -> Vec<f32> {
        self.clips
            .get(path)
            .map(|clip| clip.key_times())
            .unwrap_or_default()
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

/// 本帧触发的一个关键帧事件（#560 P1 Notify）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimEvent {
    pub entity: Entity,
    /// 事件名，由项目解释（`Footstep.L` / `Traversal.Window` / …）。
    pub name: String,
    /// 事件在剪辑里的时间（秒）。
    pub time: f32,
}

/// 每个实体动画图节点的运行时摘要（World 资源，#560 实时调试）。
///
/// **opt-in**：世界里有这个资源时引擎才每帧重建它；没有就不做额外的字符串拷贝。
/// 键是实体，值是 [`GraphNodeSnapshot`] 列表（顺序与图节点一致）。冻结 / 降频的
/// 实体保留上一次快照，方便面板显示「停在哪」。
#[derive(Debug, Clone, Default)]
pub struct AnimGraphDebug {
    pub entities: HashMap<Entity, Vec<GraphNodeSnapshot>>,
}

impl AnimGraphDebug {
    /// 某个实体的节点快照；不是图实体返回 `None`。
    pub fn states(&self, entity: Entity) -> Option<&[GraphNodeSnapshot]> {
        self.entities.get(&entity).map(Vec::as_slice)
    }
}

/// 一次 `animator_system` 调用里触发的事件（World 资源）。
///
/// 每个 tick **整体重建**（不是累加）：消费者在 `update` 之后读到的永远只是这一帧
/// 跨过的时间点，不会重复播上一帧的脚步声。要跨帧缓存就自己 `drain` 走。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnimEvents {
    pub events: Vec<AnimEvent>,
}

impl AnimEvents {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// 取走这一帧的事件（资源本身留空）。
    pub fn drain(&mut self) -> Vec<AnimEvent> {
        std::mem::take(&mut self.events)
    }

    /// 某个实体这一帧触发的事件名。
    pub fn names_for(&self, entity: Entity) -> impl Iterator<Item = &str> {
        self.events
            .iter()
            .filter(move |event| event.entity == entity)
            .map(|event| event.name.as_str())
    }
}

/// 已解码的状态机，按 `.rasm` 路径索引（#560 P1）。
///
/// 与 [`SkeletonLibrary`] 分开：状态机是可选的（多数实体只用单剪辑 / 混合空间），
/// 没挂状态机的场景不该被迫建一张空表。
#[derive(Debug, Default)]
pub struct StateMachineLibrary {
    machines: HashMap<String, Arc<StateMachine>>,
}

impl StateMachineLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// 存入状态机；返回 `true` 表示这是新路径。
    pub fn insert(&mut self, path: impl Into<String>, machine: StateMachine) -> bool {
        self.machines
            .insert(path.into(), Arc::new(machine))
            .is_none()
    }

    pub fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>> {
        self.machines.get(path).cloned()
    }

    pub fn has_state_machine(&self, path: &str) -> bool {
        self.machines.contains_key(path)
    }

    /// 所有已加载的状态机（资产解析在机器加载完之后要靠它补请求状态剪辑）。
    pub fn all(&self) -> impl Iterator<Item = &Arc<StateMachine>> {
        self.machines.values()
    }

    pub fn len(&self) -> usize {
        self.machines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.machines.is_empty()
    }
}

/// 已解码的动画图（`.rag` / `.animgraph.json`），按资产路径索引（#560 P1）。
#[derive(Debug, Default)]
pub struct AnimGraphLibrary {
    graphs: HashMap<String, Arc<AnimGraphInfo>>,
}

impl AnimGraphLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: impl Into<String>, graph: AnimGraphInfo) -> bool {
        self.graphs.insert(path.into(), Arc::new(graph)).is_none()
    }

    pub fn graph(&self, path: &str) -> Option<Arc<AnimGraphInfo>> {
        self.graphs.get(path).cloned()
    }

    pub fn has_graph(&self, path: &str) -> bool {
        self.graphs.contains_key(path)
    }

    pub fn all(&self) -> impl Iterator<Item = (&String, &Arc<AnimGraphInfo>)> {
        self.graphs.iter()
    }

    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }
}

/// 每个实体的图求值器（运行时资源，不进场景）。
#[derive(Debug, Default)]
struct AnimGraphRuntimes {
    runtimes: HashMap<Entity, GraphRuntime>,
}

#[derive(Debug)]
struct GraphRuntime {
    path: String,
    evaluator: AnimGraphEvaluator,
}

impl AnimGraphRuntimes {
    fn get_or_create(
        &mut self,
        entity: Entity,
        path: &str,
        graph: Arc<AnimGraphInfo>,
    ) -> &mut AnimGraphEvaluator {
        let slot = self.runtimes.entry(entity).or_insert_with(|| GraphRuntime {
            path: path.to_string(),
            evaluator: AnimGraphEvaluator::new((*graph).clone()),
        });
        if slot.path != path {
            slot.path = path.to_string();
            slot.evaluator = AnimGraphEvaluator::new((*graph).clone());
        }
        &mut slot.evaluator
    }
}

/// 一个实体本帧的图求值工作项（#560 P6）。
///
/// 求值器在收集阶段从 [`AnimGraphRuntimes`] 里取出来、求值完再放回，这样每个
/// 工作项独占自己的求值器，可以分给不同线程。
struct GraphPoseJob {
    entity: Entity,
    skeleton: Arc<Skeleton>,
    runtime: GraphRuntime,
    /// 这一帧该推进的时间（LOD 降频帧是 `2 * dt`）。
    dt: f32,
    result: Option<Result<(Pose, Vec<AnimEvent>), GraphEvalError>>,
}

impl GraphPoseJob {
    fn evaluate(&mut self, library: &LibraryView, dt: f32) {
        let entity = self.entity;
        let evaluated = self
            .runtime
            .evaluator
            .evaluate_with_events(library, &self.skeleton, dt);
        self.result = Some(evaluated.map(|(pose, events)| {
            let events = events
                .into_iter()
                .map(|event| AnimEvent {
                    entity,
                    name: event.name,
                    time: event.time,
                })
                .collect();
            (pose, events)
        }));
    }
}

/// 先顺序应用 Montage 请求 / 参数，再把各实体的图求值分给工作线程（#560 P6）。
///
/// 每个图求值器只属于一个实体，彼此不共享状态；`LibraryView` 只读且全是
/// `Arc`，可以安全地在多个线程里同时借用。少于两个实体或只有单核时不建线程，
/// 直接顺序求值 —— 线程开销比这点工作在单角色场景里更贵。
fn evaluate_graph_poses(
    world: &World,
    library: &LibraryView,
    graph_runtimes: &mut AnimGraphRuntimes,
    params: &AnimParameters,
    montage_requests: &mut HashMap<Entity, Vec<MontageRequest>>,
    graph_entities: &mut HashSet<Entity>,
    plans: &HashMap<Entity, f32>,
) -> HashMap<Entity, Result<(Pose, Vec<AnimEvent>), GraphEvalError>> {
    let mut jobs: Vec<GraphPoseJob> = Vec::new();
    for (entity, animator) in world.query::<Animator>() {
        let path = animator.anim_graph_path.trim();
        if path.is_empty() {
            continue;
        }
        let Some(root) = world.get::<SkeletonRoot>(entity) else {
            continue;
        };
        let Some(skeleton) = library.skeleton(&root.skeleton_path) else {
            continue;
        };
        let Some(graph) = library.graph(path) else {
            continue;
        };
        graph_entities.insert(entity);
        // 冻结 / 跳帧的实体保留求值器（状态机 / Montage 进度回来接着走），这一帧不推进。
        let Some(&entity_dt) = plans.get(&entity) else {
            continue;
        };
        let evaluator = graph_runtimes.get_or_create(entity, path, graph);
        if let Some(requests) = montage_requests.remove(&entity) {
            apply_montage_requests(evaluator, requests, &library.clips);
        }
        // 项目侧写入的 float / bool / trigger 同步进图求值器。
        for parameter in evaluator.graph().parameters.clone() {
            match parameter.kind {
                prism_asset::formats_anim_graph::GraphParameterKind::Float => {
                    if let Some(value) = params.float(&parameter.name) {
                        evaluator.parameters_mut().set_float(parameter.name, value);
                    }
                }
                prism_asset::formats_anim_graph::GraphParameterKind::Bool => {
                    if let Some(value) = params.bool(&parameter.name) {
                        evaluator.parameters_mut().set_bool(parameter.name, value);
                    }
                }
                prism_asset::formats_anim_graph::GraphParameterKind::Trigger => {
                    if params.has_trigger(&parameter.name) {
                        evaluator.parameters_mut().set_trigger(parameter.name);
                    }
                }
            }
        }
        let Some(runtime) = graph_runtimes.runtimes.remove(&entity) else {
            continue;
        };
        jobs.push(GraphPoseJob {
            entity,
            skeleton,
            runtime,
            dt: entity_dt,
            result: None,
        });
    }

    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(jobs.len())
        .min(MAX_GRAPH_EVAL_THREADS);
    prism_ecs::jobs::scoped_parallel_chunks(&mut jobs, threads, |job| {
        job.evaluate(library, job.dt);
    });

    let mut results = HashMap::with_capacity(jobs.len());
    for job in jobs {
        let GraphPoseJob {
            entity,
            runtime,
            result,
            ..
        } = job;
        if let Some(result) = result {
            results.insert(entity, result);
        }
        graph_runtimes.runtimes.insert(entity, runtime);
    }
    results
}

/// 图求值最多用几个工作线程；超过角色数之后再多线程也不涨吞吐。
const MAX_GRAPH_EVAL_THREADS: usize = 8;

/// 一次 Montage 请求的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MontageAction {
    /// 立即播放并清空该 Slot 的队列。
    PlayNow,
    /// 排到该 Slot 的队尾；空闲时立即开始。
    Enqueue,
    /// 停止该 Slot 并清空队列。
    Stop,
    /// 当前段播完后停止，不接下一段。
    StopAfterCurrent,
    /// 暂停 Slot 的活动段（保留时间 / 队列 / 权重）。
    Pause,
    /// 恢复 Slot 的活动段。
    Resume,
}

/// 项目侧对某个实体 Slot 发起的 Montage 请求（#560 P1）。
///
/// 请求只活一帧：加入 [`AnimMontageRequests`] 后，下一次 [`animator_system`] 会消费它。
/// 剪辑路径必须在 [`SkeletonLibrary`] 里已经加载；实体必须正在驱动动画图，且图里有
/// 同名 Slot 节点，否则请求会被忽略并打日志。
#[derive(Debug, Clone, PartialEq)]
pub struct MontageRequest {
    pub entity: Entity,
    pub slot: String,
    pub action: MontageAction,
    pub clip_path: String,
    pub looped: bool,
    pub speed: f32,
    pub weight: f32,
    pub start_time: f32,
}

impl MontageRequest {
    /// 立即播放。默认单次、速度 1、权重 1、从 0 开始。
    pub fn play(entity: Entity, slot: impl Into<String>, clip_path: impl Into<String>) -> Self {
        Self {
            entity,
            slot: slot.into(),
            action: MontageAction::PlayNow,
            clip_path: clip_path.into(),
            looped: false,
            speed: 1.0,
            weight: 1.0,
            start_time: 0.0,
        }
    }

    /// 排队播放。默认单次、速度 1、权重 1、从 0 开始。
    pub fn enqueue(entity: Entity, slot: impl Into<String>, clip_path: impl Into<String>) -> Self {
        Self {
            action: MontageAction::Enqueue,
            ..Self::play(entity, slot, clip_path)
        }
    }

    /// 暂停 Slot 的活动段。
    pub fn pause(entity: Entity, slot: impl Into<String>) -> Self {
        Self {
            action: MontageAction::Pause,
            ..Self::play(entity, slot, "")
        }
    }

    /// 恢复 Slot 的活动段。
    pub fn resume(entity: Entity, slot: impl Into<String>) -> Self {
        Self {
            action: MontageAction::Resume,
            ..Self::play(entity, slot, "")
        }
    }

    /// 停止 Slot 的活动段与队列。
    pub fn stop(entity: Entity, slot: impl Into<String>) -> Self {
        Self {
            action: MontageAction::Stop,
            ..Self::play(entity, slot, "")
        }
    }

    /// 当前段播完后停止。
    pub fn stop_after_current(entity: Entity, slot: impl Into<String>) -> Self {
        Self {
            action: MontageAction::StopAfterCurrent,
            ..Self::play(entity, slot, "")
        }
    }

    pub fn with_looping(mut self, looped: bool) -> Self {
        self.looped = looped;
        self
    }

    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }

    pub fn with_start_time(mut self, start_time: f32) -> Self {
        self.start_time = start_time;
        self
    }
}

/// 每帧由项目侧填充、由 [`animator_system`] 消费的 Montage 请求队列。
#[derive(Debug, Default)]
pub struct AnimMontageRequests {
    requests: Vec<MontageRequest>,
}

impl AnimMontageRequests {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, request: MontageRequest) {
        self.requests.push(request);
    }

    pub fn play(&mut self, entity: Entity, slot: impl Into<String>, clip_path: impl Into<String>) {
        self.push(MontageRequest::play(entity, slot, clip_path));
    }

    pub fn enqueue(
        &mut self,
        entity: Entity,
        slot: impl Into<String>,
        clip_path: impl Into<String>,
    ) {
        self.push(MontageRequest::enqueue(entity, slot, clip_path));
    }

    pub fn stop(&mut self, entity: Entity, slot: impl Into<String>) {
        self.push(MontageRequest::stop(entity, slot));
    }

    pub fn stop_after_current(&mut self, entity: Entity, slot: impl Into<String>) {
        self.push(MontageRequest::stop_after_current(entity, slot));
    }

    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    pub fn requests(&self) -> &[MontageRequest] {
        &self.requests
    }

    /// 取走所有待处理请求（系统每帧调用）。
    pub fn take(&mut self) -> Vec<MontageRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// 把一帧的请求应用到实体的图求值器。
fn apply_montage_requests(
    evaluator: &mut AnimGraphEvaluator,
    requests: Vec<MontageRequest>,
    clips: &HashMap<String, Arc<Clip>>,
) {
    for request in requests {
        let slot = request.slot.trim();
        if slot.is_empty() {
            log::warn!("animator: montage request has an empty slot name");
            continue;
        }
        match request.action {
            MontageAction::Stop => evaluator.stop_montage(slot),
            MontageAction::StopAfterCurrent => evaluator.stop_montage_after_current(slot),
            MontageAction::Pause => {
                if !evaluator.pause_montage(slot) {
                    log::warn!("animator: montage pause on unknown slot '{slot}'");
                }
            }
            MontageAction::Resume => {
                if !evaluator.resume_montage(slot) {
                    log::warn!("animator: montage resume on unknown slot '{slot}'");
                }
            }
            MontageAction::PlayNow | MontageAction::Enqueue => {
                let path = request.clip_path.trim();
                let Some(clip) = clips.get(path).cloned() else {
                    log::warn!("animator: montage clip '{path}' is not loaded");
                    continue;
                };
                let montage = Montage::new(clip)
                    .with_start_time(request.start_time)
                    .with_looping(request.looped)
                    .with_speed(request.speed)
                    .with_weight(request.weight);
                if request.action == MontageAction::PlayNow {
                    evaluator.play_montage(slot, montage);
                } else {
                    evaluator.enqueue_montage(slot, montage);
                }
            }
        }
    }
}

/// 根关节（joint 0）相对上一帧的位移增量（#560 P0，先只记录，P2 交给 CharacterMovement）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RootMotionDelta {
    /// 根关节局部空间的平移增量（米）。
    pub translation: glam::Vec3,
    /// 绕 Y 轴的偏航增量（弧度，最短路径）。
    pub yaw: f32,
}

/// 播放头（运行时组件，不进场景）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimatorPlayhead {
    /// 当前播放内容的键：剪辑路径，或 [`BLEND_SPACE_KEY`]。换了就从头播。
    pub clip_path: String,
    pub time: f32,
    /// 单次播放且已经播完。
    pub finished: bool,
    /// 交叉淡入的淡出源姿态（`None` = 当前没有过渡），过渡开始时冻结、权重到 1 丢掉。
    ///
    /// 存姿态而不是「旧剪辑 + 时间」：混合空间的退出、来源剪辑被删、连续换剪辑这些
    /// 情况都没有（或不该再找）一个可采样的剪辑，快照让它们都能正常淡出。
    pub previous_pose: Option<Pose>,
    /// 上一帧**实际显示**的姿态：下一次过渡的淡出源就是它。
    pub pose: Pose,
    /// 过渡已经进行的秒数（`>= blend_duration` 时清掉来源）。
    pub blend_elapsed: f32,
    /// 这一次过渡自己的时长（状态机转移带的 `duration`）；`None` = 用组件的
    /// `blend_duration`。
    pub blend_total: Option<f32>,
    /// 这一帧从根关节推导出的位移增量；首帧 / 换播放内容 / 冻结帧为零。
    pub root_motion: RootMotionDelta,
    /// 上一帧最终姿态的根关节局部变换，用来算下一帧的 delta。
    pub root_previous: Option<prism_anim::JointTransform>,
    /// 平滑后的混合空间参数（x, y）；`None` = 没开平滑或参数非有限。
    pub smoothed_blend: Option<[f32; 2]>,
}

impl AnimatorPlayhead {
    /// 过渡权重：`0` = 完全是旧剪辑，`1` = 完全是新剪辑。
    pub fn blend_weight(&self, blend_duration: f32) -> Option<f32> {
        if self.previous_pose.is_none() || !(blend_duration > 0.0) {
            return None;
        }
        Some((self.blend_elapsed / blend_duration).clamp(0.0, 1.0))
    }

    /// 用这一帧的最终姿态更新 root motion delta（#560 P0）。
    ///
    /// 只读根关节的**局部**变换：换播放内容时播放头是新建的，`root_previous` 为空，
    /// 所以过渡 / 切剪辑的第一帧 delta 为零，不会把旧剪辑的根位移重复计一次。
    fn record_root_motion(&mut self, pose: &Pose) {
        let Some(now) = pose.local(0) else {
            self.root_motion = RootMotionDelta::default();
            return;
        };
        self.root_motion = match self.root_previous {
            Some(previous) => RootMotionDelta {
                translation: now.translation - previous.translation,
                yaw: yaw_delta(previous.rotation, now.rotation),
            },
            None => RootMotionDelta::default(),
        };
        self.root_previous = Some(now);
    }
}

/// 根运动消费（#560 P2）：把这一帧的根平移从姿态里扣掉，交给实体的
/// [`LocalTransform`]。返回需要应用的模型空间增量。
fn consume_root_translation(
    animator: &Animator,
    playhead: &AnimatorPlayhead,
    pose: &mut Pose,
    entity: Entity,
    moves: &mut Vec<(Entity, glam::Vec3)>,
) {
    if !animator.root_motion_consume {
        return;
    }
    let delta = playhead.root_motion.translation;
    if !delta.is_finite() || delta.length_squared() <= f32::EPSILON {
        return;
    }
    if let Some(mut root) = pose.local(0) {
        root.translation = glam::Vec3::ZERO;
        pose.set_local(0, root);
    }
    moves.push((entity, delta));
}

/// 混合参数的指数平滑（#560 P1）。`dt = 0` 时返回当前值（首帧直接落在目标上）。
fn smoothed_blend_parameters(
    playhead: &mut AnimatorPlayhead,
    animator: &Animator,
    dt: f32,
) -> (f32, f32) {
    let target = [animator.blend_parameter, animator.blend_parameter_y];
    let tau = animator.blend_smoothing;
    if !(tau.is_finite() && tau > 0.0) || !target[0].is_finite() || !target[1].is_finite() {
        playhead.smoothed_blend = None;
        return (target[0], target[1]);
    }
    let current = playhead.smoothed_blend.unwrap_or(target);
    let alpha = if dt.is_finite() && dt > 0.0 {
        1.0 - (-dt / tau).exp()
    } else {
        0.0
    };
    let next = [
        current[0] + (target[0] - current[0]) * alpha,
        current[1] + (target[1] - current[1]) * alpha,
    ];
    playhead.smoothed_blend = Some(next);
    (next[0], next[1])
}

/// 两个四元数之间的绕 Y 偏航差（最短路径，弧度）。
fn yaw_delta(from: glam::Quat, to: glam::Quat) -> f32 {
    let relative = to * from.inverse();
    let mut yaw = 2.0 * relative.y.atan2(relative.w);
    let tau = std::f32::consts::TAU;
    if yaw > std::f32::consts::PI {
        yaw -= tau;
    } else if yaw < -std::f32::consts::PI {
        yaw += tau;
    }
    if yaw.is_finite() {
        yaw
    } else {
        0.0
    }
}

/// 图求值在播放头里的「剪辑路径」占位符。
pub const ANIM_GRAPH_KEY: &str = "<anim-graph>";

/// 混合空间在播放头里的「剪辑路径」占位符。
///
/// 混合空间不是单个剪辑，但播放头需要**一个键**来判断「播放内容变了吗」。用这个
/// 不可能与真实资产路径冲突的标记，进出混合空间就自然地触发一次交叉淡入。
pub const BLEND_SPACE_KEY: &str = "<blend-space>";

/// 二维混合空间的播放头占位键。与一维的键分开：在一维 / 二维之间切换同样要
/// 触发一次交叉淡入（它们的姿态来源完全不同）。
pub const BLEND_SPACE_2D_KEY: &str = "<blend-space-2d>";

/// 推进所有 `Animator` 并重算骨骼矩阵，返回更新过的实体数。
/// 动画 LOD 的近档起点与冻结距离；坏值回落到 25m / 50m（#560 P6）。
fn lod_thresholds(animator: &Animator) -> (f32, f32) {
    let far = if animator.lod_far.is_finite() && animator.lod_far > 0.0 {
        animator.lod_far
    } else {
        50.0
    };
    let near = if animator.lod_near.is_finite() && animator.lod_near > 0.0 {
        animator.lod_near.min(far)
    } else {
        far
    };
    (near, far)
}

/// 动画 LOD 的降频状态：记录这一帧被跳过、下一帧该更新的实体（#560 P6）。
#[derive(Debug, Default, Clone)]
pub struct AnimLodThrottle {
    skipped: HashSet<Entity>,
}

/// 第一个启用相机的世界位置；没有相机时动画 LOD 不生效。
fn camera_eye(world: &World) -> Option<glam::Vec3> {
    for (entity, camera) in world.query::<Camera>() {
        if !camera.enabled {
            continue;
        }
        if let Some(matrix) = world.get::<LocalToWorld>(entity) {
            return Some(matrix.0.w_axis.truncate());
        }
        if let Some(transform) = world.get::<LocalTransform>(entity) {
            return Some(transform.position);
        }
    }
    None
}

fn entity_position(world: &World, entity: Entity) -> glam::Vec3 {
    if let Some(matrix) = world.get::<LocalToWorld>(entity) {
        return matrix.0.w_axis.truncate();
    }
    if let Some(transform) = world.get::<LocalTransform>(entity) {
        return transform.position;
    }
    glam::Vec3::ZERO
}

/// 每个动画实体这一帧该用的 `dt`；**不在表里的实体这一帧完全跳过**。
///
/// - 超过 `lod_far`：冻结（不进表，图求值器保留）；
/// - `lod_near..lod_far`：隔帧求值，更新帧用 `2 * dt` 保持播放速度；
/// - 近处 / 没开 LOD / 没有相机：每帧 `dt`。
fn animation_frame_plans(
    world: &World,
    eye: Option<glam::Vec3>,
    dt: f32,
    throttle: &mut AnimLodThrottle,
) -> (HashMap<Entity, f32>, HashSet<Entity>) {
    let mut plans = HashMap::new();
    let mut root_only = HashSet::new();
    for (entity, animator) in world.query::<Animator>() {
        let Some(eye) = eye else {
            plans.insert(entity, dt);
            continue;
        };
        if !animator.lod_enabled {
            throttle.skipped.remove(&entity);
            plans.insert(entity, dt);
            continue;
        }
        let (near, far) = lod_thresholds(animator);
        let distance = entity_position(world, entity).distance(eye);
        if distance > far {
            throttle.skipped.remove(&entity);
            if (animator.lod_root_only || !animator.lod_bones.is_empty())
                && animator.anim_graph_path.trim().is_empty()
                && animator.state_machine_path.trim().is_empty()
                && !animator.uses_blend_space()
                && !animator.uses_blend_space_2d()
            {
                plans.insert(entity, dt);
                root_only.insert(entity);
            }
            continue;
        }
        if distance > near {
            if throttle.skipped.remove(&entity) {
                // 上一帧更新过，这一帧跳过并清掉标记。
                continue;
            }
            throttle.skipped.insert(entity);
            plans.insert(entity, dt * 2.0);
            continue;
        }
        throttle.skipped.remove(&entity);
        plans.insert(entity, dt);
    }
    (plans, root_only)
}

pub fn animator_system(world: &mut World, dt: f32) -> usize {
    let Some(library) = world.get_resource::<SkeletonLibrary>() else {
        return 0;
    };
    let machines: HashMap<String, Arc<StateMachine>> = world
        .get_resource::<StateMachineLibrary>()
        .map(|library| library.machines.clone())
        .unwrap_or_default();
    let graphs = world
        .get_resource::<AnimGraphLibrary>()
        .map(|library| library.graphs.clone())
        .unwrap_or_default();
    let library = LibraryView {
        skeletons: library.skeletons.clone(),
        clips: library.clips.clone(),
        machines,
        graphs,
    };
    let mut graph_runtimes = world
        .remove_resource::<AnimGraphRuntimes>()
        .unwrap_or_default();
    // 调试快照只在调用方主动插入资源时维护（见 `AnimGraphDebug`）。
    let mut graph_debug = world.remove_resource::<AnimGraphDebug>();
    let mut graph_entities: HashSet<Entity> = HashSet::new();
    // 状态机参数（项目侧写、引擎每帧读）。克隆一份：`take_trigger` 需要 `&mut`，
    // 而遍历实体时不能继续借用 World。
    let mut params = world
        .get_resource::<AnimParameters>()
        .cloned()
        .unwrap_or_default();
    let mut state_updates: Vec<(Entity, String)> = Vec::new();
    // 根运动消费：等遍历完实体再写 Transform，避免借 World。
    let mut root_motion_moves: Vec<(Entity, glam::Vec3)> = Vec::new();
    // 项目侧请求只活一帧；按实体分组，驱动图时消费。
    let mut montage_requests: HashMap<Entity, Vec<MontageRequest>> = HashMap::new();
    if let Some(resource) = world.get_resource_mut::<AnimMontageRequests>() {
        for request in resource.take() {
            montage_requests
                .entry(request.entity)
                .or_default()
                .push(request);
        }
    }

    let mut updates: Vec<(Entity, BonePalette, AnimatorPlayhead)> = Vec::new();
    let mut events: Vec<AnimEvent> = Vec::new();
    // 动画 LOD（#560 P6）：远处实体冻结、近档隔帧求值；图运行时保留状态等回来再续。
    let mut throttle = world
        .remove_resource::<AnimLodThrottle>()
        .unwrap_or_default();
    let (plans, root_only_entities) =
        animation_frame_plans(world, camera_eye(world), dt, &mut throttle);
    world.insert_resource(throttle);
    // 图求值先按实体并行算好（#560 P6）；下面的循环只做顺序的 World 读写。
    let mut graph_results = evaluate_graph_poses(
        world,
        &library,
        &mut graph_runtimes,
        &params,
        &mut montage_requests,
        &mut graph_entities,
        &plans,
    );
    for (entity, animator) in world.query::<Animator>() {
        let Some(root) = world.get::<SkeletonRoot>(entity) else {
            continue;
        };
        let Some(skeleton) = library.skeleton(&root.skeleton_path) else {
            continue;
        };
        let Some(&entity_dt) = plans.get(&entity) else {
            continue;
        };
        let dt = entity_dt;

        // 动画图（#560 P1）：路径非空且图已加载时优先于状态机 / 混合空间 / 单剪辑。
        // 求值器按实体持久化（状态机节点需要跨帧状态）；图换路径时重建。
        if !animator.anim_graph_path.trim().is_empty() {
            match graph_results.remove(&entity) {
                Some(Ok((pose, graph_events))) => {
                    events.extend(graph_events);
                    let mut playhead = world.get::<AnimatorPlayhead>(entity).cloned().unwrap_or(
                        AnimatorPlayhead {
                            clip_path: ANIM_GRAPH_KEY.into(),
                            time: 0.0,
                            finished: false,
                            previous_pose: None,
                            pose: Pose::default(),
                            blend_elapsed: 0.0,
                            blend_total: None,
                            root_motion: RootMotionDelta::default(),
                            root_previous: None,
                            smoothed_blend: None,
                        },
                    );
                    playhead.clip_path = ANIM_GRAPH_KEY.into();
                    playhead.finished = false;
                    playhead.previous_pose = None;
                    playhead.record_root_motion(&pose);
                    let mut pose = pose;
                    consume_root_translation(
                        animator,
                        &playhead,
                        &mut pose,
                        entity,
                        &mut root_motion_moves,
                    );
                    playhead.pose = pose.clone();
                    let palette = BonePalette::from_matrices(&skeleton.bone_palette(&pose));
                    updates.push((entity, palette, playhead));
                    continue;
                }
                Some(Err(error)) => {
                    log::warn!(
                        "animator: graph '{}' on entity {:?} failed: {error}",
                        animator.anim_graph_path.trim(),
                        entity
                    );
                }
                None => {}
            }
        }

        let mut single_path = if animator.clip_path.is_empty() {
            root.default_clip_path.clone()
        } else {
            animator.clip_path.clone()
        };

        // 状态机（#560 P1）：先看这一帧会不会转移，再用（可能已换掉的）状态决定播什么。
        //
        // 转移判据用的是**上一帧**的播放时间与当前状态的剪辑时长：那一帧确实播到了
        // 这个位置，比用「本帧推进后的时间」更接近作者对 `min_state_fraction` 的预期。
        let machine = if animator.state_machine_path.trim().is_empty() {
            None
        } else {
            library.machines.get(&animator.state_machine_path).cloned()
        };
        let mut state_name = animator.state.clone();
        let mut transition_duration: Option<f32> = None;
        let mut state_switched = false;
        let mut state_speed = 1.0f32;
        let mut state_looped = true;
        if let Some(machine) = &machine {
            let current = machine.state_index(&state_name).unwrap_or(0);
            let previous_time = world
                .get::<AnimatorPlayhead>(entity)
                .map(|head| head.time)
                .unwrap_or_else(|| sanitize_time(animator.time));
            let state_duration = machine
                .state(current)
                .and_then(|state| library.clips.get(&state.clip_path))
                .map(|clip| clip.duration)
                .unwrap_or(0.0);
            let target =
                machine.next_transition(current, &mut params, previous_time, state_duration);
            let index = match &target {
                Some(target) => target.state,
                None => current,
            };
            if let Some(target) = target {
                transition_duration = Some(target.duration);
                state_switched = true;
            }
            if let Some(state) = machine.state(index) {
                // 空 / 未知的状态名也要落成真实名字：编辑器与存档看到的就是「当前状态」，
                // 而不是一个空串。
                state_name = state.name.clone();
                state_speed = state.speed;
                state_looped = state.looped;
                if !state.clip_path.trim().is_empty() {
                    single_path = state.clip_path.clone();
                }
            }
            if state_name != animator.state {
                state_updates.push((entity, state_name.clone()));
            }
        }
        // 状态机拥有速度 / 循环语义；没挂状态机时照旧读组件。
        let speed = if machine.is_some() {
            animator.speed * state_speed
        } else {
            animator.speed
        };
        let looped = if machine.is_some() {
            state_looped
        } else {
            animator.looped
        };

        // 二维混合空间优先（#560 P1）：至少三个样本且能三角化才生效，坐标
        // 非有限 / 共线时退回一维或单剪辑 —— 少一个自由度总比实体不动好。
        let mut space_2d: Option<BlendSpace2D> = None;
        if animator.uses_blend_space_2d() {
            let samples: Vec<BlendSample2D> = animator
                .blend_2d_clip_paths
                .iter()
                .zip(animator.blend_2d_positions.iter())
                .filter_map(|(path, position)| {
                    library.clip(path).map(|clip| BlendSample2D {
                        position: *position,
                        clip,
                    })
                })
                .collect();
            let candidate = BlendSpace2D::new(samples, skeleton.rest_pose());
            if !candidate.is_empty() {
                space_2d = Some(candidate);
            }
        }
        let blend_space_2d = space_2d.is_some();

        // 一维混合空间（#560 P1）：作者列出的剪辑里至少要有两个真的加载了，
        // 否则退回单剪辑。二维生效时不再构建一维样本，避免白算。
        let mut samples: Vec<BlendSample> = Vec::new();
        if !blend_space_2d && animator.uses_blend_space() {
            samples = animator
                .blend_clip_paths
                .iter()
                .zip(animator.blend_positions.iter())
                .filter_map(|(path, position)| {
                    library.clip(path).map(|clip| BlendSample {
                        position: *position,
                        clip,
                    })
                })
                .collect();
        }
        let blend_space = !blend_space_2d && samples.len() >= 2;
        let clip = if blend_space_2d || blend_space {
            None
        } else {
            match library.clip(&single_path) {
                Some(clip) => Some(clip),
                None => continue,
            }
        };
        let playback_key = if blend_space_2d {
            BLEND_SPACE_2D_KEY.to_string()
        } else if blend_space {
            BLEND_SPACE_KEY.to_string()
        } else {
            single_path.clone()
        };

        // 播放头：换剪辑从作者给的起始时间重来，否则接着上一帧。
        let mut playhead = match world.get::<AnimatorPlayhead>(entity) {
            Some(head) if head.clip_path == playback_key => head.clone(),
            Some(head) => AnimatorPlayhead {
                // 换剪辑：把旧剪辑**停住那一刻**的姿态冻下来，作为淡出源（#560 P1）。
                // 旧剪辑已经不在库里（删资产 / 上一帧是混合空间）时沿用已有快照 ——
                // 连续换两次也不会丢掉正在淡出的那一份。
                clip_path: playback_key.clone(),
                time: if state_switched {
                    0.0
                } else {
                    sanitize_time(animator.time)
                },
                finished: false,
                previous_pose: if head.pose.len() > 0 {
                    Some(head.pose.clone())
                } else {
                    head.previous_pose.clone()
                },
                pose: Pose::default(),
                blend_elapsed: 0.0,
                blend_total: transition_duration,
                root_motion: RootMotionDelta::default(),
                root_previous: None,
                smoothed_blend: None,
            },
            None => AnimatorPlayhead {
                clip_path: playback_key.clone(),
                time: sanitize_time(animator.time),
                finished: false,
                previous_pose: None,
                pose: Pose::default(),
                blend_elapsed: 0.0,
                blend_total: transition_duration,
                root_motion: RootMotionDelta::default(),
                root_previous: None,
                smoothed_blend: None,
            },
        };
        // 混合空间没有自己的时长：取最长的那条剪辑，短的自然会循环追上。
        let duration = match &clip {
            Some(clip) => clip.duration.max(0.0),
            None => {
                let mut longest = 0.0f32;
                if let Some(space) = &space_2d {
                    for sample in space.samples() {
                        longest = longest.max(sample.clip.duration);
                    }
                } else {
                    for sample in &samples {
                        longest = longest.max(sample.clip.duration);
                    }
                }
                longest.max(0.0)
            }
        };
        // 上一帧已经在这个播放内容上时的本地时间：只有它存在才触发 notify ——
        // 刚创建 / 刚换剪辑的第一帧不该把整段剪辑的事件一次性播出来。
        let previous_local = match world.get::<AnimatorPlayhead>(entity) {
            Some(head) if head.clip_path == playback_key => {
                Some(local_time(duration, looped, head.time))
            }
            _ => None,
        };

        if animator.playing && dt.is_finite() {
            playhead.time += dt * speed;
            if !looped {
                playhead.time = playhead.time.clamp(0.0, duration);
            }
        }
        playhead.finished = !looped && duration > 0.0 && playhead.time >= duration;

        // 循环与否：状态机状态优先，否则由 `Animator` 组件说了算（剪辑里的
        // `looped` 只是导入默认值）。
        let local = local_time(duration, looped, playhead.time);

        // 关键帧事件：只在前向播放、且上一帧确实在这个剪辑上时触发。
        // 倒放（`speed < 0`）需要反向扫区间，暂时不触发 —— 宁可少一次脚步声，
        // 也不要在倒放时把整段事件倒着播一遍。
        if let (Some(previous), Some(clip)) = (previous_local, clip.as_ref()) {
            if animator.playing && dt.is_finite() && speed > 0.0 {
                for notify in clip.notifies_between(previous, local) {
                    events.push(AnimEvent {
                        entity,
                        name: notify.name.clone(),
                        time: notify.time,
                    });
                }
            }
        }
        // 动画 LOD 子集档：保住上一帧姿态，只推进时间 + 采样根关节（和 `lod_bones`）。
        // 没有名单时就是「仅根骨」；首次进入先完整求值一帧，避免 T-pose 突变。
        if root_only_entities.contains(&entity)
            && playhead.pose.len() == skeleton.joints().len()
            && playhead.pose.len() > 0
        {
            let mut pose = playhead.pose.clone();
            if let Some(clip) = clip.as_ref() {
                clip.sample_joint_into(local, 0, &mut pose);
                let mut seen = vec![false; skeleton.joints().len()];
                for name in &animator.lod_bones {
                    let Some(joint) = skeleton.joint_index(name.trim()) else {
                        continue;
                    };
                    if joint == 0 || seen[joint] {
                        continue;
                    }
                    seen[joint] = true;
                    clip.sample_joint_into(local, joint, &mut pose);
                }
            }
            playhead.previous_pose = None;
            playhead.blend_elapsed = 0.0;
            playhead.blend_total = None;
            playhead.record_root_motion(&pose);
            consume_root_translation(
                animator,
                &playhead,
                &mut pose,
                entity,
                &mut root_motion_moves,
            );
            playhead.pose = pose.clone();
            let palette = BonePalette::from_matrices(&skeleton.bone_palette(&pose));
            updates.push((entity, palette, playhead));
            continue;
        }
        let (blend_x, blend_y) = smoothed_blend_parameters(&mut playhead, animator, dt);
        let mut pose = skeleton.rest_pose();
        match &clip {
            Some(clip) => {
                clip.sample_into(local, &mut pose);
            }
            None => {
                let phase = if duration > 0.0 {
                    local / duration
                } else {
                    0.0
                };
                match &mut space_2d {
                    Some(space) => {
                        space.evaluate_into([blend_x, blend_y], phase, &mut pose);
                    }
                    None => {
                        let mut space = BlendSpace1D::new(samples.clone(), skeleton.rest_pose());
                        space.evaluate_into(blend_x, phase, &mut pose);
                    }
                }
            }
        }

        // 交叉淡入：旧剪辑冻结在过渡开始的那一刻，按权重混进新姿态。
        //
        // 先推进再取权重：换剪辑的那一帧 `dt` 是"距上一帧实际过去的时间"，
        // 过渡就该从那一刻开始计时，否则最后一帧永远到不了权重 1。
        let blend_duration = playhead
            .blend_total
            .map(sanitize_blend_duration)
            .unwrap_or_else(|| sanitize_blend_duration(animator.blend_duration));
        if blend_duration <= 0.0 {
            // 没有过渡时长 = 硬切：顺手丢掉快照，否则以后把时长调回来的第一帧
            // 会突然淡入一个早已过期的姿态。
            playhead.previous_pose = None;
            playhead.blend_elapsed = 0.0;
            playhead.blend_total = None;
        } else if playhead.previous_pose.is_some() && animator.playing && dt.is_finite() {
            playhead.blend_elapsed += dt.abs();
        }
        if let Some(weight) = playhead.blend_weight(blend_duration) {
            if let Some(fading) = playhead.previous_pose.as_ref() {
                // `fading` 是旧姿态、`pose` 是新姿态：`t = weight` 时 0 → 旧、1 → 新。
                let mut blended = skeleton.rest_pose();
                prism_anim::blend_into(&mut blended, fading, &pose, weight);
                pose = blended;
            }
            // 权重已经到 1：新姿态就是完整的新剪辑，快照可以丢了。
            if weight >= 1.0 {
                playhead.previous_pose = None;
                playhead.blend_elapsed = 0.0;
                playhead.blend_total = None;
            }
        }

        // 叠加层（#560 P1）：增量相对 reference（bind）姿态，按权重打到最终姿态上。
        // 放在交叉淡入之后 —— 叠加层不该参与「两个基础剪辑之间」的过渡。
        let additive_weight = if animator.additive_weight.is_finite() {
            animator.additive_weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if additive_weight > 0.0 && !animator.additive_clip_path.trim().is_empty() {
            if let Some(layer) = library.clip(animator.additive_clip_path.as_str()) {
                let mut additive = skeleton.rest_pose();
                layer.sample_into(layer.wrap_time(local), &mut additive);
                let reference = skeleton.rest_pose();
                prism_anim::apply_additive(&mut pose, &additive, &reference, additive_weight);
            }
        }

        // 动画图层（#560 P1）：在基础剪辑 + 交叉淡入 + 单 additive 层之后，
        // 按作者顺序把 `Animator::layers` 叠上去。每层用同一时间轴采样，
        // per-bone mask 只让列出的骨骼参与；BlendStack 负责顺序、权重和
        // additive 语义，缺失剪辑 / 0 权重的层直接跳过。
        if !animator.layers.is_empty() {
            let reference = skeleton.rest_pose();
            let mut stack = BlendStack::new(pose, reference.clone());
            for layer in &animator.layers {
                if layer.clip_path.trim().is_empty() || !(layer.weight > 0.0) {
                    continue;
                }
                let Some(clip) = library.clip(layer.clip_path.as_str()) else {
                    continue;
                };
                let mut layer_pose = reference.clone();
                clip.sample_into(clip.wrap_time(local), &mut layer_pose);
                let mask = if layer.bone_mask.is_empty() {
                    BoneMask::all()
                } else {
                    let names: Vec<&str> = layer.bone_mask.iter().map(String::as_str).collect();
                    BoneMask::from_names(&skeleton, &names)
                };
                stack.push(
                    BlendLayer::new(layer_pose)
                        .with_weight(layer.weight)
                        .with_mask(mask)
                        .with_additive(layer.additive),
                );
            }
            let mut stacked = Pose::default();
            stack.evaluate_into(&mut stacked);
            pose = stacked;
        }

        // Root motion（#560 P0）：记录根关节相对上一帧的增量；开了消费就把平移
        // 交给实体 Transform，并从姿态里扣掉（#560 P2）。
        playhead.record_root_motion(&pose);
        consume_root_translation(
            animator,
            &playhead,
            &mut pose,
            entity,
            &mut root_motion_moves,
        );
        // 这份姿态就是下一次过渡的淡出源（不需要 clone：上面已经用完 `pose`）。
        playhead.pose = pose;
        let palette = BonePalette::from_matrices(&skeleton.bone_palette(&playhead.pose));
        updates.push((entity, palette, playhead));
    }

    for (entity, requests) in montage_requests {
        for _request in requests {
            log::warn!(
                "animator: montage request on entity {entity:?} ignored (no active animation graph)"
            );
        }
    }

    let count = updates.len();
    for (entity, delta) in root_motion_moves {
        if let Some(transform) = world.get_mut::<LocalTransform>(entity) {
            transform.position += transform.rotation * delta;
        }
    }

    for (entity, palette, playhead) in updates {
        world.insert(entity, palette);
        world.insert(entity, playhead);
    }
    // 状态由引擎写回组件：编辑器 / 存档看到的就是当前状态，不用再问播放头。
    for (entity, state) in state_updates {
        if let Some(animator) = world.get_mut::<Animator>(entity) {
            animator.state = state;
        }
    }
    graph_runtimes
        .runtimes
        .retain(|entity, _| graph_entities.contains(entity));
    if let Some(debug) = graph_debug.as_mut() {
        debug.entities.clear();
        for (entity, runtime) in &graph_runtimes.runtimes {
            debug
                .entities
                .insert(*entity, runtime.evaluator.node_snapshots());
        }
    }
    world.insert_resource(graph_runtimes);
    if let Some(debug) = graph_debug {
        world.insert_resource(debug);
    }
    world.insert_resource(AnimEvents { events });
    count
}

/// 交叉淡入时长：负值 / NaN / 无穷都按 0（直接切换）处理。
fn sanitize_blend_duration(duration: f32) -> f32 {
    if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        0.0
    }
}

/// 把播放头时间折算成剪辑本地时间（循环取余、单次夹到 `0..=duration`）。
fn local_time(duration: f32, looped: bool, time: f32) -> f32 {
    if duration > 0.0 {
        if looped {
            time.rem_euclid(duration)
        } else {
            time.clamp(0.0, duration)
        }
    } else {
        0.0
    }
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
    machines: HashMap<String, Arc<StateMachine>>,
    graphs: HashMap<String, Arc<AnimGraphInfo>>,
}

impl LibraryView {
    fn skeleton(&self, path: &str) -> Option<Arc<Skeleton>> {
        self.skeletons.get(path).cloned()
    }

    fn clip(&self, path: &str) -> Option<Arc<Clip>> {
        self.clips.get(path).cloned()
    }

    fn graph(&self, path: &str) -> Option<Arc<AnimGraphInfo>> {
        self.graphs.get(path).cloned()
    }
}

impl AnimGraphSource for LibraryView {
    fn clip(&self, path: &str) -> Option<Arc<Clip>> {
        self.clips.get(path).cloned()
    }

    fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>> {
        self.machines.get(path).cloned()
    }
}

#[cfg(test)]
#[path = "animator_tests.rs"]
pub(crate) mod tests;
