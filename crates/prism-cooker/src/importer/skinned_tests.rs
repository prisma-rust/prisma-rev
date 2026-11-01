use super::*;

#[test]
fn parents_are_always_placed_before_their_children() {
    // 故意把子节点放在前面（glTF 的 skin.joints 没有顺序保证）：
    // joint2 是根，joint0 挂在 joint2 上，joint1 挂在 joint0 上。
    let parents = [Some(2), Some(0), None];
    let order = topological_order(&parents);
    assert_eq!(order.len(), 3);
    for (slot, index) in order.iter().enumerate() {
        if let Some(parent) = parents[*index] {
            let parent_slot = order.iter().position(|other| *other == parent).unwrap();
            assert!(
                parent_slot < slot,
                "joint {index} (slot {slot}) must come after its parent {parent} (slot {parent_slot})"
            );
        }
    }
}

#[test]
fn a_cycle_does_not_hang_the_ordering() {
    // 成环的骨架是坏数据，但不能让导入卡死。
    let parents = [Some(1), Some(0)];
    let order = topological_order(&parents);
    assert_eq!(order.len(), 2);
}

#[test]
fn a_translation_only_matrix_decomposes_into_a_clean_transform() {
    let matrix: Mat4 = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [2.0, 3.0, -4.0, 1.0],
    ];
    let transform = decompose(&matrix);
    assert_eq!(transform.translation, [2.0, 3.0, -4.0]);
    assert_eq!(transform.scale, [1.0, 1.0, 1.0]);
    assert_eq!(transform.rotation, [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn roughly_ninety_degrees_land_on_the_quaternion() {
    // 绕 +Y 旋转 90°：
    //   [0, 0, -1]
    //   [0, 1,  0]
    //   [1, 0,  0]
    // 列：x = [0,0,1]，y = [0,1,0]，z = [-1,0,0]。
    let matrix: Mat4 = [
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let transform = decompose(&matrix);
    // 把 +X 转到 +Z 是绕 +Y 的 −90°（右手系），所以 y 分量是负的。
    let expected = (
        -std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    );
    assert!(
        (transform.rotation[1] - expected.0).abs() < 1e-4,
        "{:?}",
        transform.rotation
    );
    assert!(
        (transform.rotation[3] - expected.1).abs() < 1e-4,
        "{:?}",
        transform.rotation
    );
    assert!(transform.rotation[0].abs() < 1e-4);
    assert!(transform.rotation[2].abs() < 1e-4);
}

#[test]
fn uniform_scale_survives_decomposition() {
    let matrix: Mat4 = [
        [2.0, 0.0, 0.0, 0.0],
        [0.0, 3.0, 0.0, 0.0],
        [0.0, 0.0, 0.5, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let transform = decompose(&matrix);
    assert_eq!(transform.scale, [2.0, 3.0, 0.5]);
    assert_eq!(transform.rotation, [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn inverting_an_affine_matrix_round_trips() {
    let matrix: Mat4 = [
        [0.0, -1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [5.0, -2.0, 7.0, 1.0],
    ];
    let inverse = invert_affine(&matrix).expect("invertible");
    let product = matrix_mul(&matrix, &inverse);
    for (col, column) in product.iter().enumerate() {
        for (row, value) in column.iter().enumerate() {
            let expected = if col == row { 1.0 } else { 0.0 };
            assert!((value - expected).abs() < 1e-5, "m[{col}][{row}] = {value}");
        }
    }
}

#[test]
fn a_collapsed_matrix_cannot_be_inverted() {
    let matrix: Mat4 = [
        [0.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    assert!(invert_affine(&matrix).is_none());
}

#[test]
fn cubic_spline_outputs_take_the_middle_value_of_each_triple() {
    // 采样器输出是「入切线、值、出切线」，时间轴只有两个采样点。
    let times = [0.0f32, 1.0];
    let values = [
        [0.0f32, 0.0, 0.0],
        [1.0, 2.0, 3.0],
        [9.0, 9.0, 9.0],
        [4.0, 4.0, 4.0],
        [5.0, 6.0, 7.0],
        [8.0, 8.0, 8.0],
    ];
    let keys = keys_for(&times, &values);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].1, [1.0, 2.0, 3.0]);
    assert_eq!(keys[1].1, [5.0, 6.0, 7.0]);
}

#[test]
fn keys_are_sorted_and_non_finite_times_dropped() {
    let times = [1.0f32, f32::NAN, 0.0, f32::INFINITY, 0.5];
    let values = [[1.0f32], [2.0], [3.0], [4.0], [5.0]];
    let keys = keys_for(&times, &values);
    let times: Vec<f32> = keys.iter().map(|key| key.0).collect();
    assert_eq!(times, vec![0.0, 0.5, 1.0]);
}

#[test]
fn inverse_bind_matrices_flatten_in_column_major_order() {
    let matrix: Mat4 = [
        [1.0, 2.0, 3.0, 4.0],
        [5.0, 6.0, 7.0, 8.0],
        [9.0, 10.0, 11.0, 12.0],
        [13.0, 14.0, 15.0, 16.0],
    ];
    assert_eq!(
        flatten_column_major(&matrix),
        [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0]
    );
}
