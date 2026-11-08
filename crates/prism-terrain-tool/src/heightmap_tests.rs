use super::*;

#[test]
fn test_generate_small() {
    let cfg = HeightmapConfig {
        width: 64,
        height: 64,
        octaves: 4,
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);
    assert_eq!(hm.data.len(), 64 * 64);
    // All values should be in [0, 1].
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v), "value {v} out of [0,1]");
    }
}

#[test]
fn test_generate_ridge() {
    let cfg = HeightmapConfig {
        width: 64,
        height: 64,
        octaves: 4,
        ridge: true,
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);
    assert_eq!(hm.data.len(), 64 * 64);
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v));
    }
}

#[test]
fn test_gradient_finite() {
    let cfg = HeightmapConfig {
        width: 32,
        height: 32,
        octaves: 3,
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);
    let (gx, gy) = hm.gradient(16, 16);
    assert!(gx.is_finite());
    assert!(gy.is_finite());
}

#[test]
fn test_default_frequency_produces_detail() {
    // Regression for #124: with the default frequency the map used to be a
    // single bilinear ramp (at most a couple of extrema per row).
    let cfg = HeightmapConfig {
        width: 512,
        height: 512,
        frequency: 0.003 * 2.0, // same features-per-pixel as the default on a 1024 px map
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);

    let y = cfg.height / 2;
    let row: Vec<f32> = (0..cfg.width).map(|x| hm.get(x, y)).collect();
    let extrema = row
        .windows(3)
        .filter(|w| (w[1] > w[0] && w[1] > w[2]) || (w[1] < w[0] && w[1] < w[2]))
        .count();
    assert!(
        extrema > 32,
        "default frequency collapsed into a smooth ramp ({extrema} extrema)"
    );
}

#[test]
fn test_cliff_enhancement() {
    let mut cfg = HeightmapConfig {
        width: 64,
        height: 64,
        octaves: 3,
        cliff: true,
        ..Default::default()
    };
    // Lower cliff into range so it's visible.
    cfg.cliff_center = 0.5;
    cfg.cliff_width = 0.1;
    cfg.cliff_amount = 0.3;
    let hm = generate_heightmap(&cfg);
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v));
    }
}
