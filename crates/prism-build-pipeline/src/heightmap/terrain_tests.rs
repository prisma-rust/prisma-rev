// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

#[test]
fn hash_is_portable_integer_mix() {
    // Lattice coordinates and the seed are the only inputs; integer mixing
    // must reproduce the same bits on every platform (a sine hash did not).
    assert_eq!(hash2([0.0, 0.0], 0), 0.138_709_410_145_554_27);
    assert_eq!(hash2([3.0, -2.0], 42), 0.319_982_729_224_393_5);
    assert_eq!(hash2([3.75, -1.25], 42), hash2([3.0, -2.0], 42));
}

#[test]
fn hash_stays_in_range_for_large_coordinates() {
    for p in [[-1e9, 1e9], [1e12, -1e12], [-0.5, -0.5]] {
        let v = hash2(p, 7);
        assert!((0.0..1.0).contains(&v), "hash2({p:?}) = {v}");
    }
}

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
