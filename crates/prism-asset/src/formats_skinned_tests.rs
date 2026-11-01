//! 解码器的“坏输入不能 panic”测试。
//!
//! 往返（encode → decode）在 `prism-cooker::cooker::skinned` 的测试里做 —— 那里才有编码器。
//! 这里只喂**任意字节**，保证解码路径的每个长度都做了边界检查。

use super::*;

/// 一个最小的 RSKN 头（3 顶点 / 3 索引 / 2 骨骼 / 1 submesh / 无附加流）。
fn minimal_rskin_header() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(RSKN_MAGIC);
    bytes.push(SKINNED_FORMAT_VERSION);
    bytes.extend_from_slice(&3u32.to_le_bytes()); // vertex_count
    bytes.extend_from_slice(&3u32.to_le_bytes()); // index_count
    bytes.extend_from_slice(&2u32.to_le_bytes()); // joint_count
    bytes.extend_from_slice(&1u32.to_le_bytes()); // submesh_count
    bytes.extend_from_slice(&0u32.to_le_bytes()); // flags
    bytes
}

fn minimal_rskin() -> Vec<u8> {
    let mut bytes = minimal_rskin_header();
    for position in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for value in position {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    for _ in 0..3 {
        for joint in [0u16, 1, 0, 0] {
            bytes.extend_from_slice(&joint.to_le_bytes());
        }
    }
    for _ in 0..3 {
        for weight in [0.5f32, 0.5, 0.0, 0.0] {
            bytes.extend_from_slice(&weight.to_le_bytes());
        }
    }
    for index in [0u32, 1, 2] {
        bytes.extend_from_slice(&index.to_le_bytes());
    }
    for value in [0u32, 0, 3] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for value in [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[test]
fn a_hand_built_skinned_mesh_decodes() {
    let mesh = decode_rskin(&minimal_rskin()).expect("decode");
    assert_eq!(mesh.vertex_count, 3);
    assert_eq!(mesh.indices, vec![0, 1, 2]);
    assert_eq!(mesh.submeshes.len(), 1);
    assert!(mesh.weights_are_normalized());
    assert!(mesh.joint_indices_are_in_range());
    assert!(!mesh.has_normals());
}

#[test]
fn every_truncation_of_a_valid_mesh_is_rejected() {
    let bytes = minimal_rskin();
    for cut in 0..bytes.len() {
        assert!(
            decode_rskin(&bytes[..cut]).is_none(),
            "cut at {cut} must be rejected"
        );
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut bytes = minimal_rskin();
    bytes.push(0);
    assert!(decode_rskin(&bytes).is_none());
}

#[test]
fn bad_magic_version_and_flags_are_rejected() {
    let mut bytes = minimal_rskin();
    bytes[..4].copy_from_slice(b"NOPE");
    assert!(decode_rskin(&bytes).is_none());

    let mut bytes = minimal_rskin();
    bytes[4] = 2;
    assert!(decode_rskin(&bytes).is_none(), "未知版本");

    let mut bytes = minimal_rskin();
    bytes[21..25].copy_from_slice(&(1u32 << 7).to_le_bytes());
    assert!(decode_rskin(&bytes).is_none(), "未知标志位");

    let mut bytes = minimal_rskin();
    bytes[13..17].copy_from_slice(&0u32.to_le_bytes());
    assert!(decode_rskin(&bytes).is_none(), "joint_count = 0");

    let mut bytes = minimal_rskin();
    // 子网格索引区间越界。
    let submesh_offset = bytes.len() - 40 - 12;
    bytes[submesh_offset + 8..submesh_offset + 12].copy_from_slice(&99u32.to_le_bytes());
    assert!(decode_rskin(&bytes).is_none());
}

#[test]
fn absurd_counts_do_not_allocate_or_panic() {
    let mut bytes = minimal_rskin_header();
    bytes[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_rskin(&bytes).is_none());
}

#[test]
fn rskel_and_rani_reject_short_or_bad_input() {
    assert!(decode_rskel(b"RSKL").is_none());
    assert!(decode_rskel(b"RSKL\x01").is_none());
    assert!(decode_ranim(b"RANI").is_none());
    assert!(decode_ranim(&[]).is_none());
    // 合法 magic、未知版本。
    assert!(decode_rskel(b"RSKL\x09\x00\x00\x00\x00\x00\x00\x00\x00\x00").is_none());
}
