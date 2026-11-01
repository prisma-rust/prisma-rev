use super::*;

/// 两根骨骼：root 在原点，child 相对 root +2m（x）。
pub fn two_bone_skeleton() -> Skeleton {
    let root_bind = Mat4::IDENTITY;
    let child_world = Mat4::from_translation(Vec3::new(2.0, 0.0, 0.0));
    Skeleton::new(vec![
        Joint {
            name: "root".into(),
            parent: None,
            reference_local: JointTransform::IDENTITY,
            inverse_bind: root_bind.inverse(),
        },
        Joint {
            name: "child".into(),
            parent: Some(0),
            reference_local: JointTransform::new(
                Vec3::new(2.0, 0.0, 0.0),
                Quat::IDENTITY,
                Vec3::ONE,
            ),
            inverse_bind: child_world.inverse(),
        },
    ])
    .expect("valid hierarchy")
}

#[test]
fn a_hierarchy_needs_its_parents_first() {
    let child_first = Skeleton::new(vec![
        Joint {
            name: "child".into(),
            parent: Some(1),
            reference_local: JointTransform::IDENTITY,
            inverse_bind: Mat4::IDENTITY,
        },
        Joint {
            name: "root".into(),
            parent: None,
            reference_local: JointTransform::IDENTITY,
            inverse_bind: Mat4::IDENTITY,
        },
    ]);
    assert!(child_first.is_none(), "父在子后面时必须拒绝");

    let empty = Skeleton::new(Vec::new());
    assert!(empty.is_none());
}

#[test]
fn the_reference_pose_reproduces_the_bind_pose() {
    let skeleton = two_bone_skeleton();
    let pose = skeleton.rest_pose();
    let world = skeleton.world_matrices(&pose);
    assert_eq!(world[0], Mat4::IDENTITY);
    assert!(
        (world[1].w_axis.x - 2.0).abs() < 1e-5,
        "child sits 2m along x"
    );

    // bind 矩阵的逆刚好把世界矩阵抵消：reference 姿态下 palette 是单位矩阵。
    let palette = skeleton.bone_palette(&pose);
    assert!(palette[0].abs_diff_eq(Mat4::IDENTITY, 1e-5));
    assert!(palette[1].abs_diff_eq(Mat4::IDENTITY, 1e-5));
}

#[test]
fn rotating_the_root_moves_the_child_with_it() {
    let skeleton = two_bone_skeleton();
    let mut pose = skeleton.rest_pose();
    // 父骨骼绕 Y 转 90°：子骨骼的世界位置从 +X 转到 -Z（右手、+Y up）。
    pose.set_local(
        0,
        JointTransform::new(
            Vec3::ZERO,
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::ONE,
        ),
    );
    let world = skeleton.world_matrices(&pose);
    let child = world[1].w_axis.truncate();
    assert!((child.z + 2.0).abs() < 1e-5, "child at {child:?}");
    assert!(child.x.abs() < 1e-5, "child at {child:?}");
}

#[test]
fn the_palette_tracks_the_pose_offset() {
    let skeleton = two_bone_skeleton();
    let mut pose = skeleton.rest_pose();
    pose.set_local(
        0,
        JointTransform::new(Vec3::new(0.0, 5.0, 0.0), Quat::IDENTITY, Vec3::ONE),
    );
    let palette = skeleton.bone_palette(&pose);
    // 顶点在 bind 姿态原点，现在应该被抬到 y=5。
    let moved = palette[0].transform_point3(Vec3::ZERO);
    assert!((moved.y - 5.0).abs() < 1e-5, "got {moved:?}");
    // 子骨骼也跟着平移 5m。
    let child = palette[1].transform_point3(Vec3::new(2.0, 0.0, 0.0));
    assert!((child.y - 5.0).abs() < 1e-5, "got {child:?}");
}

#[test]
fn joint_lookup_is_by_name() {
    let skeleton = two_bone_skeleton();
    assert_eq!(skeleton.joint_index("child"), Some(1));
    assert_eq!(skeleton.joint_index("nope"), None);
    assert_eq!(skeleton.joint_count(), 2);
}

#[test]
fn transforms_round_trip_through_the_cooked_layout() {
    let transform = JointTransform::new(
        Vec3::new(1.0, 2.0, 3.0),
        Quat::from_rotation_z(0.5),
        Vec3::new(2.0, 2.0, 2.0),
    );
    let round_trip = JointTransform::from_array(transform.to_array());
    assert!((round_trip.translation - transform.translation).length() < 1e-6);
    assert!(round_trip.rotation.abs_diff_eq(transform.rotation, 1e-6));
    assert!((round_trip.scale - transform.scale).length() < 1e-6);

    // 矩阵往返。
    let matrix = JointTransform::from_matrix(transform.matrix());
    assert!((matrix.translation - transform.translation).length() < 1e-5);
    assert!((matrix.scale - transform.scale).length() < 1e-5);
}

#[test]
fn lerp_goes_halfway_on_every_channel() {
    let a = JointTransform::IDENTITY;
    let b = JointTransform::new(
        Vec3::new(2.0, 0.0, 0.0),
        Quat::from_rotation_y(std::f32::consts::PI),
        Vec3::new(3.0, 3.0, 3.0),
    );
    let half = a.lerp(b, 0.5);
    assert!((half.translation.x - 1.0).abs() < 1e-6);
    assert!((half.scale.x - 2.0).abs() < 1e-6);
    // 180° 的一半是 90°。
    let rotated = half.rotation * Vec3::X;
    assert!(rotated.z.abs() > 0.99, "got {rotated:?}");
}
