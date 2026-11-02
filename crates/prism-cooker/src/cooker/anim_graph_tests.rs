use super::{encode_rag, parse_anim_graph_json};
use prism_asset::formats_anim_graph::{decode_rag, GraphNodeKind};
use serde_json::json;

fn document() -> serde_json::Value {
    json!({
        "parameters": [
            { "name": "speed", "type": "float", "default": 0.0 },
            { "name": "aiming", "type": "bool", "default": false }
        ],
        "nodes": [
            {
                "name": "locomotion",
                "type": "blend_space_1d",
                "clips": ["idle.ranim", "run.ranim"],
                "positions": [0.0, 5.0],
                "parameter": "speed"
            },
            {
                "name": "turn",
                "type": "blend_space_2d",
                "clips": ["run_f.ranim", "run_l.ranim", "run_r.ranim"],
                "positions": [[0.0, 0.0], [-1.0, 0.0], [1.0, 0.0]],
                "parameter_x": "speed",
                "parameter_y": "direction"
            },
            {
                "name": "machine",
                "type": "state_machine",
                "state_machine": "hero.rasm"
            },
            {
                "name": "aim",
                "type": "clip_player",
                "clip": "aim.ranim",
                "additive": true,
                "weight": 0.5,
                "bone_mask": ["spine_02", "hand_r"]
            },
            { "name": "slot", "type": "slot", "input": "aim" },
            { "name": "cached", "type": "cached_pose", "input": "slot" },
            {
                "name": "stack",
                "type": "blend_stack",
                "inputs": [
                    "locomotion",
                    { "node": "cached", "weight": 0.5, "bone_mask": ["spine_02"] }
                ]
            },
            {
                "name": "layered",
                "type": "layered_blend",
                "inputs": ["stack", "turn"]
            },
            { "name": "out", "type": "output", "input": "layered" }
        ]
    })
}

#[test]
fn a_graph_round_trips_through_json_and_ragp() {
    let graph = parse_anim_graph_json(&document()).expect("parses");
    assert_eq!(graph.parameters.len(), 2);
    assert_eq!(graph.nodes.len(), 9);
    assert_eq!(
        graph.nodes[graph.output as usize].kind,
        GraphNodeKind::Output
    );
    assert_eq!(graph.nodes[0].positions, [[0.0, 0.0], [5.0, 0.0]]);
    assert_eq!(graph.nodes[5].inputs[0].node, 4); // cached -> slot
    assert_eq!(graph.nodes[6].inputs[1].bone_mask, ["spine_02"]);

    let bytes = encode_rag(&graph).expect("encodes");
    let decoded = decode_rag(&bytes).expect("decodes");
    assert_eq!(decoded, graph);
}

#[test]
fn a_missing_output_is_an_error() {
    let mut document = document();
    document["nodes"].as_array_mut().unwrap().pop();
    let error = parse_anim_graph_json(&document).expect_err("must fail");
    assert!(error.contains("exactly one output"), "{error}");
}

#[test]
fn duplicate_and_unknown_node_names_are_errors() {
    let mut duplicate = document();
    duplicate["nodes"][1]["name"] = json!("locomotion");
    assert!(parse_anim_graph_json(&duplicate)
        .unwrap_err()
        .contains("duplicate node name"));

    let mut unknown = document();
    unknown["nodes"][4]["input"] = json!("does-not-exist");
    assert!(parse_anim_graph_json(&unknown)
        .unwrap_err()
        .contains("unknown node"));
}

#[test]
fn a_cycle_is_rejected() {
    let mut document = document();
    // slot -> cached -> slot
    document["nodes"][3]["input"] = json!("cached");
    let error = parse_anim_graph_json(&document).expect_err("must fail");
    assert!(error.contains("cycle"), "{error}");
}

#[test]
fn blend_space_sample_counts_must_match_the_clips() {
    let mut document_1d = document();
    document_1d["nodes"][0]["positions"] = json!([0.0]);
    let error = parse_anim_graph_json(&document_1d).expect_err("must fail");
    assert!(error.contains("one position per clip"), "{error}");

    let mut document_2d = document();
    document_2d["nodes"][1]["positions"] = json!([[0.0, 0.0]]);
    let error = parse_anim_graph_json(&document_2d).expect_err("must fail");
    assert!(error.contains("one [x, y] per clip"), "{error}");
}

#[test]
fn output_and_state_machine_shapes_are_checked() {
    let mut document_output = document();
    document_output["nodes"][8]["inputs"] = json!(["stack", "turn"]);
    let error = parse_anim_graph_json(&document_output).expect_err("must fail");
    assert!(error.contains("exactly one input"), "{error}");

    let mut document_machine = document();
    document_machine["nodes"][2]
        .as_object_mut()
        .unwrap()
        .remove("state_machine");
    let error = parse_anim_graph_json(&document_machine).expect_err("must fail");
    assert!(error.contains("machine path"), "{error}");
}

#[test]
fn chainable_nodes_need_inputs() {
    let mut document = document();
    document["nodes"][6]["inputs"] = json!([]);
    let error = parse_anim_graph_json(&document).expect_err("must fail");
    assert!(error.contains("at least one input"), "{error}");
}
