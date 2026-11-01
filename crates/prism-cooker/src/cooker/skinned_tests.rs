use super::*;
use prism_asset::formats_skinned::{
    decode_ranim, decode_rskel, decode_rskin, AnimTrackInfo, JointInfo, JointTransform,
    QuatKeyInfo, SubmeshInfo, Vec3KeyInfo,
};

fn identity_matrix() -> [f32; 16] {
    let mut matrix = [0.0f32; 16];
    for (index, slot) in matrix.iter_mut().enumerate() {
        if index % 5 == 0 {
            *slot = 1.0;
        }
    }
    matrix
}

fn joint(name: &str, parent: Option<u16>) -> JointInfo {
    JointInfo {
        name: name.to_string(),
        parent,
        reference_local: JointTransform::default(),
        inverse_bind: identity_matrix(),
    }
}

fn two_bone_skeleton() -> SkeletonInfo {
    let mut root = joint("root", None);
    root.reference_local.translation = [0.0, 1.0, 0.0];
    let mut spine = joint("spine", Some(0));
    spine.reference_local.translation = [0.0, 0.5, 0.0];
    spine.reference_local.scale = [2.0, 2.0, 2.0];
    SkeletonInfo {
        joints: vec![root, spine],
    }
}

fn quad_mesh() -> SkinnedMeshInfo {
    SkinnedMeshInfo {
        vertex_count: 4,
        index_count: 6,
        joint_count: 2,
        flags: RSKN_HAS_NORMALS | RSKN_HAS_UV0,
        positions: vec![
            [-0.5, 0.0, -0.5],
            [0.5, 0.0, -0.5],
            [-0.5, 0.0, 0.5],
            [0.5, 0.0, 0.5],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        uv0: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
        tangents: Vec::new(),
        joints: vec![[0, 1, 0, 0]; 4],
        // 故意不归一化：编码时应该被修正。
        weights: vec![[2.0, 2.0, 0.0, 0.0]; 4],
        indices: vec![0, 2, 1, 1, 2, 3],
        submeshes: vec![SubmeshInfo {
            material_index: 0,
            index_offset: 0,
            index_count: 6,
        }],
        bind_pose: JointTransform::default(),
    }
}

fn walk_clip() -> AnimClipInfo {
    AnimClipInfo {
        name: "walk".into(),
        duration: 1.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![AnimTrackInfo {
            joint: 1,
            // 故意乱序：编码器要按时间排好。
            positions: vec![
                Vec3KeyInfo {
                    time: 0.5,
                    value: [0.0, 0.2, 0.0],
                },
                Vec3KeyInfo {
                    time: 0.0,
                    value: [0.0, 0.0, 0.0],
                },
            ],
            rotations: vec![QuatKeyInfo {
                time: 0.0,
                // 未归一化的四元数：解码端会归一化。
                value: [0.0, 0.0, 0.0, 2.0],
            }],
            scales: Vec::new(),
        }],
    }
}

#[test]
fn a_skeleton_round_trips_through_rskel() {
    let skeleton = two_bone_skeleton();
    let bytes = encode_rskel(&skeleton).expect("encode");
    assert_eq!(&bytes[..4], RSKL_MAGIC);
    let decoded = decode_rskel(&bytes).expect("decode");
    assert_eq!(decoded.joint_count(), 2);
    assert_eq!(decoded.joints[0].name, "root");
    assert_eq!(decoded.joints[1].parent, Some(0));
    assert_eq!(decoded.joints[1].reference_local.scale, [2.0, 2.0, 2.0]);
    assert_eq!(decoded.joint_index("spine"), Some(1));
}

#[test]
fn a_skeleton_with_an_out_of_order_parent_is_rejected() {
    let mut skeleton = two_bone_skeleton();
    skeleton.joints[0].parent = Some(1);
    assert!(encode_rskel(&skeleton).is_none(), "父索引必须在自身之前");

    // 解码端也挡：手工把根骨骼的 parent 改成 1。
    let mut bytes = encode_rskel(&two_bone_skeleton()).expect("encode");
    bytes[13..17].copy_from_slice(&1i32.to_le_bytes());
    assert!(decode_rskel(&bytes).is_none());
}

#[test]
fn an_empty_skeleton_cannot_be_encoded() {
    assert!(encode_rskel(&SkeletonInfo { joints: Vec::new() }).is_none());
}

#[test]
fn a_skinned_mesh_round_trips_and_normalises_weights() {
    let mesh = quad_mesh();
    let bytes = encode_rskin(&mesh).expect("encode");
    assert_eq!(&bytes[..4], RSKN_MAGIC);
    let decoded = decode_rskin(&bytes).expect("decode");
    assert_eq!(decoded.vertex_count, 4);
    assert_eq!(decoded.positions[3], [0.5, 0.0, 0.5]);
    assert_eq!(decoded.normals.len(), 4);
    assert!(decoded.has_uv0());
    assert!(!decoded.has_tangents());
    assert_eq!(decoded.indices, vec![0, 2, 1, 1, 2, 3]);
    assert_eq!(decoded.submeshes.len(), 1);
    assert_eq!(decoded.submeshes[0].index_count, 6);
    assert!(decoded.weights_are_normalized(), "编码时归一化");
    assert!((decoded.weights[0][0] - 0.5).abs() < 1e-5);
    assert!(decoded.joint_indices_are_in_range());
}

#[test]
fn zero_weights_fall_back_to_the_first_joint() {
    let mut mesh = quad_mesh();
    mesh.weights = vec![[0.0; 4]; 4];
    let decoded = decode_rskin(&encode_rskin(&mesh).expect("encode")).expect("decode");
    assert_eq!(decoded.joints[0], [0, 0, 0, 0]);
    assert_eq!(decoded.weights[0], [1.0, 0.0, 0.0, 0.0]);
    assert!(decoded.weights_are_normalized());
}

#[test]
fn joint_indices_are_clamped_to_the_skeleton() {
    let mut mesh = quad_mesh();
    mesh.joints = vec![[7, 9, 1, 0]; 4];
    let decoded = decode_rskin(&encode_rskin(&mesh).expect("encode")).expect("decode");
    assert!(decoded.joint_indices_are_in_range());
    assert_eq!(decoded.joints[0][0], 1, "joint_count - 1 = 1");
}

#[test]
fn a_mesh_without_streams_round_trips() {
    let mut mesh = quad_mesh();
    mesh.normals.clear();
    mesh.uv0.clear();
    let decoded = decode_rskin(&encode_rskin(&mesh).expect("encode")).expect("decode");
    assert!(!decoded.has_normals());
    assert!(!decoded.has_uv0());
    assert!(decoded.normals.is_empty());
    assert_eq!(decoded.positions.len(), 4);
}

#[test]
fn a_mesh_whose_streams_disagree_on_length_is_rejected() {
    let mut mesh = quad_mesh();
    mesh.normals.pop();
    assert!(encode_rskin(&mesh).is_none(), "法线流长度必须和顶点数一致");
}

#[test]
fn a_clip_round_trips_with_sorted_keys_and_unit_quaternions() {
    let bytes = encode_ranim(&walk_clip()).expect("encode");
    assert_eq!(&bytes[..4], RANI_MAGIC);
    let decoded = decode_ranim(&bytes).expect("decode");
    assert_eq!(decoded.name, "walk");
    assert!(decoded.looped);
    assert!(!decoded.additive);
    assert_eq!(decoded.fps, 30.0);
    let track = &decoded.tracks[0];
    assert_eq!(track.joint, 1);
    assert_eq!(track.positions.len(), 2);
    assert_eq!(track.positions[0].time, 0.0, "关键帧按时间排序");
    assert_eq!(track.positions[1].value, [0.0, 0.2, 0.0]);
    assert_eq!(track.rotations[0].value, [0.0, 0.0, 0.0, 1.0], "归一化");
    assert_eq!(decoded.clip_duration(), 1.0);
}

#[test]
fn an_empty_clip_cannot_be_encoded() {
    let mut clip = walk_clip();
    clip.tracks.clear();
    assert!(encode_ranim(&clip).is_none());
}

#[test]
fn truncated_buffers_are_rejected_instead_of_panicking() {
    let skeleton = encode_rskel(&two_bone_skeleton()).expect("encode");
    let mesh = encode_rskin(&quad_mesh()).expect("encode");
    let clip = encode_ranim(&walk_clip()).expect("encode");
    for cut in [0usize, 3, 10, 20, 40] {
        if cut < skeleton.len() {
            assert!(decode_rskel(&skeleton[..cut]).is_none(), "RSKL cut {cut}");
        }
        if cut < mesh.len() {
            assert!(decode_rskin(&mesh[..cut]).is_none(), "RSKN cut {cut}");
        }
        if cut < clip.len() {
            assert!(decode_ranim(&clip[..cut]).is_none(), "RANI cut {cut}");
        }
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut skeleton = encode_rskel(&two_bone_skeleton()).expect("encode");
    skeleton.push(0);
    assert!(decode_rskel(&skeleton).is_none());
    let mut clip = encode_ranim(&walk_clip()).expect("encode");
    clip.push(0);
    assert!(decode_ranim(&clip).is_none());
    let mut mesh = encode_rskin(&quad_mesh()).expect("encode");
    mesh.push(0);
    assert!(decode_rskin(&mesh).is_none());
}
