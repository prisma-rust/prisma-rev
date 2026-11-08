use super::*;

#[test]
fn test_noise_range() {
    // Value noise should stay in [-1, 1].
    for x in 0..10 {
        for y in 0..10 {
            let v = noise_2d([x as f64 * 0.3, y as f64 * 0.3]);
            assert!((-1.0..=1.0).contains(&v), "noise out of range: {v}");
        }
    }
}

#[test]
fn test_fbm_range() {
    let v = fbm_2d([1.5, 2.7], 4, 0.5, 2.0);
    assert!((-2.0..=2.0).contains(&v));
}

#[test]
fn test_ridge_domain_warp_range() {
    let v = ridge_domain_warp([0.5, 1.2], 6, 0.5, 2.0, 2.0);
    assert!(v.is_finite());
}

#[test]
fn test_reproducibility_seed_equivalent() {
    // Same input should give same output.
    let a = noise_2d([1.234, 5.678]);
    let b = noise_2d([1.234, 5.678]);
    assert!((a - b).abs() < 1e-10);
}

#[test]
fn test_noised_derivative_finite() {
    let n = noised_2d([std::f64::consts::PI, 2.71]);
    assert!(n[1].is_finite());
    assert!(n[2].is_finite());
}

#[test]
fn test_generate_parallel_samples_in_pixel_space() {
    // `frequency` is cycles per pixel: sample (x, y) must receive
    // [x * frequency, y * frequency].
    let (width, height, freq) = (8u32, 4u32, 0.5);
    let data = generate_parallel(width, height, freq, |p| p[0] + p[1] * 100.0);
    for y in 0..height {
        for x in 0..width {
            let expected = x as f64 * freq + y as f64 * freq * 100.0;
            let got = data[(y * width + x) as usize] as f64;
            assert!(
                (got - expected).abs() < 1e-4,
                "({x},{y}): got {got}, expected {expected}"
            );
        }
    }
}

#[test]
fn test_default_frequency_crosses_lattice_cells() {
    // Regression for #124: sampling normalized UV * 0.003 never left lattice
    // cells -1/0, so the whole map collapsed into one smooth patch.
    let data = generate_parallel(1024, 1024, 0.003, |p| p[0].floor());
    let mut cells: Vec<i32> = data.iter().map(|v| *v as i32).collect();
    cells.sort_unstable();
    cells.dedup();
    assert!(
        cells.len() >= 4,
        "default frequency only reached {} lattice cells: {cells:?}",
        cells.len()
    );
}
