//! 动画图求值器（#560 P1）。
//!
//! [`AnimGraphEvaluator`] 把 cooked 的 [`AnimGraphInfo`]（RAGP v1）求成一个
//! [`Pose`]。它不依赖 ECS / 渲染 / 资产加载；调用方通过 [`AnimGraphSource`]
//! 提供已解码的剪辑和状态机，再传入骨架（取 reference 姿态与骨骼名 → 下标）。
//!
//! 求值是每帧一次的有向无环图遍历：
//!
//! - ClipPlayer / BlendSpace / StateMachine 各自维护时间；
//! - BlendStack / LayeredBlend 把第一个输入当 base，其余输入按顺序叠加；
//! - Slot 在基础输入上可选覆盖一段 montage 剪辑（无剪辑时透明传递）；
//! - CachedPose / Output 是透明传递节点；
//! - 同一帧内一个节点只求值一次（fan-out 不会重复推进时间）；
//! - 成环的图（例如手工构造的 `AnimGraphInfo`）返回错误而不是栈溢出。
//!
//! 状态机节点在求值器内部维护「当前状态 + 状态时间 + 交叉淡入快照」：转移条件
//! 用 [`StateMachine::next_transition`] 求值，Trigger 会被消耗；转移时长 > 0 时
//! 从转移前的姿态淡入。这和引擎 `animator` 的播放头语义一致，但这一层不碰 ECS。
//!
//! Slot 的 montage 可以通过 [`AnimGraphEvaluator::play_montage`] /
//! [`AnimGraphEvaluator::enqueue_montage`] 立即覆盖或排队，`stop_montage` /
//! `stop_montage_after_current` 清空对应状态；Montage Notify 会并入图事件。
//!
//! 已知边界：Montage Notify 区间（State Notify）还未实现；CachedPose 复用
//! 每帧缓存，不跨帧冻结姿态。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use prism_asset::formats_anim_graph::{
    AnimGraphInfo, GraphNodeInfo, GraphNodeKind, GraphParameterKind,
};

use crate::blend::{blend_into, BlendSample, BlendSpace1D};
use crate::blend_space2d::{BlendSample2D, BlendSpace2D};
use crate::blend_stack::{BlendLayer, BlendStack, BoneMask};
use crate::clip::Clip;
use crate::montage::{Montage, MontagePlayer};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::state_machine::{AnimParameters, StateMachine};

/// 求值器需要的资源：按路径查已解码的剪辑 / 状态机。
pub trait AnimGraphSource {
    fn clip(&self, path: &str) -> Option<Arc<Clip>>;
    fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>>;
}

/// 图求值过程中跨过的一个剪辑 Notify（#560 P1）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNotifyEvent {
    /// 触发事件的图节点名。
    pub node: String,
    /// 事件所属剪辑路径。
    pub clip_path: String,
    pub name: String,
    /// 事件在剪辑里的本地时间（秒）。
    pub time: f32,
}

/// 图求值错误。坏数据不应该 panic，也不应该让宿主渲染一半姿态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphEvalError {
    MissingOutput,
    NodeIndex(u32),
    MissingClip { node: String, path: String },
    MissingStateMachine { node: String, path: String },
    EmptyBlendSpace { node: String },
    EmptyStateMachine { node: String },
    MissingInput { node: String },
    Cycle { node: String },
}

impl std::fmt::Display for GraphEvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOutput => write!(f, "animation graph has no output node"),
            Self::NodeIndex(index) => {
                write!(f, "animation graph references node #{index} out of range")
            }
            Self::MissingClip { node, path } => {
                write!(f, "node '{node}' cannot find clip '{path}'")
            }
            Self::MissingStateMachine { node, path } => {
                write!(f, "node '{node}' cannot find state machine '{path}'")
            }
            Self::EmptyBlendSpace { node } => {
                write!(f, "node '{node}' has no usable blend-space samples")
            }
            Self::EmptyStateMachine { node } => {
                write!(f, "node '{node}' has an empty state machine")
            }
            Self::MissingInput { node } => write!(f, "node '{node}' has no input"),
            Self::Cycle { node } => write!(f, "animation graph has a cycle at '{node}'"),
        }
    }
}

impl std::error::Error for GraphEvalError {}

#[derive(Debug, Clone, Default)]
struct NodeRuntime {
    time: f32,
    state: usize,
    blend: Option<StateBlend>,
    stamp: u64,
    cached: Pose,
    /// 每样本独立播放头的归一化相位（仅配置了速度的 BlendSpace 节点使用）。
    sample_times: Vec<f32>,
}

#[derive(Debug, Clone)]
struct StateBlend {
    from: Pose,
    elapsed: f32,
    total: f32,
}

/// BlendSpace 节点的 Notify 策略（#560）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendSpaceNotifyPolicy {
    /// 不产生事件（旧行为，避免多个采样点重复脚步声）。
    None,
    /// 只让当前权重最大的样本产生事件；用一个剪辑的本地时间区间扫描。
    #[default]
    Dominant,
}

/// 状态机发生转移的那一帧，离开状态的事件怎么处理（#560 Notify 区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateTransitionNotifyPolicy {
    /// 照常扫描到转移点为止（旧行为）。
    #[default]
    Keep,
    /// 本帧发生了转移就不发离开状态的事件，避免刚抬脚就被切走的脚步声。
    SuppressOnTransition,
}

/// 一个节点在这一帧的运行时摘要（[`AnimGraphEvaluator::node_snapshots`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNodeSnapshot {
    pub name: String,
    pub kind: GraphNodeKind,
    /// 节点本地时间（秒）；状态机节点是当前状态自己的时间。
    pub time: f32,
    /// 状态机节点的当前状态下标；其他节点为 0。
    pub state: usize,
    /// 状态机是否正在交叉淡入。
    pub blending: bool,
    /// 交叉淡入权重 `0..=1`（没有过渡时为 1）。
    pub blend_weight: f32,
    /// `cached_pose` 节点是否已经有缓存姿态。
    pub cached: bool,
    /// 这个节点对最终姿态的权重 `0..=1`（从输出节点按连线权重往下乘）。
    pub weight: f32,
}

/// 一个动画图求值器实例，对应一个角色。
#[derive(Debug, Clone)]
pub struct AnimGraphEvaluator {
    graph: AnimGraphInfo,
    runtime: Vec<NodeRuntime>,
    parameters: AnimParameters,
    frame: u64,
    pending_events: Vec<GraphNotifyEvent>,
    /// 每个 Slot 节点自己的 Montage 播放队列（#560 P1）。
    montages: HashMap<String, MontagePlayer>,
    /// BlendSpace 的事件策略（#560 Notify 区间）。
    blend_space_notify: BlendSpaceNotifyPolicy,
    /// 状态转移帧的事件策略。
    state_transition_notify: StateTransitionNotifyPolicy,
    /// 按节点名覆盖 BlendSpace 每样本播放速度（空 = 共享 phase）。
    blend_space_speeds: HashMap<String, Vec<f32>>,
}

impl AnimGraphEvaluator {
    /// 建立求值器，并按图参数声明初始化默认值。
    pub fn new(graph: AnimGraphInfo) -> Self {
        let mut parameters = AnimParameters::new();
        for parameter in &graph.parameters {
            match parameter.kind {
                GraphParameterKind::Float => {
                    parameters.set_float(parameter.name.clone(), parameter.default)
                }
                GraphParameterKind::Bool => {
                    parameters.set_bool(parameter.name.clone(), parameter.default != 0.0)
                }
                GraphParameterKind::Trigger => {}
            }
        }
        let runtime = vec![NodeRuntime::default(); graph.nodes.len()];
        Self {
            graph,
            runtime,
            parameters,
            frame: 0,
            pending_events: Vec::new(),
            montages: HashMap::new(),
            blend_space_notify: BlendSpaceNotifyPolicy::default(),
            state_transition_notify: StateTransitionNotifyPolicy::default(),
            blend_space_speeds: HashMap::new(),
        }
    }

    pub fn graph(&self) -> &AnimGraphInfo {
        &self.graph
    }

    pub fn parameters(&self) -> &AnimParameters {
        &self.parameters
    }

    pub fn parameters_mut(&mut self) -> &mut AnimParameters {
        &mut self.parameters
    }

    /// 立即在命名为 `slot` 的 Slot 节点上播放 Montage（替换该槽当前段）。
    pub fn play_montage(&mut self, slot: &str, montage: Montage) -> bool {
        self.montages
            .entry(slot.to_string())
            .or_default()
            .play_now(montage)
    }

    /// 在 `slot` 上排队一段 Montage；空闲时立即开始。
    pub fn enqueue_montage(&mut self, slot: &str, montage: Montage) -> bool {
        self.montages
            .entry(slot.to_string())
            .or_default()
            .enqueue(montage)
    }

    /// 停止 `slot` 的活动段与队列。
    pub fn stop_montage(&mut self, slot: &str) {
        if let Some(player) = self.montages.get_mut(slot) {
            player.stop();
        }
    }

    /// 让 `slot` 当前段播完后停止，不接队列下一段。
    pub fn stop_montage_after_current(&mut self, slot: &str) {
        if let Some(player) = self.montages.get_mut(slot) {
            player.stop_after_current();
        }
    }

    /// 暂停 Slot 的活动 Montage（队列 / 权重不变）；Slot 不存在返回 `false`。
    pub fn pause_montage(&mut self, slot: &str) -> bool {
        match self.montages.get_mut(slot) {
            Some(player) => {
                player.set_paused(true);
                true
            }
            None => false,
        }
    }

    /// 恢复 Slot 的活动 Montage；Slot 不存在返回 `false`。
    pub fn resume_montage(&mut self, slot: &str) -> bool {
        match self.montages.get_mut(slot) {
            Some(player) => {
                player.set_paused(false);
                true
            }
            None => false,
        }
    }

    pub fn montage_player(&self, slot: &str) -> Option<&MontagePlayer> {
        self.montages.get(slot)
    }

    /// 设置 BlendSpace 的 Notify 策略；`None` 恢复旧行为。
    pub fn set_blend_space_notify_policy(&mut self, policy: BlendSpaceNotifyPolicy) {
        self.blend_space_notify = policy;
    }

    pub fn blend_space_notify_policy(&self) -> BlendSpaceNotifyPolicy {
        self.blend_space_notify
    }

    /// 设置状态转移帧的事件策略。
    pub fn set_state_transition_notify_policy(&mut self, policy: StateTransitionNotifyPolicy) {
        self.state_transition_notify = policy;
    }

    pub fn state_transition_notify_policy(&self) -> StateTransitionNotifyPolicy {
        self.state_transition_notify
    }

    /// 给 BlendSpace 节点配置每样本播放速度（按 `node.clips` 顺序）。
    ///
    /// 配置后该节点为每个样本维护独立播放头，不再共用全局 phase，也不再做
    /// SyncGroup 映射；空列表 / 全为 1 等价于默认共享 phase。
    pub fn set_blend_space_speeds(&mut self, node: &str, speeds: Vec<f32>) {
        if speeds.is_empty() || speeds.iter().all(|speed| (*speed - 1.0).abs() < 1e-6) {
            self.blend_space_speeds.remove(node);
        } else {
            self.blend_space_speeds.insert(node.to_string(), speeds);
        }
    }

    pub fn blend_space_speeds(&self, node: &str) -> Option<&[f32]> {
        self.blend_space_speeds.get(node).map(Vec::as_slice)
    }

    /// 每个节点这一帧的运行时摘要（调试面板 / 叠加层用）。
    ///
    /// 返回顺序与图里的节点顺序一致；`kind` 让调用方决定怎么显示，`time` 是节点
    /// 自己的本地时间（状态机节点是当前状态的时间），`blending` / `blend_weight`
    /// 反映状态机是否正在交叉淡入。只拷贝标量，不动求值器状态。
    pub fn node_snapshots(&self) -> Vec<GraphNodeSnapshot> {
        let weights = self.node_weights();
        self.graph
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let runtime = &self.runtime[index];
                let (blending, blend_weight) = match &runtime.blend {
                    Some(blend) if blend.total > 0.0 => {
                        (true, (blend.elapsed / blend.total).clamp(0.0, 1.0))
                    }
                    Some(_) => (true, 1.0),
                    None => (false, 1.0),
                };
                GraphNodeSnapshot {
                    name: node.name.clone(),
                    kind: node.kind,
                    time: runtime.time,
                    state: runtime.state,
                    blending,
                    blend_weight,
                    cached: !runtime.cached.is_empty(),
                    weight: weights[index],
                }
            })
            .collect()
    }

    /// 从输出节点往下走，按连线权重给出每个节点对最终姿态的贡献（`0..=1`）。
    ///
    /// 共享节点取路径上的最大值（同一个节点被两条线引用时按权重更大的那条算）；
    /// 没连到输出节点的是 0。BlendStack / LayeredBlend 的连线权重就是层权重，
    /// 其他节点通常是 1 —— 只用于调试显示，不参与求值。
    fn node_weights(&self) -> Vec<f32> {
        let mut weights = vec![0.0f32; self.graph.nodes.len()];
        let mut seen = vec![false; self.graph.nodes.len()];
        self.collect_weights(self.graph.output, 1.0, &mut weights, &mut seen);
        weights
    }

    fn collect_weights(&self, index: u32, weight: f32, weights: &mut [f32], seen: &mut [bool]) {
        let index = index as usize;
        if index >= self.graph.nodes.len() {
            return;
        }
        if weight > weights[index] {
            weights[index] = weight;
        }
        if seen[index] {
            return;
        }
        seen[index] = true;
        for input in &self.graph.nodes[index].inputs {
            let weight = (weight * input.weight).clamp(0.0, 1.0);
            self.collect_weights(input.node, weight, weights, seen);
        }
    }

    /// 当前状态机节点的状态下标（调试面板 / 测试用）；不是状态机节点时返回 `None`。
    pub fn state_machine_state(&self, name: &str) -> Option<usize> {
        let index = self.graph.nodes.iter().position(|node| node.name == name)?;
        (self.graph.nodes[index].kind == GraphNodeKind::StateMachine)
            .then_some(self.runtime[index].state)
    }

    /// 求值一帧，返回输出节点的姿态。
    ///
    /// `dt` 非有限或为负时按 0；缺失资源返回错误。失败时求值器的缓存与状态可能
    /// 已部分推进，调用方应保留上一帧姿态，而不是显示半份结果。
    pub fn evaluate(
        &mut self,
        source: &impl AnimGraphSource,
        skeleton: &Skeleton,
        dt: f32,
    ) -> Result<Pose, GraphEvalError> {
        self.evaluate_with_events(source, skeleton, dt)
            .map(|(pose, _)| pose)
    }

    /// 求值一帧并返回这一帧跨过的 Notify。
    ///
    /// 只有 ClipPlayer / StateMachine 节点会产生事件；BlendSpace 同时采样多个
    /// 剪辑，重复发事件会把一声脚步放大成几声，留给后续「主导样本」策略。
    /// 第一帧不会补发 time = 0 的事件，循环回绕会按两段扫描。
    pub fn evaluate_with_events(
        &mut self,
        source: &impl AnimGraphSource,
        skeleton: &Skeleton,
        dt: f32,
    ) -> Result<(Pose, Vec<GraphNotifyEvent>), GraphEvalError> {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.frame = self.frame.wrapping_add(1);
        self.pending_events.clear();
        let reference = skeleton.rest_pose();
        let mut visiting = vec![false; self.graph.nodes.len()];
        let pose = self.evaluate_node(
            self.graph.output,
            source,
            skeleton,
            &reference,
            dt,
            &mut visiting,
            &mut HashSet::new(),
        )?;
        Ok((pose, std::mem::take(&mut self.pending_events)))
    }

    fn record_notifies(
        &mut self,
        node: &str,
        clip_path: &str,
        clip: &Clip,
        previous_local: f32,
        current_local: f32,
    ) {
        for notify in clip.notifies_between(previous_local, current_local) {
            self.pending_events.push(GraphNotifyEvent {
                node: node.to_string(),
                clip_path: clip_path.to_string(),
                name: notify.name.clone(),
                time: notify.time,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_node(
        &mut self,
        index: u32,
        source: &impl AnimGraphSource,
        skeleton: &Skeleton,
        reference: &Pose,
        dt: f32,
        visiting: &mut [bool],
        stack: &mut HashSet<u32>,
    ) -> Result<Pose, GraphEvalError> {
        let index_usize = index as usize;
        if index_usize >= self.graph.nodes.len() {
            return Err(GraphEvalError::NodeIndex(index));
        }
        if self.runtime[index_usize].stamp == self.frame {
            return Ok(self.runtime[index_usize].cached.clone());
        }
        if !stack.insert(index) || visiting[index_usize] {
            return Err(GraphEvalError::Cycle {
                node: self.graph.nodes[index_usize].name.clone(),
            });
        }
        visiting[index_usize] = true;

        let node = self.graph.nodes[index_usize].clone();
        let pose = match node.kind {
            GraphNodeKind::ClipPlayer => {
                self.evaluate_clip(index_usize, &node, source, reference, dt)?
            }
            GraphNodeKind::BlendSpace1D | GraphNodeKind::BlendSpace2D => {
                self.evaluate_blend_space(index_usize, &node, source, reference, dt)?
            }
            GraphNodeKind::BlendStack | GraphNodeKind::LayeredBlend => {
                self.evaluate_stack(&node, source, skeleton, reference, dt, visiting, stack)?
            }
            GraphNodeKind::Slot => self.evaluate_slot(
                index_usize,
                &node,
                source,
                skeleton,
                reference,
                dt,
                visiting,
                stack,
            )?,
            GraphNodeKind::CachedPose | GraphNodeKind::Output => {
                let Some(input) = node.inputs.first() else {
                    return Err(GraphEvalError::MissingInput {
                        node: node.name.clone(),
                    });
                };
                self.evaluate_node(input.node, source, skeleton, reference, dt, visiting, stack)?
            }
            GraphNodeKind::StateMachine => {
                self.evaluate_state_machine(index_usize, &node, source, reference, dt)?
            }
        };

        visiting[index_usize] = false;
        stack.remove(&index);
        self.runtime[index_usize].stamp = self.frame;
        self.runtime[index_usize].cached = pose.clone();
        Ok(pose)
    }

    fn evaluate_clip(
        &mut self,
        index: usize,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        reference: &Pose,
        dt: f32,
    ) -> Result<Pose, GraphEvalError> {
        let path = node
            .clips
            .first()
            .ok_or_else(|| GraphEvalError::MissingClip {
                node: node.name.clone(),
                path: String::new(),
            })?;
        let clip = source
            .clip(path)
            .ok_or_else(|| GraphEvalError::MissingClip {
                node: node.name.clone(),
                path: path.clone(),
            })?;
        let previous_time = self.runtime[index].time;
        let time = advance_time(previous_time, dt, node.speed);
        self.runtime[index].time = time;
        let previous_local = local_time(clip.duration, node.looped, previous_time);
        let local = local_time(clip.duration, node.looped, time);
        if dt > 0.0 && node.speed >= 0.0 {
            self.record_notifies(&node.name, path, &clip, previous_local, local);
        }
        let mut pose = reference.clone();
        clip.sample_into(local, &mut pose);
        Ok(pose)
    }

    fn evaluate_blend_space(
        &mut self,
        index: usize,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        reference: &Pose,
        dt: f32,
    ) -> Result<Pose, GraphEvalError> {
        let previous_time = self.runtime[index].time;
        let duration = node
            .clips
            .iter()
            .filter_map(|path| source.clip(path).map(|clip| clip.duration))
            .fold(0.0f32, f32::max);
        let previous_phase = if duration > 0.0 {
            previous_time.rem_euclid(duration) / duration
        } else {
            0.0
        };
        let time = advance_time(previous_time, dt, node.speed);
        self.runtime[index].time = time;
        let phase = if duration > 0.0 {
            time.rem_euclid(duration) / duration
        } else {
            0.0
        };

        // 每样本速度：给这个节点维护独立播放头，相位数组按 node.clips 顺序。
        let speeds = self
            .blend_space_speeds
            .get(&node.name)
            .cloned()
            .or_else(|| (!node.sample_speeds.is_empty()).then(|| node.sample_speeds.clone()));
        let (previous_sample_phases, sample_phases) = if let Some(speeds) = speeds {
            let runtime = &mut self.runtime[index];
            if runtime.sample_times.len() != node.clips.len() {
                runtime.sample_times = vec![0.0; node.clips.len()];
            }
            let previous = runtime.sample_times.clone();
            for (sample, time) in runtime.sample_times.iter_mut().enumerate() {
                let speed = speeds
                    .get(sample)
                    .copied()
                    .filter(|speed| speed.is_finite())
                    .unwrap_or(1.0);
                *time = (*time + dt * speed).rem_euclid(1.0);
            }
            (Some(previous), Some(runtime.sample_times.clone()))
        } else {
            (None, None)
        };

        let mut pose = reference.clone();
        if node.kind == GraphNodeKind::BlendSpace1D {
            let samples = node
                .clips
                .iter()
                .enumerate()
                .map(|(sample, path)| {
                    let clip = source
                        .clip(path)
                        .ok_or_else(|| GraphEvalError::MissingClip {
                            node: node.name.clone(),
                            path: path.clone(),
                        })?;
                    Ok(BlendSample {
                        position: node
                            .positions
                            .get(sample)
                            .map_or(0.0, |position| position[0]),
                        clip,
                    })
                })
                .collect::<Result<Vec<_>, GraphEvalError>>()?;
            let parameter = self.parameters.float(&node.parameter_x).unwrap_or(0.0);
            let mut space = BlendSpace1D::new(samples, reference.clone());
            let clips: Vec<Arc<Clip>> = space
                .samples()
                .iter()
                .map(|sample| sample.clip.clone())
                .collect();
            let ordered = sample_phases
                .as_ref()
                .map(|phases| self.phases_in_space_order(&clips, node, source, phases));
            let ordered_previous = previous_sample_phases
                .as_ref()
                .map(|phases| self.phases_in_space_order(&clips, node, source, phases));
            let evaluated = match ordered.as_ref() {
                Some(phases) => space.evaluate_into_with_phases(parameter, phases, &mut pose),
                None => space.evaluate_into(parameter, phase, &mut pose),
            };
            if !evaluated {
                return Err(GraphEvalError::EmptyBlendSpace {
                    node: node.name.clone(),
                });
            }
            let dominant_index = space.dominant_sample(parameter);
            let dominant = if self.blend_space_notify == BlendSpaceNotifyPolicy::Dominant {
                dominant_index
                    .and_then(|sample| space.samples().get(sample))
                    .map(|sample| sample.clip.clone())
            } else {
                None
            };
            let (previous_phase, phase) = match (&ordered_previous, &ordered, dominant_index) {
                (Some(previous), Some(current), Some(index)) => (
                    previous.get(index).copied().unwrap_or(previous_phase),
                    current.get(index).copied().unwrap_or(phase),
                ),
                _ => (previous_phase, phase),
            };
            self.record_blend_space_notifies(
                &node.name,
                node,
                source,
                dominant,
                previous_phase,
                phase,
                dt,
            );
        } else {
            let samples = node
                .clips
                .iter()
                .enumerate()
                .map(|(sample, path)| {
                    let clip = source
                        .clip(path)
                        .ok_or_else(|| GraphEvalError::MissingClip {
                            node: node.name.clone(),
                            path: path.clone(),
                        })?;
                    Ok(BlendSample2D {
                        position: node.positions.get(sample).copied().unwrap_or([0.0, 0.0]),
                        clip,
                    })
                })
                .collect::<Result<Vec<_>, GraphEvalError>>()?;
            let at = [
                self.parameters.float(&node.parameter_x).unwrap_or(0.0),
                self.parameters.float(&node.parameter_y).unwrap_or(0.0),
            ];
            let mut space = BlendSpace2D::new(samples, reference.clone());
            let clips: Vec<Arc<Clip>> = space
                .samples()
                .iter()
                .map(|sample| sample.clip.clone())
                .collect();
            let ordered = sample_phases
                .as_ref()
                .map(|phases| self.phases_in_space_order(&clips, node, source, phases));
            let ordered_previous = previous_sample_phases
                .as_ref()
                .map(|phases| self.phases_in_space_order(&clips, node, source, phases));
            let evaluated = match ordered.as_ref() {
                Some(phases) => space.evaluate_into_with_phases(at, phases, &mut pose),
                None => space.evaluate_into(at, phase, &mut pose),
            };
            if !evaluated {
                return Err(GraphEvalError::EmptyBlendSpace {
                    node: node.name.clone(),
                });
            }
            let dominant_index = space.dominant_sample(at);
            let dominant = if self.blend_space_notify == BlendSpaceNotifyPolicy::Dominant {
                dominant_index
                    .and_then(|sample| space.samples().get(sample))
                    .map(|sample| sample.clip.clone())
            } else {
                None
            };
            let (previous_phase, phase) = match (&ordered_previous, &ordered, dominant_index) {
                (Some(previous), Some(current), Some(index)) => (
                    previous.get(index).copied().unwrap_or(previous_phase),
                    current.get(index).copied().unwrap_or(phase),
                ),
                _ => (previous_phase, phase),
            };
            self.record_blend_space_notifies(
                &node.name,
                node,
                source,
                dominant,
                previous_phase,
                phase,
                dt,
            );
        }
        Ok(pose)
    }

    /// 让权重最大的 BlendSpace 样本按自己的本地时间区间发 Notify。
    #[allow(clippy::too_many_arguments)]
    fn record_blend_space_notifies(
        &mut self,
        node_name: &str,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        clip: Option<Arc<Clip>>,
        previous_phase: f32,
        phase: f32,
        dt: f32,
    ) {
        if dt <= 0.0 || node.speed < 0.0 {
            return;
        }
        let Some(clip) = clip else {
            return;
        };
        let Some(path) = node.clips.iter().find(|path| {
            source
                .clip(path)
                .is_some_and(|candidate| Arc::ptr_eq(&candidate, &clip))
        }) else {
            return;
        };
        let from = previous_phase * clip.duration;
        let to = phase * clip.duration;
        self.record_notifies(node_name, path, &clip, from, to);
    }

    /// 把按 `node.clips` 顺序的相位重排成 BlendSpace 内部样本顺序。
    fn phases_in_space_order(
        &self,
        samples: &[Arc<Clip>],
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        phases: &[f32],
    ) -> Vec<f32> {
        samples
            .iter()
            .map(|clip| {
                node.clips
                    .iter()
                    .position(|path| {
                        source
                            .clip(path)
                            .is_some_and(|candidate| Arc::ptr_eq(&candidate, clip))
                    })
                    .and_then(|index| phases.get(index))
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_slot(
        &mut self,
        index: usize,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        skeleton: &Skeleton,
        reference: &Pose,
        dt: f32,
        visiting: &mut [bool],
        stack: &mut HashSet<u32>,
    ) -> Result<Pose, GraphEvalError> {
        let Some(input) = node.inputs.first() else {
            return Err(GraphEvalError::MissingInput {
                node: node.name.clone(),
            });
        };
        let base =
            self.evaluate_node(input.node, source, skeleton, reference, dt, visiting, stack)?;
        // MontagePlayer 优先：游戏侧 play/enqueue 的段覆盖作者写的静态 slot 剪辑。
        let mut montage_events = Vec::new();
        let mut montage_pose = None;
        if let Some(player) = self.montages.get_mut(&node.name) {
            if player.is_playing() {
                player.tick(dt);
                montage_events = player.take_notifies();
                let mut overlay = reference.clone();
                if player.sample_into(reference, &mut overlay) {
                    montage_pose = Some((overlay, player.active_weight()));
                }
            }
        }
        for event in montage_events {
            self.pending_events.push(GraphNotifyEvent {
                node: node.name.clone(),
                clip_path: event.clip_name,
                name: event.name,
                time: event.time,
            });
        }
        if let Some((overlay, weight)) = montage_pose {
            if weight >= 1.0 {
                return Ok(overlay);
            }
            if weight <= 0.0 {
                return Ok(base);
            }
            let mut blended = Pose::default();
            blend_into(&mut blended, &base, &overlay, weight);
            return Ok(blended);
        }

        let Some(path) = node.clips.first().filter(|path| !path.trim().is_empty()) else {
            return Ok(base);
        };
        let weight = if node.weight.is_finite() {
            node.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if weight <= 0.0 {
            return Ok(base);
        }
        let clip = source
            .clip(path)
            .ok_or_else(|| GraphEvalError::MissingClip {
                node: node.name.clone(),
                path: path.clone(),
            })?;
        let previous_time = self.runtime[index].time;
        let time = advance_time(previous_time, dt, node.speed);
        self.runtime[index].time = time;
        let previous_local = local_time(clip.duration, node.looped, previous_time);
        let local = local_time(clip.duration, node.looped, time);
        if dt > 0.0 && node.speed >= 0.0 {
            self.record_notifies(&node.name, path, &clip, previous_local, local);
        }
        let mut overlay = reference.clone();
        clip.sample_into(local, &mut overlay);
        if weight >= 1.0 {
            return Ok(overlay);
        }
        let mut blended = Pose::default();
        blend_into(&mut blended, &base, &overlay, weight);
        Ok(blended)
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_stack(
        &mut self,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        skeleton: &Skeleton,
        reference: &Pose,
        dt: f32,
        visiting: &mut [bool],
        stack: &mut HashSet<u32>,
    ) -> Result<Pose, GraphEvalError> {
        let Some(first) = node.inputs.first() else {
            return Err(GraphEvalError::MissingInput {
                node: node.name.clone(),
            });
        };
        let base =
            self.evaluate_node(first.node, source, skeleton, reference, dt, visiting, stack)?;
        if node.inputs.len() == 1 {
            return Ok(base);
        }
        let mut blend_stack = BlendStack::new(base, reference.clone());
        for input in node.inputs.iter().skip(1) {
            let layer_pose =
                self.evaluate_node(input.node, source, skeleton, reference, dt, visiting, stack)?;
            let mask = if input.bone_mask.is_empty() {
                BoneMask::all()
            } else {
                let names: Vec<&str> = input.bone_mask.iter().map(String::as_str).collect();
                BoneMask::from_names(skeleton, &names)
            };
            blend_stack.push(
                BlendLayer::new(layer_pose)
                    .with_weight(input.weight)
                    .with_mask(mask),
            );
        }
        let mut out = Pose::default();
        blend_stack.evaluate_into(&mut out);
        Ok(out)
    }

    fn evaluate_state_machine(
        &mut self,
        index: usize,
        node: &GraphNodeInfo,
        source: &impl AnimGraphSource,
        reference: &Pose,
        dt: f32,
    ) -> Result<Pose, GraphEvalError> {
        let machine = source
            .state_machine(&node.state_machine_path)
            .ok_or_else(|| GraphEvalError::MissingStateMachine {
                node: node.name.clone(),
                path: node.state_machine_path.clone(),
            })?;
        if machine.states.is_empty() {
            return Err(GraphEvalError::EmptyStateMachine {
                node: node.name.clone(),
            });
        }

        let previous_state = self.runtime[index].state.min(machine.states.len() - 1);
        let previous_time = self.runtime[index].time;
        let mut state = previous_state;
        let mut state_time = self.runtime[index].time;
        let current = &machine.states[state];
        let current_clip =
            if current.clip_path.trim().is_empty() {
                None
            } else {
                Some(source.clip(&current.clip_path).ok_or_else(|| {
                    GraphEvalError::MissingClip {
                        node: node.name.clone(),
                        path: current.clip_path.clone(),
                    }
                })?)
            };
        let current_duration = current_clip.as_ref().map_or(0.0, |clip| clip.duration);
        let current_looped = current.looped;
        let speed = sanitize_speed(current.speed);
        let previous_local = local_time(current_duration, current_looped, previous_time);
        state_time = advance_time(state_time, dt, speed);
        let active_local = local_time(current_duration, current_looped, state_time);
        let transition =
            machine.next_transition(state, &mut self.parameters, state_time, current_duration);
        let suppress = transition.is_some()
            && self.state_transition_notify == StateTransitionNotifyPolicy::SuppressOnTransition;
        if dt > 0.0 && speed >= 0.0 && !suppress {
            if let Some(clip) = current_clip.as_ref() {
                self.record_notifies(
                    &node.name,
                    &current.clip_path,
                    clip,
                    previous_local,
                    active_local,
                );
            }
        }

        let mut previous = None;
        if let Some(target) = transition.filter(|target| target.state < machine.states.len()) {
            let from_clip = current_clip.as_ref();
            if let Some(clip) = from_clip {
                let mut pose = reference.clone();
                let local = local_time(clip.duration, current.looped, state_time);
                clip.sample_into(local, &mut pose);
                previous = Some(pose);
            }
            state = target.state;
            state_time = 0.0;
            self.runtime[index].blend = Some(StateBlend {
                from: previous.clone().unwrap_or_else(|| reference.clone()),
                elapsed: 0.0,
                total: target.duration.max(0.0),
            });
        }

        let state_info = &machine.states[state];
        let clip =
            if state_info.clip_path.trim().is_empty() {
                None
            } else {
                Some(source.clip(&state_info.clip_path).ok_or_else(|| {
                    GraphEvalError::MissingClip {
                        node: node.name.clone(),
                        path: state_info.clip_path.clone(),
                    }
                })?)
            };
        let mut pose = reference.clone();
        if let Some(clip) = clip {
            let local = local_time(clip.duration, state_info.looped, state_time);
            clip.sample_into(local, &mut pose);
        }

        if let Some(blend) = self.runtime[index].blend.as_mut() {
            blend.elapsed += dt;
            if blend.total > 0.0 {
                let weight = (blend.elapsed / blend.total).clamp(0.0, 1.0);
                let mut blended = reference.clone();
                blend_into(&mut blended, &blend.from, &pose, weight);
                pose = blended;
                if weight >= 1.0 {
                    self.runtime[index].blend = None;
                }
            } else {
                self.runtime[index].blend = None;
            }
        }

        let runtime = &mut self.runtime[index];
        runtime.state = state;
        runtime.time = state_time;
        Ok(pose)
    }
}

fn advance_time(time: f32, dt: f32, speed: f32) -> f32 {
    let speed = sanitize_speed(speed);
    let next = time + dt * speed;
    if next.is_finite() {
        next
    } else {
        time
    }
}

fn sanitize_speed(speed: f32) -> f32 {
    if speed.is_finite() {
        speed
    } else {
        1.0
    }
}

fn local_time(duration: f32, looped: bool, time: f32) -> f32 {
    if duration <= 0.0 {
        return 0.0;
    }
    if looped {
        time.rem_euclid(duration)
    } else {
        time.clamp(0.0, duration)
    }
}

#[cfg(test)]
#[path = "graph_eval_tests.rs"]
mod tests;
