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
//! - Slot / CachedPose / Output 是透明传递节点；
//! - 同一帧内一个节点只求值一次（fan-out 不会重复推进时间）；
//! - 成环的图（例如手工构造的 `AnimGraphInfo`）返回错误而不是栈溢出。
//!
//! 状态机节点在求值器内部维护「当前状态 + 状态时间 + 交叉淡入快照」：转移条件
//! 用 [`StateMachine::next_transition`] 求值，Trigger 会被消耗；转移时长 > 0 时
//! 从转移前的姿态淡入。这和引擎 `animator` 的播放头语义一致，但这一层不碰 ECS。
//!
//! 已知边界：Slot 目前只做透明传递（Montage / Notify 队列是后续切片）；
//! CachedPose 复用每帧缓存，不跨帧冻结姿态。

use std::collections::HashSet;
use std::sync::Arc;

use prism_asset::formats_anim_graph::{
    AnimGraphInfo, GraphNodeInfo, GraphNodeKind, GraphParameterKind,
};

use crate::blend::{blend_into, BlendSample, BlendSpace1D};
use crate::blend_space2d::{BlendSample2D, BlendSpace2D};
use crate::blend_stack::{BlendLayer, BlendStack, BoneMask};
use crate::clip::Clip;
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::state_machine::{AnimParameters, StateMachine};

/// 求值器需要的资源：按路径查已解码的剪辑 / 状态机。
pub trait AnimGraphSource {
    fn clip(&self, path: &str) -> Option<Arc<Clip>>;
    fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>>;
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
}

#[derive(Debug, Clone)]
struct StateBlend {
    from: Pose,
    elapsed: f32,
    total: f32,
}

/// 一个动画图求值器实例，对应一个角色。
#[derive(Debug, Clone)]
pub struct AnimGraphEvaluator {
    graph: AnimGraphInfo,
    runtime: Vec<NodeRuntime>,
    parameters: AnimParameters,
    frame: u64,
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
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.frame = self.frame.wrapping_add(1);
        let reference = skeleton.rest_pose();
        let mut visiting = vec![false; self.graph.nodes.len()];
        self.evaluate_node(
            self.graph.output,
            source,
            skeleton,
            &reference,
            dt,
            &mut visiting,
            &mut HashSet::new(),
        )
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
            GraphNodeKind::Slot | GraphNodeKind::CachedPose | GraphNodeKind::Output => {
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
        let runtime = &mut self.runtime[index];
        runtime.time = advance_time(runtime.time, dt, node.speed);
        let local = local_time(clip.duration, node.looped, runtime.time);
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
        let runtime = &mut self.runtime[index];
        runtime.time = advance_time(runtime.time, dt, node.speed);
        let duration = node
            .clips
            .iter()
            .filter_map(|path| source.clip(path).map(|clip| clip.duration))
            .fold(0.0f32, f32::max);
        let phase = if duration > 0.0 {
            runtime.time.rem_euclid(duration) / duration
        } else {
            0.0
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
            if !space.evaluate_into(parameter, phase, &mut pose) {
                return Err(GraphEvalError::EmptyBlendSpace {
                    node: node.name.clone(),
                });
            }
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
            if !space.evaluate_into(at, phase, &mut pose) {
                return Err(GraphEvalError::EmptyBlendSpace {
                    node: node.name.clone(),
                });
            }
        }
        Ok(pose)
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

        let mut state = self.runtime[index].state.min(machine.states.len() - 1);
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
        let speed = sanitize_speed(current.speed);
        state_time = advance_time(state_time, dt, speed);

        let transition =
            machine.next_transition(state, &mut self.parameters, state_time, current_duration);

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
