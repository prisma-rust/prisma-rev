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
            .filter_map(condition_summary)
            .map(|summary| AnimConditionView { summary })
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

fn condition_summary(condition: &Value) -> Option<String> {
    let kind = condition.get("type").and_then(Value::as_str)?;
    let name = condition.get("name").and_then(Value::as_str)?;
    Some(match kind {
        "float_greater" => format!(
            "{name} > {}",
            condition
                .get("threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        ),
        "float_less" => format!(
            "{name} < {}",
            condition
                .get("threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        ),
        "bool_is" => format!(
            "{name} == {}",
            condition
                .get("value")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        ),
        "trigger" => format!("{name} (trigger)"),
        other => format!("{name} ({other})"),
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
        assert_eq!(view.transitions[0].conditions[1].summary, "jump (trigger)");
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
}
