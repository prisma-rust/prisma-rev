use super::*;
use prism_asset::formats_skinned::{AnimClipInfo, SkeletonInfo, SkinnedMeshInfo};
use prism_asset::formats_skinned::{
    AnimTrackInfo, JointInfo, JointTransform, QuatKeyInfo, SubmeshInfo, Vec3KeyInfo,
};
use prism_cooker::cooker::skinned::{encode_ranim, encode_rskel, encode_rskin};

fn identity() -> [f32; 16] {
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
        inverse_bind: identity(),
    }
}

fn skeleton() -> SkeletonInfo {
    SkeletonInfo {
        joints: vec![joint("root", None), joint("spine", Some(0))],
    }
}

fn skinned_mesh() -> SkinnedMeshInfo {
    SkinnedMeshInfo {
        vertex_count: 3,
        index_count: 3,
        joint_count: 2,
        flags: 0,
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: Vec::new(),
        uv0: Vec::new(),
        tangents: Vec::new(),
        joints: vec![[0, 1, 0, 0]; 3],
        weights: vec![[0.5, 0.5, 0.0, 0.0]; 3],
        indices: vec![0, 1, 2],
        submeshes: vec![SubmeshInfo {
            material_index: 0,
            index_offset: 0,
            index_count: 3,
        }],
        bind_pose: JointTransform::default(),
    }
}

fn clip() -> AnimClipInfo {
    AnimClipInfo {
        name: "idle".into(),
        duration: 2.0,
        fps: 30.0,
        looped: true,
        additive: false,
        tracks: vec![AnimTrackInfo {
            joint: 1,
            positions: vec![Vec3KeyInfo {
                time: 0.0,
                value: [0.0, 0.0, 0.0],
            }],
            rotations: vec![QuatKeyInfo {
                time: 0.0,
                value: [0.0, 0.0, 0.0, 1.0],
            }],
            scales: Vec::new(),
        }],
    }
}

#[test]
fn a_valid_skeleton_reports_no_problems() {
    let bytes = encode_rskel(&skeleton()).expect("encode");
    let (kind, summary, problems) = inspect_bytes(&bytes);
    assert_eq!(kind, "rskel");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(summary["jointCount"], 2);
    assert_eq!(summary["names"][1], "spine");
    assert_eq!(summary["rootCount"], 1);
}

#[test]
fn a_valid_skinned_mesh_reports_counts_and_no_problems() {
    let bytes = encode_rskin(&skinned_mesh()).expect("encode");
    let (kind, summary, problems) = inspect_bytes(&bytes);
    assert_eq!(kind, "rskin");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(summary["vertexCount"], 3);
    assert_eq!(summary["jointCount"], 2);
    assert_eq!(summary["submeshes"], 1);
}

#[test]
fn a_valid_clip_reports_its_tracks() {
    let bytes = encode_ranim(&clip()).expect("encode");
    let (kind, summary, problems) = inspect_bytes(&bytes);
    assert_eq!(kind, "ranim");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(summary["name"], "idle");
    assert_eq!(summary["trackCount"], 1);
    assert_eq!(summary["keyCount"], 2);
}

#[test]
fn a_clip_track_past_the_skeleton_is_reported() {
    let bytes = encode_ranim(&clip()).expect("encode");
    // 只声明 1 根骨骼，轨道却指向 joint 1。
    let (_, _, problems) = inspect_clip_with_skeleton(&bytes, Some(1));
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].contains("track targets joint 1"),
        "{problems:?}"
    );
    // 骨骼够多时没有问题。
    assert!(inspect_clip_with_skeleton(&bytes, Some(2)).2.is_empty());
}

#[test]
fn a_short_mesh_with_broken_weights_is_flagged() {
    // 权重和 0.8：cooker 会归一化，但手工构造的坏文件必须被检查出来。
    let mut mesh = skinned_mesh();
    mesh.weights = vec![[0.4, 0.4, 0.0, 0.0]; 3];
    // 直接绕过 encoder 的归一化：先编码合法的，再把权重流改坏。
    let mut bytes = encode_rskin(&skinned_mesh()).expect("encode");
    // 权重流从 joint 流之后开始：header(25) + positions(3*12) + joints(3*8)。
    let weight_start = 25 + 3 * 12 + 3 * 8;
    for vertex in 0..3 {
        let offset = weight_start + vertex * 16;
        bytes[offset..offset + 4].copy_from_slice(&0.4f32.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&0.4f32.to_le_bytes());
    }
    let (kind, _, problems) = inspect_bytes(&bytes);
    assert_eq!(kind, "rskin");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("do not sum to 1")),
        "{problems:?}"
    );
    let _ = mesh; // 只用于说明原值
}

#[test]
fn an_index_count_that_is_not_a_triangle_list_is_flagged() {
    let mut mesh = skinned_mesh();
    mesh.indices = vec![0, 1, 2, 0];
    mesh.index_count = 4;
    mesh.submeshes[0].index_count = 4;
    let bytes = encode_rskin(&mesh).expect("encode");
    let (_, _, problems) = inspect_bytes(&bytes);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("multiple of 3")),
        "{problems:?}"
    );
}

#[test]
fn unknown_magic_is_reported_not_panicking() {
    let (kind, _, problems) = inspect_bytes(b"nope");
    assert_eq!(kind, "unknown");
    assert_eq!(problems.len(), 1);
    // 空文件同样安全。
    assert_eq!(inspect_bytes(b"").0, "unknown");
    // 只有 magic、没有内容的文件也不 panic。
    let (kind, _, problems) = inspect_bytes(b"RSKN");
    assert_eq!(kind, "rskin");
    assert!(!problems.is_empty());
}

#[test]
fn inspect_and_validate_work_on_a_file() {
    let root = std::env::temp_dir().join(format!("prism-asset-inspect-{}", std::process::id()));
    let assets = root.join("assets/cooked");
    std::fs::create_dir_all(&assets).expect("create temp dir");
    let skeleton_path = assets.join("hero.rskel");
    let mesh_path = assets.join("hero.rskin");
    std::fs::write(&skeleton_path, encode_rskel(&skeleton()).expect("encode")).expect("write");
    std::fs::write(&mesh_path, encode_rskin(&skinned_mesh()).expect("encode")).expect("write");

    let inspection = inspect_asset(&root, "assets/cooked/hero.rskin").expect("inspect");
    assert_eq!(inspection.path, "assets/cooked/hero.rskin");
    assert_eq!(inspection.kind, "rskin");
    assert!(inspection.is_ok());
    assert!(inspection.byte_size > 0);

    assert!(validate_asset(&root, "assets/cooked/hero.rskin", None)
        .expect("validate")
        .is_empty());

    let missing = inspect_asset(&root, "assets/cooked/nope.rskin");
    assert!(missing.is_err());

    std::fs::remove_dir_all(&root).ok();
}
