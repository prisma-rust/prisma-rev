//! 骨骼 / 蒙皮 / 动画剪辑的 cooked 格式**编码器**（#560 P0）。
//!
//! 解码端在 `prism_asset::formats_skinned`（运行时只解码）—— 与 RMES 的分层一致。
//! 字节布局写在那里模块文档里，两份实现必须同步改。
//!
//! cooker 这一侧多做两件运行时不做的事：
//! - 拒绝父索引不指向更早骨骼（成环 / 乱序）的 skeleton；
//! - 把权重归一化、把未使用的 joint 槽写成 0，保证 GPU 侧直接可用。

use prism_asset::formats_skinned::{
    AnimClipInfo, SkeletonInfo, SkinnedMeshInfo, RANI_ADDITIVE, RANI_LOOPED, RANI_MAGIC,
    RSKL_JOINT_RECORD_SIZE, RSKL_MAGIC, RSKN_BIND_POSE_SIZE, RSKN_HAS_NORMALS, RSKN_HAS_TANGENTS,
    RSKN_HAS_UV0, RSKN_HEADER_SIZE, RSKN_MAGIC, SKINNED_FORMAT_VERSION,
};

/// 编码 `.rskel`。`skeleton` 必须是拓扑序（父索引 < 自身下标），否则返回 `None`。
pub fn encode_rskel(skeleton: &SkeletonInfo) -> Option<Vec<u8>> {
    if skeleton.joints.is_empty() || !skeleton.is_topologically_sorted() {
        return None;
    }
    let mut names = Vec::new();
    for joint in &skeleton.joints {
        let bytes = joint.name.as_bytes();
        if bytes.len() > u16::MAX as usize {
            return None;
        }
        names.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        names.extend_from_slice(bytes);
    }

    let mut out = Vec::with_capacity(13 + skeleton.joints.len() * RSKL_JOINT_RECORD_SIZE);
    out.extend_from_slice(RSKL_MAGIC);
    out.push(SKINNED_FORMAT_VERSION);
    out.extend_from_slice(&(skeleton.joints.len() as u32).to_le_bytes());
    out.extend_from_slice(&(names.len() as u32).to_le_bytes());

    for joint in &skeleton.joints {
        let parent = joint.parent.map_or(-1i32, i32::from);
        out.extend_from_slice(&parent.to_le_bytes());
        out.push(0); // flags
        out.extend_from_slice(&[0, 0, 0]); // pad
        for value in joint.reference_local.to_array() {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in joint.inverse_bind {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(&names);
    Some(out)
}

/// 把权重归一化：和为 0 时退化成「全部挂在 joint 0 上」，否则按和缩放。
fn normalize_weights(weights: [f32; 4], joints: &mut [u16; 4]) -> [f32; 4] {
    let cleaned = weights.map(|weight| {
        if weight.is_finite() {
            weight.max(0.0)
        } else {
            0.0
        }
    });
    let sum: f32 = cleaned.iter().sum();
    if sum <= 1e-6 {
        *joints = [0, 0, 0, 0];
        return [1.0, 0.0, 0.0, 0.0];
    }
    cleaned.map(|weight| weight / sum)
}

/// 编码 `.rskin`。权重会归一化，joint 索引会按 `joint_count` 夹紧。
pub fn encode_rskin(mesh: &SkinnedMeshInfo) -> Option<Vec<u8>> {
    let verts = mesh.positions.len();
    if verts == 0 || mesh.joints.len() != verts || mesh.weights.len() != verts {
        return None;
    }
    if mesh.joint_count == 0 {
        return None;
    }
    if !mesh.normals.is_empty() && mesh.normals.len() != verts {
        return None;
    }
    if !mesh.uv0.is_empty() && mesh.uv0.len() != verts {
        return None;
    }
    if !mesh.tangents.is_empty() && mesh.tangents.len() != verts {
        return None;
    }

    let mut flags = 0u32;
    if !mesh.normals.is_empty() {
        flags |= RSKN_HAS_NORMALS;
    }
    if !mesh.uv0.is_empty() {
        flags |= RSKN_HAS_UV0;
    }
    if !mesh.tangents.is_empty() {
        flags |= RSKN_HAS_TANGENTS;
    }

    let stream_floats = verts
        * (3 + if flags & RSKN_HAS_NORMALS != 0 { 3 } else { 0 }
            + if flags & RSKN_HAS_UV0 != 0 { 2 } else { 0 }
            + if flags & RSKN_HAS_TANGENTS != 0 { 4 } else { 0 });
    let mut out = Vec::with_capacity(
        RSKN_HEADER_SIZE
            + stream_floats * 4
            + verts * 12
            + mesh.indices.len() * 4
            + mesh.submeshes.len() * 12
            + RSKN_BIND_POSE_SIZE,
    );

    out.extend_from_slice(RSKN_MAGIC);
    out.push(SKINNED_FORMAT_VERSION);
    out.extend_from_slice(&(verts as u32).to_le_bytes());
    out.extend_from_slice(&(mesh.indices.len() as u32).to_le_bytes());
    out.extend_from_slice(&mesh.joint_count.to_le_bytes());
    out.extend_from_slice(&(mesh.submeshes.len() as u32).to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());

    for position in &mesh.positions {
        for value in position {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    if flags & RSKN_HAS_NORMALS != 0 {
        for normal in &mesh.normals {
            for value in normal {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    if flags & RSKN_HAS_UV0 != 0 {
        for uv in &mesh.uv0 {
            for value in uv {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    if flags & RSKN_HAS_TANGENTS != 0 {
        for tangent in &mesh.tangents {
            for value in tangent {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    // joint 索引与权重是两个独立的流（先全部 joints，再全部 weights）——解码端按流读。
    let mut joint_stream = Vec::with_capacity(verts * 8);
    let mut weight_stream = Vec::with_capacity(verts * 16);
    for index in 0..verts {
        let mut joints = mesh.joints[index];
        for joint in &mut joints {
            *joint = (*joint).min((mesh.joint_count - 1) as u16);
        }
        let weights = normalize_weights(mesh.weights[index], &mut joints);
        for joint in joints {
            joint_stream.extend_from_slice(&joint.to_le_bytes());
        }
        for weight in weights {
            weight_stream.extend_from_slice(&weight.to_le_bytes());
        }
    }
    out.extend_from_slice(&joint_stream);
    out.extend_from_slice(&weight_stream);

    for index in &mesh.indices {
        out.extend_from_slice(&index.to_le_bytes());
    }
    for submesh in &mesh.submeshes {
        out.extend_from_slice(&submesh.material_index.to_le_bytes());
        out.extend_from_slice(&submesh.index_offset.to_le_bytes());
        out.extend_from_slice(&submesh.index_count.to_le_bytes());
    }
    for value in mesh.bind_pose.to_array() {
        out.extend_from_slice(&value.to_le_bytes());
    }

    Some(out)
}

/// 编码 `.ranim`。关键帧按时间升序写出（同刻保留原始顺序）。
pub fn encode_ranim(clip: &AnimClipInfo) -> Option<Vec<u8>> {
    let name = clip.name.as_bytes();
    if name.len() > u16::MAX as usize || clip.tracks.is_empty() {
        return None;
    }
    let mut flags = 0u16;
    if clip.looped {
        flags |= RANI_LOOPED;
    }
    if clip.additive {
        flags |= RANI_ADDITIVE;
    }

    let mut out = Vec::new();
    out.extend_from_slice(RANI_MAGIC);
    out.push(SKINNED_FORMAT_VERSION);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&clip.duration.to_le_bytes());
    out.extend_from_slice(&clip.fps.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(clip.tracks.len() as u32).to_le_bytes());

    for track in &clip.tracks {
        out.extend_from_slice(&track.joint.to_le_bytes());

        let mut positions = track.positions.clone();
        positions.sort_by(|a, b| a.time.total_cmp(&b.time));
        out.extend_from_slice(&(positions.len() as u32).to_le_bytes());
        for key in &positions {
            out.extend_from_slice(&key.time.to_le_bytes());
            for value in key.value {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }

        let mut rotations = track.rotations.clone();
        rotations.sort_by(|a, b| a.time.total_cmp(&b.time));
        out.extend_from_slice(&(rotations.len() as u32).to_le_bytes());
        for key in &rotations {
            out.extend_from_slice(&key.time.to_le_bytes());
            for value in key.value {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }

        let mut scales = track.scales.clone();
        scales.sort_by(|a, b| a.time.total_cmp(&b.time));
        out.extend_from_slice(&(scales.len() as u32).to_le_bytes());
        for key in &scales {
            out.extend_from_slice(&key.time.to_le_bytes());
            for value in key.value {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    Some(out)
}

#[cfg(test)]
#[path = "skinned_tests.rs"]
mod tests;
