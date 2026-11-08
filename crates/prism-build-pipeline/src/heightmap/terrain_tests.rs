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
fn terrain_noise_stays_stable_at_large_coordinates() {
    // Regression for the sin-based hash: large world offsets must not collapse
    // neighbouring samples to the same value through f64 precision loss.
    let a = noise_2d([1.0e9, 1.0e9], 7);
    let b = noise_2d([1.0e9 + 0.25, 1.0e9 + 0.75], 7);
    assert!((-1.0..=1.0).contains(&a), "noise out of range: {a}");
    assert!((-1.0..=1.0).contains(&b), "noise out of range: {b}");
    assert!(
        (a - b).abs() > 1e-12,
        "hash collapsed at large coordinates: {a} == {b}"
    );
}
