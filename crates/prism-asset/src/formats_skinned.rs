//! 骨骼 / 蒙皮 / 动画剪辑的 cooked 格式解码（#560 P0）。
//!
//! 三种格式都是「magic + version + 定长记录 + 变长块」，所有长度都由计数推算，
//! 不存指针；解码端每个长度都做 checked 运算，坏文件返回 `None`。
//!
//! 编码端在 `prism-cooker`（`cooker/skinned.rs`），运行时只解码 —— 与 RMES 的分层一致。
//!
//! ## RSKL v1（skeleton）
//!
//! ```text
//! [magic:4 "RSKL"][version:1][joint_count:4][name_bytes:4]
//! joints: joint_count × {
//!     parent: i32            // -1 = 根
//!     flags: u8, pad: [u8;3]
//!     reference_local: 10×f32   // translation(3) + rotation xyzw(4) + scale(3)
//!     inverse_bind: 16×f32      // 列主序
//! }
//! names: joint_count × { len: u16, utf8[len] }
//! ```
//!
//! ## RSKN v1（skinned mesh）
//!
//! ```text
//! [magic:4 "RSKN"][version:1][vertex_count:4][index_count:4][joint_count:4]
//! [submesh_count:4][flags:4]
//! positions: v×3 f32
//! normals:   v×3 f32        // flags & RSKN_HAS_NORMALS
//! uv0:       v×2 f32        // flags & RSKN_HAS_UV0
//! tangents:  v×4 f32        // flags & RSKN_HAS_TANGENTS
//! joints:    v×4 u16
//! weights:   v×4 f32
//! indices:   i×u32
//! submeshes: s×{ material_index:u32, index_offset:u32, index_count:u32 }
//! bind_pose: 10×f32
//! ```
//!
//! ## RANI v1（animation clip）
//!
//! ```text
//! [magic:4 "RANI"][version:1][name_len:u16][name][duration:f32][fps:f32][flags:u16][track_count:4]
//! tracks: track_count × {
//!     joint: u32
//!     pos_count: u32,   pos keys:   ×{time:f32, value:[f32;3]}
//!     rot_count: u32,   rot keys:   ×{time:f32, value:[f32;4]}
//!     scale_count: u32, scale keys: ×{time:f32, value:[f32;3]}
//! }
//! ```

/// RSKL magic。
pub const RSKL_MAGIC: &[u8; 4] = b"RSKL";
/// RSKN magic。
pub const RSKN_MAGIC: &[u8; 4] = b"RSKN";
/// RANI magic。
pub const RANI_MAGIC: &[u8; 4] = b"RANI";

/// 三种格式共用的版本号。
pub const SKINNED_FORMAT_VERSION: u8 = 1;

/// 每根骨骼的 cooked 记录大小：parent(4) + flags(1) + pad(3) + local(40) + inverse_bind(64)。
pub const RSKL_JOINT_RECORD_SIZE: usize = 4 + 1 + 3 + 40 + 64;
/// RSKL 固定头大小。
pub const RSKL_HEADER_SIZE: usize = 4 + 1 + 4 + 4;
/// RSKN 固定头大小：magic(4) + version(1) + 5×u32。
pub const RSKN_HEADER_SIZE: usize = 4 + 1 + 4 * 5;
/// RSKN 里 bind pose 的字节数（10 × f32）。
pub const RSKN_BIND_POSE_SIZE: usize = 40;

/// RSKN 标志：带法线流。
pub const RSKN_HAS_NORMALS: u32 = 1 << 0;
/// RSKN 标志：带 uv0 流。
pub const RSKN_HAS_UV0: u32 = 1 << 1;
/// RSKN 标志：带切线流。
pub const RSKN_HAS_TANGENTS: u32 = 1 << 2;

/// RANI 标志：循环播放。
pub const RANI_LOOPED: u16 = 1 << 0;
/// RANI 标志：additive 剪辑。
pub const RANI_ADDITIVE: u16 = 1 << 1;

// ---------------------------------------------------------------------------
// 读取器
// ---------------------------------------------------------------------------

/// 只前进的字节读取器：每次读都做边界检查，越界返回 `None`。
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        if end > self.data.len() {
            return None;
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f32s(&mut self, count: usize) -> Option<Vec<f32>> {
        let bytes = count.checked_mul(4)?;
        let slice = self.take(bytes)?;
        Some(
            slice
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| f32::from_le_bytes(*chunk))
                .collect(),
        )
    }

    fn u16s(&mut self, count: usize) -> Option<Vec<u16>> {
        let bytes = count.checked_mul(2)?;
        let slice = self.take(bytes)?;
        Some(
            slice
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect(),
        )
    }

    fn u32s(&mut self, count: usize) -> Option<Vec<u32>> {
        let bytes = count.checked_mul(4)?;
        let slice = self.take(bytes)?;
        Some(
            slice
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| u32::from_le_bytes(*chunk))
                .collect(),
        )
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
}

// ---------------------------------------------------------------------------
// 骨骼
// ---------------------------------------------------------------------------

/// 骨骼的局部 TRS（translation / rotation 四元数 xyzw / scale）。
///
/// 用独立 struct 而不是 `glam` 类型：`prism-asset` 不依赖数学库。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointTransform {
    pub translation: [f32; 3],
    /// 四元数，`xyzw` 布局。
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Default for JointTransform {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

impl JointTransform {
    /// 打包成 10 个 f32（与 `.rskel` / `.rskin` 的布局一致）。
    pub fn to_array(self) -> [f32; 10] {
        [
            self.translation[0],
            self.translation[1],
            self.translation[2],
            self.rotation[0],
            self.rotation[1],
            self.rotation[2],
            self.rotation[3],
            self.scale[0],
            self.scale[1],
            self.scale[2],
        ]
    }

    /// 从 10 个 f32 读回。
    pub fn from_array(values: [f32; 10]) -> Self {
        Self {
            translation: [values[0], values[1], values[2]],
            rotation: [values[3], values[4], values[5], values[6]],
            scale: [values[7], values[8], values[9]],
        }
    }
}

/// 一根骨骼：父索引（`None` = 根）、reference pose 的局部变换、inverse bind（列主序）。
#[derive(Debug, Clone, PartialEq)]
pub struct JointInfo {
    pub name: String,
    pub parent: Option<u16>,
    pub reference_local: JointTransform,
    pub inverse_bind: [f32; 16],
}

/// 解码后的 `.rskel`。
#[derive(Debug, Clone, PartialEq)]
pub struct SkeletonInfo {
    pub joints: Vec<JointInfo>,
}

impl SkeletonInfo {
    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    /// 按名字找骨骼索引（动画绑定 / 编辑器骨骼树用）。
    pub fn joint_index(&self, name: &str) -> Option<usize> {
        self.joints.iter().position(|joint| joint.name == name)
    }

    /// 父索引必须严格小于自身下标（拓扑序）。解码与 cooker 都拿它挡成环的 skeleton。
    pub fn is_topologically_sorted(&self) -> bool {
        self.joints
            .iter()
            .enumerate()
            .all(|(index, joint)| joint.parent.is_none_or(|parent| (parent as usize) < index))
    }
}

/// 解码 `.rskel`。
pub fn decode_rskel(data: &[u8]) -> Option<SkeletonInfo> {
    if data.len() < RSKL_HEADER_SIZE || &data[..4] != RSKL_MAGIC {
        return None;
    }
    let mut reader = Reader::new(data, 4);
    if reader.u8()? != SKINNED_FORMAT_VERSION {
        return None;
    }
    let joint_count = reader.u32()? as usize;
    let name_bytes = reader.u32()? as usize;
    if joint_count == 0 {
        return None;
    }

    let mut joints = Vec::with_capacity(joint_count.min(4096));
    for _ in 0..joint_count {
        let parent_raw = reader.i32()?;
        let parent = match parent_raw {
            -1 => None,
            value if value >= 0 => Some(u16::try_from(value).ok()?),
            _ => return None,
        };
        let _flags = reader.u8()?;
        reader.take(3)?;
        let local = reader.f32s(10)?;
        let inverse = reader.f32s(16)?;
        let mut inverse_bind = [0.0f32; 16];
        for (slot, value) in inverse_bind.iter_mut().enumerate() {
            *value = inverse[slot];
        }
        joints.push(JointInfo {
            name: String::new(),
            parent,
            reference_local: JointTransform::from_array([
                local[0], local[1], local[2], local[3], local[4], local[5], local[6], local[7],
                local[8], local[9],
            ]),
            inverse_bind,
        });
    }

    if reader.remaining() != name_bytes {
        return None;
    }
    for joint in &mut joints {
        let len = reader.u16()? as usize;
        let bytes = reader.take(len)?;
        joint.name = String::from_utf8(bytes.to_vec()).ok()?;
    }
    if reader.remaining() != 0 {
        return None;
    }

    let skeleton = SkeletonInfo { joints };
    skeleton.is_topologically_sorted().then_some(skeleton)
}

// ---------------------------------------------------------------------------
// 蒙皮网格
// ---------------------------------------------------------------------------

/// 一个 submesh：材质槽 + 索引区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmeshInfo {
    pub material_index: u32,
    pub index_offset: u32,
    pub index_count: u32,
}

/// 解码后的 `.rskin`：每顶点 4 骨影响 + 索引 + submesh + bind pose。
#[derive(Debug, Clone, PartialEq)]
pub struct SkinnedMeshInfo {
    pub vertex_count: u32,
    pub index_count: u32,
    /// 权重索引指向的 skeleton 的骨骼数量（越界校验用）。
    pub joint_count: u32,
    pub flags: u32,
    pub positions: Vec<[f32; 3]>,
    /// 无法线流时为空。
    pub normals: Vec<[f32; 3]>,
    /// 无 uv0 流时为空。
    pub uv0: Vec<[f32; 2]>,
    /// 无切线流时为空。
    pub tangents: Vec<[f32; 4]>,
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
    pub submeshes: Vec<SubmeshInfo>,
    /// 网格自己的 bind pose（与 skeleton 的 reference pose 同布局）。
    pub bind_pose: JointTransform,
}

impl SkinnedMeshInfo {
    pub fn has_normals(&self) -> bool {
        self.flags & RSKN_HAS_NORMALS != 0
    }

    pub fn has_uv0(&self) -> bool {
        self.flags & RSKN_HAS_UV0 != 0
    }

    pub fn has_tangents(&self) -> bool {
        self.flags & RSKN_HAS_TANGENTS != 0
    }

    /// 每个顶点 4 个权重是否都归一化（对 `sum ≈ 1` 的检查，容差 1e-3）。
    pub fn weights_are_normalized(&self) -> bool {
        self.weights
            .iter()
            .all(|weights| (weights.iter().sum::<f32>() - 1.0).abs() <= 1e-3)
    }

    /// 所有 joint 索引是否都在 `joint_count` 范围内。
    pub fn joint_indices_are_in_range(&self) -> bool {
        self.joints.iter().flatten().all(|joint| {
            // 权重为 0 的槽允许任意索引（cooker 会把未用槽写成 0）。
            (*joint as u32) < self.joint_count
        })
    }
}

/// 解码 `.rskin`。
pub fn decode_rskin(data: &[u8]) -> Option<SkinnedMeshInfo> {
    if data.len() < RSKN_HEADER_SIZE + RSKN_BIND_POSE_SIZE || &data[..4] != RSKN_MAGIC {
        return None;
    }
    let mut reader = Reader::new(data, 4);
    if reader.u8()? != SKINNED_FORMAT_VERSION {
        return None;
    }
    let vertex_count = reader.u32()?;
    let index_count = reader.u32()?;
    let joint_count = reader.u32()?;
    let submesh_count = reader.u32()?;
    let flags = reader.u32()?;
    const KNOWN_FLAGS: u32 = RSKN_HAS_NORMALS | RSKN_HAS_UV0 | RSKN_HAS_TANGENTS;
    if flags & !KNOWN_FLAGS != 0 || joint_count == 0 {
        return None;
    }

    let verts = vertex_count as usize;
    let position_values = reader.f32s(verts.checked_mul(3)?)?;
    let positions: Vec<[f32; 3]> = position_values
        .as_chunks::<3>()
        .0
        .iter()
        .map(|chunk| [chunk[0], chunk[1], chunk[2]])
        .collect();

    let normals = if flags & RSKN_HAS_NORMALS != 0 {
        reader
            .f32s(verts.checked_mul(3)?)?
            .as_chunks::<3>()
            .0
            .iter()
            .map(|chunk| [chunk[0], chunk[1], chunk[2]])
            .collect()
    } else {
        Vec::new()
    };
    let uv0 = if flags & RSKN_HAS_UV0 != 0 {
        reader
            .f32s(verts.checked_mul(2)?)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| [chunk[0], chunk[1]])
            .collect()
    } else {
        Vec::new()
    };
    let tangents = if flags & RSKN_HAS_TANGENTS != 0 {
        reader
            .f32s(verts.checked_mul(4)?)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
            .collect()
    } else {
        Vec::new()
    };

    let joint_values = reader.u16s(verts.checked_mul(4)?)?;
    let joints: Vec<[u16; 4]> = joint_values
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
        .collect();
    let weight_values = reader.f32s(verts.checked_mul(4)?)?;
    let weights: Vec<[f32; 4]> = weight_values
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
        .collect();

    let indices = reader.u32s(index_count as usize)?;

    let mut submeshes = Vec::with_capacity((submesh_count as usize).min(256));
    for _ in 0..submesh_count {
        let material_index = reader.u32()?;
        let index_offset = reader.u32()?;
        let index_count = reader.u32()?;
        let end = index_offset.checked_add(index_count)?;
        if end as usize > indices.len() {
            return None;
        }
        submeshes.push(SubmeshInfo {
            material_index,
            index_offset,
            index_count,
        });
    }

    let bind = reader.f32s(10)?;
    let mut bind_pose = [0.0f32; 10];
    bind_pose.copy_from_slice(&bind);
    if reader.remaining() != 0 {
        return None;
    }

    Some(SkinnedMeshInfo {
        vertex_count,
        index_count,
        joint_count,
        flags,
        positions,
        normals,
        uv0,
        tangents,
        joints,
        weights,
        indices,
        submeshes,
        bind_pose: JointTransform::from_array(bind_pose),
    })
}

// ---------------------------------------------------------------------------
// 动画剪辑
// ---------------------------------------------------------------------------

/// 一个 vec3 关键帧。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3KeyInfo {
    pub time: f32,
    pub value: [f32; 3],
}

/// 一个旋转关键帧（四元数 xyzw，解码时归一化并统一到 `w >= 0` 半球）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuatKeyInfo {
    pub time: f32,
    pub value: [f32; 4],
}

/// 单条骨骼轨道。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimTrackInfo {
    pub joint: u32,
    pub positions: Vec<Vec3KeyInfo>,
    pub rotations: Vec<QuatKeyInfo>,
    pub scales: Vec<Vec3KeyInfo>,
}

/// 解码后的 `.ranim`。
#[derive(Debug, Clone, PartialEq)]
pub struct AnimClipInfo {
    pub name: String,
    pub duration: f32,
    pub fps: f32,
    pub looped: bool,
    pub additive: bool,
    pub tracks: Vec<AnimTrackInfo>,
}

impl AnimClipInfo {
    /// 所有轨道里最晚的关键帧时间（忽略非有限时间）。
    pub fn max_key_time(&self) -> f32 {
        let mut max = 0.0f32;
        for track in &self.tracks {
            for key in &track.positions {
                if key.time.is_finite() {
                    max = max.max(key.time);
                }
            }
            for key in &track.rotations {
                if key.time.is_finite() {
                    max = max.max(key.time);
                }
            }
            for key in &track.scales {
                if key.time.is_finite() {
                    max = max.max(key.time);
                }
            }
        }
        max
    }

    /// 实际时长：`duration` 为正就用它，否则取最晚关键帧。
    pub fn clip_duration(&self) -> f32 {
        if self.duration > 0.0 {
            self.duration
        } else {
            self.max_key_time()
        }
    }
}

/// 解码 `.ranim`。
pub fn decode_ranim(data: &[u8]) -> Option<AnimClipInfo> {
    if data.len() < 27 || &data[..4] != RANI_MAGIC {
        return None;
    }
    let mut reader = Reader::new(data, 4);
    if reader.u8()? != SKINNED_FORMAT_VERSION {
        return None;
    }
    let name_len = reader.u16()? as usize;
    let name = String::from_utf8(reader.take(name_len)?.to_vec()).ok()?;
    let duration = reader.f32()?;
    let fps = reader.f32()?;
    let flags = reader.u16()?;
    if flags & !(RANI_LOOPED | RANI_ADDITIVE) != 0 {
        return None;
    }
    let track_count = reader.u32()? as usize;

    let mut tracks = Vec::with_capacity(track_count.min(4096));
    for _ in 0..track_count {
        let joint = reader.u32()?;

        let pos_count = reader.u32()? as usize;
        let mut positions = Vec::with_capacity(pos_count.min(65536));
        for _ in 0..pos_count {
            let time = reader.f32()?;
            let values = reader.f32s(3)?;
            positions.push(Vec3KeyInfo {
                time,
                value: [values[0], values[1], values[2]],
            });
        }

        let rot_count = reader.u32()? as usize;
        let mut rotations = Vec::with_capacity(rot_count.min(65536));
        for _ in 0..rot_count {
            let time = reader.f32()?;
            let values = reader.f32s(4)?;
            let mut value = [values[0], values[1], values[2], values[3]];
            let length = value.iter().map(|v| v * v).sum::<f32>().sqrt();
            if length > 1e-8 && length.is_finite() {
                for component in &mut value {
                    *component /= length;
                }
                if value[3] < 0.0 {
                    for component in &mut value {
                        *component = -*component;
                    }
                }
            } else {
                value = [0.0, 0.0, 0.0, 1.0];
            }
            rotations.push(QuatKeyInfo { time, value });
        }

        let scale_count = reader.u32()? as usize;
        let mut scales = Vec::with_capacity(scale_count.min(65536));
        for _ in 0..scale_count {
            let time = reader.f32()?;
            let values = reader.f32s(3)?;
            scales.push(Vec3KeyInfo {
                time,
                value: [values[0], values[1], values[2]],
            });
        }

        tracks.push(AnimTrackInfo {
            joint,
            positions,
            rotations,
            scales,
        });
    }

    if reader.remaining() != 0 {
        return None;
    }

    Some(AnimClipInfo {
        name,
        duration,
        fps,
        looped: flags & RANI_LOOPED != 0,
        additive: flags & RANI_ADDITIVE != 0,
        tracks,
    })
}

#[cfg(test)]
#[path = "formats_skinned_tests.rs"]
mod tests;
