//! 动画图 / 状态机资产的只读视图（#560 编辑器面板）。
//!
//! 这里不复制 cooker 的严格校验，只把作者 JSON 里适合展示的字段摊平成 UI 好用的
//! 结构：参数、节点类型、剪辑、连线、输出，以及状态机的状态 / 转移 / 条件。
//! 错误信息保留到第一个坏字段，面板可以直接显示。

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// 动画图中的一个参数。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimParameterView {
    pub name: String,
    pub kind: String,
    pub default: f32,
}

/// 动画图里的一条连线。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimInputView {
    pub node: String,
    pub weight: f32,
    /// 只影响这些关节（空 = 整条姿态）。
    pub bone_mask: Vec<String>,
}

/// 动画图中的一个节点。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimNodeView {
    pub name: String,
    pub kind: String,
    pub clips: Vec<String>,
    pub inputs: Vec<AnimInputView>,
    pub state_machine: String,
    pub parameter: String,
}

impl AnimNodeView {
    pub fn is_output(&self) -> bool {
        self.kind.eq_ignore_ascii_case("output")
    }
}

/// 整个动画图的只读摘要。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnimGraphView {
    pub parameters: Vec<AnimParameterView>,
    pub nodes: Vec<AnimNodeView>,
    pub output: Option<String>,
}

/// 状态机里的一个状态。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimStateView {
    pub name: String,
    pub clip: String,
    pub speed: f32,
    pub looped: bool,
}

/// 一条转移条件，`summary` 已经是可显示的形式。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimConditionView {
    /// `float_greater` / `float_less` / `bool_is` / `trigger`。
    pub kind: String,
    pub name: String,
    /// 只对 float 条件有意义。
    pub threshold: f32,
    /// 只对 `bool_is` 有意义。
    pub value: bool,
    pub summary: String,
}

/// 状态机里的一条转移。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimTransitionView {
    /// `None` 表示任意状态（`*` / `any` / 空串）。
    pub from: Option<String>,
    pub to: String,
    pub duration: f32,
    pub min_state_fraction: f32,
    pub conditions: Vec<AnimConditionView>,
}

/// 整个状态机的只读摘要。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnimStateMachineView {
    pub states: Vec<AnimStateView>,
    pub transitions: Vec<AnimTransitionView>,
}

/// 解析 `*.animgraph.json`。
pub fn parse_anim_graph_view(document: &Value) -> Result<AnimGraphView, String> {
    let parameters = document
        .get("parameters")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            let name = parameter
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("parameter #{index} needs a name"))?;
            Ok(AnimParameterView {
                name: name.to_string(),
                kind: parameter
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("float")
                    .to_string(),
                default: parameter
                    .get("default")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0) as f32,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let nodes_json = document
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "animation graph needs a `nodes` array".to_string())?;
    if nodes_json.is_empty() {
        return Err("animation graph needs at least one node".to_string());
    }

    let mut nodes = Vec::with_capacity(nodes_json.len());
    for (index, node) in nodes_json.iter().enumerate() {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("node #{index} needs a non-empty name"))?
            .to_string();
        let mut clips = Vec::new();
        if let Some(clip) = node
            .get("clip")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|clip| !clip.is_empty())
        {
            clips.push(clip.to_string());
        }
        if let Some(list) = node.get("clips").and_then(Value::as_array) {
            for clip in list.iter().filter_map(Value::as_str) {
                if !clip.trim().is_empty() && !clips.iter().any(|known| known == clip) {
                    clips.push(clip.trim().to_string());
                }
            }
        }

        let mut inputs = Vec::new();
        if let Some(input) = node.get("input") {
            if let Some(input) = parse_input(input) {
                inputs.push(input);
            }
        }
        if let Some(list) = node.get("inputs").and_then(Value::as_array) {
            for input in list.iter().filter_map(parse_input) {
                inputs.push(input);
            }
        }

        let parameter = node
            .get("parameter")
            .or_else(|| node.get("parameter_x"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        nodes.push(AnimNodeView {
            name,
            kind: node
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            clips,
            inputs,
            state_machine: node
                .get("state_machine")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            parameter,
        });
    }

    let output = nodes
        .iter()
        .find(|node| node.is_output())
        .map(|node| node.name.clone());
    Ok(AnimGraphView {
        parameters,
        nodes,
        output,
    })
}

fn parse_input(value: &Value) -> Option<AnimInputView> {
    match value {
        Value::String(node) if !node.trim().is_empty() => Some(AnimInputView {
            node: node.trim().to_string(),
            weight: 1.0,
            bone_mask: Vec::new(),
        }),
        Value::Object(map) => {
            let node = map
                .get("node")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|node| !node.is_empty())?;
            Some(AnimInputView {
                node: node.to_string(),
                weight: map
                    .get("weight")
                    .and_then(Value::as_f64)
                    .map(|weight| weight as f32)
                    .unwrap_or(1.0),
                bone_mask: map
                    .get("bone_mask")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|joint| !joint.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        }
        _ => None,
    }
}

/// 解析 `*.statemachine.json`。
pub fn parse_state_machine_view(document: &Value) -> Result<AnimStateMachineView, String> {
    let states_json = document
        .get("states")
        .and_then(Value::as_array)
        .ok_or_else(|| "state machine needs a `states` array".to_string())?;
    if states_json.is_empty() {
        return Err("state machine needs at least one state".to_string());
    }

    let mut states = Vec::with_capacity(states_json.len());
    for (index, state) in states_json.iter().enumerate() {
        let name = state
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("state #{index} needs a non-empty name"))?;
        states.push(AnimStateView {
            name: name.to_string(),
            clip: state
                .get("clip")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            speed: state.get("speed").and_then(Value::as_f64).unwrap_or(1.0) as f32,
            looped: state.get("looped").and_then(Value::as_bool).unwrap_or(true),
        });
    }

    let mut transitions = Vec::new();
    for (index, transition) in document
        .get("transitions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let to = transition
            .get("to")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|to| !to.is_empty())
            .ok_or_else(|| format!("transition #{index} needs a `to` state"))?
            .to_string();
        let from = transition
            .get("from")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|from| !from.is_empty() && *from != "*" && !from.eq_ignore_ascii_case("any"))
            .map(str::to_string);
        let conditions = transition
            .get("conditions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(parse_condition)
            .collect();
        transitions.push(AnimTransitionView {
            from,
            to,
            duration: transition
                .get("duration")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as f32,
            min_state_fraction: transition
                .get("min_state_fraction")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as f32,
            conditions,
        });
    }

    Ok(AnimStateMachineView {
        states,
        transitions,
    })
}

fn parse_condition(condition: &Value) -> Option<AnimConditionView> {
    let kind = condition.get("type").and_then(Value::as_str)?;
    let name = condition.get("name").and_then(Value::as_str)?;
    let threshold = condition
        .get("threshold")
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let value = condition
        .get("value")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = match kind {
        "float_greater" => format!("{name} > {threshold}"),
        "float_less" => format!("{name} < {threshold}"),
        "bool_is" => format!("{name} == {value}"),
        "trigger" => format!("{name} (trigger)"),
        other => format!("{name} ({other})"),
    };
    Some(AnimConditionView {
        kind: kind.to_string(),
        name: name.to_string(),
        threshold,
        value,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_graph_view_lists_parameters_links_and_the_output() {
        let view = parse_anim_graph_view(&json!({
            "parameters": [{ "name": "speed", "type": "float", "default": 1.5 }],
            "nodes": [
                { "name": "locomotion", "type": "blend_space_1d",
                  "clips": ["idle.ranim", "run.ranim"], "parameter": "speed" },
                { "name": "aim", "type": "clip_player", "clip": "aim.ranim" },
                { "name": "stack", "type": "blend_stack",
                  "inputs": ["locomotion", { "node": "aim", "weight": 0.25 }] },
                { "name": "out", "type": "output", "input": "stack" }
            ]
        }))
        .expect("valid graph");
        assert_eq!(view.parameters[0].name, "speed");
        assert_eq!(view.parameters[0].default, 1.5);
        assert_eq!(view.nodes.len(), 4);
        assert_eq!(view.nodes[0].clips, ["idle.ranim", "run.ranim"]);
        assert_eq!(view.nodes[2].inputs[1].node, "aim");
        assert_eq!(view.nodes[2].inputs[1].weight, 0.25);
        assert_eq!(view.output.as_deref(), Some("out"));
    }

    #[test]
    fn a_graph_view_surfaces_missing_nodes_and_bad_names() {
        assert!(parse_anim_graph_view(&json!({})).is_err());
        assert!(parse_anim_graph_view(&json!({ "nodes": [] })).is_err());
        assert!(parse_anim_graph_view(&json!({ "nodes": [{ "type": "output" }] })).is_err());
    }

    #[test]
    fn a_state_machine_view_lists_states_and_readable_conditions() {
        let view = parse_state_machine_view(&json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim", "speed": 1.2, "looped": false }
            ],
            "transitions": [
                { "from": "Idle", "to": "Run", "duration": 0.2,
                  "conditions": [
                    { "type": "float_greater", "name": "speed", "threshold": 1.5 },
                    { "type": "trigger", "name": "jump" }
                  ] },
                { "from": "*", "to": "Idle", "duration": 0.0 }
            ]
        }))
        .expect("valid state machine");
        assert_eq!(view.states[1].speed, 1.2);
        assert!(!view.states[1].looped);
        assert_eq!(view.transitions[0].from.as_deref(), Some("Idle"));
        assert_eq!(view.transitions[0].conditions[0].summary, "speed > 1.5");
        assert_eq!(view.transitions[0].conditions[0].kind, "float_greater");
        assert_eq!(view.transitions[0].conditions[0].name, "speed");
        assert_eq!(view.transitions[0].conditions[0].threshold, 1.5);
        assert_eq!(view.transitions[0].conditions[1].summary, "jump (trigger)");
        assert_eq!(view.transitions[0].conditions[1].kind, "trigger");
        assert!(view.transitions[1].from.is_none());
    }
}

/// 动画图节点的画布位置（`<graph>.layout.json`）。
///
/// 与图本身分开存：改布局不会改动画数据，也不会让 cook 缓存失效。坐标是面板内的
/// 逻辑像素，原点在画布左上角。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnimGraphLayout {
    pub positions: BTreeMap<String, [f32; 2]>,
}

impl AnimGraphLayout {
    /// 布局文件路径：紧挨着图资产，例如 `graphs/hero.animgraph.json.layout.json`。
    pub fn path_for(graph_path: &str) -> String {
        format!("{graph_path}.layout.json")
    }

    pub fn position(&self, node: &str) -> Option<[f32; 2]> {
        self.positions.get(node).copied()
    }

    pub fn set_position(&mut self, node: impl Into<String>, position: [f32; 2]) {
        if position[0].is_finite() && position[1].is_finite() {
            self.positions.insert(node.into(), position);
        }
    }

    pub fn to_json(&self) -> Value {
        let nodes: serde_json::Map<String, Value> = self
            .positions
            .iter()
            .map(|(name, position)| (name.clone(), json!(position)))
            .collect();
        json!({ "version": 1, "nodes": nodes })
    }

    /// 解析布局；缺文件 / 空对象得到空布局，坏 JSON 返回错误。
    pub fn from_json(document: &Value) -> Result<Self, String> {
        let Some(nodes) = document.get("nodes") else {
            return Ok(Self::default());
        };
        let Some(nodes) = nodes.as_object() else {
            return Err("animation graph layout needs a `nodes` object".to_string());
        };
        let mut positions = BTreeMap::new();
        for (name, position) in nodes {
            let Some(values) = position.as_array() else {
                continue;
            };
            let (Some(x), Some(y)) = (
                values.first().and_then(Value::as_f64),
                values.get(1).and_then(Value::as_f64),
            ) else {
                continue;
            };
            if x.is_finite() && y.is_finite() {
                positions.insert(name.clone(), [x as f32, y as f32]);
            }
        }
        Ok(Self { positions })
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn a_layout_round_trips_node_positions() {
        let mut layout = AnimGraphLayout::default();
        layout.set_position("locomotion", [40.0, 80.0]);
        layout.set_position("out", [320.5, 12.0]);
        let json = layout.to_json();
        let parsed = AnimGraphLayout::from_json(&json).expect("parses");
        assert_eq!(parsed.position("locomotion"), Some([40.0, 80.0]));
        assert_eq!(parsed.position("out"), Some([320.5, 12.0]));
    }

    #[test]
    fn a_missing_or_malformed_layout_falls_back_to_empty() {
        assert!(AnimGraphLayout::from_json(&json!({}))
            .unwrap()
            .positions
            .is_empty());
        assert!(AnimGraphLayout::from_json(&json!({ "nodes": [] })).is_err());
        let layout = AnimGraphLayout::from_json(&json!({
            "nodes": { "a": [1.0, 2.0], "b": "bad", "c": [3.0] }
        }))
        .expect("tolerates bad entries");
        assert_eq!(layout.positions.len(), 1);
        assert_eq!(layout.position("a"), Some([1.0, 2.0]));
    }

    #[test]
    fn the_layout_path_sits_next_to_the_graph() {
        assert_eq!(
            AnimGraphLayout::path_for("graphs/hero.animgraph.json"),
            "graphs/hero.animgraph.json.layout.json"
        );
    }
}

fn find_node_mut<'a>(document: &'a mut Value, name: &str) -> Option<&'a mut Value> {
    document
        .get_mut("nodes")?
        .as_array_mut()?
        .iter_mut()
        .find(|node| node.get("name").and_then(Value::as_str) == Some(name))
}

fn input_name(value: &Value) -> Option<&str> {
    match value {
        Value::String(name) => Some(name),
        Value::Object(map) => map.get("node").and_then(Value::as_str),
        _ => None,
    }
}

/// 给图节点追加一条连线（写回作者的 `inputs` 数组）。
///
/// 链式节点（用单数 `input` 的 `slot` / `cached_pose` / `output`）不混用数组：
/// 对它们返回错误，避免生成 cooker 不认识的形状。
pub fn add_graph_input(document: &mut Value, target: &str, source: &str) -> Result<(), String> {
    if target == source {
        return Err("a node cannot connect to itself".to_string());
    }
    let names: Vec<String> = document
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|node| node.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if !names.iter().any(|name| name == source) {
        return Err(format!("unknown source node '{source}'"));
    }
    let Some(node) = find_node_mut(document, target) else {
        return Err(format!("unknown target node '{target}'"));
    };
    if node.get("input").is_some() {
        return Err(format!(
            "node '{target}' uses a single input; edit it as JSON"
        ));
    }
    if node.get("inputs").is_none() {
        node["inputs"] = json!([]);
    }
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_array_mut) else {
        return Err(format!("node '{target}' has an invalid `inputs` field"));
    };
    if inputs
        .iter()
        .filter_map(input_name)
        .any(|name| name == source)
    {
        return Err(format!("node '{target}' already connects to '{source}'"));
    }
    inputs.push(Value::String(source.to_string()));
    Ok(())
}

/// 设置一条连线的权重（0..=1）。
///
/// 作者写字符串 `"aim"` 时自动升级成 `{ "node": "aim", "weight": w }`；已经是对象
/// 的连线只改 `weight`，保留 `bone_mask` 等其它字段。
pub fn set_graph_input_weight(
    document: &mut Value,
    target: &str,
    index: usize,
    weight: f32,
) -> Result<(), String> {
    if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return Err("link weight must be finite and between 0 and 1".to_string());
    }
    let Some(node) = find_node_mut(document, target) else {
        return Err(format!("unknown target node '{target}'"));
    };
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_array_mut) else {
        return Err(format!("node '{target}' has no editable `inputs` array"));
    };
    let Some(input) = inputs.get_mut(index) else {
        return Err(format!("node '{target}' has no input #{index}"));
    };
    match input {
        Value::String(name) => {
            let name = name.clone();
            *input = json!({ "node": name, "weight": weight });
            Ok(())
        }
        Value::Object(map) => {
            map.insert("weight".to_string(), json!(weight));
            Ok(())
        }
        _ => Err(format!("node '{target}' input #{index} is malformed")),
    }
}

/// 删除图节点 `inputs` 数组里的第 `index` 条连线。
pub fn remove_graph_input(document: &mut Value, target: &str, index: usize) -> Result<(), String> {
    let Some(node) = find_node_mut(document, target) else {
        return Err(format!("unknown target node '{target}'"));
    };
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_array_mut) else {
        return Err(format!("node '{target}' has no editable `inputs` array"));
    };
    if index >= inputs.len() {
        return Err(format!("node '{target}' has no input #{index}"));
    }
    inputs.remove(index);
    Ok(())
}

/// 设置一条连线的骨骼掩码；空列表表示恢复「整条姿态」。
///
/// 字符串连线在需要掩码时升级成对象（权重保持 1），已有对象只替换 `bone_mask`，
/// 保留 `weight` 等字段。关节名去空白、去重、保序；上限与 cooker 一致（1024）。
pub fn set_graph_input_bone_mask(
    document: &mut Value,
    target: &str,
    index: usize,
    joints: &[String],
) -> Result<(), String> {
    const MAX_MASK: usize = 1024;
    let mut mask: Vec<&str> = Vec::new();
    for joint in joints {
        let joint = joint.trim();
        if !joint.is_empty() && !mask.contains(&joint) {
            mask.push(joint);
        }
    }
    if mask.len() > MAX_MASK {
        return Err(format!("a bone mask can hold at most {MAX_MASK} joints"));
    }
    let Some(node) = find_node_mut(document, target) else {
        return Err(format!("unknown target node '{target}'"));
    };
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_array_mut) else {
        return Err(format!("node '{target}' has no editable `inputs` array"));
    };
    let Some(input) = inputs.get_mut(index) else {
        return Err(format!("node '{target}' has no input #{index}"));
    };
    match input {
        Value::String(name) => {
            if mask.is_empty() {
                return Ok(());
            }
            let name = name.clone();
            *input = json!({ "node": name, "weight": 1.0, "bone_mask": mask });
            Ok(())
        }
        Value::Object(map) => {
            if mask.is_empty() {
                map.remove("bone_mask");
            } else {
                map.insert("bone_mask".to_string(), json!(mask));
            }
            Ok(())
        }
        _ => Err(format!("node '{target}' input #{index} is malformed")),
    }
}

/// 追加一个剪辑节点（`clip_player`，单剪辑）。
///
/// 只创建**当帧就能通过 cooker 校验**的形状：剪辑节点的必需字段只有剪辑路径，
/// 连线可以后补。`blend_space_*` / `state_machine` / `blend_stack` 需要剪辑、
/// 坐标、机器路径或至少一条输入才算合法，凭空创建会写出 cook 不了的文档，
/// 所以这些类型仍然手写 JSON。
pub fn add_clip_node(document: &mut Value, name: &str, clip: &str) -> Result<(), String> {
    let name = name.trim();
    let clip = clip.trim();
    if name.is_empty() {
        return Err("a node needs a non-empty name".to_string());
    }
    if clip.is_empty() {
        return Err("a clip node needs a clip path".to_string());
    }
    let Some(nodes) = document.get_mut("nodes").and_then(Value::as_array_mut) else {
        return Err("animation graph needs a `nodes` array".to_string());
    };
    if nodes
        .iter()
        .any(|node| node.get("name").and_then(Value::as_str) == Some(name))
    {
        return Err(format!("a node named '{name}' already exists"));
    }
    nodes.push(json!({ "name": name, "type": "clip_player", "clip": clip }));
    Ok(())
}

/// 删除一个节点；输出节点、最后一个节点和被引用的节点不能直接删。
///
/// 被引用时返回错误并点名引用者，而不是顺手断开连线：先由作者决定那条线接
/// 到哪里，面板再写回，半截图不会落盘。
pub fn remove_graph_node(document: &mut Value, name: &str) -> Result<(), String> {
    let nodes = document
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "animation graph needs a `nodes` array".to_string())?;
    let index = nodes
        .iter()
        .position(|node| node.get("name").and_then(Value::as_str) == Some(name))
        .ok_or_else(|| format!("unknown node '{name}'"))?;
    if nodes.len() <= 1 {
        return Err("an animation graph needs at least one node".to_string());
    }
    if nodes[index]
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("output"))
    {
        return Err(format!(
            "node '{name}' is the graph output; rewire it instead"
        ));
    }
    for node in nodes {
        let target = node.get("name").and_then(Value::as_str).unwrap_or("?");
        let referenced = node
            .get("inputs")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(input_name)
            .any(|source| source == name)
            || node
                .get("input")
                .and_then(Value::as_str)
                .is_some_and(|source| source == name);
        if referenced {
            return Err(format!(
                "node '{target}' still connects to '{name}'; remove that link first"
            ));
        }
    }
    let Some(nodes) = document.get_mut("nodes").and_then(Value::as_array_mut) else {
        return Err("animation graph needs a `nodes` array".to_string());
    };
    nodes.remove(index);
    Ok(())
}

#[cfg(test)]
mod node_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim" },
                { "name": "b", "type": "clip_player", "clip": "b.ranim" },
                { "name": "stack", "type": "blend_stack", "inputs": ["a"] },
                { "name": "out", "type": "output", "input": "stack" }
            ]
        })
    }

    #[test]
    fn a_clip_node_can_be_added_and_shows_up_in_the_view() {
        let mut doc = document();
        add_clip_node(&mut doc, "  aim  ", " aim.ranim ").expect("adds");
        assert_eq!(doc["nodes"][4]["name"], "aim");
        assert_eq!(doc["nodes"][4]["type"], "clip_player");
        assert_eq!(doc["nodes"][4]["clip"], "aim.ranim");
        let view = parse_anim_graph_view(&doc).expect("still parses");
        assert_eq!(view.nodes.len(), 5);
    }

    #[test]
    fn duplicate_or_blank_clip_nodes_are_rejected() {
        let mut doc = document();
        assert!(add_clip_node(&mut doc, "a", "a.ranim").is_err());
        assert!(add_clip_node(&mut doc, "  ", "a.ranim").is_err());
        assert!(add_clip_node(&mut doc, "aim", "  ").is_err());
        assert!(add_clip_node(&mut json!({}), "aim", "a.ranim").is_err());
    }

    #[test]
    fn an_unreferenced_node_can_be_removed() {
        let mut doc = document();
        remove_graph_node(&mut doc, "b").expect("removes");
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 3);
        assert!(doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node["name"] != "b"));
    }

    #[test]
    fn the_output_referenced_and_missing_nodes_are_protected() {
        let mut doc = document();
        assert!(remove_graph_node(&mut doc, "out").is_err());
        assert!(remove_graph_node(&mut doc, "a").is_err());
        assert!(remove_graph_node(&mut doc, "stack").is_err());
        assert!(remove_graph_node(&mut doc, "missing").is_err());
        let mut single = json!({ "nodes": [{ "name": "a", "type": "clip_player" }] });
        assert!(remove_graph_node(&mut single, "a").is_err());
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    fn document() -> Value {
        json!({
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim" },
                { "name": "b", "type": "clip_player", "clip": "b.ranim" },
                { "name": "stack", "type": "blend_stack", "inputs": ["a"] },
                { "name": "out", "type": "output", "input": "stack" }
            ]
        })
    }

    #[test]
    fn links_can_be_added_and_removed() {
        let mut doc = document();
        add_graph_input(&mut doc, "stack", "b").expect("adds");
        assert_eq!(doc["nodes"][2]["inputs"], json!(["a", "b"]));
        remove_graph_input(&mut doc, "stack", 0).expect("removes");
        assert_eq!(doc["nodes"][2]["inputs"], json!(["b"]));
    }

    #[test]
    fn self_unknown_duplicate_and_chain_nodes_are_rejected() {
        let mut doc = document();
        assert!(add_graph_input(&mut doc, "stack", "stack").is_err());
        assert!(add_graph_input(&mut doc, "stack", "missing").is_err());
        assert!(add_graph_input(&mut doc, "stack", "a").is_err());
        assert!(add_graph_input(&mut doc, "out", "a").is_err());
        assert!(remove_graph_input(&mut doc, "stack", 9).is_err());
    }
}

/// 给状态机追加一条转移（写回作者的 `transitions` 数组）。
///
/// `from = None` 表示任意状态。状态名必须存在，不能自连，同一 `(from, to)`
/// 不能重复；新转移默认 `duration = 0`、无条件的硬切。
pub fn add_state_transition(
    document: &mut Value,
    from: Option<&str>,
    to: &str,
) -> Result<(), String> {
    let states: Vec<String> = document
        .get("states")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|state| state.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if !states.iter().any(|name| name == to) {
        return Err(format!("unknown target state '{to}'"));
    }
    if let Some(from) = from {
        if !states.iter().any(|name| name == from) {
            return Err(format!("unknown source state '{from}'"));
        }
        if from == to {
            return Err("a transition cannot start and end on the same state".to_string());
        }
    }
    if document.get("transitions").is_none() {
        document["transitions"] = json!([]);
    }
    let Some(transitions) = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
    else {
        return Err("state machine has an invalid `transitions` field".to_string());
    };
    if transitions.iter().any(|transition| {
        let existing_from = transition
            .get("from")
            .and_then(Value::as_str)
            .filter(|from| *from != "*" && !from.eq_ignore_ascii_case("any"));
        let existing_to = transition.get("to").and_then(Value::as_str);
        existing_to == Some(to) && existing_from == from
    }) {
        return Err(format!(
            "a transition from '{}' to '{to}' already exists",
            from.unwrap_or("any")
        ));
    }
    transitions.push(json!({ "from": from.unwrap_or("*"), "to": to, "duration": 0.0 }));
    Ok(())
}

/// 删除状态机 `transitions` 数组里的第 `index` 条转移。
pub fn remove_state_transition(document: &mut Value, index: usize) -> Result<(), String> {
    let Some(transitions) = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
    else {
        return Err("state machine has no editable `transitions` array".to_string());
    };
    if index >= transitions.len() {
        return Err(format!("state machine has no transition #{index}"));
    }
    transitions.remove(index);
    Ok(())
}

fn find_transition_mut(document: &mut Value, index: usize) -> Result<&mut Value, String> {
    let transition = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "state machine has no editable `transitions` array".to_string())?
        .get_mut(index)
        .ok_or_else(|| format!("state machine has no transition #{index}"))?;
    if !transition.is_object() {
        return Err(format!("transition #{index} is malformed"));
    }
    Ok(transition)
}

fn find_condition_mut<'a>(
    document: &'a mut Value,
    index: usize,
    condition: usize,
) -> Result<&'a mut Value, String> {
    let transition = find_transition_mut(document, index)?;
    let entry = transition
        .get_mut("conditions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("transition #{index} has no editable `conditions` array"))?
        .get_mut(condition)
        .ok_or_else(|| format!("transition #{index} has no condition #{condition}"))?;
    if !entry.is_object() {
        return Err(format!(
            "transition #{index} condition #{condition} is malformed"
        ));
    }
    Ok(entry)
}

/// 设置转移的混合时长与最小状态占比。
///
/// 两个值都与运行时的含义一致：`duration` 是淡入秒数（0 = 硬切），
/// `min_state_fraction` 是允许离开源状态前至少播过的比例（0..=1）。
pub fn set_transition_timing(
    document: &mut Value,
    index: usize,
    duration: f32,
    min_state_fraction: f32,
) -> Result<(), String> {
    if !duration.is_finite() || duration < 0.0 {
        return Err("transition duration must be finite and >= 0".to_string());
    }
    if !min_state_fraction.is_finite() || !(0.0..=1.0).contains(&min_state_fraction) {
        return Err("min_state_fraction must be finite and between 0 and 1".to_string());
    }
    let transition = find_transition_mut(document, index)?;
    transition["duration"] = json!(duration);
    transition["min_state_fraction"] = json!(min_state_fraction);
    Ok(())
}

/// 给第 `index` 条转移追加一条条件。
///
/// `kind` 只接受运行时认识的四种；`threshold` 只用于 float 条件，`value` 只用于
/// `bool_is`。同一个 `(type, name)` 不重复添加 —— 两个相同条件只是白写一遍 AND。
pub fn add_state_condition(
    document: &mut Value,
    index: usize,
    kind: &str,
    name: &str,
    threshold: f32,
    value: bool,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a condition needs a non-empty parameter name".to_string());
    }
    let kind = match kind.trim() {
        kind @ ("float_greater" | "float_less" | "bool_is" | "trigger") => kind,
        other => {
            return Err(format!(
                "unknown condition type '{other}' \
                 (expected float_greater / float_less / bool_is / trigger)"
            ))
        }
    };
    if !threshold.is_finite() {
        return Err("a condition threshold must be finite".to_string());
    }
    let transition = find_transition_mut(document, index)?;
    if transition.get("conditions").is_none() {
        transition["conditions"] = json!([]);
    }
    let Some(conditions) = transition
        .get_mut("conditions")
        .and_then(Value::as_array_mut)
    else {
        return Err(format!(
            "transition #{index} has an invalid `conditions` field"
        ));
    };
    if conditions.iter().any(|condition| {
        condition.get("type").and_then(Value::as_str) == Some(kind)
            && condition.get("name").and_then(Value::as_str) == Some(name)
    }) {
        return Err(format!(
            "transition #{index} already has the condition '{name} ({kind})'"
        ));
    }
    let condition = match kind {
        "float_greater" | "float_less" => {
            json!({ "type": kind, "name": name, "threshold": threshold })
        }
        "bool_is" => json!({ "type": kind, "name": name, "value": value }),
        _ => json!({ "type": kind, "name": name }),
    };
    conditions.push(condition);
    Ok(())
}

/// 删除第 `index` 条转移里的第 `condition` 条条件。
pub fn remove_state_condition(
    document: &mut Value,
    index: usize,
    condition: usize,
) -> Result<(), String> {
    let transition = find_transition_mut(document, index)?;
    let Some(conditions) = transition
        .get_mut("conditions")
        .and_then(Value::as_array_mut)
    else {
        return Err(format!(
            "transition #{index} has no editable `conditions` array"
        ));
    };
    if condition >= conditions.len() {
        return Err(format!("transition #{index} has no condition #{condition}"));
    }
    conditions.remove(condition);
    Ok(())
}

/// 改一条 float 条件的阈值；`bool_is` / `trigger` 没有阈值，返回错误。
pub fn set_condition_threshold(
    document: &mut Value,
    index: usize,
    condition: usize,
    threshold: f32,
) -> Result<(), String> {
    if !threshold.is_finite() {
        return Err("a condition threshold must be finite".to_string());
    }
    let entry = find_condition_mut(document, index, condition)?;
    if !matches!(
        entry.get("type").and_then(Value::as_str),
        Some("float_greater" | "float_less")
    ) {
        return Err(format!(
            "transition #{index} condition #{condition} has no threshold"
        ));
    }
    entry["threshold"] = json!(threshold);
    Ok(())
}

/// 改一条 `bool_is` 条件的期望值；其它条件类型返回错误。
pub fn set_condition_value(
    document: &mut Value,
    index: usize,
    condition: usize,
    value: bool,
) -> Result<(), String> {
    let entry = find_condition_mut(document, index, condition)?;
    if entry.get("type").and_then(Value::as_str) != Some("bool_is") {
        return Err(format!(
            "transition #{index} condition #{condition} is not a bool condition"
        ));
    }
    entry["value"] = json!(value);
    Ok(())
}

#[cfg(test)]
mod condition_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim" }
            ],
            "transitions": [
                { "from": "Idle", "to": "Run", "duration": 0.2,
                  "conditions": [{ "type": "float_greater", "name": "speed", "threshold": 1.5 }] }
            ]
        })
    }

    #[test]
    fn transition_timing_can_be_edited_and_is_validated() {
        let mut doc = document();
        set_transition_timing(&mut doc, 0, 0.35, 0.5).expect("sets");
        assert_eq!(
            doc["transitions"][0]["duration"].as_f64(),
            Some(0.35f32 as f64)
        );
        assert_eq!(
            doc["transitions"][0]["min_state_fraction"].as_f64(),
            Some(0.5)
        );
        assert!(set_transition_timing(&mut doc, 0, -1.0, 0.0).is_err());
        assert!(set_transition_timing(&mut doc, 0, f32::NAN, 0.0).is_err());
        assert!(set_transition_timing(&mut doc, 0, 0.1, 1.5).is_err());
        assert!(set_transition_timing(&mut doc, 9, 0.1, 0.0).is_err());
        assert!(
            set_transition_timing(&mut json!({ "transitions": ["bad"] }), 0, 0.1, 0.0).is_err()
        );
    }

    #[test]
    fn conditions_can_be_added_and_removed() {
        let mut doc = document();
        add_state_condition(&mut doc, 0, "float_less", "speed", 0.25, false).expect("adds float");
        add_state_condition(&mut doc, 0, "bool_is", "grounded", 0.0, false).expect("adds bool");
        add_state_condition(&mut doc, 0, "trigger", "jump", 0.0, false).expect("adds trigger");
        assert_eq!(
            doc["transitions"][0]["conditions"][1],
            json!({ "type": "float_less", "name": "speed", "threshold": 0.25 })
        );
        assert_eq!(
            doc["transitions"][0]["conditions"][2],
            json!({ "type": "bool_is", "name": "grounded", "value": false })
        );
        assert_eq!(
            doc["transitions"][0]["conditions"][3],
            json!({ "type": "trigger", "name": "jump" })
        );
        remove_state_condition(&mut doc, 0, 0).expect("removes");
        assert_eq!(doc["transitions"][0]["conditions"][0]["name"], "speed");
        assert_eq!(doc["transitions"][0]["conditions"][0]["type"], "float_less");
    }

    #[test]
    fn adding_conditions_creates_the_array_and_rejects_bad_input() {
        let mut doc = json!({
            "states": [{ "name": "Idle", "clip": "idle.ranim" }],
            "transitions": [{ "from": "*", "to": "Idle" }]
        });
        add_state_condition(&mut doc, 0, "trigger", "jump", 0.0, false).expect("adds");
        assert_eq!(
            doc["transitions"][0]["conditions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let mut doc = document();
        assert!(add_state_condition(&mut doc, 0, "nope", "speed", 0.0, false).is_err());
        assert!(add_state_condition(&mut doc, 0, "trigger", "  ", 0.0, false).is_err());
        assert!(add_state_condition(&mut doc, 0, "float_greater", "speed", 2.0, false).is_err());
        assert!(add_state_condition(&mut doc, 0, "float_less", "speed", f32::NAN, false).is_err());
        assert!(add_state_condition(&mut doc, 9, "trigger", "jump", 0.0, false).is_err());
        assert!(remove_state_condition(&mut doc, 0, 9).is_err());
    }

    #[test]
    fn thresholds_and_bool_values_can_be_edited() {
        let mut doc = document();
        set_condition_threshold(&mut doc, 0, 0, 2.5).expect("sets threshold");
        assert_eq!(doc["transitions"][0]["conditions"][0]["threshold"], 2.5);
        assert!(set_condition_threshold(&mut doc, 0, 0, f32::NAN).is_err());

        add_state_condition(&mut doc, 0, "bool_is", "grounded", 0.0, false).expect("adds bool");
        set_condition_value(&mut doc, 0, 1, true).expect("sets bool");
        assert_eq!(doc["transitions"][0]["conditions"][1]["value"], true);

        assert!(set_condition_value(&mut doc, 0, 0, true).is_err());
        assert!(set_condition_threshold(&mut doc, 0, 1, 0.5).is_err());
        assert!(set_condition_threshold(&mut doc, 0, 9, 0.5).is_err());
        assert!(set_condition_value(&mut doc, 9, 0, true).is_err());
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn document() -> Value {
        json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim" },
                { "name": "Jump", "clip": "jump.ranim" }
            ],
            "transitions": [
                { "from": "Idle", "to": "Run", "duration": 0.2 }
            ]
        })
    }

    #[test]
    fn transitions_can_be_added_from_a_state_or_any_state() {
        let mut doc = document();
        add_state_transition(&mut doc, Some("Run"), "Jump").expect("adds");
        add_state_transition(&mut doc, None, "Idle").expect("adds any");
        assert_eq!(doc["transitions"][1]["from"], "Run");
        assert_eq!(doc["transitions"][2]["from"], "*");
    }

    #[test]
    fn transitions_can_be_removed() {
        let mut doc = document();
        add_state_transition(&mut doc, Some("Run"), "Jump").expect("adds");
        remove_state_transition(&mut doc, 0).expect("removes");
        assert_eq!(doc["transitions"].as_array().unwrap().len(), 1);
        assert_eq!(doc["transitions"][0]["to"], "Jump");
    }

    #[test]
    fn bad_transition_names_duplicates_and_self_links_are_rejected() {
        let mut doc = document();
        assert!(add_state_transition(&mut doc, Some("Idle"), "Missing").is_err());
        assert!(add_state_transition(&mut doc, Some("Missing"), "Run").is_err());
        assert!(add_state_transition(&mut doc, Some("Idle"), "Idle").is_err());
        assert!(add_state_transition(&mut doc, Some("Idle"), "Run").is_err());
        assert!(remove_state_transition(&mut doc, 9).is_err());
    }
}

#[cfg(test)]
mod weight_tests {
    use super::*;

    fn document() -> Value {
        json!({
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim" },
                { "name": "b", "type": "clip_player", "clip": "b.ranim" },
                { "name": "stack", "type": "blend_stack",
                  "inputs": ["a", { "node": "b", "weight": 0.5, "bone_mask": ["spine"] }] }
            ]
        })
    }

    #[test]
    fn a_string_link_is_upgraded_to_an_object_with_the_new_weight() {
        let mut doc = document();
        set_graph_input_weight(&mut doc, "stack", 0, 0.25).expect("sets");
        assert_eq!(
            doc["nodes"][2]["inputs"][0],
            json!({ "node": "a", "weight": 0.25 })
        );
    }

    #[test]
    fn an_object_link_keeps_its_other_fields() {
        let mut doc = document();
        set_graph_input_weight(&mut doc, "stack", 1, 0.75).expect("sets");
        assert_eq!(doc["nodes"][2]["inputs"][1]["weight"], 0.75);
        assert_eq!(doc["nodes"][2]["inputs"][1]["bone_mask"], json!(["spine"]));
    }

    #[test]
    fn invalid_weights_and_indices_are_rejected() {
        let mut doc = document();
        assert!(set_graph_input_weight(&mut doc, "stack", 0, 2.0).is_err());
        assert!(set_graph_input_weight(&mut doc, "stack", 0, f32::NAN).is_err());
        assert!(set_graph_input_weight(&mut doc, "stack", 9, 0.5).is_err());
        assert!(set_graph_input_weight(&mut doc, "missing", 0, 0.5).is_err());
    }

    #[test]
    fn a_string_link_gains_a_mask_and_upgrades_to_an_object() {
        let mut doc = document();
        set_graph_input_bone_mask(&mut doc, "stack", 0, &[" spine ".to_string()]).expect("sets");
        assert_eq!(
            doc["nodes"][2]["inputs"][0],
            json!({ "node": "a", "weight": 1.0, "bone_mask": ["spine"] })
        );
    }

    #[test]
    fn an_object_link_keeps_its_weight_and_the_mask_is_deduplicated() {
        let mut doc = document();
        let mask = vec![
            "spine".to_string(),
            " arm ".to_string(),
            "spine".to_string(),
            String::new(),
        ];
        set_graph_input_bone_mask(&mut doc, "stack", 1, &mask).expect("sets");
        assert_eq!(doc["nodes"][2]["inputs"][1]["weight"], 0.5);
        assert_eq!(
            doc["nodes"][2]["inputs"][1]["bone_mask"],
            json!(["spine", "arm"])
        );
    }

    #[test]
    fn an_empty_mask_clears_the_field() {
        let mut doc = document();
        set_graph_input_bone_mask(&mut doc, "stack", 1, &[]).expect("clears");
        assert!(doc["nodes"][2]["inputs"][1].get("bone_mask").is_none());
        assert_eq!(doc["nodes"][2]["inputs"][1]["weight"], 0.5);
        // 字符串连线本来就没有掩码，清空是无操作。
        set_graph_input_bone_mask(&mut doc, "stack", 0, &[]).expect("no-op");
        assert_eq!(doc["nodes"][2]["inputs"][0], json!("a"));
    }

    #[test]
    fn masks_are_read_back_into_the_view() {
        let doc = json!({
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim" },
                { "name": "stack", "type": "layered_blend",
                  "inputs": [{ "node": "a", "weight": 0.5, "bone_mask": ["spine", "arm"] }] }
            ]
        });
        let view = parse_anim_graph_view(&doc).expect("parses");
        assert_eq!(view.nodes[1].inputs[0].bone_mask, ["spine", "arm"]);
    }

    #[test]
    fn bad_mask_targets_are_rejected() {
        let mut doc = document();
        assert!(set_graph_input_bone_mask(&mut doc, "missing", 0, &["spine".to_string()]).is_err());
        assert!(set_graph_input_bone_mask(&mut doc, "stack", 9, &["spine".to_string()]).is_err());
        assert!(set_graph_input_bone_mask(&mut doc, "out", 0, &["spine".to_string()]).is_err());
        assert!(set_graph_input_bone_mask(
            &mut json!({ "nodes": [{ "name": "a", "inputs": [42] }] }),
            "a",
            0,
            &["spine".to_string()]
        )
        .is_err());
    }
}
