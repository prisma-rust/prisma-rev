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
    // Pin the integer hash so nobody reintroduces a libm path: the old
    // `sin(dot)` hash differed between Windows, glibc and bionic, which made
    // the same seed generate different terrain per platform.
    assert_eq!(hash2([3, -7], 42), 0.224_541_710_977_518_85);
}
