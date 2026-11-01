//! 已 cook 资源的检查与校验（MCP `asset_inspect` / `asset_validate` 的后端，#560 P0）。
//!
//! UI / Vulkan 无关：只读字节、复用 `prism_asset` 的解码器，产出一份可 JSON 化的报告。
//! 骨骼相关的问题（权重没归一化、joint 越界、轨道指向不存在的骨骼、关键帧乱序）
//! 必须能在这里被检查出来 —— 否则只能靠「看起来没穿模」判断。
//!
//! ```text
//! asset_inspect  → { path, kind, byteSize, summary, problems }
//! asset_validate → { ok, problems }
//! ```

use std::path::Path;

use prism_asset::formats::{decode_rmat, decode_rmes, decode_rtex, MATERIAL_SCALAR_COUNT};
use prism_asset::formats_skinned::{decode_ranim, decode_rskel, decode_rskin};
use serde::Serialize;
use serde_json::{json, Value};

/// 权重和与 1 的容差（与 cooker 的归一化容差一致）。
const WEIGHT_SUM_TOLERANCE: f32 = 1e-3;
/// 骨骼数量上限的提示阈值（超过 GPU 侧常见 palette 预算）。
const JOINT_COUNT_WARN: usize = 512;

/// 一次检查的结果。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetInspection {
    pub path: String,
    /// `rskel` / `rskin` / `ranim` / `rmes` / `rtex` / `rmat` / `unknown`。
    pub kind: String,
    pub byte_size: usize,
    /// 每种格式的摘要（计数、名字、时长……）。
    pub summary: Value,
    /// 校验问题（空 = 通过）。
    pub problems: Vec<String>,
}

impl AssetInspection {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// 按 magic 分派，返回 `(kind, summary, problems)`。
pub fn inspect_bytes(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    match data.get(..4) {
        Some(b"RSKL") => inspect_skeleton(data),
        Some(b"RSKN") => inspect_skinned_mesh(data),
        Some(b"RANI") => inspect_clip(data, None),
        Some(b"RMES") => inspect_static_mesh(data),
        Some(b"RTEX") => inspect_texture(data),
        Some(b"RMAT") => inspect_material(data),
        _ => (
            "unknown",
            json!({ "note": "not a cooked PrismaRev asset (magic not recognised)" }),
            vec!["unknown asset magic".to_string()],
        ),
    }
}

fn inspect_skeleton(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_rskel(data) {
        Some(skeleton) => {
            if !skeleton.is_topologically_sorted() {
                problems.push("joint parents are not in topological order".into());
            }
            if skeleton.joint_count() > JOINT_COUNT_WARN {
                problems.push(format!(
                    "skeleton has {} joints (over the {JOINT_COUNT_WARN} palette budget)",
                    skeleton.joint_count()
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for (index, joint) in skeleton.joints.iter().enumerate() {
                if joint.name.is_empty() {
                    problems.push(format!("joint {index} has an empty name"));
                } else if !seen.insert(joint.name.clone()) {
                    problems.push(format!("duplicate joint name '{}'", joint.name));
                }
                if !joint
                    .reference_local
                    .to_array()
                    .iter()
                    .all(|value| value.is_finite())
                {
                    problems.push(format!("joint {index} reference transform is not finite"));
                }
                if !joint.inverse_bind.iter().all(|value| value.is_finite()) {
                    problems.push(format!("joint {index} inverse bind matrix is not finite"));
                }
            }
            json!({
                "jointCount": skeleton.joint_count(),
                "rootCount": skeleton.joints.iter().filter(|joint| joint.parent.is_none()).count(),
                "names": skeleton.joints.iter().map(|joint| joint.name.as_str()).collect::<Vec<_>>(),
            })
        }
        None => {
            problems.push("invalid RSKL data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("rskel", summary, problems)
}

fn inspect_skinned_mesh(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_rskin(data) {
        Some(mesh) => {
            if !mesh.weights_are_normalized() {
                let bad = mesh
                    .weights
                    .iter()
                    .filter(|weights| {
                        (weights.iter().sum::<f32>() - 1.0).abs() > WEIGHT_SUM_TOLERANCE
                    })
                    .count();
                problems.push(format!("{bad} vertex weight sets do not sum to 1"));
            }
            if !mesh.joint_indices_are_in_range() {
                problems.push(format!(
                    "joint indices exceed the declared joint_count {}",
                    mesh.joint_count
                ));
            }
            if mesh.indices.len() % 3 != 0 {
                problems.push(format!(
                    "index count {} is not a multiple of 3 (not a triangle list)",
                    mesh.indices.len()
                ));
            }
            if mesh.index_count as usize != mesh.indices.len() {
                problems
                    .push("declared index_count does not match the decoded index stream".into());
            }
            if mesh.submeshes.is_empty() && !mesh.indices.is_empty() {
                problems.push("mesh has indices but no submeshes (no material assignment)".into());
            }
            for (index, submesh) in mesh.submeshes.iter().enumerate() {
                let end = submesh.index_offset as usize + submesh.index_count as usize;
                if end > mesh.indices.len() {
                    problems.push(format!(
                        "submesh {index} index range {}..{} escapes {} indices",
                        submesh.index_offset,
                        end,
                        mesh.indices.len()
                    ));
                }
            }
            if !mesh
                .positions
                .iter()
                .flatten()
                .all(|value| value.is_finite())
            {
                problems.push("positions contain non-finite values".into());
            }
            json!({
                "vertexCount": mesh.vertex_count,
                "indexCount": mesh.index_count,
                "jointCount": mesh.joint_count,
                "hasNormals": mesh.has_normals(),
                "hasUv0": mesh.has_uv0(),
                "hasTangents": mesh.has_tangents(),
                "submeshes": mesh.submeshes.len(),
                "materialIndices": mesh.submeshes.iter().map(|s| s.material_index).collect::<Vec<_>>(),
            })
        }
        None => {
            problems.push("invalid RSKN data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("rskin", summary, problems)
}

/// `skeleton_joint_count`：给了就用它检查轨道越界（从 `.rskel` 先 inspect 拿）。
pub fn inspect_clip_with_skeleton(
    data: &[u8],
    skeleton_joint_count: Option<usize>,
) -> (&'static str, Value, Vec<String>) {
    inspect_clip(data, skeleton_joint_count)
}

fn inspect_clip(
    data: &[u8],
    skeleton_joint_count: Option<usize>,
) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_ranim(data) {
        Some(clip) => {
            if clip.tracks.is_empty() {
                problems.push("clip has no tracks".into());
            }
            if !clip.duration.is_finite() || clip.duration < 0.0 {
                problems.push(format!("invalid duration {}", clip.duration));
            }
            if clip.duration > 0.0 {
                let latest = clip.max_key_time();
                if latest > clip.duration + 1e-3 {
                    problems.push(format!(
                        "key at {latest:.3}s is past the declared duration {:.3}s",
                        clip.duration
                    ));
                }
            }
            if let Some(joint_count) = skeleton_joint_count {
                for track in &clip.tracks {
                    if track.joint as usize >= joint_count {
                        problems.push(format!(
                            "track targets joint {} but the skeleton has {joint_count}",
                            track.joint
                        ));
                    }
                }
            }
            for (index, track) in clip.tracks.iter().enumerate() {
                for (label, times) in [
                    (
                        "position",
                        track
                            .positions
                            .iter()
                            .map(|key| key.time)
                            .collect::<Vec<_>>(),
                    ),
                    (
                        "rotation",
                        track
                            .rotations
                            .iter()
                            .map(|key| key.time)
                            .collect::<Vec<_>>(),
                    ),
                    (
                        "scale",
                        track.scales.iter().map(|key| key.time).collect::<Vec<_>>(),
                    ),
                ] {
                    if times.iter().any(|time| !time.is_finite()) {
                        problems.push(format!("track {index} {label} key has a non-finite time"));
                    }
                    if times.windows(2).any(|pair| pair[1] < pair[0]) {
                        problems.push(format!("track {index} {label} keys are not sorted by time"));
                    }
                }
            }
            json!({
                "name": clip.name,
                "duration": clip.clip_duration(),
                "fps": clip.fps,
                "looped": clip.looped,
                "additive": clip.additive,
                "trackCount": clip.tracks.len(),
                "keyCount": clip.tracks.iter().map(|track| {
                    track.positions.len() + track.rotations.len() + track.scales.len()
                }).sum::<usize>(),
                "joints": clip.tracks.iter().map(|track| track.joint).collect::<Vec<_>>(),
            })
        }
        None => {
            problems.push("invalid RANI data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("ranim", summary, problems)
}

fn inspect_static_mesh(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_rmes(data) {
        Some(mesh) => {
            if mesh.idx_count % 3 != 0 {
                problems.push(format!(
                    "index count {} is not a multiple of 3",
                    mesh.idx_count
                ));
            }
            json!({
                "vertexCount": mesh.vert_count,
                "indexCount": mesh.idx_count,
                "uvChannels": mesh.uv_channels,
                "strideBytes": mesh.stride_bytes,
            })
        }
        None => {
            problems.push("invalid RMES data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("rmes", summary, problems)
}

fn inspect_texture(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_rtex(data) {
        Some(texture) => json!({
            "width": texture.width,
            "height": texture.height,
            "mipLevels": texture.mip_levels,
            "format": texture.format,
            "mipBytes": texture.mip_data.iter().map(|mip| mip.len()).collect::<Vec<_>>(),
        }),
        None => {
            problems.push("invalid RTEX data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("rtex", summary, problems)
}

fn inspect_material(data: &[u8]) -> (&'static str, Value, Vec<String>) {
    let mut problems = Vec::new();
    let summary = match decode_rmat(data) {
        Some(material) => {
            if !material.scalars.iter().all(|value| value.is_finite()) {
                problems.push(format!(
                    "material has non-finite scalars (expected {MATERIAL_SCALAR_COUNT})"
                ));
            }
            json!({
                "scalars": material.scalars.to_vec(),
                "textureSlots": material.texture_ids.iter().map(|id| id.is_some()).collect::<Vec<_>>(),
            })
        }
        None => {
            problems.push("invalid RMAT data (bad magic / version / truncated)".into());
            json!({})
        }
    };
    ("rmat", summary, problems)
}

/// 检查一个文件（路径只用于报告与错误信息）。
pub fn inspect_file(path: &Path) -> Result<AssetInspection, String> {
    let data = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (kind, summary, problems) = inspect_bytes(&data);
    Ok(AssetInspection {
        path: path.display().to_string(),
        kind: kind.to_string(),
        byte_size: data.len(),
        summary,
        problems,
    })
}

/// 检查项目里的一个资源（`relative` 相对项目根，例如 `assets/cooked/hero.rskin`）。
pub fn inspect_asset(root: &Path, relative: &str) -> Result<AssetInspection, String> {
    let mut inspection = inspect_file(&root.join(relative))?;
    inspection.path = relative.replace('\\', "/");
    Ok(inspection)
}

/// 只跑校验，返回问题列表（空 = 通过）。
///
/// `skeleton` 给了的话，`.ranim` 会额外检查轨道索引是否超出骨骼数量。
pub fn validate_asset(
    root: &Path,
    relative: &str,
    skeleton: Option<&str>,
) -> Result<Vec<String>, String> {
    let data =
        std::fs::read(root.join(relative)).map_err(|error| format!("{relative}: {error}"))?;
    let (_, _, mut problems) = if relative.ends_with(".ranim") {
        let joint_count = match skeleton {
            Some(path) => match inspect_file(&root.join(path)) {
                Ok(inspection) => inspection
                    .summary
                    .get("jointCount")
                    .and_then(Value::as_u64)
                    .map(|count| count as usize),
                Err(error) => return Err(format!("cannot inspect skeleton '{path}': {error}")),
            },
            None => None,
        };
        inspect_clip(&data, joint_count)
    } else {
        inspect_bytes(&data)
    };
    problems.sort();
    Ok(problems)
}

#[cfg(test)]
#[path = "asset_inspect_tests.rs"]
mod tests;
