// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

#[test]
fn terrain_bounds_and_size() {
    let hm = generate_terrain(128, 96, 42, 128.0);
    assert_eq!(hm.width, 128);
    assert_eq!(hm.height, 96);
    assert_eq!(hm.data.len(), 128 * 96);
    assert!((hm.min_height - 0.0).abs() < 1e-9);
    assert!((hm.max_height - 1.0).abs() < 1e-9);
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v));
    }
}

#[test]
fn terrain_seed_variation() {
    let a = generate_terrain(64, 64, 1, 64.0);
    let b = generate_terrain(64, 64, 2, 64.0);
    let same = a
        .data
        .iter()
        .zip(b.data.iter())
        .filter(|(x, y)| (**x - **y).abs() < 1e-6)
        .count();
    // 不同 seed 必须产生明显不同的地形（容差：至少 90% 像元不同）。
    assert!(
        same < a.data.len() / 10,
        "seeds produced nearly identical terrain"
    );
}

#[test]
fn terrain_deterministic() {
    let a = generate_terrain(64, 64, 7, 64.0);
    let b = generate_terrain(64, 64, 7, 64.0);
    assert_eq!(a.data, b.data);
}

#[test]
fn hash_is_platform_independent() {
    // The lattice hash used to be `sin(dot) * 43758.5453`, which depends on the
    // platform libm. Pin an exact value so a switch back to that trick fails.
    let v = hash2([3.0, -7.0], 42);
    assert_eq!(v, 0.295_792_354_259_441_06);
}
