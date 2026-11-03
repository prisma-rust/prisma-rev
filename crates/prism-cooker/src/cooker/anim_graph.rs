//! 动画图的 cooked 格式**编码器**（#560 P1）。
//!
//! 解码端在 `prism_asset::formats_anim_graph`（运行时只解码，字节布局写在那里）。
//! cooker 这一侧负责：
//!
//! - 解析作者的 `*.animgraph.json`（[`parse_anim_graph_json`]）；
//! - 把连线里的**节点名字**解析成下标，拒绝未知 / 自连 / 成环的图；
//! - 校验每种节点必需的字段（剪辑数、采样点数、参数名、状态机路径）；
//! - 编码 `.rag`。任何一个节点不合法都返回 `Err` / `None`，不写出半份图。

use prism_asset::formats_anim_graph::{
    AnimGraphInfo, GraphInputInfo, GraphNodeInfo, GraphNodeKind, GraphParameterInfo,
    GraphParameterKind, MAX_GRAPH_BONE_MASK, MAX_GRAPH_CLIPS, MAX_GRAPH_INPUTS, MAX_GRAPH_NODES,
    MAX_GRAPH_PARAMETERS, RAGP_MAGIC, RAGP_NODE_ADDITIVE, RAGP_NODE_LOOPED, RAGP_VERSION,
};
use serde_json::Value;

/// 编码 `.rag`。结构不合法时返回 `None`。
pub fn encode_rag(graph: &AnimGraphInfo) -> Option<Vec<u8>> {
    if graph.parameters.len() > MAX_GRAPH_PARAMETERS
        || graph.nodes.is_empty()
        || graph.nodes.len() > MAX_GRAPH_NODES
        || graph.output as usize >= graph.nodes.len()
        || graph.nodes[graph.output as usize].kind != GraphNodeKind::Output
    {
        return None;
    }
    let mut names = std::collections::HashSet::new();
    for node in &graph.nodes {
        if node.name.trim().is_empty() || !names.insert(node.name.as_str()) {
            return None;
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(RAGP_MAGIC);
    out.push(RAGP_VERSION);
    out.extend_from_slice(&(graph.parameters.len() as u32).to_le_bytes());
    for parameter in &graph.parameters {
        write_string(&mut out, &parameter.name)?;
        out.push(parameter.kind.to_u8());
        out.extend_from_slice(&finite_or(parameter.default, 0.0).to_le_bytes());
    }

    out.extend_from_slice(&(graph.nodes.len() as u32).to_le_bytes());
    for node in &graph.nodes {
        write_string(&mut out, &node.name)?;
        out.push(node.kind.to_u8());
        let mut flags = 0u8;
        if node.looped {
            flags |= RAGP_NODE_LOOPED;
        }
        if node.additive {
            flags |= RAGP_NODE_ADDITIVE;
        }
        out.push(flags);
        out.extend_from_slice(&finite_or(node.speed, 1.0).to_le_bytes());
        out.extend_from_slice(&finite_or(node.weight, 1.0).to_le_bytes());

        if node.clips.len() > MAX_GRAPH_CLIPS
            || node.positions.len() > MAX_GRAPH_CLIPS
            || node.inputs.len() > MAX_GRAPH_INPUTS
        {
            return None;
        }
        out.extend_from_slice(&(node.clips.len() as u32).to_le_bytes());
        for clip in &node.clips {
            write_string(&mut out, clip)?;
        }
        out.extend_from_slice(&(node.positions.len() as u32).to_le_bytes());
        for [x, y] in &node.positions {
            out.extend_from_slice(&finite_or(*x, 0.0).to_le_bytes());
            out.extend_from_slice(&finite_or(*y, 0.0).to_le_bytes());
        }
        write_string(&mut out, &node.parameter_x)?;
        write_string(&mut out, &node.parameter_y)?;
        write_string(&mut out, &node.state_machine_path)?;

        out.extend_from_slice(&(node.inputs.len() as u32).to_le_bytes());
        for input in &node.inputs {
            if input.node as usize >= graph.nodes.len()
                || input.bone_mask.len() > MAX_GRAPH_BONE_MASK
            {
                return None;
            }
            out.extend_from_slice(&input.node.to_le_bytes());
            out.extend_from_slice(&finite_or(input.weight, 1.0).to_le_bytes());
            out.extend_from_slice(&(input.bone_mask.len() as u32).to_le_bytes());
            for joint in &input.bone_mask {
                write_string(&mut out, joint)?;
            }
        }

        // v2：每样本速度表。
        if node.sample_speeds.len() > MAX_GRAPH_CLIPS {
            return None;
        }
        out.extend_from_slice(&(node.sample_speeds.len() as u32).to_le_bytes());
        for speed in &node.sample_speeds {
            if !speed.is_finite() || *speed < 0.0 {
                return None;
            }
            out.extend_from_slice(&speed.to_le_bytes());
        }
    }

    out.extend_from_slice(&graph.output.to_le_bytes());
    Some(out)
}

/// 解析作者的 `*.animgraph.json`。
///
/// ```json
/// {
///   "parameters": [{ "name": "speed", "type": "float", "default": 0.0 }],
///   "nodes": [
///     { "name": "locomotion", "type": "blend_space_1d",
///       "clips": ["idle.ranim", "run.ranim"], "positions": [0, 5],
///       "parameter": "speed" },
///     { "name": "aim", "type": "clip_player", "clip": "aim.ranim",
///       "additive": true, "weight": 0.5, "bone_mask": ["spine_02"] },
///     { "name": "stack", "type": "blend_stack",
///       "inputs": ["locomotion", { "node": "aim", "weight": 0.5 }] },
///     { "name": "out", "type": "output", "input": "stack" }
///   ]
/// }
/// ```
///
/// `type` 接受 `clip_player` / `blend_space_1d` / `blend_space_2d` /
/// `blend_stack` / `slot` / `layered_blend` / `cached_pose` /
/// `state_machine` / `output`。`inputs` 里的字符串等价于权重 1 的连线。
pub fn parse_anim_graph_json(document: &Value) -> Result<AnimGraphInfo, String> {
    let parameters = parse_parameters(document)?;
    let nodes_json = document
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "animation graph needs a `nodes` array".to_string())?;
    if nodes_json.is_empty() {
        return Err("animation graph needs at least one node".to_string());
    }
    if nodes_json.len() > MAX_GRAPH_NODES {
        return Err(format!("animation graph exceeds {MAX_GRAPH_NODES} nodes"));
    }

    let mut nodes = Vec::with_capacity(nodes_json.len());
    let mut names = Vec::with_capacity(nodes_json.len());
    for (index, node_json) in nodes_json.iter().enumerate() {
        let name = node_json
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("node #{index} needs a non-empty `name`"))?;
        if names.iter().any(|known| known == name) {
            return Err(format!("duplicate node name '{name}'"));
        }
        names.push(name.to_string());

        let kind_name = node_json
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("node '{name}' needs a `type`"))?;
        let kind = parse_kind(kind_name)
            .ok_or_else(|| format!("node '{name}' has unknown type '{kind_name}'"))?;

        let clips = parse_clips(node_json);
        let positions = parse_positions(node_json, kind, name)?;
        let sample_speeds = parse_sample_speeds(node_json, name, kind, clips.len())?;
        let parameter_x = node_json
            .get("parameter")
            .or_else(|| node_json.get("parameter_x"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let parameter_y = node_json
            .get("parameter_y")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let state_machine_path = node_json
            .get("state_machine")
            .or_else(|| node_json.get("machine"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        validate_node_shape(
            name,
            kind,
            &clips,
            &positions,
            &parameter_x,
            &parameter_y,
            &state_machine_path,
        )?;

        nodes.push(ParsedNode {
            name: name.to_string(),
            kind,
            looped: node_json
                .get("looped")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            additive: node_json
                .get("additive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            speed: finite_or(
                node_json
                    .get("speed")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0) as f32,
                1.0,
            ),
            weight: finite_or(
                node_json
                    .get("weight")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0) as f32,
                1.0,
            ),
            clips,
            positions,
            sample_speeds,
            parameter_x,
            parameter_y,
            state_machine_path,
            inputs: parse_inputs(node_json, name)?,
        });
    }

    // 解析连线名字 -> 下标。
    let mut resolved: Vec<GraphNodeInfo> = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut inputs = Vec::with_capacity(node.inputs.len());
        for input in node.inputs {
            let target = names
                .iter()
                .position(|name| name == &input.node)
                .ok_or_else(|| {
                    format!(
                        "node '{}' connects to unknown node '{}'",
                        node.name, input.node
                    )
                })?;
            if target == resolved.len() {
                return Err(format!("node '{}' cannot connect to itself", node.name));
            }
            inputs.push(GraphInputInfo {
                node: target as u32,
                weight: finite_or(input.weight, 1.0).clamp(0.0, 1.0),
                bone_mask: input.bone_mask,
            });
        }
        resolved.push(GraphNodeInfo {
            name: node.name,
            kind: node.kind,
            looped: node.looped,
            additive: node.additive,
            speed: node.speed,
            weight: node.weight.clamp(0.0, 1.0),
            clips: node.clips,
            positions: node.positions,
            sample_speeds: node.sample_speeds,
            parameter_x: node.parameter_x,
            parameter_y: node.parameter_y,
            state_machine_path: node.state_machine_path,
            inputs,
        });
    }

    reject_cycles(&resolved)?;
    validate_connections(&resolved)?;

    let outputs: Vec<usize> = resolved
        .iter()
        .enumerate()
        .filter(|(_, node)| node.kind == GraphNodeKind::Output)
        .map(|(index, _)| index)
        .collect();
    if outputs.len() != 1 {
        return Err(format!(
            "animation graph needs exactly one output node, found {}",
            outputs.len()
        ));
    }
    let output = outputs[0];
    if let Some(named) = document.get("output").and_then(Value::as_str) {
        if resolved[output].name != named.trim() {
            return Err(format!(
                "top-level `output` names '{named}', but the output node is '{}'",
                resolved[output].name
            ));
        }
    }

    Ok(AnimGraphInfo {
        parameters,
        nodes: resolved,
        output: output as u32,
    })
}

struct ParsedNode {
    name: String,
    kind: GraphNodeKind,
    looped: bool,
    additive: bool,
    speed: f32,
    weight: f32,
    clips: Vec<String>,
    positions: Vec<[f32; 2]>,
    sample_speeds: Vec<f32>,
    parameter_x: String,
    parameter_y: String,
    state_machine_path: String,
    inputs: Vec<ParsedInput>,
}

struct ParsedInput {
    node: String,
    weight: f32,
    bone_mask: Vec<String>,
}

fn parse_parameters(document: &Value) -> Result<Vec<GraphParameterInfo>, String> {
    let entries = document
        .get("parameters")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if entries.len() > MAX_GRAPH_PARAMETERS {
        return Err(format!(
            "animation graph exceeds {MAX_GRAPH_PARAMETERS} parameters"
        ));
    }
    let mut parameters = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("parameter #{index} needs a non-empty `name`"))?;
        if parameters
            .iter()
            .any(|known: &GraphParameterInfo| known.name == name)
        {
            return Err(format!("duplicate parameter name '{name}'"));
        }
        let kind = match entry.get("type").and_then(Value::as_str).unwrap_or("float") {
            "float" => GraphParameterKind::Float,
            "bool" => GraphParameterKind::Bool,
            "trigger" => GraphParameterKind::Trigger,
            other => return Err(format!("parameter '{name}' has unknown type '{other}'")),
        };
        let default = entry
            .get("default")
            .map(|value| match value.as_f64() {
                Some(number) => finite_or(number as f32, 0.0),
                None => match value.as_bool() {
                    Some(true) => 1.0,
                    Some(false) | None => 0.0,
                },
            })
            .unwrap_or(0.0);
        parameters.push(GraphParameterInfo {
            name: name.to_string(),
            kind,
            default,
        });
    }
    Ok(parameters)
}

fn parse_kind(value: &str) -> Option<GraphNodeKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "clip" | "clip_player" | "clipplayer" => Some(GraphNodeKind::ClipPlayer),
        "blend_space_1d" | "blendspace1d" | "blend_space" => Some(GraphNodeKind::BlendSpace1D),
        "blend_space_2d" | "blendspace2d" => Some(GraphNodeKind::BlendSpace2D),
        "blend_stack" | "blendstack" => Some(GraphNodeKind::BlendStack),
        "slot" => Some(GraphNodeKind::Slot),
        "layered_blend" | "layeredblend" => Some(GraphNodeKind::LayeredBlend),
        "cached_pose" | "cachedpose" => Some(GraphNodeKind::CachedPose),
        "state_machine" | "statemachine" => Some(GraphNodeKind::StateMachine),
        "output" => Some(GraphNodeKind::Output),
        _ => None,
    }
}

fn parse_clips(node: &Value) -> Vec<String> {
    let mut clips = Vec::new();
    if let Some(clip) = node.get("clip").and_then(Value::as_str) {
        let clip = clip.trim();
        if !clip.is_empty() {
            clips.push(clip.to_string());
        }
    }
    if let Some(entries) = node.get("clips").and_then(Value::as_array) {
        for entry in entries {
            if let Some(clip) = entry
                .as_str()
                .map(str::trim)
                .filter(|clip| !clip.is_empty())
            {
                clips.push(clip.to_string());
            }
        }
    }
    clips
}

fn parse_sample_speeds(
    node_json: &Value,
    name: &str,
    kind: GraphNodeKind,
    clip_count: usize,
) -> Result<Vec<f32>, String> {
    let Some(entries) = node_json.get("speeds").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if !matches!(
        kind,
        GraphNodeKind::BlendSpace1D | GraphNodeKind::BlendSpace2D
    ) {
        return Err(format!(
            "node '{name}' has sample speeds but is not a blend space"
        ));
    }
    if entries.len() != clip_count {
        return Err(format!(
            "node '{name}' needs one speed per clip ({} clips, {} speeds)",
            clip_count,
            entries.len()
        ));
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let speed = value
                .as_f64()
                .ok_or_else(|| format!("node '{name}' speed #{index} must be a number"))?
                as f32;
            if !speed.is_finite() || speed < 0.0 {
                return Err(format!(
                    "node '{name}' speed #{index} must be finite and >= 0"
                ));
            }
            Ok(speed)
        })
        .collect()
}

fn parse_positions(node: &Value, kind: GraphNodeKind, name: &str) -> Result<Vec<[f32; 2]>, String> {
    let Some(entries) = node.get("positions").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut positions = Vec::with_capacity(entries.len());
    match kind {
        GraphNodeKind::BlendSpace1D => {
            for entry in entries {
                let x = entry
                    .as_f64()
                    .ok_or_else(|| format!("node '{name}' positions must be numbers"))?;
                positions.push([finite_or(x as f32, 0.0), 0.0]);
            }
        }
        GraphNodeKind::BlendSpace2D => {
            for entry in entries {
                let pair = entry
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or_else(|| format!("node '{name}' positions must be [x, y] pairs"))?;
                positions.push([
                    finite_or(pair[0].as_f64().unwrap_or(0.0) as f32, 0.0),
                    finite_or(pair[1].as_f64().unwrap_or(0.0) as f32, 0.0),
                ]);
            }
        }
        _ => {
            return Err(format!(
                "node '{name}' of type '{}' does not take positions",
                kind.label()
            ))
        }
    }
    Ok(positions)
}

fn validate_node_shape(
    name: &str,
    kind: GraphNodeKind,
    clips: &[String],
    positions: &[[f32; 2]],
    parameter_x: &str,
    parameter_y: &str,
    state_machine: &str,
) -> Result<(), String> {
    match kind {
        GraphNodeKind::ClipPlayer => {
            if clips.len() != 1 {
                return Err(format!("clip node '{name}' needs exactly one clip"));
            }
        }
        GraphNodeKind::BlendSpace1D => {
            if clips.len() < 2 || clips.len() != positions.len() {
                return Err(format!(
                    "blend_space_1d node '{name}' needs >= 2 clips and one position per clip"
                ));
            }
            if parameter_x.is_empty() {
                return Err(format!("blend_space_1d node '{name}' needs a `parameter`"));
            }
        }
        GraphNodeKind::BlendSpace2D => {
            if clips.len() < 3 || clips.len() != positions.len() {
                return Err(format!(
                    "blend_space_2d node '{name}' needs >= 3 clips and one [x, y] per clip"
                ));
            }
            if parameter_x.is_empty() || parameter_y.is_empty() {
                return Err(format!(
                    "blend_space_2d node '{name}' needs `parameter_x` and `parameter_y`"
                ));
            }
        }
        GraphNodeKind::StateMachine if state_machine.is_empty() => {
            return Err(format!("state_machine node '{name}' needs a machine path"))
        }
        _ => {}
    }
    Ok(())
}

fn parse_inputs(node: &Value, name: &str) -> Result<Vec<ParsedInput>, String> {
    let mut inputs = Vec::new();
    if let Some(single) = node.get("input").and_then(Value::as_str) {
        let single = single.trim();
        if !single.is_empty() {
            inputs.push(ParsedInput {
                node: single.to_string(),
                weight: 1.0,
                bone_mask: Vec::new(),
            });
        }
    }
    if let Some(entries) = node.get("inputs").and_then(Value::as_array) {
        if entries.len() > MAX_GRAPH_INPUTS {
            return Err(format!("node '{name}' exceeds {MAX_GRAPH_INPUTS} inputs"));
        }
        for entry in entries {
            if let Some(reference) = entry.as_str() {
                let reference = reference.trim();
                if reference.is_empty() {
                    return Err(format!("node '{name}' has an empty input name"));
                }
                inputs.push(ParsedInput {
                    node: reference.to_string(),
                    weight: 1.0,
                    bone_mask: Vec::new(),
                });
            } else {
                let reference = entry
                    .get("node")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reference| !reference.is_empty())
                    .ok_or_else(|| format!("node '{name}' input needs a `node` name"))?;
                let names = entry.get("bone_mask").and_then(Value::as_array);
                if names.is_some_and(|names| names.len() > MAX_GRAPH_BONE_MASK) {
                    return Err(format!(
                        "node '{name}' exceeds {MAX_GRAPH_BONE_MASK} masked bones"
                    ));
                }
                let bone_mask = names
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::trim)
                            .filter(|joint| !joint.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                inputs.push(ParsedInput {
                    node: reference.to_string(),
                    weight: finite_or(
                        entry.get("weight").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                        1.0,
                    ),
                    bone_mask,
                });
            }
        }
    }
    Ok(inputs)
}

fn validate_connections(nodes: &[GraphNodeInfo]) -> Result<(), String> {
    for node in nodes {
        match node.kind {
            GraphNodeKind::Output | GraphNodeKind::Slot | GraphNodeKind::CachedPose => {
                if node.inputs.len() != 1 {
                    return Err(format!(
                        "{} node '{}' needs exactly one input",
                        node.kind.label(),
                        node.name
                    ));
                }
            }
            GraphNodeKind::BlendStack | GraphNodeKind::LayeredBlend if node.inputs.is_empty() => {
                return Err(format!(
                    "{} node '{}' needs at least one input",
                    node.kind.label(),
                    node.name
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn reject_cycles(nodes: &[GraphNodeInfo]) -> Result<(), String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit(
        index: usize,
        nodes: &[GraphNodeInfo],
        marks: &mut [Option<Mark>],
    ) -> Result<(), String> {
        match marks[index] {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                return Err(format!(
                    "animation graph has a cycle at '{}'",
                    nodes[index].name
                ))
            }
            None => {}
        }
        marks[index] = Some(Mark::Visiting);
        for input in &nodes[index].inputs {
            visit(input.node as usize, nodes, marks)?;
        }
        marks[index] = Some(Mark::Done);
        Ok(())
    }
    let mut marks = vec![None; nodes.len()];
    for index in 0..nodes.len() {
        visit(index, nodes, &mut marks)?;
    }
    Ok(())
}

fn write_string(out: &mut Vec<u8>, value: &str) -> Option<()> {
    let bytes = value.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return None;
    }
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    Some(())
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

#[cfg(test)]
#[path = "anim_graph_tests.rs"]
mod tests;
