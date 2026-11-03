use std::collections::HashMap;
use std::sync::Arc;

use glam::{Mat4, Vec3};

use super::{AnimGraphEvaluator, AnimGraphSource, GraphEvalError};
use crate::clip::{Clip, ClipNotify, Track, Vec3Key};
use crate::graph_eval::{BlendSpaceNotifyPolicy, GraphNodeKind, StateTransitionNotifyPolicy};
use crate::montage::Montage;
use prism_asset::formats_anim_graph::{
    AnimGraphInfo, GraphInputInfo, GraphNodeInfo, GraphParameterInfo, GraphParameterKind,
};

use crate::skeleton::{Joint, JointTransform, Skeleton};
use crate::state_machine::{AnimState, AnimTransition, StateMachine, TransitionCondition};

fn skeleton() -> Skeleton {
    Skeleton::new(vec![Joint {
        name: "root".into(),
        parent: None,
        reference_local: JointTransform::IDENTITY,
        inverse_bind: Mat4::IDENTITY,
    }])
    .expect("skeleton")
}

fn translation_clip(name: &str, x: f32) -> Clip {
    Clip {
        name: name.into(),
        duration: 1.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 0,
            translation: vec![
                Vec3Key {
                    time: 0.0,
                    value: Vec3::new(x, 0.0, 0.0),
                },
                Vec3Key {
                    time: 1.0,
                    value: Vec3::new(x, 0.0, 0.0),
                },
            ],
            rotation: Vec::new(),
            scale: Vec::new(),
        }],
        ..Default::default()
    }
}

fn node(name: &str, kind: GraphNodeKind) -> GraphNodeInfo {
    GraphNodeInfo {
        name: name.into(),
        kind,
        looped: true,
        additive: false,
        speed: 1.0,
        weight: 1.0,
        clips: Vec::new(),
        positions: Vec::new(),
        sample_speeds: Vec::new(),
        parameter_x: String::new(),
        parameter_y: String::new(),
        state_machine_path: String::new(),
        inputs: Vec::new(),
    }
}

fn output(input: u32) -> GraphNodeInfo {
    let mut node = node("out", GraphNodeKind::Output);
    node.inputs.push(GraphInputInfo {
        node: input,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    node
}

fn graph(nodes: Vec<GraphNodeInfo>, output: u32) -> AnimGraphInfo {
    AnimGraphInfo {
        parameters: Vec::new(),
        nodes,
        output,
    }
}

struct Source {
    clips: HashMap<String, Arc<Clip>>,
    machines: HashMap<String, Arc<StateMachine>>,
}

impl Source {
    fn new(clips: &[(&str, Clip)]) -> Self {
        Self {
            clips: clips
                .iter()
                .map(|(path, clip)| ((*path).to_string(), Arc::new(clip.clone())))
                .collect(),
            machines: HashMap::new(),
        }
    }

    fn with_machine(mut self, path: &str, machine: StateMachine) -> Self {
        self.machines.insert(path.to_string(), Arc::new(machine));
        self
    }
}

impl AnimGraphSource for Source {
    fn clip(&self, path: &str) -> Option<Arc<Clip>> {
        self.clips.get(path).cloned()
    }

    fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>> {
        self.machines.get(path).cloned()
    }
}

fn x(pose: &crate::Pose) -> f32 {
    pose.local(0).unwrap().translation.x
}

#[test]
fn a_clip_player_samples_the_clip_and_advances_its_own_time() {
    let mut player = node("idle", GraphNodeKind::ClipPlayer);
    player.clips.push("idle.ranim".into());
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![player, output(0)], 1));
    let skeleton = skeleton();
    let source = Source::new(&[("idle.ranim", translation_clip("idle", 2.0))]);

    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 2.0).abs() < 1e-5);
    // A second evaluation on the same frame is cached (no double time advance),
    // and a new frame still returns the same static clip.
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.5)
        .expect("evaluates");
    assert!((x(&pose) - 2.0).abs() < 1e-5);
}

#[test]
fn blend_space_1d_uses_the_named_parameter() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace1D);
    space.clips = vec!["idle.ranim".into(), "run.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0]];
    space.parameter_x = "speed".into();
    let mut graph = graph(vec![space, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "speed".into(),
        kind: GraphParameterKind::Float,
        default: 0.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    evaluator.parameters_mut().set_float("speed", 5.0);

    let skeleton = skeleton();
    let source = Source::new(&[
        ("idle.ranim", translation_clip("idle", 0.0)),
        ("run.ranim", translation_clip("run", 10.0)),
    ]);
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 5.0).abs() < 1e-4, "x={}", x(&pose));
}

#[test]
fn blend_space_2d_uses_both_named_parameters() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace2D);
    space.clips = vec!["a.ranim".into(), "b.ranim".into(), "c.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0], [0.0, 10.0]];
    space.parameter_x = "speed".into();
    space.parameter_y = "direction".into();
    let mut graph = graph(vec![space, output(0)], 1);
    for name in ["speed", "direction"] {
        graph.parameters.push(GraphParameterInfo {
            name: name.into(),
            kind: GraphParameterKind::Float,
            default: 0.0,
        });
    }
    let mut evaluator = AnimGraphEvaluator::new(graph);
    evaluator.parameters_mut().set_float("speed", 10.0);
    let skeleton = skeleton();
    let source = Source::new(&[
        ("a.ranim", translation_clip("a", 0.0)),
        ("b.ranim", translation_clip("b", 10.0)),
        ("c.ranim", translation_clip("c", 20.0)),
    ]);
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 10.0).abs() < 1e-4, "x={}", x(&pose));
}

#[test]
fn a_blend_space_emits_notifies_from_its_dominant_sample() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace1D);
    space.clips = vec!["idle.ranim".into(), "run.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0]];
    space.parameter_x = "speed".into();
    let mut graph = graph(vec![space, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "speed".into(),
        kind: GraphParameterKind::Float,
        default: 10.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    let skeleton = skeleton();
    let source = Source::new(&[
        ("idle.ranim", translation_clip("idle", 0.0)),
        ("run.ranim", clip_with_notify("run", 0.5, "Footstep.R")),
    ]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.6)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "Footstep.R");
    assert_eq!(events[0].clip_path, "run.ranim");
    assert_eq!(
        evaluator.blend_space_notify_policy(),
        BlendSpaceNotifyPolicy::Dominant
    );
}

#[test]
fn the_none_blend_space_policy_suppresses_sample_notifies() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace1D);
    space.clips = vec!["idle.ranim".into(), "run.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0]];
    space.parameter_x = "speed".into();
    let mut graph = graph(vec![space, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "speed".into(),
        kind: GraphParameterKind::Float,
        default: 10.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    evaluator.set_blend_space_notify_policy(BlendSpaceNotifyPolicy::None);
    let source = Source::new(&[
        ("idle.ranim", translation_clip("idle", 0.0)),
        ("run.ranim", clip_with_notify("run", 0.5, "Footstep.R")),
    ]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton(), 0.6)
        .expect("evaluates");
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn a_two_d_blend_space_emits_from_the_dominant_vertex() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace2D);
    space.clips = vec!["a.ranim".into(), "b.ranim".into(), "c.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0], [0.0, 10.0]];
    space.parameter_x = "speed".into();
    space.parameter_y = "direction".into();
    let mut graph = graph(vec![space, output(0)], 1);
    for name in ["speed", "direction"] {
        graph.parameters.push(GraphParameterInfo {
            name: name.into(),
            kind: GraphParameterKind::Float,
            default: 0.0,
        });
    }
    let mut evaluator = AnimGraphEvaluator::new(graph);
    evaluator.parameters_mut().set_float("speed", 10.0);
    let source = Source::new(&[
        ("a.ranim", translation_clip("a", 0.0)),
        ("b.ranim", clip_with_notify("b", 0.5, "Footstep.B")),
        ("c.ranim", translation_clip("c", 20.0)),
    ]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton(), 0.6)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "Footstep.B");
    assert_eq!(events[0].clip_path, "b.ranim");
}

#[test]
fn a_blend_stack_uses_the_first_input_as_base() {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut layer = node("layer", GraphNodeKind::ClipPlayer);
    layer.clips.push("layer.ranim".into());
    let mut stack = node("stack", GraphNodeKind::BlendStack);
    stack.inputs = vec![
        GraphInputInfo {
            node: 0,
            weight: 1.0,
            bone_mask: Vec::new(),
        },
        GraphInputInfo {
            node: 1,
            weight: 0.5,
            bone_mask: Vec::new(),
        },
    ];
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![base, layer, stack, output(2)], 3));
    let skeleton = skeleton();
    let source = Source::new(&[
        ("base.ranim", translation_clip("base", 0.0)),
        ("layer.ranim", translation_clip("layer", 10.0)),
    ]);
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 5.0).abs() < 1e-4, "x={}", x(&pose));
}

#[test]
fn a_state_machine_transitions_on_a_trigger() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut graph = graph(vec![machine_node, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "go".into(),
        kind: GraphParameterKind::Trigger,
        default: 0.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    let skeleton = skeleton();
    let machine = StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("walk", "walk.ranim"),
        ],
        vec![AnimTransition::new(0, 1)
            .with_conditions(vec![TransitionCondition::Trigger { name: "go".into() }])],
    );
    let source = Source::new(&[
        ("idle.ranim", translation_clip("idle", 0.0)),
        ("walk.ranim", translation_clip("walk", 5.0)),
    ])
    .with_machine("hero.rasm", machine);

    let pose = evaluator.evaluate(&source, &skeleton, 0.1).expect("idle");
    assert!(x(&pose).abs() < 1e-5);
    assert_eq!(evaluator.state_machine_state("sm"), Some(0));

    evaluator.parameters_mut().set_trigger("go");
    let pose = evaluator.evaluate(&source, &skeleton, 0.1).expect("walk");
    assert!((x(&pose) - 5.0).abs() < 1e-5, "x={}", x(&pose));
    assert_eq!(evaluator.state_machine_state("sm"), Some(1));
}

#[test]
fn node_snapshots_report_link_weights() {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut layer = node("layer", GraphNodeKind::ClipPlayer);
    layer.clips.push("layer.ranim".into());
    let mut stack = node("stack", GraphNodeKind::BlendStack);
    stack.inputs = vec![
        GraphInputInfo {
            node: 0,
            weight: 1.0,
            bone_mask: Vec::new(),
        },
        GraphInputInfo {
            node: 1,
            weight: 0.25,
            bone_mask: Vec::new(),
        },
    ];
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![base, layer, stack, output(2)], 3));
    let skeleton = skeleton();
    let source = Source::new(&[
        ("base.ranim", translation_clip("base", 0.0)),
        ("layer.ranim", translation_clip("layer", 10.0)),
    ]);
    evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    let snapshots = evaluator.node_snapshots();
    assert!((snapshots[0].weight - 1.0).abs() < 1e-6, "base 权重 1");
    assert!(
        (snapshots[1].weight - 0.25).abs() < 1e-6,
        "layer 权重 0.25，得到 {}",
        snapshots[1].weight
    );
    assert!((snapshots[2].weight - 1.0).abs() < 1e-6);
    assert!((snapshots[3].weight - 1.0).abs() < 1e-6);
}

#[test]
fn node_snapshots_report_time_state_and_blend() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut graph = graph(vec![machine_node, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "go".into(),
        kind: GraphParameterKind::Trigger,
        default: 0.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    let skeleton = skeleton();
    let machine = StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("walk", "walk.ranim"),
        ],
        vec![AnimTransition::new(0, 1)
            .with_conditions(vec![TransitionCondition::Trigger { name: "go".into() }])
            .with_duration(0.2)],
    );
    let source = Source::new(&[
        ("idle.ranim", translation_clip("idle", 0.0)),
        ("walk.ranim", translation_clip("walk", 5.0)),
    ])
    .with_machine("hero.rasm", machine);

    evaluator.evaluate(&source, &skeleton, 0.1).expect("idle");
    let snapshots = evaluator.node_snapshots();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].name, "sm");
    assert_eq!(snapshots[0].kind, GraphNodeKind::StateMachine);
    assert_eq!(snapshots[0].state, 0);
    assert!(!snapshots[0].blending);
    assert!((snapshots[0].time - 0.1).abs() < 1e-5);
    assert_eq!(snapshots[1].kind, GraphNodeKind::Output);
    assert!((snapshots[0].weight - 1.0).abs() < 1e-6, "输出链上权重 1");
    assert!((snapshots[1].weight - 1.0).abs() < 1e-6, "输出节点权重 1");

    evaluator.parameters_mut().set_trigger("go");
    evaluator.evaluate(&source, &skeleton, 0.05).expect("walk");
    let snapshot = &evaluator.node_snapshots()[0];
    assert_eq!(snapshot.state, 1);
    assert_eq!(snapshot.state_name, "walk");
    assert_eq!(snapshot.state_clip, "walk.ranim");
    assert!(snapshot.blending, "转移中");
    assert!(
        snapshot.blend_weight > 0.0 && snapshot.blend_weight < 1.0,
        "weight {}",
        snapshot.blend_weight
    );

    evaluator.evaluate(&source, &skeleton, 0.2).expect("blend");
    assert!(!evaluator.node_snapshots()[0].blending);
}

fn transition_machine() -> StateMachine {
    StateMachine::new(
        vec![
            AnimState::new("idle", "idle.ranim"),
            AnimState::new("walk", "walk.ranim"),
        ],
        vec![AnimTransition::new(0, 1)
            .with_conditions(vec![TransitionCondition::Trigger { name: "go".into() }])],
    )
}

#[test]
fn a_transition_frame_keeps_leaving_state_notifies_by_default() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut graph = graph(vec![machine_node, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "go".into(),
        kind: GraphParameterKind::Trigger,
        default: 0.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    let source = Source::new(&[
        ("idle.ranim", clip_with_notify("idle", 0.5, "IdleStep")),
        ("walk.ranim", translation_clip("walk", 5.0)),
    ])
    .with_machine("hero.rasm", transition_machine());
    let skeleton = skeleton();
    evaluator.evaluate(&source, &skeleton, 0.1).expect("idle");
    evaluator.parameters_mut().set_trigger("go");
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.5)
        .expect("transitions");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "IdleStep");
}

#[test]
fn suppress_on_transition_drops_the_leaving_state_interval() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut graph = graph(vec![machine_node, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "go".into(),
        kind: GraphParameterKind::Trigger,
        default: 0.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    evaluator.set_state_transition_notify_policy(StateTransitionNotifyPolicy::SuppressOnTransition);
    let source = Source::new(&[
        ("idle.ranim", clip_with_notify("idle", 0.5, "IdleStep")),
        ("walk.ranim", translation_clip("walk", 5.0)),
    ])
    .with_machine("hero.rasm", transition_machine());
    let skeleton = skeleton();
    evaluator.evaluate(&source, &skeleton, 0.1).expect("idle");
    evaluator.parameters_mut().set_trigger("go");
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.5)
        .expect("transitions");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(evaluator.state_machine_state("sm"), Some(1));
}

#[test]
fn missing_resources_and_cycles_are_errors() {
    let mut player = node("missing", GraphNodeKind::ClipPlayer);
    player.clips.push("nope.ranim".into());
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![player, output(0)], 1));
    let skeleton = skeleton();
    let source = Source::new(&[]);
    assert!(matches!(
        evaluator.evaluate(&source, &skeleton, 0.0),
        Err(GraphEvalError::MissingClip { .. })
    ));

    let mut loop_node = node("loop", GraphNodeKind::Output);
    loop_node.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![loop_node], 0));
    assert!(matches!(
        evaluator.evaluate(&source, &skeleton, 0.0),
        Err(GraphEvalError::Cycle { .. })
    ));
}

#[test]
fn a_missing_state_machine_is_reported() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![machine_node, output(0)], 1));
    let source = Source::new(&[]);
    assert!(matches!(
        evaluator.evaluate(&source, &skeleton(), 0.0),
        Err(GraphEvalError::MissingStateMachine { .. })
    ));
}

#[test]
fn every_node_kind_is_covered_by_the_switch() {
    // 编译期保证枚举匹配完整；运行时确认 Slots / CachedPose 也是透明传递。
    let mut clip = node("clip", GraphNodeKind::ClipPlayer);
    clip.clips.push("a.ranim".into());
    let mut slot = node("slot", GraphNodeKind::Slot);
    slot.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut cached = node("cached", GraphNodeKind::CachedPose);
    cached.inputs.push(GraphInputInfo {
        node: 1,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![clip, slot, cached, output(2)], 3));
    let source = Source::new(&[("a.ranim", translation_clip("a", 3.0))]);
    let pose = evaluator
        .evaluate(&source, &skeleton(), 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 3.0).abs() < 1e-5);
}

fn clip_with_notify(name: &str, time: f32, notify: &str) -> Clip {
    let mut clip = translation_clip(name, 0.0);
    clip.notifies.push(ClipNotify {
        time,
        name: notify.into(),
    });
    clip
}

fn single_clip_graph(path: &str, looped: bool) -> AnimGraphInfo {
    let mut clip = node("clip", GraphNodeKind::ClipPlayer);
    clip.clips.push(path.into());
    clip.looped = looped;
    graph(vec![clip, output(0)], 1)
}

#[test]
fn a_clip_player_emits_notifies_between_frames() {
    let mut evaluator = AnimGraphEvaluator::new(single_clip_graph("foot.ranim", false));
    let skeleton = skeleton();
    let source = Source::new(&[("foot.ranim", clip_with_notify("foot", 0.5, "Footstep.L"))]);

    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.4)
        .expect("evaluates");
    assert!(events.is_empty(), "event is still ahead: {events:?}");

    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.2)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "Footstep.L");
    assert_eq!(events[0].node, "clip");
    assert_eq!(events[0].clip_path, "foot.ranim");
    assert!((events[0].time - 0.5).abs() < 1e-5);
}

#[test]
fn a_notify_at_time_zero_is_not_retroactively_emitted() {
    let mut evaluator = AnimGraphEvaluator::new(single_clip_graph("foot.ranim", false));
    let skeleton = skeleton();
    let source = Source::new(&[("foot.ranim", clip_with_notify("foot", 0.0, "Start"))]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.5)
        .expect("evaluates");
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn a_looping_clip_emits_notifies_across_the_wrap() {
    let mut evaluator = AnimGraphEvaluator::new(single_clip_graph("foot.ranim", true));
    let skeleton = skeleton();
    let source = Source::new(&[("foot.ranim", clip_with_notify("foot", 0.9, "Wrap"))]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.5)
        .expect("first half");
    assert!(events.is_empty(), "{events:?}");
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.5)
        .expect("wrap");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "Wrap");
}

#[test]
fn a_shared_clip_node_emits_each_notify_only_once() {
    let mut clip = node("clip", GraphNodeKind::ClipPlayer);
    clip.clips.push("foot.ranim".into());
    clip.looped = false;
    let mut stack = node("stack", GraphNodeKind::BlendStack);
    stack.inputs = vec![
        GraphInputInfo {
            node: 0,
            weight: 1.0,
            bone_mask: Vec::new(),
        },
        GraphInputInfo {
            node: 0,
            weight: 1.0,
            bone_mask: Vec::new(),
        },
    ];
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![clip, stack, output(1)], 2));
    let skeleton = skeleton();
    let source = Source::new(&[("foot.ranim", clip_with_notify("foot", 0.5, "Once"))]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.6)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
}

#[test]
fn a_state_machine_emits_notifies_from_its_current_state_clip() {
    let mut machine_node = node("sm", GraphNodeKind::StateMachine);
    machine_node.state_machine_path = "hero.rasm".into();
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![machine_node, output(0)], 1));
    let skeleton = skeleton();
    let machine = StateMachine::new(vec![AnimState::new("idle", "foot.ranim")], Vec::new());
    let source = Source::new(&[("foot.ranim", clip_with_notify("foot", 0.5, "IdleStep"))])
        .with_machine("hero.rasm", machine);

    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.4)
        .expect("before");
    assert!(events.is_empty(), "{events:?}");
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.2)
        .expect("after");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "IdleStep");
}

#[test]
fn a_slot_overlays_an_optional_montage_clip_by_weight() {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut slot = node("slot", GraphNodeKind::Slot);
    slot.clips.push("montage.ranim".into());
    slot.weight = 0.5;
    slot.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![base, slot, output(1)], 2));
    let skeleton = skeleton();
    let source = Source::new(&[
        ("base.ranim", translation_clip("base", 0.0)),
        ("montage.ranim", translation_clip("montage", 10.0)),
    ]);
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 5.0).abs() < 1e-5, "x={}", x(&pose));

    // Weight 0 keeps the base untouched.
    let mut evaluator = AnimGraphEvaluator::new(graph(
        vec![
            {
                let mut base = node("base", GraphNodeKind::ClipPlayer);
                base.clips.push("base.ranim".into());
                base
            },
            {
                let mut slot = node("slot", GraphNodeKind::Slot);
                slot.clips.push("montage.ranim".into());
                slot.weight = 0.0;
                slot.inputs.push(GraphInputInfo {
                    node: 0,
                    weight: 1.0,
                    bone_mask: Vec::new(),
                });
                slot
            },
            output(1),
        ],
        2,
    ));
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!(x(&pose).abs() < 1e-5, "x={}", x(&pose));
}

#[test]
fn a_slot_without_a_clip_is_transparent() {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut slot = node("slot", GraphNodeKind::Slot);
    slot.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![base, slot, output(1)], 2));
    let source = Source::new(&[("base.ranim", translation_clip("base", 3.0))]);
    let pose = evaluator
        .evaluate(&source, &skeleton(), 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 3.0).abs() < 1e-5);
}

#[test]
fn a_slot_montage_emits_its_own_notifies() {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut slot = node("slot", GraphNodeKind::Slot);
    slot.clips.push("montage.ranim".into());
    slot.looped = false;
    slot.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    let mut evaluator = AnimGraphEvaluator::new(graph(vec![base, slot, output(1)], 2));
    let skeleton = skeleton();
    let source = Source::new(&[
        ("base.ranim", translation_clip("base", 0.0)),
        (
            "montage.ranim",
            clip_with_notify("montage", 0.5, "MontageHit"),
        ),
    ]);
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.6)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "MontageHit");
    assert_eq!(events[0].node, "slot");
}

fn slot_graph() -> AnimGraphInfo {
    let mut base = node("base", GraphNodeKind::ClipPlayer);
    base.clips.push("base.ranim".into());
    let mut slot = node("slot", GraphNodeKind::Slot);
    slot.inputs.push(GraphInputInfo {
        node: 0,
        weight: 1.0,
        bone_mask: Vec::new(),
    });
    graph(vec![base, slot, output(1)], 2)
}

#[test]
fn an_evaluator_montage_overrides_the_slot_input() {
    let mut evaluator = AnimGraphEvaluator::new(slot_graph());
    let skeleton = skeleton();
    let source = Source::new(&[("base.ranim", translation_clip("base", 0.0))]);
    evaluator.play_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("montage", 10.0)))
            .with_weight(0.5)
            .with_looping(false),
    );
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!((x(&pose) - 5.0).abs() < 1e-5, "x={}", x(&pose));
}

#[test]
fn queued_evaluator_montages_advance_after_the_current_one() {
    let mut evaluator = AnimGraphEvaluator::new(slot_graph());
    let skeleton = skeleton();
    let source = Source::new(&[("base.ranim", translation_clip("base", 0.0))]);
    evaluator.play_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("a", 1.0))).with_looping(false),
    );
    evaluator.enqueue_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("b", 2.0))).with_looping(false),
    );
    let pose = evaluator.evaluate(&source, &skeleton, 0.4).expect("first");
    assert!((x(&pose) - 1.0).abs() < 1e-5, "x={}", x(&pose));
    let pose = evaluator.evaluate(&source, &skeleton, 0.7).expect("next");
    assert!((x(&pose) - 2.0).abs() < 1e-5, "x={}", x(&pose));
}

#[test]
fn montage_snapshots_report_slot_state() {
    let mut evaluator = AnimGraphEvaluator::new(slot_graph());
    let skeleton = skeleton();
    let source = Source::new(&[("base.ranim", translation_clip("base", 0.0))]);
    assert!(
        evaluator.montage_snapshots().is_empty(),
        "没播放过就没有 Slot"
    );

    evaluator.play_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("montage", 10.0))).with_weight(0.5),
    );
    evaluator.enqueue_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("next", 1.0))).with_looping(false),
    );
    evaluator
        .evaluate(&source, &skeleton, 0.2)
        .expect("evaluates");

    let snapshots = evaluator.montage_snapshots();
    assert_eq!(snapshots.len(), 1);
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.slot, "slot");
    assert_eq!(snapshot.clip.as_deref(), Some("montage"));
    assert!((snapshot.time.unwrap() - 0.2).abs() < 1e-5);
    assert!((snapshot.weight - 0.5).abs() < 1e-6);
    assert!(snapshot.playing && !snapshot.paused && !snapshot.finished);
    assert_eq!(snapshot.queued, 1);

    evaluator.pause_montage("slot");
    assert!(evaluator.montage_snapshots()[0].paused);
    evaluator.resume_montage("slot");
    assert!(!evaluator.montage_snapshots()[0].paused);
}

#[test]
fn stopping_a_slot_montage_returns_to_the_slot_input() {
    let mut evaluator = AnimGraphEvaluator::new(slot_graph());
    let skeleton = skeleton();
    let source = Source::new(&[("base.ranim", translation_clip("base", 0.0))]);
    evaluator.play_montage(
        "slot",
        Montage::new(Arc::new(translation_clip("montage", 10.0))),
    );
    evaluator.stop_montage("slot");
    let pose = evaluator
        .evaluate(&source, &skeleton, 0.0)
        .expect("evaluates");
    assert!(x(&pose).abs() < 1e-5, "x={}", x(&pose));
}

#[test]
fn slot_montage_notifies_reach_graph_events() {
    let mut evaluator = AnimGraphEvaluator::new(slot_graph());
    let skeleton = skeleton();
    let source = Source::new(&[("base.ranim", translation_clip("base", 0.0))]);
    evaluator.play_montage(
        "slot",
        Montage::new(Arc::new(clip_with_notify("montage", 0.5, "MontageStep"))).with_looping(false),
    );
    let (_, events) = evaluator
        .evaluate_with_events(&source, &skeleton, 0.6)
        .expect("evaluates");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].name, "MontageStep");
    assert_eq!(events[0].node, "slot");
    assert_eq!(events[0].clip_path, "montage");
}

fn ramp_clip(name: &str, end_x: f32) -> Clip {
    Clip {
        name: name.into(),
        duration: 1.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![Track {
            joint: 0,
            translation: vec![
                Vec3Key {
                    time: 0.0,
                    value: glam::Vec3::ZERO,
                },
                Vec3Key {
                    time: 1.0,
                    value: glam::Vec3::new(end_x, 0.0, 0.0),
                },
            ],
            rotation: Vec::new(),
            scale: Vec::new(),
        }],
        ..Default::default()
    }
}

#[test]
fn blend_space_per_sample_speeds_drive_independent_playheads() {
    let mut space = node("locomotion", GraphNodeKind::BlendSpace1D);
    space.clips = vec!["idle.ranim".into(), "run.ranim".into()];
    space.positions = vec![[0.0, 0.0], [10.0, 0.0]];
    space.sample_speeds = vec![1.0, 2.0];
    space.parameter_x = "speed".into();
    let mut graph = graph(vec![space, output(0)], 1);
    graph.parameters.push(GraphParameterInfo {
        name: "speed".into(),
        kind: GraphParameterKind::Float,
        default: 10.0,
    });
    let mut evaluator = AnimGraphEvaluator::new(graph);
    let source = Source::new(&[
        ("idle.ranim", ramp_clip("idle", 10.0)),
        ("run.ranim", ramp_clip("run", 20.0)),
    ]);
    let skeleton = skeleton();

    // run 的播放头速度是 2：0.25s 后到相位 0.5，线性轨道 x = 10。
    let pose = evaluator.evaluate(&source, &skeleton, 0.25).expect("first");
    assert!((x(&pose) - 10.0).abs() < 1e-4, "x={}", x(&pose));

    // 再来 0.25s 正好回到 0。
    let pose = evaluator.evaluate(&source, &skeleton, 0.25).expect("wrap");
    assert!(x(&pose).abs() < 1e-4, "x={}", x(&pose));

    // 清空配置后回到共享 phase。
    evaluator.set_blend_space_speeds("locomotion", Vec::new());
    assert!(evaluator.blend_space_speeds("locomotion").is_none());
}
