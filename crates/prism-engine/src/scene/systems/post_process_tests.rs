use super::*;
use prism_ecs::World;
use prism_render::PostGrade;

use crate::scene::components::{Active, Hidden};

fn base() -> PostGrade {
    PostGrade {
        contrast: 1.0,
        saturation: 1.0,
        ..PostGrade::default()
    }
}

struct Volume {
    entity: prism_ecs::Entity,
}

fn spawn_volume(
    world: &mut World,
    transform: glam::Mat4,
    configure: impl FnOnce(&mut PostProcessVolume),
) -> Volume {
    let entity = world.spawn();
    world.insert(entity, LocalToWorld(transform));
    let mut volume = PostProcessVolume::default();
    configure(&mut volume);
    world.insert(entity, volume);
    Volume { entity }
}

#[test]
fn no_volumes_returns_the_base_grade_unchanged() {
    let world = World::new();
    let grade = resolve_post_grade(&world, Vec3::ZERO, base());
    assert_eq!(grade, base());
}

#[test]
fn a_global_volume_overrides_the_base_parameters() {
    let mut world = World::new();
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.contrast = 1.5;
        volume.saturation = 0.25;
        volume.vignette = 0.75;
        volume.bloom = true;
        volume.bloom_intensity = 0.5;
        volume.dof_focus_distance = 4.0;
        volume.dof_aperture = 0.5;
        volume.dof_max_blur = 12.0;
    });
    let grade = resolve_post_grade(&world, Vec3::splat(999.0), base());
    assert!((grade.contrast - 1.5).abs() < 1e-6);
    assert!((grade.saturation - 0.25).abs() < 1e-6);
    assert!((grade.vignette - 0.75).abs() < 1e-6);
    assert!((grade.bloom_intensity - 0.5).abs() < 1e-6);
    assert!((grade.dof_focus_distance - 4.0).abs() < 1e-6);
    assert!((grade.dof_aperture - 0.5).abs() < 1e-6);
    assert!((grade.dof_max_blur - 12.0).abs() < 1e-6);
}

#[test]
fn a_camera_inside_the_box_gets_the_full_volume() {
    let mut world = World::new();
    spawn_volume(
        &mut world,
        glam::Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)),
        |volume| {
            volume.size = [5.0, 2.0, 5.0];
            volume.contrast = 2.0;
            volume.blend_distance = 0.0;
        },
    );
    // 盒内（中心附近）。
    let inside = resolve_post_grade(&world, Vec3::new(12.0, 1.0, 0.0), base());
    assert!((inside.contrast - 2.0).abs() < 1e-6);
    // 盒外且 blend_distance = 0 → 完全不参与。
    let outside = resolve_post_grade(&world, Vec3::new(16.0, 0.0, 0.0), base());
    assert!((outside.contrast - 1.0).abs() < 1e-6);
}

#[test]
fn blend_distance_fades_out_linearly() {
    let mut world = World::new();
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.size = [1.0, 1.0, 1.0];
        volume.blend_distance = 4.0;
        volume.contrast = 3.0;
    });
    // 盒外 2 米 = 淡出一半 → contrast 1 + (3 - 1) * 0.5 = 2。
    let half = resolve_post_grade(&world, Vec3::new(3.0, 0.0, 0.0), base());
    assert!((half.contrast - 2.0).abs() < 1e-6, "got {}", half.contrast);
    // 超过 4 米 → 完全退出。
    let far = resolve_post_grade(&world, Vec3::new(6.0, 0.0, 0.0), base());
    assert!((far.contrast - 1.0).abs() < 1e-6);
}

#[test]
fn higher_priority_volumes_are_applied_last() {
    let mut world = World::new();
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.priority = 10.0;
        volume.contrast = 3.0;
    });
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.priority = 20.0;
        volume.contrast = 0.5;
    });
    let grade = resolve_post_grade(&world, Vec3::ZERO, base());
    assert!(
        (grade.contrast - 0.5).abs() < 1e-6,
        "got {}",
        grade.contrast
    );
}

#[test]
fn a_disabled_or_hidden_volume_is_ignored() {
    let mut world = World::new();
    let volume = spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.enabled = false;
        volume.contrast = 4.0;
    });
    let grade = resolve_post_grade(&world, Vec3::ZERO, base());
    assert!((grade.contrast - 1.0).abs() < 1e-6);

    world
        .get_mut::<PostProcessVolume>(volume.entity)
        .unwrap()
        .enabled = true;
    world.insert(volume.entity, Hidden(true));
    let hidden = resolve_post_grade(&world, Vec3::ZERO, base());
    assert!((hidden.contrast - 1.0).abs() < 1e-6);

    world.insert(volume.entity, Active(true));
    world.remove::<Hidden>(volume.entity);
    let active = resolve_post_grade(&world, Vec3::ZERO, base());
    assert!((active.contrast - 4.0).abs() < 1e-6);
}

#[test]
fn the_box_is_scaled_by_the_entity_transform() {
    let mut world = World::new();
    spawn_volume(
        &mut world,
        glam::Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            glam::Quat::IDENTITY,
            Vec3::ZERO,
        ),
        |volume| {
            volume.size = [1.0, 1.0, 1.0];
            volume.blend_distance = 0.0;
            volume.contrast = 2.0;
        },
    );
    // 盒半尺寸 1 * 缩放 2 = 2；x = 1.5 在盒内。
    let inside = resolve_post_grade(&world, Vec3::new(1.5, 0.0, 0.0), base());
    assert!((inside.contrast - 2.0).abs() < 1e-6);
    // x = 2.5 在盒外。
    let outside = resolve_post_grade(&world, Vec3::new(2.5, 0.0, 0.0), base());
    assert!((outside.contrast - 1.0).abs() < 1e-6);
}

#[test]
fn non_finite_parameters_are_sanitized() {
    let mut world = World::new();
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.priority = f32::NAN;
        volume.size = [f32::NAN, 1.0, 1.0];
        volume.blend_distance = f32::NAN;
        volume.contrast = f32::NAN;
        volume.vignette = 9.0;
    });
    let grade = resolve_post_grade(&world, Vec3::ZERO, base());
    // 非有限值在 PostGrade::sanitized 里回落到默认。
    assert!((grade.contrast - 1.0).abs() < 1e-6);
    assert!((grade.vignette - 1.0).abs() < 1e-6);
}

#[test]
fn a_non_finite_camera_position_is_rejected() {
    let mut world = World::new();
    spawn_volume(&mut world, glam::Mat4::IDENTITY, |volume| {
        volume.global = true;
        volume.contrast = 2.0;
    });
    let grade = resolve_post_grade(&world, Vec3::splat(f32::NAN), base());
    assert!((grade.contrast - 1.0).abs() < 1e-6);
}
