use std::collections::HashMap;
use std::sync::Arc;

use glam::{Mat4, Vec3};

use super::{AnimGraphEvaluator, AnimGraphSource, GraphEvalError};
use crate::clip::{Clip, Track, Vec3Key};
use crate::graph_eval::GraphNodeKind;
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
