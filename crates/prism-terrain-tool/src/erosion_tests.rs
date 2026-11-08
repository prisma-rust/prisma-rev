use super::*;
use crate::heightmap::{generate_heightmap, HeightmapConfig};

fn make_test_heightmap() -> Heightmap {
    let cfg = HeightmapConfig {
        width: 32,
        height: 32,
        octaves: 3,
        ..Default::default()
    };
    generate_heightmap(&cfg)
}

#[test]
fn test_thermal_erosion_runs() {
    let mut hm = make_test_heightmap();
    let thermal = ThermalErosion::default();
    thermal.erode(&mut hm, 10);
    assert_eq!(hm.data.len(), 32 * 32);
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v));
    }
}

#[test]
fn test_thermal_erosion_is_mass_conserving() {
    let mut data = vec![0.0f32; 81];
    data[40] = 1.0; // interior spike
    let mut hm = Heightmap::new(9, 9, data);
    let before: f64 = hm.data.iter().map(|&v| v as f64).sum();

    ThermalErosion::default().erode(&mut hm, 25);

    let after: f64 = hm.data.iter().map(|&v| v as f64).sum();
    assert!(
        (before - after).abs() < 1e-5,
        "thermal erosion destroyed material: {before} -> {after}"
    );
    assert!(hm.get(4, 4) < 1.0, "spike should be eroded");
    assert!(hm.get(3, 4) > 0.0, "neighbour should receive the material");
}

#[test]
fn test_thermal_erosion_acts_on_normalized_terrain_at_defaults() {
    let cfg = HeightmapConfig {
        width: 256,
        height: 256,
        ..Default::default()
    };
    let mut hm = generate_heightmap(&cfg);
    let before = hm.data.clone();

    ThermalErosion::default().erode(&mut hm, 10);

    let changed = hm
        .data
        .iter()
        .zip(before.iter())
        .filter(|(after, before)| (*after - *before).abs() > 1e-6)
        .count();
    assert!(
        changed > 0,
        "default 30° talus must affect a normalized heightmap"
    );
}

#[test]
fn test_hydraulic_erosion_runs() {
    let mut hm = make_test_heightmap();
    let hydraulic = HydraulicErosion {
        particle_count: 500,
        ..Default::default()
    };
    hydraulic.erode(&mut hm, 1);
    assert_eq!(hm.data.len(), 32 * 32);
}

#[test]
fn test_erode_both() {
    let mut hm = make_test_heightmap();
    let thermal = ThermalErosion::default();
    let hydraulic = HydraulicErosion {
        particle_count: 500,
        ..Default::default()
    };
    erode_both(&mut hm, &thermal, &hydraulic, 5, 1);
    for &v in &hm.data {
        assert!((0.0..=1.0).contains(&v));
    }
}
