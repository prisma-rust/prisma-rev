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
    /// 混合空间的样本坐标；一维节点的 y 恒为 0。
    pub positions: Vec<[f32; 2]>,
    pub inputs: Vec<AnimInputView>,
    pub state_machine: String,
    pub parameter: String,
    /// 二维混合空间的第二个参数名；其它节点为空。
    pub parameter_y: String,
    pub speed: f32,
    pub weight: f32,
    pub looped: bool,
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
        let parameter_y = node
            .get("parameter_y")
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
            positions: parse_positions(node),
            inputs,
            state_machine: node
                .get("state_machine")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            parameter,
            parameter_y,
            speed: node.get("speed").and_then(Value::as_f64).unwrap_or(1.0) as f32,
            weight: node.get("weight").and_then(Value::as_f64).unwrap_or(1.0) as f32,
            looped: node.get("looped").and_then(Value::as_bool).unwrap_or(true),
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

fn parse_positions(node: &Value) -> Vec<[f32; 2]> {
    node.get("positions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|position| match position {
            Value::Number(number) => [number.as_f64().unwrap_or(0.0) as f32, 0.0],
            Value::Array(pair) => [
                pair.first().and_then(Value::as_f64).unwrap_or(0.0) as f32,
                pair.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32,
            ],
            _ => [0.0, 0.0],
        })
        .collect()
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
    fn nodes_can_be_duplicated_with_unique_names() {
        let mut doc = json!({
            "parameters": [],
            "nodes": [
                { "name": "walk", "type": "clip_player", "clip": "walk.ranim", "speed": 1.5 },
                { "name": "out", "type": "output", "input": "walk" }
            ]
        });
        let copy = duplicate_graph_node(&mut doc, "walk").expect("copies");
        assert_eq!(copy, "walk_copy");
        assert_eq!(doc["nodes"][2]["name"], "walk_copy");
        assert_eq!(doc["nodes"][2]["clip"], "walk.ranim");
        assert_eq!(doc["nodes"][2]["speed"].as_f64(), Some(1.5));
        assert_eq!(doc["nodes"][1]["input"], "walk", "原引用不受影响");

        let second = duplicate_graph_node(&mut doc, "walk").expect("copies again");
        assert_eq!(second, "walk_copy2");
        let third = duplicate_graph_node(&mut doc, "walk_copy").expect("copies a copy");
        assert_eq!(third, "walk_copy_copy");
    }

    #[test]
    fn the_output_node_cannot_be_duplicated() {
        let mut doc = json!({
            "nodes": [
                { "name": "walk", "type": "clip_player", "clip": "walk.ranim" },
                { "name": "out", "type": "output", "input": "walk" }
            ]
        });
        assert!(duplicate_graph_node(&mut doc, "out").is_err());
        assert!(duplicate_graph_node(&mut doc, "missing").is_err());
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 2, "失败不改文档");
    }

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
    fn states_can_be_duplicated_with_unique_names() {
        let mut doc = json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim", "speed": 1.5, "looped": false }
            ],
            "transitions": [{ "from": "Idle", "to": "Run", "duration": 0.2 }]
        });
        let copy = duplicate_state(&mut doc, "Run").expect("copies");
        assert_eq!(copy, "Run_copy");
        assert_eq!(doc["states"][2]["name"], "Run_copy");
        assert_eq!(doc["states"][2]["clip"], "run.ranim");
        assert_eq!(doc["states"][2]["speed"].as_f64(), Some(1.5));
        assert_eq!(doc["states"][2]["looped"], false);
        assert_eq!(doc["transitions"][0]["to"], "Run", "转移表不变");
        assert_eq!(
            duplicate_state(&mut doc, "Run").expect("again"),
            "Run_copy2"
        );
        assert!(duplicate_state(&mut doc, "Missing").is_err());
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

/// 重命名图节点，并同步改写所有引用它的地方。
///
/// 会改：节点自身的 `name`、其它节点的 `inputs` / `input` 连线、顶层 `output`
/// （如果它指向这个节点）。图布局在 `<graph>.layout.json` 里，单独存，不跟着改名
/// —— 重命名后节点会回到画布的默认网格位置，重新拖动即可。
pub fn rename_graph_node(document: &mut Value, old: &str, new: &str) -> Result<(), String> {
    let new = new.trim();
    if new.is_empty() {
        return Err("a node needs a non-empty name".to_string());
    }
    if old == new {
        return Ok(());
    }
    let Some(old_name) = find_node_mut(document, old)
        .and_then(|node| node.get("name").and_then(Value::as_str))
        .map(str::to_string)
    else {
        return Err(format!("unknown node '{old}'"));
    };
    let names: Vec<String> = document
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|node| node.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if names.iter().any(|name| name == new) {
        return Err(format!("a node named '{new}' already exists"));
    }
    let Some(node) = find_node_mut(document, &old_name) else {
        return Err(format!("unknown node '{old_name}'"));
    };
    node["name"] = json!(new);
    if let Some(nodes) = document.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes.iter_mut() {
            if let Some(input) = node.get_mut("input") {
                if input.as_str() == Some(old_name.as_str()) {
                    *input = json!(new);
                }
            }
            let Some(inputs) = node.get_mut("inputs").and_then(Value::as_array_mut) else {
                continue;
            };
            for input in inputs.iter_mut() {
                match input {
                    Value::String(name) if name == &old_name => *name = new.to_string(),
                    Value::Object(map)
                        if map.get("node").and_then(Value::as_str) == Some(old_name.as_str()) =>
                    {
                        map.insert("node".to_string(), json!(new));
                    }
                    _ => {}
                }
            }
        }
    }
    if document.get("output").and_then(Value::as_str) == Some(old_name.as_str()) {
        document["output"] = json!(new);
    }
    Ok(())
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

/// 改一个剪辑节点（`clip_player`）的剪辑路径：写单数 `clip`，并删掉 `clips` 数组。
///
/// cooker 会把 `clip` 和 `clips` 拼在一起，两者同时存在就变成「多于一个剪辑」，
/// 剪辑节点会被拒绝，所以设置单个剪辑时必须清掉数组。
pub fn set_graph_node_clip(document: &mut Value, name: &str, clip: &str) -> Result<(), String> {
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a clip node needs a clip path".to_string());
    }
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    node["clip"] = json!(clip);
    if node.get("clips").is_some() {
        node.as_object_mut().expect("checked above").remove("clips");
    }
    Ok(())
}

/// 节点是几维混合空间：`Some(1)` / `Some(2)`，其它节点返回 `None`。
fn blend_space_dims(node: &Value) -> Option<usize> {
    match node
        .get("type")
        .and_then(Value::as_str)?
        .to_ascii_lowercase()
        .as_str()
    {
        "blend_space_1d" | "blendspace1d" | "blend_space" => Some(1),
        "blend_space_2d" | "blendspace2d" => Some(2),
        _ => None,
    }
}

fn normalize_parameter_kind(kind: &str) -> Result<&'static str, String> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "float" => Ok("float"),
        "bool" => Ok("bool"),
        "trigger" => Ok("trigger"),
        other => Err(format!(
            "unknown parameter type '{other}' (expected float / bool / trigger)"
        )),
    }
}

/// bool / trigger 的默认值只有 0 / 1 两种，其他数字按非零归一。
fn parameter_default_for(kind: &str, default: f32) -> f32 {
    if kind == "float" {
        default
    } else if default >= 0.5 {
        1.0
    } else {
        0.0
    }
}

/// 新加一个参数。
pub fn add_parameter(
    document: &mut Value,
    name: &str,
    kind: &str,
    default: f32,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a parameter needs a name".to_string());
    }
    if !default.is_finite() {
        return Err("a parameter default must be finite".to_string());
    }
    let kind = normalize_parameter_kind(kind)?;
    if !document.get("parameters").is_some_and(Value::is_array) {
        document["parameters"] = json!([]);
    }
    let parameters = document
        .get_mut("parameters")
        .and_then(Value::as_array_mut)
        .expect("created above");
    if parameters
        .iter()
        .any(|parameter| parameter.get("name").and_then(Value::as_str) == Some(name))
    {
        return Err(format!("a parameter named '{name}' already exists"));
    }
    parameters.push(json!({
        "name": name,
        "type": kind,
        "default": parameter_default_for(kind, default),
    }));
    Ok(())
}

/// 删除一个参数；被节点引用的参数不允许删（会静默改变混合空间的行为）。
pub fn remove_parameter(document: &mut Value, name: &str) -> Result<(), String> {
    if let Some(nodes) = document.get("nodes").and_then(Value::as_array) {
        for (index, node) in nodes.iter().enumerate() {
            for field in ["parameter", "parameter_x", "parameter_y"] {
                if node.get(field).and_then(Value::as_str) == Some(name) {
                    let node_name = node
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unnamed>");
                    return Err(format!(
                        "parameter '{name}' is used by node '{node_name}' (#{index}) as `{field}`"
                    ));
                }
            }
        }
    }
    let parameters = document
        .get_mut("parameters")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("parameter '{name}' does not exist"))?;
    let Some(index) = parameters
        .iter()
        .position(|parameter| parameter.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Err(format!("parameter '{name}' does not exist"));
    };
    parameters.remove(index);
    Ok(())
}

/// 改一个参数的默认值；bool / trigger 归一成 0 / 1。
pub fn set_parameter_default(document: &mut Value, name: &str, default: f32) -> Result<(), String> {
    if !default.is_finite() {
        return Err("a parameter default must be finite".to_string());
    }
    let Some(parameters) = document.get_mut("parameters").and_then(Value::as_array_mut) else {
        return Err(format!("parameter '{name}' does not exist"));
    };
    let Some(parameter) = parameters
        .iter_mut()
        .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Err(format!("parameter '{name}' does not exist"));
    };
    let kind = parameter
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "float".to_string());
    parameter["default"] = json!(parameter_default_for(kind.trim(), default));
    Ok(())
}

/// 样本点围成的多边形面积（鞋带公式，绝对值）；少于 3 个点或坐标非有限时返回 0。
///
/// 面板用它判断二维混合空间是否**退化**：所有样本共线时运行时三角化会失败、
/// 悄悄退回一维 / 单剪辑，作者很难从数字上看出来。
pub fn blend_space_2d_area(positions: &[[f32; 2]]) -> f32 {
    if positions.len() < 3 {
        return 0.0;
    }
    if positions
        .iter()
        .any(|position| !position[0].is_finite() || !position[1].is_finite())
    {
        return 0.0;
    }
    let mut twice = 0.0f64;
    for (index, position) in positions.iter().enumerate() {
        let next = positions[(index + 1) % positions.len()];
        twice += position[0] as f64 * next[1] as f64 - next[0] as f64 * position[1] as f64;
    }
    (twice * 0.5).abs() as f32
}

/// 二维样本是否退化（面积相对包围盒接近 0）。
///
/// 阈值按包围盒面积缩放：`area <= 1e-5 * max(1, width * height)`，所以速度轴
/// 0..10 和 0..1 两套坐标都能用同一套判断。
pub fn blend_space_2d_degenerate(positions: &[[f32; 2]]) -> bool {
    if positions.len() < 3 {
        return false;
    }
    let area = blend_space_2d_area(positions);
    let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for position in positions {
        if !position[0].is_finite() || !position[1].is_finite() {
            return true;
        }
        min_x = min_x.min(position[0]);
        max_x = max_x.max(position[0]);
        min_y = min_y.min(position[1]);
        max_y = max_y.max(position[1]);
    }
    let extent = ((max_x - min_x) * (max_y - min_y)).max(1.0);
    area <= 1e-5 * extent
}

/// 改混合空间的参数名：一维只写 `parameter`，二维同时写 `parameter_y`。
pub fn set_blend_space_parameter(
    document: &mut Value,
    name: &str,
    x: &str,
    y: &str,
) -> Result<(), String> {
    let x = x.trim();
    if x.is_empty() {
        return Err("a blend space needs a parameter name".to_string());
    }
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    let Some(dims) = blend_space_dims(node) else {
        return Err(format!("node '{name}' is not a blend space"));
    };
    node["parameter"] = json!(x);
    if dims == 2 {
        let y = y.trim();
        if y.is_empty() {
            return Err("a 2D blend space needs `parameter_y`".to_string());
        }
        node["parameter_y"] = json!(y);
    } else if node.get("parameter_y").is_some() {
        node.as_object_mut()
            .expect("checked above")
            .remove("parameter_y");
    }
    Ok(())
}

/// 重命名一个参数，并改写图内对它的引用
/// （混合空间节点的 `parameter` / `parameter_x` / `parameter_y`）。
///
/// 只改图本身；图引用的 `.statemachine.json` 里的条件名需要用
/// [`rename_condition_parameter`]，跨文件版本见 `Workspace::rename_anim_graph_parameter`。
pub fn rename_parameter(document: &mut Value, old: &str, new: &str) -> Result<(), String> {
    let old = old.trim();
    let new = new.trim();
    if new.is_empty() {
        return Err("a parameter needs a name".to_string());
    }
    if old == new {
        return Ok(());
    }
    let Some(parameters) = document.get_mut("parameters").and_then(Value::as_array_mut) else {
        return Err(format!("parameter '{old}' does not exist"));
    };
    if parameters
        .iter()
        .any(|parameter| parameter.get("name").and_then(Value::as_str) == Some(new))
    {
        return Err(format!("a parameter named '{new}' already exists"));
    }
    let Some(parameter) = parameters
        .iter_mut()
        .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some(old))
    else {
        return Err(format!("parameter '{old}' does not exist"));
    };
    parameter["name"] = json!(new);
    if let Some(nodes) = document.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            for field in ["parameter", "parameter_x", "parameter_y"] {
                if node.get(field).and_then(Value::as_str) == Some(old) {
                    node[field] = json!(new);
                }
            }
        }
    }
    Ok(())
}

/// 把状态机里所有引用 `old` 的条件改名成 `new`；返回是否有改动。
///
/// 只改条件名，不动条件类型 / 阈值；状态机里不存在的参数名不会被造出来。
pub fn rename_condition_parameter(
    document: &mut Value,
    old: &str,
    new: &str,
) -> Result<bool, String> {
    let old = old.trim();
    let new = new.trim();
    if new.is_empty() {
        return Err("a parameter needs a name".to_string());
    }
    let mut changed = false;
    let Some(transitions) = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    for transition in transitions {
        let Some(conditions) = transition
            .get_mut("conditions")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for condition in conditions {
            if condition.get("name").and_then(Value::as_str) == Some(old) {
                condition["name"] = json!(new);
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// 改一个混合空间样本的坐标；一维节点只写 x（`positions` 里是数字）。
pub fn set_blend_space_position(
    document: &mut Value,
    name: &str,
    index: usize,
    position: [f32; 2],
) -> Result<(), String> {
    if !position[0].is_finite() || !position[1].is_finite() {
        return Err("a blend space position must be finite".to_string());
    }
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    };
    let Some(dims) = blend_space_dims(node) else {
        return Err(format!("node '{name}' is not a blend space"));
    };
    let Some(positions) = node.get_mut("positions").and_then(Value::as_array_mut) else {
        return Err(format!("node '{name}' has no editable `positions` array"));
    };
    let Some(entry) = positions.get_mut(index) else {
        return Err(format!("node '{name}' has no sample #{index}"));
    };
    *entry = if dims == 1 {
        json!(position[0])
    } else {
        json!([position[0], position[1]])
    };
    Ok(())
}

/// 给混合空间追加一个样本（`clips` / `positions` 同步增长）。
///
/// 节点带每样本 `speeds`（v2）时也补一个 `1.0`，否则 cooker 会因为
/// 「剪辑数与速度数不一致」拒绝整张图。`clip` 与 `clips` 同时存在的节点先报错，
/// 因为样例顺序会变得有歧义。
pub fn add_blend_space_sample(
    document: &mut Value,
    name: &str,
    clip: &str,
    position: [f32; 2],
) -> Result<(), String> {
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a blend space sample needs a clip path".to_string());
    }
    if !position[0].is_finite() || !position[1].is_finite() {
        return Err("a blend space position must be finite".to_string());
    }
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    let Some(dims) = blend_space_dims(node) else {
        return Err(format!("node '{name}' is not a blend space"));
    };
    if node.get("clip").is_some() {
        return Err(format!(
            "node '{name}' uses a single `clip`; move it into `clips` first"
        ));
    }
    if node.get("clips").is_none() {
        node["clips"] = json!([]);
    }
    if node.get("positions").is_none() {
        node["positions"] = json!([]);
    }
    let clip_count = node
        .get("clips")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let position_count = node
        .get("positions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    if clip_count != position_count {
        return Err(format!(
            "node '{name}' has {clip_count} clips but {position_count} positions; fix the mismatch first"
        ));
    }
    if let Some(speeds) = node.get("speeds").and_then(Value::as_array) {
        if speeds.len() != clip_count {
            return Err(format!(
                "node '{name}' has {clip_count} clips but {} sample speeds; fix the mismatch first",
                speeds.len()
            ));
        }
    }
    let Some(clips) = node.get_mut("clips").and_then(Value::as_array_mut) else {
        return Err(format!("node '{name}' has an invalid `clips` field"));
    };
    clips.push(json!(clip));
    let Some(positions) = node.get_mut("positions").and_then(Value::as_array_mut) else {
        return Err(format!("node '{name}' has an invalid `positions` field"));
    };
    positions.push(if dims == 1 {
        json!(position[0])
    } else {
        json!([position[0], position[1]])
    });
    if let Some(speeds) = node.get_mut("speeds").and_then(Value::as_array_mut) {
        speeds.push(json!(1.0));
    }
    Ok(())
}

/// 删除混合空间的一个样本；一维至少留 2 个、二维至少留 3 个，否则 cooker 拒绝。
pub fn remove_blend_space_sample(
    document: &mut Value,
    name: &str,
    index: usize,
) -> Result<(), String> {
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    let Some(dims) = blend_space_dims(node) else {
        return Err(format!("node '{name}' is not a blend space"));
    };
    let Some(clips) = node.get_mut("clips").and_then(Value::as_array_mut) else {
        return Err(format!("node '{name}' has no editable `clips` array"));
    };
    if index >= clips.len() {
        return Err(format!("node '{name}' has no sample #{index}"));
    }
    let minimum = if dims == 1 { 2 } else { 3 };
    if clips.len() <= minimum {
        return Err(format!(
            "a {dims}D blend space needs at least {minimum} samples"
        ));
    }
    clips.remove(index);
    if let Some(positions) = node.get_mut("positions").and_then(Value::as_array_mut) {
        if index < positions.len() {
            positions.remove(index);
        }
    }
    if let Some(speeds) = node.get_mut("speeds").and_then(Value::as_array_mut) {
        if index < speeds.len() {
            speeds.remove(index);
        }
    }
    Ok(())
}

/// 改一个混合空间样本的剪辑路径（`clips[index]`）。
pub fn set_blend_space_sample_clip(
    document: &mut Value,
    name: &str,
    index: usize,
    clip: &str,
) -> Result<(), String> {
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a blend space sample needs a clip path".to_string());
    }
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    };
    if blend_space_dims(node).is_none() {
        return Err(format!("node '{name}' is not a blend space"));
    }
    let Some(clips) = node.get_mut("clips").and_then(Value::as_array_mut) else {
        return Err(format!("node '{name}' has no editable `clips` array"));
    };
    let Some(entry) = clips.get_mut(index) else {
        return Err(format!("node '{name}' has no sample #{index}"));
    };
    *entry = json!(clip);
    Ok(())
}

/// 改节点的播放速度（`0` = 冻结，必须有限且非负）。
pub fn set_graph_node_speed(document: &mut Value, name: &str, speed: f32) -> Result<(), String> {
    if !speed.is_finite() || speed < 0.0 {
        return Err("node speed must be finite and >= 0".to_string());
    }
    set_node_field(document, name, "speed", json!(speed))
}

/// 改节点自身权重（0..=1；cooker 也会夹在这个范围）。
pub fn set_graph_node_weight(document: &mut Value, name: &str, weight: f32) -> Result<(), String> {
    if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return Err("node weight must be finite and between 0 and 1".to_string());
    }
    set_node_field(document, name, "weight", json!(weight))
}

/// 改节点是否循环播放。
pub fn set_graph_node_looped(document: &mut Value, name: &str, looped: bool) -> Result<(), String> {
    set_node_field(document, name, "looped", json!(looped))
}

fn set_node_field(
    document: &mut Value,
    name: &str,
    field: &str,
    value: Value,
) -> Result<(), String> {
    let Some(node) = find_node_mut(document, name) else {
        return Err(format!("unknown node '{name}'"));
    };
    if !node.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    node[field] = value;
    Ok(())
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

/// 复制一个节点，返回新名字。
///
/// 新名字在原名后加 `_copy` / `_copy2`…；其它字段（剪辑、坐标、inputs、bone mask）
/// 原样保留，所以复制出来的节点自己就是合法的，接线仍然指向原来的上游。
/// 输出节点不能复制（图里只能有一个 output）。
pub fn duplicate_graph_node(document: &mut Value, name: &str) -> Result<String, String> {
    let nodes = document
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "animation graph needs a `nodes` array".to_string())?;
    let Some(source) = nodes
        .iter()
        .find(|node| node.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Err(format!("unknown node '{name}'"));
    };
    if source
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("output"))
    {
        return Err(format!(
            "node '{name}' is the graph output; a graph has exactly one"
        ));
    }
    let existing: std::collections::HashSet<&str> = nodes
        .iter()
        .filter_map(|node| node.get("name").and_then(Value::as_str))
        .collect();
    let base = format!("{name}_copy");
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.contains(candidate.as_str()) {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    let mut copy = source.clone();
    if !copy.is_object() {
        return Err(format!("node '{name}' is malformed"));
    }
    copy["name"] = json!(candidate);
    let Some(nodes) = document.get_mut("nodes").and_then(Value::as_array_mut) else {
        return Err("animation graph needs a `nodes` array".to_string());
    };
    nodes.push(copy);
    Ok(candidate)
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
mod parameter_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "parameters": [{ "name": "speed", "type": "float", "default": 1.5 }],
            "nodes": [
                { "name": "locomotion", "type": "blend_space_1d",
                  "clips": ["idle.ranim", "run.ranim"], "parameter": "speed" },
                { "name": "out", "type": "output", "input": "locomotion" }
            ]
        })
    }

    #[test]
    fn parameters_can_be_added_and_defaults_edited() {
        let mut doc = document();
        add_parameter(&mut doc, " grounded ", "Bool", 3.0).expect("adds");
        assert_eq!(doc["parameters"][1]["name"], "grounded");
        assert_eq!(doc["parameters"][1]["type"], "bool");
        assert_eq!(doc["parameters"][1]["default"].as_f64(), Some(1.0));

        set_parameter_default(&mut doc, "speed", 2.5).expect("float keeps value");
        assert_eq!(doc["parameters"][0]["default"].as_f64(), Some(2.5));
        set_parameter_default(&mut doc, "grounded", 0.2).expect("bool normalizes");
        assert_eq!(doc["parameters"][1]["default"].as_f64(), Some(0.0));

        let mut empty = json!({ "nodes": [] });
        add_parameter(&mut empty, "trigger", "trigger", 0.0).expect("creates the list");
        assert_eq!(empty["parameters"][0]["name"], "trigger");
    }

    #[test]
    fn parameters_can_be_renamed_across_graph_references() {
        let mut doc = document();
        rename_parameter(&mut doc, "speed", " velocity ").expect("renames");
        assert_eq!(doc["parameters"][0]["name"], "velocity");
        assert_eq!(doc["nodes"][0]["parameter"], "velocity");

        rename_parameter(&mut doc, "velocity", "velocity").expect("same name is a no-op");
        assert!(rename_parameter(&mut doc, "missing", "other").is_err());
        assert!(rename_parameter(&mut doc, "velocity", " ").is_err());
    }

    #[test]
    fn renaming_rejects_names_that_already_exist() {
        let mut doc = document();
        add_parameter(&mut doc, "grounded", "bool", 0.0).expect("adds");
        let error = rename_parameter(&mut doc, "speed", "grounded").expect_err("duplicate");
        assert!(error.contains("grounded"), "{error}");
        assert_eq!(doc["parameters"][0]["name"], "speed", "失败时不能改名");
    }

    #[test]
    fn state_machine_conditions_follow_a_parameter_rename() {
        let mut machine = json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim" }
            ],
            "transitions": [
                { "from": "Idle", "to": "Run", "conditions": [
                    { "type": "float_greater", "name": "speed", "threshold": 1.0 },
                    { "type": "trigger", "name": "jump" }
                ] }
            ]
        });
        assert!(rename_condition_parameter(&mut machine, "speed", "velocity").expect("renames"));
        assert_eq!(
            machine["transitions"][0]["conditions"][0]["name"],
            "velocity"
        );
        assert_eq!(machine["transitions"][0]["conditions"][1]["name"], "jump");
        assert!(!rename_condition_parameter(&mut machine, "missing", "other").expect("no-op"));
        assert!(rename_condition_parameter(&mut machine, "jump", "").is_err());
    }

    #[test]
    fn referenced_parameters_cannot_be_removed() {
        let mut doc = document();
        let error = remove_parameter(&mut doc, "speed").expect_err("referenced");
        assert!(error.contains("locomotion"), "{error}");
        add_parameter(&mut doc, "extra", "float", 0.0).expect("adds");
        remove_parameter(&mut doc, "extra").expect("unreferenced removal");
        assert!(remove_parameter(&mut doc, "extra").is_err(), "already gone");
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let mut doc = document();
        assert!(add_parameter(&mut doc, "  ", "float", 0.0).is_err());
        assert!(
            add_parameter(&mut doc, "speed", "float", 0.0).is_err(),
            "duplicate"
        );
        assert!(
            add_parameter(&mut doc, "ok", "int", 0.0).is_err(),
            "unknown type"
        );
        assert!(add_parameter(&mut doc, "ok", "float", f32::NAN).is_err());
        assert!(set_parameter_default(&mut doc, "missing", 0.0).is_err());
        assert!(set_parameter_default(&mut doc, "speed", f32::INFINITY).is_err());
        assert!(remove_parameter(&mut doc, "missing").is_err());
    }
}

#[cfg(test)]
mod blend_space_area_tests {
    use super::*;

    #[test]
    fn the_area_uses_the_shoelace_formula() {
        let square = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        assert!((blend_space_2d_area(&square) - 4.0).abs() < 1e-6);
        assert!((blend_space_2d_area(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]) - 0.5).abs() < 1e-6);
        assert_eq!(blend_space_2d_area(&[[0.0, 0.0], [1.0, 1.0]]), 0.0);
        assert_eq!(
            blend_space_2d_area(&[[0.0, 0.0], [f32::NAN, 1.0], [1.0, 0.0]]),
            0.0
        );
    }

    #[test]
    fn degenerate_sample_sets_are_flagged() {
        assert!(blend_space_2d_degenerate(&[
            [0.0, 0.0],
            [1.0, 1.0],
            [2.0, 2.0]
        ]));
        assert!(blend_space_2d_degenerate(&[
            [0.0, 0.0],
            [1.0, 0.0],
            [f32::INFINITY, 1.0]
        ]));
        assert!(!blend_space_2d_degenerate(&[
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0]
        ]));
        assert!(
            !blend_space_2d_degenerate(&[[0.0, 0.0], [1.0, 1.0]]),
            "两点还不够判断退化"
        );
    }
}

#[cfg(test)]
mod blend_space_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "nodes": [
                { "name": "locomotion", "type": "blend_space_1d",
                  "clips": ["idle.ranim", "run.ranim"], "positions": [0.0, 5.0],
                  "parameter": "speed" },
                { "name": "strafe", "type": "blend_space_2d",
                  "clips": ["f.ranim", "b.ranim", "l.ranim"],
                  "positions": [[0.0, 1.0], [0.0, -1.0], [-1.0, 0.0]],
                  "parameter_x": "forward", "parameter_y": "right" },
                { "name": "out", "type": "output", "input": "locomotion" }
            ]
        })
    }

    #[test]
    fn a_view_reads_sample_positions_for_both_dimensions() {
        let view = parse_anim_graph_view(&document()).expect("parses");
        assert_eq!(view.nodes[0].positions, [[0.0, 0.0], [5.0, 0.0]]);
        assert_eq!(view.nodes[0].parameter, "speed");
        assert!(view.nodes[0].parameter_y.is_empty());
        assert_eq!(
            view.nodes[1].positions,
            [[0.0, 1.0], [0.0, -1.0], [-1.0, 0.0]]
        );
        assert_eq!(view.nodes[1].parameter, "forward");
        assert_eq!(view.nodes[1].parameter_y, "right");
    }

    #[test]
    fn blend_space_parameters_can_be_edited() {
        let mut doc = document();
        set_blend_space_parameter(&mut doc, "locomotion", " locomotion_speed ", "").expect("1d");
        assert_eq!(doc["nodes"][0]["parameter"], "locomotion_speed");

        set_blend_space_parameter(&mut doc, "strafe", " fwd ", " side ").expect("2d");
        assert_eq!(doc["nodes"][1]["parameter"], "fwd");
        assert_eq!(doc["nodes"][1]["parameter_y"], "side");
    }

    #[test]
    fn bad_blend_space_parameters_are_rejected() {
        let mut doc = document();
        assert!(set_blend_space_parameter(&mut doc, "locomotion", "  ", "").is_err());
        assert!(set_blend_space_parameter(&mut doc, "strafe", "fwd", "  ").is_err());
        assert!(set_blend_space_parameter(&mut doc, "out", "fwd", "side").is_err());
        assert!(set_blend_space_parameter(&mut doc, "missing", "fwd", "side").is_err());
    }

    #[test]
    fn sample_positions_keep_their_dimension() {
        let mut doc = document();
        set_blend_space_position(&mut doc, "locomotion", 1, [7.5, 99.0]).expect("1d");
        assert_eq!(doc["nodes"][0]["positions"][1], json!(7.5));
        assert_eq!(doc["nodes"][0]["positions"][0], json!(0.0));

        set_blend_space_position(&mut doc, "strafe", 2, [-2.0, 0.5]).expect("2d");
        assert_eq!(doc["nodes"][1]["positions"][2], json!([-2.0, 0.5]));
    }

    #[test]
    fn bad_sample_positions_are_rejected() {
        let mut doc = document();
        assert!(set_blend_space_position(&mut doc, "locomotion", 0, [f32::NAN, 0.0]).is_err());
        assert!(set_blend_space_position(&mut doc, "locomotion", 9, [0.0, 0.0]).is_err());
        assert!(set_blend_space_position(&mut doc, "out", 0, [0.0, 0.0]).is_err());
        assert!(set_blend_space_position(&mut doc, "missing", 0, [0.0, 0.0]).is_err());
        let mut without = json!({
            "nodes": [{ "name": "space", "type": "blend_space_1d", "parameter": "speed" }]
        });
        assert!(set_blend_space_position(&mut without, "space", 0, [0.0, 0.0]).is_err());
    }

    #[test]
    fn samples_can_be_added_to_1d_and_2d_spaces() {
        let mut doc = document();
        add_blend_space_sample(&mut doc, "locomotion", " sprint.ranim ", [9.0, 1.0]).expect("1d");
        assert_eq!(
            doc["nodes"][0]["clips"],
            json!(["idle.ranim", "run.ranim", "sprint.ranim"])
        );
        assert_eq!(doc["nodes"][0]["positions"], json!([0.0, 5.0, 9.0]));

        add_blend_space_sample(&mut doc, "strafe", "r.ranim", [1.0, 1.0]).expect("2d");
        assert_eq!(doc["nodes"][1]["clips"].as_array().unwrap().len(), 4);
        assert_eq!(doc["nodes"][1]["positions"][3], json!([1.0, 1.0]));
    }

    #[test]
    fn adding_a_sample_keeps_the_speed_table_in_sync() {
        let mut doc = json!({
            "nodes": [{ "name": "space", "type": "blend_space_1d",
                        "clips": ["a.ranim", "b.ranim"], "positions": [0.0, 1.0],
                        "parameter": "speed", "speeds": [1.0, 2.0] }]
        });
        add_blend_space_sample(&mut doc, "space", "c.ranim", [2.0, 0.0]).expect("adds");
        assert_eq!(doc["nodes"][0]["speeds"], json!([1.0, 2.0, 1.0]));
        remove_blend_space_sample(&mut doc, "space", 1).expect("removes");
        assert_eq!(doc["nodes"][0]["speeds"], json!([1.0, 1.0]));
        assert_eq!(doc["nodes"][0]["positions"], json!([0.0, 2.0]));
    }

    #[test]
    fn removing_keeps_the_minimum_sample_count() {
        let mut doc = document();
        assert!(remove_blend_space_sample(&mut doc, "locomotion", 0).is_err());
        assert!(remove_blend_space_sample(&mut doc, "strafe", 0).is_err());
        assert!(remove_blend_space_sample(&mut doc, "locomotion", 5).is_err());
        assert!(remove_blend_space_sample(&mut doc, "missing", 0).is_err());
        assert!(remove_blend_space_sample(&mut doc, "out", 0).is_err());
    }

    #[test]
    fn adding_samples_validates_clips_positions_and_speeds() {
        let mut doc = document();
        assert!(add_blend_space_sample(&mut doc, "locomotion", "  ", [0.0, 0.0]).is_err());
        assert!(
            add_blend_space_sample(&mut doc, "locomotion", "x.ranim", [f32::NAN, 0.0]).is_err()
        );
        assert!(add_blend_space_sample(&mut doc, "out", "x.ranim", [0.0, 0.0]).is_err());
        assert!(add_blend_space_sample(&mut doc, "missing", "x.ranim", [0.0, 0.0]).is_err());

        let mut single = json!({
            "nodes": [{ "name": "s", "type": "blend_space_1d", "clip": "a.ranim",
                        "clips": ["a.ranim"], "positions": [0.0], "parameter": "speed" }]
        });
        assert!(add_blend_space_sample(&mut single, "s", "b.ranim", [1.0, 0.0]).is_err());

        let mut mismatched = json!({
            "nodes": [{ "name": "m", "type": "blend_space_1d",
                        "clips": ["a.ranim", "b.ranim"], "positions": [0.0],
                        "parameter": "speed" }]
        });
        assert!(add_blend_space_sample(&mut mismatched, "m", "c.ranim", [2.0, 0.0]).is_err());

        let mut bad_speeds = json!({
            "nodes": [{ "name": "v", "type": "blend_space_1d",
                        "clips": ["a.ranim", "b.ranim"], "positions": [0.0, 1.0],
                        "parameter": "speed", "speeds": [1.0] }]
        });
        assert!(add_blend_space_sample(&mut bad_speeds, "v", "c.ranim", [2.0, 0.0]).is_err());
    }

    #[test]
    fn sample_clips_can_be_replaced() {
        let mut doc = document();
        set_blend_space_sample_clip(&mut doc, "locomotion", 1, " jog.ranim ").expect("sets");
        assert_eq!(doc["nodes"][0]["clips"][1], "jog.ranim");
        assert!(set_blend_space_sample_clip(&mut doc, "locomotion", 0, "  ").is_err());
        assert!(set_blend_space_sample_clip(&mut doc, "locomotion", 9, "a.ranim").is_err());
        assert!(set_blend_space_sample_clip(&mut doc, "out", 0, "a.ranim").is_err());
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "output": "out",
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim" },
                { "name": "b", "type": "clip_player", "clip": "b.ranim" },
                { "name": "stack", "type": "blend_stack",
                  "inputs": ["a", { "node": "b", "weight": 0.5 }] },
                { "name": "out", "type": "output", "input": "stack" }
            ]
        })
    }

    #[test]
    fn renaming_rewrites_every_reference() {
        let mut doc = document();
        rename_graph_node(&mut doc, "a", " run ").expect("renames");
        assert_eq!(doc["nodes"][0]["name"], "run");
        assert_eq!(doc["nodes"][2]["inputs"][0], "run");
        assert_eq!(doc["nodes"][2]["inputs"][1]["node"], "b");

        rename_graph_node(&mut doc, "b", "aim").expect("renames");
        assert_eq!(doc["nodes"][2]["inputs"][1]["node"], "aim");
        assert_eq!(doc["nodes"][2]["inputs"][1]["weight"], 0.5);

        rename_graph_node(&mut doc, "out", "root").expect("renames output");
        assert_eq!(doc["nodes"][3]["name"], "root");
        assert_eq!(doc["nodes"][3]["input"], "stack");
        assert_eq!(doc["output"], "root");
        assert_eq!(
            parse_anim_graph_view(&doc)
                .expect("parses")
                .output
                .as_deref(),
            Some("root")
        );
    }

    #[test]
    fn rename_rejects_duplicates_blanks_and_unknown_names() {
        let mut doc = document();
        assert!(rename_graph_node(&mut doc, "a", "b").is_err());
        assert!(rename_graph_node(&mut doc, "a", "  ").is_err());
        assert!(rename_graph_node(&mut doc, "missing", "aim").is_err());
        rename_graph_node(&mut doc, "a", "a").expect("same name is a no-op");
        assert_eq!(doc["nodes"][0]["name"], "a");
    }
}

#[cfg(test)]
mod node_prop_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "nodes": [
                { "name": "a", "type": "clip_player", "clip": "a.ranim", "clips": ["a.ranim"] },
                { "name": "out", "type": "output", "input": "a" }
            ]
        })
    }

    #[test]
    fn a_clip_node_can_point_at_another_clip() {
        let mut doc = document();
        set_graph_node_clip(&mut doc, "a", " b.ranim ").expect("sets");
        assert_eq!(doc["nodes"][0]["clip"], "b.ranim");
        assert!(
            doc["nodes"][0].get("clips").is_none(),
            "单个剪辑不能同时留 clips 数组：{:?}",
            doc["nodes"][0]
        );
    }

    #[test]
    fn node_speed_weight_and_loop_can_be_edited() {
        let mut doc = document();
        set_graph_node_speed(&mut doc, "a", 1.5).expect("speed");
        set_graph_node_weight(&mut doc, "a", 0.25).expect("weight");
        set_graph_node_looped(&mut doc, "a", false).expect("looped");
        assert_eq!(doc["nodes"][0]["speed"].as_f64(), Some(1.5));
        assert_eq!(doc["nodes"][0]["weight"].as_f64(), Some(0.25));
        assert_eq!(doc["nodes"][0]["looped"], false);
    }

    #[test]
    fn bad_node_properties_are_rejected() {
        let mut doc = document();
        assert!(set_graph_node_clip(&mut doc, "a", "  ").is_err());
        assert!(set_graph_node_clip(&mut doc, "missing", "b.ranim").is_err());
        assert!(set_graph_node_speed(&mut doc, "a", -1.0).is_err());
        assert!(set_graph_node_speed(&mut doc, "a", f32::NAN).is_err());
        assert!(set_graph_node_weight(&mut doc, "a", 1.5).is_err());
        assert!(set_graph_node_weight(&mut doc, "a", f32::NAN).is_err());
        assert!(set_graph_node_looped(&mut doc, "missing", true).is_err());
        assert!(set_graph_node_clip(&mut json!({ "nodes": ["bad"] }), "a", "b.ranim").is_err());
    }
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

/// 改一条转移的起点 / 终点状态；`from = None` 表示任意状态。
///
/// 状态名必须存在、不能自连，也不能改出一条与现有转移完全相同的 `(from, to)`
/// （重复转移在运行时只会永远命中第一条，没有意义）。其它字段原样保留。
pub fn set_transition_states(
    document: &mut Value,
    index: usize,
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
    let transitions = document
        .get("transitions")
        .and_then(Value::as_array)
        .ok_or_else(|| "state machine has no editable `transitions` array".to_string())?;
    if index >= transitions.len() {
        return Err(format!("state machine has no transition #{index}"));
    }
    for (other_index, transition) in transitions.iter().enumerate() {
        if other_index == index {
            continue;
        }
        let other_from = transition
            .get("from")
            .and_then(Value::as_str)
            .filter(|from| *from != "*" && !from.eq_ignore_ascii_case("any"));
        let other_to = transition.get("to").and_then(Value::as_str);
        if other_to == Some(to) && other_from == from {
            return Err(format!(
                "a transition from '{}' to '{to}' already exists",
                from.unwrap_or("any")
            ));
        }
    }
    let transition = find_transition_mut(document, index)?;
    transition["from"] = json!(from.unwrap_or("*"));
    transition["to"] = json!(to);
    Ok(())
}

/// 在 `transitions` 数组里前后移动一条转移，返回移动后的下标。
///
/// 运行时按数组顺序取**第一条**条件命中且起点状态匹配的转移（`state_machine.rs`
/// 的 `next_transition`），所以顺序就是优先级。越界移动返回错误而不是静默夹到端点，
/// 面板据此禁用端点上的箭头。
pub fn move_state_transition(
    document: &mut Value,
    index: usize,
    offset: isize,
) -> Result<usize, String> {
    let transitions = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "state machine has no editable `transitions` array".to_string())?;
    if index >= transitions.len() {
        return Err(format!("state machine has no transition #{index}"));
    }
    let len = transitions.len() as isize;
    let target = index as isize + offset;
    if target < 0 || target >= len {
        return Err(format!(
            "transition #{index} cannot move by {offset} (only {len} transitions)"
        ));
    }
    let transition = transitions.remove(index);
    transitions.insert(target as usize, transition);
    Ok(target as usize)
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

fn find_state_mut(document: &mut Value, index: usize) -> Result<&mut Value, String> {
    let state = document
        .get_mut("states")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "state machine needs a `states` array".to_string())?
        .get_mut(index)
        .ok_or_else(|| format!("state machine has no state #{index}"))?;
    if !state.is_object() {
        return Err(format!("state #{index} is malformed"));
    }
    Ok(state)
}

/// 改一个状态的剪辑路径（去空白后必须非空）。
pub fn set_state_clip(document: &mut Value, index: usize, clip: &str) -> Result<(), String> {
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a state needs a non-empty clip path".to_string());
    }
    find_state_mut(document, index)?["clip"] = json!(clip);
    Ok(())
}

/// 改一个状态的播放速度倍率（`0` = 冻结，必须有限且非负）。
pub fn set_state_speed(document: &mut Value, index: usize, speed: f32) -> Result<(), String> {
    if !speed.is_finite() || speed < 0.0 {
        return Err("state speed must be finite and >= 0".to_string());
    }
    find_state_mut(document, index)?["speed"] = json!(speed);
    Ok(())
}

/// 改一个状态是否循环。
pub fn set_state_looped(document: &mut Value, index: usize, looped: bool) -> Result<(), String> {
    find_state_mut(document, index)?["looped"] = json!(looped);
    Ok(())
}

/// 复制一个状态，返回新名字。
///
/// 新名字自动加 `_copy` / `_copy2`；转移表原样保留（复制出来的状态暂时没有
/// 任何转移，作者再连线）。名字唯一性跟其它状态一致。
pub fn duplicate_state(document: &mut Value, name: &str) -> Result<String, String> {
    let states = document
        .get("states")
        .and_then(Value::as_array)
        .ok_or_else(|| "state machine needs a `states` array".to_string())?;
    let Some(source) = states
        .iter()
        .find(|state| state.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Err(format!("unknown state '{name}'"));
    };
    if !source.is_object() {
        return Err(format!("state '{name}' is malformed"));
    }
    let existing: std::collections::HashSet<&str> = states
        .iter()
        .filter_map(|state| state.get("name").and_then(Value::as_str))
        .collect();
    let base = format!("{name}_copy");
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.contains(candidate.as_str()) {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    let mut copy = source.clone();
    copy["name"] = json!(candidate);
    let Some(states) = document.get_mut("states").and_then(Value::as_array_mut) else {
        return Err("state machine needs a `states` array".to_string());
    };
    states.push(copy);
    Ok(candidate)
}

/// 追加一个状态。
///
/// 名字与剪辑路径都去空白后必须非空（cooker 会拒绝空剪辑）；名字不能与现有
/// 状态重复。新状态默认 `speed = 1.0`、`looped = true`，由 cooker 的缺省值补齐。
pub fn add_state(document: &mut Value, name: &str, clip: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a state needs a non-empty name".to_string());
    }
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a state needs a non-empty clip path".to_string());
    }
    let states = document
        .get_mut("states")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "state machine needs a `states` array".to_string())?;
    if states.iter().any(|state| {
        !state.is_object()
            || state
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .is_none()
    }) {
        return Err("state machine has a malformed state entry".to_string());
    }
    if states
        .iter()
        .any(|state| state.get("name").and_then(Value::as_str) == Some(name))
    {
        return Err(format!("a state named '{name}' already exists"));
    }
    states.push(json!({ "name": name, "clip": clip }));
    Ok(())
}

/// 删除一个状态，并移除所有引用它的转移，返回被一并删除的转移数量。
///
/// 状态机至少要保留一个状态（cooker 与运行时都按「第一个状态是初始状态」处理）；
/// 删除第一个状态后，下一个状态成为初始状态。任意状态（`*` / `any`）的转移不受影响。
pub fn remove_state(document: &mut Value, name: &str) -> Result<usize, String> {
    let name = name.trim();
    let state_index = {
        let states = document
            .get("states")
            .and_then(Value::as_array)
            .ok_or_else(|| "state machine needs a `states` array".to_string())?;
        let index = states
            .iter()
            .position(|state| state.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| format!("unknown state '{name}'"))?;
        if states.len() <= 1 {
            return Err("a state machine needs at least one state".to_string());
        }
        index
    };
    if let Some(value) = document.get("transitions") {
        if !value.is_array() {
            return Err("state machine has an invalid `transitions` field".to_string());
        }
    }
    let states = document
        .get_mut("states")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "state machine needs a `states` array".to_string())?;
    states.remove(state_index);

    let Some(value) = document.get_mut("transitions") else {
        return Ok(0);
    };
    let transitions = value
        .as_array_mut()
        .ok_or_else(|| "state machine has an invalid `transitions` field".to_string())?;
    let before = transitions.len();
    transitions.retain(|transition| {
        let from = transition
            .get("from")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|from| !from.is_empty() && *from != "*" && !from.eq_ignore_ascii_case("any"));
        let to = transition.get("to").and_then(Value::as_str).map(str::trim);
        from != Some(name) && to != Some(name)
    });
    Ok(before - transitions.len())
}

/// 重命名一个状态，并同步改写转移里的 `from` / `to`。
///
/// 只改状态机文档：运行时 `Animator.state` 和条件里的参数名不在这里，面板会提示
/// 作者自己确认 `.scene.json` 有没有按旧名字写死。名字去空白后必须非空、不能与
/// 其它状态重名。
pub fn rename_state(document: &mut Value, index: usize, new_name: &str) -> Result<(), String> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("a state needs a non-empty name".to_string());
    }
    let old_name = find_state_mut(document, index)?
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if old_name == new_name {
        return Ok(());
    }
    let states = document
        .get("states")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if states
        .iter()
        .any(|state| state.get("name").and_then(Value::as_str) == Some(new_name))
    {
        return Err(format!("a state named '{new_name}' already exists"));
    }
    find_state_mut(document, index)?["name"] = json!(new_name);
    if let Some(transitions) = document
        .get_mut("transitions")
        .and_then(Value::as_array_mut)
    {
        for transition in transitions.iter_mut() {
            for key in ["from", "to"] {
                if transition.get(key).and_then(Value::as_str) == Some(old_name.as_str()) {
                    transition[key] = json!(new_name);
                }
            }
        }
    }
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
mod state_tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim", "speed": 1.2, "looped": true }
            ],
            "transitions": [
                { "from": "Idle", "to": "Run", "duration": 0.2 },
                { "from": "*", "to": "Idle", "duration": 0.0 }
            ]
        })
    }

    #[test]
    fn a_state_clip_speed_and_loop_flag_can_be_edited() {
        let mut doc = document();
        set_state_clip(&mut doc, 0, " idle_alt.ranim ").expect("sets clip");
        set_state_speed(&mut doc, 0, 0.75).expect("sets speed");
        set_state_looped(&mut doc, 1, false).expect("sets loop");
        assert_eq!(doc["states"][0]["clip"], "idle_alt.ranim");
        assert_eq!(doc["states"][0]["speed"].as_f64(), Some(0.75f32 as f64));
        assert_eq!(doc["states"][1]["looped"], false);
    }

    #[test]
    fn bad_state_values_and_indices_are_rejected() {
        let mut doc = document();
        assert!(set_state_clip(&mut doc, 0, "  ").is_err());
        assert!(set_state_clip(&mut doc, 9, "a.ranim").is_err());
        assert!(set_state_speed(&mut doc, 0, -1.0).is_err());
        assert!(set_state_speed(&mut doc, 0, f32::NAN).is_err());
        assert!(set_state_speed(&mut doc, 9, 1.0).is_err());
        assert!(set_state_looped(&mut doc, 9, true).is_err());
        assert!(set_state_clip(&mut json!({ "states": ["bad"] }), 0, "a.ranim").is_err());
    }

    #[test]
    fn states_can_be_added_with_a_name_and_clip() {
        let mut doc = document();
        add_state(&mut doc, " Jump ", " jump.ranim ").expect("adds");
        assert_eq!(
            doc["states"][2],
            json!({ "name": "Jump", "clip": "jump.ranim" })
        );
        assert_eq!(
            doc["transitions"].as_array().unwrap().len(),
            2,
            "转移表不变"
        );
    }

    #[test]
    fn adding_states_rejects_blank_duplicate_and_malformed_input() {
        let mut doc = document();
        assert!(add_state(&mut doc, " ", "jump.ranim").is_err());
        assert!(add_state(&mut doc, "Jump", "  ").is_err());
        assert!(add_state(&mut doc, "Idle", "idle.ranim").is_err());
        assert!(add_state(&mut json!({}), "Jump", "jump.ranim").is_err());
        assert!(add_state(&mut json!({ "states": ["bad"] }), "Jump", "jump.ranim").is_err());
        assert_eq!(doc["states"].as_array().unwrap().len(), 2, "失败不改文档");
    }

    #[test]
    fn removing_a_state_drops_transitions_that_reference_it() {
        let mut doc = document();
        add_state_transition(&mut doc, Some("Run"), "Idle").expect("adds");
        let removed = remove_state(&mut doc, "Run").expect("removes");
        assert_eq!(removed, 2);
        assert_eq!(doc["states"].as_array().unwrap().len(), 1);
        assert_eq!(doc["states"][0]["name"], "Idle");
        assert_eq!(
            doc["transitions"],
            json!([{ "from": "*", "to": "Idle", "duration": 0.0 }]),
            "只保留与 Run 无关的任意状态转移"
        );
    }

    #[test]
    fn removing_states_rejects_unknown_names_the_last_state_and_bad_transitions() {
        let mut doc = document();
        assert!(remove_state(&mut doc, "Missing").is_err());
        remove_state(&mut doc, "Run").expect("removes");
        assert!(remove_state(&mut doc, "Idle").is_err(), "不能删到零个状态");
        assert_eq!(doc["states"].as_array().unwrap().len(), 1);

        let mut doc = json!({
            "states": [
                { "name": "Idle", "clip": "idle.ranim" },
                { "name": "Run", "clip": "run.ranim" }
            ],
            "transitions": "bad"
        });
        assert!(remove_state(&mut doc, "Run").is_err());
        assert_eq!(doc["states"].as_array().unwrap().len(), 2, "失败不改状态表");
    }

    #[test]
    fn renaming_a_state_rewrites_transition_ends() {
        let mut doc = document();
        rename_state(&mut doc, 1, " Sprint ").expect("renames");
        assert_eq!(doc["states"][1]["name"], "Sprint");
        assert_eq!(doc["transitions"][0]["to"], "Sprint");
        // 任意状态（`*`）不受影响，也不误伤其它状态。
        assert_eq!(doc["transitions"][1]["from"], "*");
        assert_eq!(doc["transitions"][1]["to"], "Idle");
    }

    #[test]
    fn renaming_rejects_duplicates_blanks_and_unknown_indices() {
        let mut doc = document();
        assert!(rename_state(&mut doc, 0, "Run").is_err());
        assert!(rename_state(&mut doc, 0, " ").is_err());
        assert!(rename_state(&mut doc, 9, "Jump").is_err());
        rename_state(&mut doc, 0, "Idle").expect("same name is a no-op");
        assert_eq!(doc["states"][0]["name"], "Idle");
    }
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

    #[test]
    fn transitions_can_be_retargeted() {
        let mut doc = document();
        set_transition_states(&mut doc, 0, Some("Run"), "Jump").expect("retargets");
        assert_eq!(doc["transitions"][0]["from"], "Run");
        assert_eq!(doc["transitions"][0]["to"], "Jump");
        assert_eq!(doc["transitions"][0]["duration"].as_f64(), Some(0.2));

        set_transition_states(&mut doc, 0, None, "Idle").expect("any state");
        assert_eq!(doc["transitions"][0]["from"], "*");
        assert_eq!(doc["transitions"][0]["to"], "Idle");
    }

    #[test]
    fn transitions_move_and_clamp_at_the_ends() {
        let mut doc = document();
        add_state_transition(&mut doc, Some("Run"), "Jump").expect("adds");
        let moved = move_state_transition(&mut doc, 1, -1).expect("moves up");
        assert_eq!(moved, 0);
        assert_eq!(doc["transitions"][0]["to"], "Jump");
        assert_eq!(doc["transitions"][1]["to"], "Run");
        let moved = move_state_transition(&mut doc, 0, 1).expect("moves down");
        assert_eq!(moved, 1);
        assert_eq!(doc["transitions"][1]["to"], "Jump");
        assert!(move_state_transition(&mut doc, 0, -1).is_err(), "已在最前");
        assert!(move_state_transition(&mut doc, 1, 1).is_err(), "已在最后");
        assert!(move_state_transition(&mut doc, 9, -1).is_err());
    }

    #[test]
    fn retargeting_rejects_bad_names_self_links_and_duplicates() {
        let mut doc = document();
        add_state_transition(&mut doc, Some("Run"), "Jump").expect("adds");
        assert!(set_transition_states(&mut doc, 0, Some("Idle"), "Missing").is_err());
        assert!(set_transition_states(&mut doc, 0, Some("Missing"), "Run").is_err());
        assert!(set_transition_states(&mut doc, 0, Some("Run"), "Run").is_err());
        assert!(set_transition_states(&mut doc, 1, Some("Idle"), "Run").is_err());
        assert!(set_transition_states(&mut doc, 9, None, "Run").is_err());
        set_transition_states(&mut doc, 0, Some("Idle"), "Run").expect("same pair is fine");
        assert_eq!(doc["transitions"][0]["from"], "Idle");
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
