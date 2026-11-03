//! 动画图作者 JSON 的快速形状检查（#560 节点检查器）。
//!
//! 这里只复刻 cooker 在 cook 前会拒绝的**形状**问题（节点类型、剪辑数量、
//! 连线目标、输出数量、环、参数类型），让面板在保存前就能指出问题；资产是否存在、
//! 剪辑能不能解码仍以 cooker 为准。检查是只读的，不修改文档。

use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn is_clip_player(kind: &str) -> bool {
    matches!(kind, "clip" | "clip_player" | "clipplayer")
}

fn is_blend_space_1d(kind: &str) -> bool {
    matches!(kind, "blend_space_1d" | "blendspace1d" | "blend_space")
}

fn is_blend_space_2d(kind: &str) -> bool {
    matches!(kind, "blend_space_2d" | "blendspace2d")
}

fn is_known_kind(kind: &str) -> bool {
    is_clip_player(kind)
        || is_blend_space_1d(kind)
        || is_blend_space_2d(kind)
        || matches!(
            kind,
            "blend_stack"
                | "blendstack"
                | "slot"
                | "layered_blend"
                | "layeredblend"
                | "cached_pose"
                | "cachedpose"
                | "state_machine"
                | "statemachine"
                | "output"
        )
}

fn string_list(node: &Value, key: &str) -> Vec<String> {
    node.get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 节点的剪辑列表：`clip`（单数）在前，`clips` 依次追加，和 cooker 一致。
fn node_clips(node: &Value) -> Vec<String> {
    let mut clips = Vec::new();
    if let Some(clip) = node
        .get("clip")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|clip| !clip.is_empty())
    {
        clips.push(clip.to_string());
    }
    clips.extend(string_list(node, "clips"));
    clips
}

/// 节点的连线目标：`input`（单数）+ `inputs`（字符串或 `{ node }`）。
fn node_inputs(node: &Value) -> Vec<String> {
    let mut inputs = Vec::new();
    if let Some(input) = node
        .get("input")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|input| !input.is_empty())
    {
        inputs.push(input.to_string());
    }
    if let Some(entries) = node.get("inputs").and_then(Value::as_array) {
        for entry in entries {
            let target = entry
                .as_str()
                .map(str::trim)
                .or_else(|| entry.get("node").and_then(Value::as_str).map(str::trim))
                .filter(|target| !target.is_empty());
            if let Some(target) = target {
                inputs.push(target.to_string());
            }
        }
    }
    inputs
}

/// 检查整份图；返回人类可读的问题列表（空 = 没发现形状问题）。
pub fn validate_anim_graph(document: &Value) -> Vec<String> {
    let mut issues = Vec::new();
    validate_parameters(document, &mut issues);

    let Some(nodes) = document.get("nodes").and_then(Value::as_array) else {
        issues.push("graph needs a `nodes` array".to_string());
        return issues;
    };
    if nodes.is_empty() {
        issues.push("graph needs at least one node".to_string());
        return issues;
    }

    let mut names: HashMap<String, usize> = HashMap::new();
    let mut types: Vec<String> = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if name.is_empty() {
            issues.push(format!("node #{index} needs a name"));
        } else if names.insert(name.to_string(), index).is_some() {
            issues.push(format!("duplicate node name '{name}'"));
        }
        let kind = node
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if kind.is_empty() {
            issues.push(format!("node '{name}' needs a `type`"));
        } else if !is_known_kind(&kind) {
            issues.push(format!("node '{name}' has unknown type '{kind}'"));
        }
        types.push(kind);
    }

    let mut outputs = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let kind = types[index].as_str();
        let clips = node_clips(node);
        let inputs = node_inputs(node);
        let positions = node
            .get("positions")
            .and_then(Value::as_array)
            .map(|entries| entries.len())
            .unwrap_or(0);
        let parameter = node
            .get("parameter")
            .or_else(|| node.get("parameter_x"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let parameter_y = node
            .get("parameter_y")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let state_machine = node
            .get("state_machine")
            .or_else(|| node.get("machine"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();

        if kind == "output" {
            outputs.push(index);
        }
        if is_clip_player(kind) && clips.len() != 1 {
            issues.push(format!("clip node '{name}' needs exactly one clip"));
        }
        if is_blend_space_1d(kind) {
            if clips.len() < 2 || clips.len() != positions {
                issues.push(format!(
                    "blend_space_1d node '{name}' needs >= 2 clips and one position per clip"
                ));
            }
            if parameter.is_empty() {
                issues.push(format!("blend_space_1d node '{name}' needs a `parameter`"));
            }
        }
        if is_blend_space_2d(kind) {
            if clips.len() < 3 || clips.len() != positions {
                issues.push(format!(
                    "blend_space_2d node '{name}' needs >= 3 clips and one [x, y] per clip"
                ));
            }
            if parameter.is_empty() || parameter_y.is_empty() {
                issues.push(format!(
                    "blend_space_2d node '{name}' needs `parameter_x` and `parameter_y`"
                ));
            }
        }
        if matches!(kind, "state_machine" | "statemachine") && state_machine.is_empty() {
            issues.push(format!("state_machine node '{name}' needs a machine path"));
        }
        if matches!(
            kind,
            "blend_stack" | "blendstack" | "layered_blend" | "layeredblend"
        ) && inputs.is_empty()
        {
            issues.push(format!("node '{name}' needs at least one input"));
        }
        if matches!(kind, "slot" | "cached_pose" | "cachedpose" | "output") && inputs.len() != 1 {
            issues.push(format!("node '{name}' needs exactly one input"));
        }
        if let Some(speeds) = node.get("speeds").and_then(Value::as_array) {
            if !is_blend_space_1d(kind) && !is_blend_space_2d(kind) {
                issues.push(format!(
                    "node '{name}' has sample speeds but is not a blend space"
                ));
            } else if speeds.len() != clips.len() {
                issues.push(format!(
                    "node '{name}' has {} sample speeds for {} clips",
                    speeds.len(),
                    clips.len()
                ));
            }
        }

        for input in inputs {
            if input == name {
                issues.push(format!("node '{name}' cannot connect to itself"));
            } else if !names.contains_key(&input) {
                issues.push(format!("node '{name}' connects to unknown node '{input}'"));
            }
        }
    }

    if outputs.len() != 1 {
        issues.push(format!(
            "graph needs exactly one output node, found {}",
            outputs.len()
        ));
    }
    if let Some(named) = document.get("output").and_then(Value::as_str) {
        let named = named.trim();
        if let Some(index) = outputs.first() {
            let output_name = nodes[*index]
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !named.is_empty() && named != output_name {
                issues.push(format!(
                    "top-level `output` names '{named}', but the output node is '{output_name}'"
                ));
            }
        }
    }
    if has_cycle(nodes) {
        issues.push("graph has a cycle".to_string());
    }
    issues
}

fn validate_parameters(document: &Value, issues: &mut Vec<String>) {
    let Some(parameters) = document.get("parameters").and_then(Value::as_array) else {
        return;
    };
    let mut seen = HashSet::new();
    for (index, parameter) in parameters.iter().enumerate() {
        let name = parameter
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if name.is_empty() {
            issues.push(format!("parameter #{index} needs a name"));
        } else if !seen.insert(name.to_string()) {
            issues.push(format!("duplicate parameter name '{name}'"));
        }
        let kind = parameter
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("float");
        if !matches!(kind, "float" | "bool" | "trigger") {
            issues.push(format!("parameter '{name}' has unknown type '{kind}'"));
        }
    }
}

/// 按名字建图做 DFS；只报告「有没有环」，不展开每个节点。
fn has_cycle(nodes: &[Value]) -> bool {
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for node in nodes {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        edges.entry(name).or_default().extend(node_inputs(node));
    }
    fn visit(
        node: &str,
        edges: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
        done: &mut HashSet<String>,
    ) -> bool {
        if done.contains(node) {
            return false;
        }
        if !visiting.insert(node.to_string()) {
            return true;
        }
        if let Some(next) = edges.get(node) {
            for target in next {
                if visit(target, edges, visiting, done) {
                    return true;
                }
            }
        }
        visiting.remove(node);
        done.insert(node.to_string());
        false
    }
    let mut visiting = HashSet::new();
    let mut done = HashSet::new();
    let names: Vec<String> = edges.keys().cloned().collect();
    for name in names {
        if visit(&name, &edges, &mut visiting, &mut done) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_graph() -> Value {
        json!({
            "parameters": [{ "name": "speed", "type": "float", "default": 0.0 }],
            "nodes": [
                { "name": "locomotion", "type": "blend_space_1d",
                  "clips": ["a.ranim", "b.ranim"], "positions": [0.0, 1.0],
                  "parameter": "speed" },
                { "name": "stack", "type": "blend_stack", "inputs": ["locomotion"] },
                { "name": "out", "type": "output", "input": "stack" }
            ],
            "output": "out"
        })
    }

    fn issues(document: &Value) -> Vec<String> {
        validate_anim_graph(document)
    }

    fn has(document: &Value, needle: &str) -> bool {
        issues(document).iter().any(|issue| issue.contains(needle))
    }

    #[test]
    fn a_valid_graph_has_no_shape_issues() {
        assert_eq!(issues(&valid_graph()), Vec::<String>::new());
    }

    #[test]
    fn node_names_and_types_are_checked() {
        let mut doc = valid_graph();
        doc["nodes"][1]["name"] = json!("locomotion");
        assert!(has(&doc, "duplicate node name 'locomotion'"));

        let mut doc = valid_graph();
        doc["nodes"][0]["name"] = json!("");
        doc["nodes"][2]["type"] = json!("wat");
        let found = issues(&doc);
        assert!(found
            .iter()
            .any(|issue| issue.contains("node #0 needs a name")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("unknown type 'wat'")));
    }

    #[test]
    fn node_shapes_are_checked_against_the_cooker_rules() {
        let doc = json!({
            "nodes": [
                { "name": "clip", "type": "clip_player", "clips": ["a.ranim", "b.ranim"] },
                { "name": "bs1", "type": "blend_space_1d", "clips": ["a.ranim"], "positions": [0.0] },
                { "name": "bs2", "type": "blend_space_2d", "clips": ["a.ranim", "b.ranim"],
                  "positions": [[0.0, 0.0], [1.0, 1.0]] },
                { "name": "sm", "type": "state_machine" },
                { "name": "stack", "type": "blend_stack" },
                { "name": "slot", "type": "slot", "inputs": ["clip", "bs1"] },
                { "name": "out", "type": "output", "input": "stack" }
            ]
        });
        let found = issues(&doc);
        assert!(found.iter().any(|issue| issue.contains("exactly one clip")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("blend_space_1d node 'bs1'")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("blend_space_2d node 'bs2'")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("needs a machine path")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("node 'stack' needs at least one input")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("node 'slot' needs exactly one input")));
    }

    #[test]
    fn connections_and_outputs_are_validated() {
        let mut doc = valid_graph();
        doc["nodes"][1]["inputs"] = json!(["locomotion", "missing", "stack"]);
        doc["nodes"][1]["name"] = json!("stack");
        let found = issues(&doc);
        assert!(found
            .iter()
            .any(|issue| issue.contains("unknown node 'missing'")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("cannot connect to itself")));

        let mut doc = valid_graph();
        doc["nodes"].as_array_mut().unwrap().push(json!({
            "name": "out2", "type": "output", "input": "stack"
        }));
        assert!(has(&doc, "exactly one output node, found 2"));

        let mut doc = valid_graph();
        doc["output"] = json!("other");
        assert!(has(&doc, "top-level `output` names 'other'"));
    }

    #[test]
    fn cycles_parameters_and_sample_speeds_are_reported() {
        let doc = json!({
            "parameters": [
                { "name": "speed", "type": "float" },
                { "name": "speed", "type": "int" }
            ],
            "nodes": [
                { "name": "a", "type": "blend_stack", "inputs": ["b"],
                  "speeds": [1.0] },
                { "name": "b", "type": "blend_stack", "inputs": ["a"] },
                { "name": "out", "type": "output", "input": "a" }
            ]
        });
        let found = issues(&doc);
        assert!(found
            .iter()
            .any(|issue| issue.contains("graph has a cycle")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("duplicate parameter name 'speed'")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("unknown type 'int'")));
        assert!(found
            .iter()
            .any(|issue| issue.contains("sample speeds but is not a blend space")));
    }

    #[test]
    fn a_missing_nodes_array_is_reported_once() {
        let found = issues(&json!({ "parameters": [] }));
        assert_eq!(found, vec!["graph needs a `nodes` array".to_string()]);
        let found = issues(&json!({ "nodes": [] }));
        assert_eq!(found, vec!["graph needs at least one node".to_string()]);
    }
}
