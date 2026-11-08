use super::*;
use crate::heightmap::{generate_heightmap, HeightmapConfig};
use std::path::Path;

#[test]
fn test_export_png() {
    let cfg = HeightmapConfig {
        width: 16,
        height: 16,
        octaves: 2,
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);
    let tmp = std::env::temp_dir().join("test_heightmap.png");
    export_heightmap(&hm, &tmp, ExportFormat::Png).unwrap();
    assert!(tmp.exists());
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn test_export_exr() {
    let cfg = HeightmapConfig {
        width: 16,
        height: 16,
        octaves: 2,
        ..Default::default()
    };
    let hm = generate_heightmap(&cfg);
    let tmp = std::env::temp_dir().join("test_heightmap.exr");
    export_heightmap(&hm, &tmp, ExportFormat::Exr).unwrap();
    assert!(tmp.exists());
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn test_format_from_extension() {
    assert_eq!(
        format_from_extension(Path::new("out.png")).unwrap(),
        ExportFormat::Png
    );
    assert_eq!(
        format_from_extension(Path::new("out.exr")).unwrap(),
        ExportFormat::Exr
    );
    assert!(format_from_extension(Path::new("out.jpg")).is_err());
}
