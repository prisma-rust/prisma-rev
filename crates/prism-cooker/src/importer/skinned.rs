//! glTF 蒙皮骨骼 + 动画导入（#560 P0-1b）。
//!
//! 这里只做「源文件 → 中间结构」：把 glTF 的 `skins` / `JOINTS_0` / `WEIGHTS_0` /
//! `animations` 变成 [`prism_asset::formats_skinned`] 里的 `SkeletonInfo` /
//! `SkinnedMeshInfo` / `AnimClipInfo`，再由 [`GltfSideAssets`] 里的调用方编码成
//! `.rskel` / `.rskin` / `.ranim`。
//!
//! 三个约定（都写进了 PR 说明）：
//!
//! 1. **不转换坐标**：glTF 本身就是右手 +Y up，与引擎一致；UE 导出的 cm / Z-up
//!    在导出时就要换算，不在导入器里猜。
//! 2. **骨骼顺序必须是拓扑序**（父在子前）。glTF 的 `skin.joints` 没有这个保证，
//!    这里统一重排，并把每个 primitive 的 `JOINTS_0` 一起重映射。
//! 3. **`reference_local` 由 inverse bind 矩阵推出来**：`inverse_bind = inverse(世界绑定姿态)`，
//!    所以参考姿态不必再遍历节点树，也不会和文件里的 bind pose 不一致。
use std::collections::{BTreeMap, HashMap};

use prism_asset::formats_skinned::{
    AnimClipInfo, AnimTrackInfo, JointInfo, JointTransform, QuatKeyInfo, SkeletonInfo,
    SkinnedMeshInfo, SubmeshInfo as SkinnedSubmeshInfo, Vec3KeyInfo,
};

use glam::{Mat3, Mat4, Quat, Vec3};

/// 一根骨骼在 glTF 里的来源（重映射用）。
#[derive(Debug, Clone)]
struct JointSource {
    node: usize,
    inverse_bind: Mat4,
}

/// 解析好的骨骼 + 每个 skin 的「本地索引 → 全局索引」表。
#[derive(Debug, Clone)]
pub struct GltfSkinning {
    /// 全局骨架（已按拓扑序排列）。
    pub skeleton: SkeletonInfo,
    /// `skin 索引` → 该 skin 的 `joints[i]` 在全局骨架里的下标。
    pub remaps: HashMap<usize, Vec<u16>>,
}

impl Default for GltfSkinning {
    fn default() -> Self {
        Self {
            skeleton: SkeletonInfo { joints: Vec::new() },
            remaps: HashMap::new(),
        }
    }
}

impl GltfSkinning {
    pub fn is_empty(&self) -> bool {
        self.skeleton.joints.is_empty()
    }

    pub fn joint_count(&self) -> usize {
        self.skeleton.joint_count()
    }

    /// 某个 skin 的本地 joint 索引 → 全局骨骼下标。
    pub fn remap(&self, skin: usize) -> Option<&[u16]> {
        self.remaps.get(&skin).map(Vec::as_slice)
    }
}

/// 一次导出的蒙皮结果：骨架 + 每个 skin 的重映射 + 所有动画剪辑。
#[derive(Debug, Clone, Default)]
pub struct GltfSkinResult {
    pub skinning: GltfSkinning,
    pub clips: Vec<AnimClipInfo>,
}

/// 解析 `document.skins()` + `document.animations()`。
pub fn read_skinning(document: &gltf::Document, buffers: &[gltf::buffer::Data]) -> GltfSkinResult {
    let mut joints: Vec<JointSource> = Vec::new();
    // 节点索引 → 全局骨骼下标（同一个节点被多个 skin 引用时只保留一份）。
    let mut by_node: HashMap<usize, u16> = HashMap::new();
    let mut remaps: HashMap<usize, Vec<u16>> = HashMap::new();

    for (skin_index, skin) in document.skins().enumerate() {
        let reader = skin.reader(|buffer| Some(&buffers[buffer.index()]));
        let inverse_binds: Vec<Mat4> = reader
            .read_inverse_bind_matrices()
            .map(|iter| iter.map(|m| Mat4::from_cols_array_2d(&m)).collect())
            .unwrap_or_default();

        let mut local_remap = Vec::new();
        for (local, node) in skin.joints().enumerate() {
            let node_index = node.index();
            let inverse_bind = inverse_binds.get(local).copied().unwrap_or(IDENTITY_4X4);
            let global = *by_node.entry(node_index).or_insert_with(|| {
                let index = joints.len() as u16;
                joints.push(JointSource {
                    node: node_index,
                    inverse_bind,
                });
                index
            });
            local_remap.push(global);
        }
        remaps.insert(skin_index, local_remap);
    }

    if joints.is_empty() {
        return GltfSkinResult::default();
    }

    // 节点索引 → glTF 节点 + 父节点表（gltf 1.4 没有 `Node::parent()`，自己走一遍树）。
    let nodes: Vec<gltf::Node<'_>> = document.nodes().collect();
    let position_of_node: HashMap<usize, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.index(), position))
        .collect();
    let parent_of_node = node_parents(document, &nodes);
    let is_joint: HashMap<usize, ()> = joints.iter().map(|joint| (joint.node, ())).collect();

    // 每根骨骼的父骨骼 = 父链上最近的那个 joint 节点。
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(joints.len());
    for joint in &joints {
        let mut parent = None;
        let mut cursor = parent_of_node.get(&joint.node).copied();
        while let Some(node) = cursor {
            if is_joint.contains_key(&node) {
                parent = by_node.get(&node).map(|index| *index as usize);
                break;
            }
            cursor = parent_of_node.get(&node).copied();
        }
        parents.push(parent);
    }

    // 拓扑序：父必须在子之前。环/断链的骨骼直接落到根上（glTF 里不该出现，但不能挂死）。
    let order = topological_order(&parents);

    // 重排后：全局下标变化，重映射表也要跟着换。
    let mut new_index: Vec<u16> = vec![0; joints.len()];
    for (new_slot, old_index) in order.iter().enumerate() {
        new_index[*old_index] = new_slot as u16;
    }
    let remaps = remaps
        .into_iter()
        .map(|(skin, local)| {
            (
                skin,
                local
                    .into_iter()
                    .map(|global| new_index[global as usize])
                    .collect(),
            )
        })
        .collect();

    // 参考姿态：inverse_bind 反过来就是绑定姿态的世界矩阵。
    let mut world: Vec<Option<Mat4>> = Vec::with_capacity(order.len());
    for old_index in &order {
        world.push(invert_affine(&joints[*old_index].inverse_bind));
    }

    let mut sorted: Vec<JointInfo> = Vec::with_capacity(order.len());
    for (slot, old_index) in order.iter().enumerate() {
        let joint = &joints[*old_index];
        let parent = parents[*old_index].map(|parent| new_index[parent]);
        let reference_world = world[slot];
        let local = match (reference_world, parent) {
            (Some(child), Some(parent)) => match world[parent as usize] {
                Some(parent_world) => invert_affine(&parent_world)
                    .map(|inverse| inverse * child)
                    .unwrap_or(IDENTITY_4X4),
                None => child,
            },
            (Some(child), None) => child,
            _ => IDENTITY_4X4,
        };
        let name = nodes
            .get(*position_of_node.get(&joint.node).unwrap_or(&0))
            .and_then(|node| node.name().map(str::to_string))
            .unwrap_or_else(|| format!("joint_{slot}"));
        sorted.push(JointInfo {
            name,
            parent,
            reference_local: decompose(&local),
            inverse_bind: flatten_column_major(&joint.inverse_bind),
        });
    }

    let skeleton = SkeletonInfo { joints: sorted };
    let clips = read_animations(document, buffers, &by_node, &new_index);

    GltfSkinResult {
        skinning: GltfSkinning { skeleton, remaps },
        clips,
    }
}

// ---------------------------------------------------------------------------
// 动画
// ---------------------------------------------------------------------------

fn read_animations(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    by_node: &HashMap<usize, u16>,
    new_index: &[u16],
) -> Vec<AnimClipInfo> {
    use gltf::animation::util::ReadOutputs;

    let joint_index = |node: usize| -> Option<u16> {
        let global = by_node.get(&node)?;
        new_index.get(*global as usize).copied()
    };

    let mut clips = Vec::new();
    for (clip_index, animation) in document.animations().enumerate() {
        // node 索引 → 轨道（同一节点可能有多条 channel，按属性合并进同一条轨道）。
        let mut tracks: BTreeMap<u16, AnimTrackInfo> = BTreeMap::new();
        let mut duration = 0.0f32;
        let mut skipped_nodes = 0usize;
        let mut morph_channels = 0usize;

        for channel in animation.channels() {
            let Some(joint) = joint_index(channel.target().node().index()) else {
                skipped_nodes += 1;
                continue;
            };
            let reader = channel.reader(|buffer| Some(&buffers[buffer.index()]));
            let Some(inputs) = reader.read_inputs() else {
                continue;
            };
            let times: Vec<f32> = inputs.collect();
            let Some(outputs) = reader.read_outputs() else {
                continue;
            };
            duration = duration.max(
                times
                    .iter()
                    .copied()
                    .filter(|t| t.is_finite())
                    .fold(0.0, f32::max),
            );

            let track = tracks.entry(joint).or_insert_with(|| AnimTrackInfo {
                joint: u32::from(joint),
                positions: Vec::new(),
                rotations: Vec::new(),
                scales: Vec::new(),
            });
            match outputs {
                ReadOutputs::Translations(values) => {
                    track.positions = collect_vec3_keys(&times, values);
                }
                ReadOutputs::Scales(values) => {
                    track.scales = collect_vec3_keys(&times, values);
                }
                ReadOutputs::Rotations(values) => {
                    track.rotations = collect_quat_keys(&times, values.into_f32());
                }
                ReadOutputs::MorphTargetWeights(_) => {
                    morph_channels += 1;
                }
            }
        }

        if morph_channels > 0 {
            tracing::debug!(
                "glTF animation {clip_index}: {morph_channels} morph-weight channel(s) ignored (no morph targets yet)"
            );
        }
        if skipped_nodes > 0 {
            tracing::debug!(
                "glTF animation {clip_index}: skipped {skipped_nodes} channel(s) that do not target a skeleton joint"
            );
        }

        let tracks: Vec<AnimTrackInfo> = tracks.into_values().collect();
        if tracks.is_empty() {
            continue;
        }

        let name = animation
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("clip_{clip_index}"));
        let key_time = tracks
            .iter()
            .flat_map(|track| {
                track
                    .positions
                    .iter()
                    .map(|key| key.time)
                    .chain(track.rotations.iter().map(|key| key.time))
                    .chain(track.scales.iter().map(|key| key.time))
            })
            .filter(|time| time.is_finite())
            .fold(0.0f32, f32::max);
        let duration = duration.max(key_time);
        clips.push(AnimClipInfo {
            name,
            duration,
            // glTF 不带帧率（关键帧是时间戳），这里给 30 当作编辑器默认预览帧率。
            fps: 30.0,
            looped: false,
            additive: false,
            tracks,
            // glTF 动画没有事件通道（#560 P1 Notify）：Notify 由作者在引擎侧补，
            // 导出的剪辑事件表为空。
            notifies: Vec::new(),
            sync_markers: Vec::new(),
            sections: Vec::new(),
            warp_windows: Vec::new(),
        });
    }
    clips
}

fn collect_vec3_keys(times: &[f32], values: impl Iterator<Item = [f32; 3]>) -> Vec<Vec3KeyInfo> {
    let values: Vec<[f32; 3]> = values.collect();
    keys_for(times, &values)
        .into_iter()
        .map(|(time, value)| Vec3KeyInfo { time, value })
        .collect()
}

fn collect_quat_keys(times: &[f32], values: impl Iterator<Item = [f32; 4]>) -> Vec<QuatKeyInfo> {
    let values: Vec<[f32; 4]> = values.collect();
    keys_for(times, &values)
        .into_iter()
        .map(|(time, value)| QuatKeyInfo { time, value })
        .collect()
}

/// 时间与数值一一配对。
///
/// CubicSpline 采样器的输出是「入切线 + 值 + 出切线」三元组，这里取中间那份值
/// （曲线细节留给将来的重采样，不能直接把三元组按顺序当关键帧）。
fn keys_for<T: Copy>(times: &[f32], values: &[T]) -> Vec<(f32, T)> {
    let stride = if times.is_empty() || values.len() == times.len() {
        1
    } else if values.len() == times.len() * 3 {
        3
    } else {
        1
    };
    let mut keys = Vec::with_capacity(times.len());
    for (index, time) in times.iter().enumerate() {
        if !time.is_finite() {
            continue;
        }
        let value = match values.get(index * stride + (stride - 1) / 2) {
            Some(value) => *value,
            None => break,
        };
        keys.push((*time, value));
    }
    keys.sort_by(|a, b| a.0.total_cmp(&b.0));
    keys.dedup_by(|a, b| (a.0 - b.0).abs() <= f32::EPSILON);
    keys
}

// ---------------------------------------------------------------------------
// 蒙皮网格
// ---------------------------------------------------------------------------

/// 把导入器的顶点数据 + 每顶点骨骼影响打包成 `.rskin` 的中间结构。
///
/// 权重会在这里归一化（glTF 允许不精确），全零的顶点退化成「全部挂在 joint 0」。
pub(super) fn skinned_mesh_info(
    mesh: &super::MeshData,
    joint_count_u32: u32,
) -> Option<SkinnedMeshInfo> {
    if joint_count_u32 == 0 || mesh.joints.len() != mesh.positions.len() {
        return None;
    }
    if mesh.weights.len() != mesh.positions.len() {
        return None;
    }
    let mut weights = mesh.weights.clone();
    for weight in &mut weights {
        let sum: f32 = weight.iter().sum();
        if !sum.is_finite() || sum <= 1e-6 {
            *weight = [1.0, 0.0, 0.0, 0.0];
        } else {
            for value in weight.iter_mut() {
                *value /= sum;
            }
        }
    }

    let flags = (if mesh.all_normals {
        prism_asset::formats_skinned::RSKN_HAS_NORMALS
    } else {
        0
    }) | (if mesh.all_uv0 {
        prism_asset::formats_skinned::RSKN_HAS_UV0
    } else {
        0
    });

    let submeshes = mesh
        .submeshes
        .iter()
        .map(|submesh| SkinnedSubmeshInfo {
            material_index: submesh.material_index.unwrap_or(u32::MAX as usize) as u32,
            index_offset: submesh.index_start,
            index_count: submesh.index_count,
        })
        .collect();

    Some(SkinnedMeshInfo {
        vertex_count: mesh.positions.len() as u32,
        index_count: mesh.indices.len() as u32,
        joint_count: joint_count_u32,
        flags,
        positions: mesh.positions.clone(),
        normals: if mesh.all_normals {
            mesh.normals.clone()
        } else {
            Vec::new()
        },
        uv0: if mesh.all_uv0 {
            mesh.uv0.clone()
        } else {
            Vec::new()
        },
        tangents: Vec::new(),
        joints: mesh.joints.clone(),
        weights,
        indices: mesh.indices.clone(),
        submeshes,
        // glTF 的顶点已经在 bind pose 上，网格自己不需要再摆一次。
        bind_pose: JointTransform::default(),
    })
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

const IDENTITY_4X4: Mat4 = Mat4::IDENTITY;

/// 父在子前的顺序；成环的骨骼会被追加到末尾（不会挂死）。
fn topological_order(parents: &[Option<usize>]) -> Vec<usize> {
    let mut order = Vec::with_capacity(parents.len());
    let mut placed = vec![false; parents.len()];
    let mut progress = true;
    while progress && order.len() < parents.len() {
        progress = false;
        for (index, parent) in parents.iter().enumerate() {
            if placed[index] {
                continue;
            }
            let ready = match parent {
                None => true,
                Some(parent) => placed[*parent],
            };
            if ready {
                placed[index] = true;
                order.push(index);
                progress = true;
            }
        }
    }
    for (index, _) in parents.iter().enumerate() {
        if !placed[index] {
            order.push(index);
        }
    }
    order
}

/// 节点索引 → 父节点索引。从每个 scene 的根开始走，没被任何 scene 引用的节点
/// 当成根（与 `read_gltf` 的遍历口径一致）。
fn node_parents(document: &gltf::Document, nodes: &[gltf::Node<'_>]) -> HashMap<usize, usize> {
    fn walk(node: gltf::Node<'_>, parents: &mut HashMap<usize, usize>) {
        for child in node.children() {
            parents.insert(child.index(), node.index());
            walk(child, parents);
        }
    }

    let mut parents = HashMap::new();
    if let Some(scene) = document.default_scene() {
        for node in scene.nodes() {
            walk(node, &mut parents);
        }
    } else {
        let mut children = std::collections::HashSet::new();
        for node in nodes {
            for child in node.children() {
                children.insert(child.index());
            }
        }
        for node in nodes {
            if !children.contains(&node.index()) {
                walk(node.clone(), &mut parents);
            }
        }
    }
    parents
}

/// 列主序 4×4 → 16 个 f32（`.rskel` 里 `inverse_bind` 的布局）。
fn flatten_column_major(m: &Mat4) -> [f32; 16] {
    m.to_cols_array()
}

/// 仿射 4×4 求逆（列主序）。退化矩阵（行列式接近 0）返回 `None`。
fn invert_affine(m: &Mat4) -> Option<Mat4> {
    let determinant = Mat3::from_mat4(*m).determinant();
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return None;
    }
    Some(m.inverse())
}

/// 把 4×4 仿射矩阵拆成 T/R/S（`JointTransform` 的表达能力）。
fn decompose(m: &Mat4) -> JointTransform {
    let x = m.x_axis.truncate();
    let y = m.y_axis.truncate();
    let z = m.z_axis.truncate();
    let scale = [x.length(), y.length(), z.length()];
    let normalize = |v: Vec3, fallback: Vec3| {
        if v.length() > 1e-8 {
            v.normalize()
        } else {
            fallback
        }
    };
    let basis = Mat3::from_cols(
        normalize(x, Vec3::X),
        normalize(y, Vec3::Y),
        normalize(z, Vec3::Z),
    );
    JointTransform {
        translation: m.w_axis.truncate().to_array(),
        rotation: quat_from_basis(basis),
        scale,
    }
}

/// 3×3 旋转矩阵 → 四元数（`xyzw`）。
fn quat_from_basis(basis: Mat3) -> [f32; 4] {
    let quat = Quat::from_mat3(&basis);
    let length = quat.length();
    if length > 1e-8 {
        // A quaternion and its negation describe the same rotation. Pinning
        // the sign (`w >= 0`) keeps the baked data stable instead of letting
        // the extraction branch decide it.
        let signed = if quat.w < 0.0 { -quat } else { quat };
        (signed / length).to_array()
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

#[cfg(test)]
#[path = "skinned_tests.rs"]
mod tests;
