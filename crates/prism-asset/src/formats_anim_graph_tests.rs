use super::*;

fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// 两个节点：ClipPlayer → Output。
fn minimal_graph() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RAGP_MAGIC);
    out.push(RAGP_VERSION);
    out.extend_from_slice(&0u32.to_le_bytes()); // parameters

    out.extend_from_slice(&2u32.to_le_bytes()); // nodes
    push_string(&mut out, "clip");
    out.push(GraphNodeKind::ClipPlayer.to_u8());
    out.push(RAGP_NODE_LOOPED);
    out.extend_from_slice(&1.0f32.to_le_bytes()); // speed
    out.extend_from_slice(&1.0f32.to_le_bytes()); // weight
    out.extend_from_slice(&1u32.to_le_bytes()); // clips
    push_string(&mut out, "idle.ranim");
    out.extend_from_slice(&0u32.to_le_bytes()); // positions
    push_string(&mut out, "");
    push_string(&mut out, "");
    push_string(&mut out, "");
    out.extend_from_slice(&0u32.to_le_bytes()); // inputs

    push_string(&mut out, "out");
    out.push(GraphNodeKind::Output.to_u8());
    out.push(0);
    out.extend_from_slice(&1.0f32.to_le_bytes());
    out.extend_from_slice(&1.0f32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    push_string(&mut out, "");
    push_string(&mut out, "");
    push_string(&mut out, "");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // input node
    out.extend_from_slice(&1.0f32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // mask

    out.extend_from_slice(&1u32.to_le_bytes()); // output
    out
}

#[test]
fn a_minimal_graph_decodes() {
    let graph = decode_rag(&minimal_graph()).expect("valid RAGP");
    assert_eq!(graph.output, 1);
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.nodes[0].kind, GraphNodeKind::ClipPlayer);
    assert!(graph.nodes[0].looped);
    assert_eq!(graph.nodes[0].clips, ["idle.ranim"]);
    assert_eq!(graph.nodes[1].inputs[0].node, 0);
}

#[test]
fn malformed_headers_and_truncation_are_rejected() {
    assert!(decode_rag(&[]).is_none());
    assert!(decode_rag(b"RAGP").is_none());
    let mut bad_magic = minimal_graph();
    bad_magic[0] = b'X';
    assert!(decode_rag(&bad_magic).is_none());
    let mut bad_version = minimal_graph();
    bad_version[4] = RAGP_VERSION + 1;
    assert!(decode_rag(&bad_version).is_none());
    let good = minimal_graph();
    assert!(decode_rag(&good[..good.len() - 1]).is_none());
}

#[test]
fn trailing_bytes_and_bad_indices_are_rejected() {
    let mut trailing = minimal_graph();
    trailing.push(0);
    assert!(decode_rag(&trailing).is_none());

    let mut bad_output = minimal_graph();
    let output_offset = bad_output.len() - 4;
    bad_output[output_offset..].copy_from_slice(&99u32.to_le_bytes());
    assert!(decode_rag(&bad_output).is_none());

    // node 0 的 kind 字节：header(9) + name_len(2) + name(4)。
    let mut bad_kind = minimal_graph();
    bad_kind[15] = 0xFF;
    assert!(decode_rag(&bad_kind).is_none());
}

#[test]
fn unknown_flag_bits_are_rejected() {
    let mut flags = minimal_graph();
    flags[16] = 0b1000_0000; // ClipPlayer flags（kind 后一字节）
    assert!(decode_rag(&flags).is_none());
}
