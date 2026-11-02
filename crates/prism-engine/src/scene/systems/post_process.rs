//! 局部后处理体积（#502 Phase 2）。
//!
//! 场景里的 [`PostProcessVolume`] 用轴对齐盒圈出一块区域；主相机进入后，盒内
//! 参数按 `priority` 从低到高混合到本帧的后处理参数（[`PostGrade`]）上。
//!
//! - 没有体积时返回调用方给的 `base`（通常是 `RenderSettings::post_grade()`），
//!   所以默认路径与旧画面逐位一致；
//! - 盒子外的 `blend_distance` 内按线性淡出，超出后不参与；
//! - `global = true` 的体积忽略盒子，永远参与；
//! - 只读取实体世界变换的平移 + 轴长（缩放），旋转不参与 —— 与地形高度场一致；
//! - 体积挂在未激活的父级下时不生效，与渲染实体的 `Active` 规则一致。
//!
//! 多相机合成（#461）共用一次后处理，所以这里只按**主相机**（`active_camera`
//! 的位置，即 `SceneChanges::eye`）解析；逐相机后处理需要多份后处理目标，留后续。

use glam::Vec3;
use prism_ecs::World;
use prism_render::PostGrade;

use crate::scene::components::{LocalToWorld, PostProcessVolume};

/// 把 `base` 和所有命中的体积按优先级混合，得到本帧的后处理参数。
pub fn resolve_post_grade(world: &World, eye: Vec3, base: PostGrade) -> PostGrade {
    if !eye.is_finite() {
        return base.sanitized();
    }
    let mut volumes: Vec<(u32, f32, PostProcessVolume, Vec3, Vec3)> = Vec::new();
    for (entity, volume, transform) in world.query2::<PostProcessVolume, LocalToWorld>() {
        if !volume.enabled {
            continue;
        }
        if !crate::scene::systems::hierarchy::is_active_in_hierarchy(world, entity) {
            continue;
        }
        let matrix = transform.0;
        let center = matrix.w_axis.truncate();
        // 轴长当缩放；旋转被刻意忽略（盒子保持世界轴对齐）。
        let scale = Vec3::new(
            matrix.x_axis.truncate().length(),
            matrix.y_axis.truncate().length(),
            matrix.z_axis.truncate().length(),
        );
        let priority = if volume.priority.is_finite() {
            volume.priority
        } else {
            0.0
        };
        volumes.push((entity.id(), priority, volume.clone(), center, scale));
    }
    if volumes.is_empty() {
        return base.sanitized();
    }
    // 稳定顺序：优先级升序，同优先级按实体 id，保证结果可复现。
    volumes.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut grade = base.sanitized();
    for (_, _, volume, center, scale) in volumes {
        let weight = volume_weight(&volume, center, scale, eye);
        if weight <= 0.0 {
            continue;
        }
        grade = lerp_grade(grade, volume_grade(&volume), weight);
    }
    grade
}

/// 体积对相机位置的权重：盒内 1，`blend_distance` 内线性淡出，盒外 0。
fn volume_weight(volume: &PostProcessVolume, center: Vec3, scale: Vec3, point: Vec3) -> f32 {
    if volume.global {
        return 1.0;
    }
    let half = Vec3::from_array(volume.size);
    let half = Vec3::new(
        finite_abs(half.x * scale.x),
        finite_abs(half.y * scale.y),
        finite_abs(half.z * scale.z),
    );
    // 到盒子的有符号距离：所有轴都在盒内时 <= 0。
    let offset = (point - center).abs() - half;
    let outside = offset.max_element();
    if outside <= 0.0 {
        return 1.0;
    }
    let blend = if volume.blend_distance.is_finite() {
        volume.blend_distance.max(0.0)
    } else {
        0.0
    };
    if blend <= 0.0 {
        0.0
    } else {
        (1.0 - outside / blend).clamp(0.0, 1.0)
    }
}

fn finite_abs(value: f32) -> f32 {
    if value.is_finite() {
        value.abs()
    } else {
        0.0
    }
}

/// 组件参数映射成后处理参数（`bloom = false` 时强度归零）。
fn volume_grade(volume: &PostProcessVolume) -> PostGrade {
    PostGrade {
        contrast: volume.contrast,
        saturation: volume.saturation,
        vignette: volume.vignette,
        bloom_intensity: if volume.bloom {
            volume.bloom_intensity
        } else {
            0.0
        },
        bloom_threshold: volume.bloom_threshold,
        chromatic_aberration: volume.chromatic_aberration,
        film_grain: volume.film_grain,
        temperature: volume.temperature,
        tint: volume.tint,
        dof_focus_distance: volume.dof_focus_distance,
        dof_aperture: volume.dof_aperture,
        dof_max_blur: volume.dof_max_blur,
        // LUT 是全局资源；体积只混合上面这些逐像素参数，强度由
        // `render_system` 在解析后统一覆盖为 `settings.lut_strength`。
        lut_strength: 0.0,
    }
    .sanitized()
}

/// 逐字段线性混合 `a → b`，再夹到合法范围。
fn lerp_grade(a: PostGrade, b: PostGrade, t: f32) -> PostGrade {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mix = |from: f32, to: f32| from + (to - from) * t;
    PostGrade {
        contrast: mix(a.contrast, b.contrast),
        saturation: mix(a.saturation, b.saturation),
        vignette: mix(a.vignette, b.vignette),
        bloom_intensity: mix(a.bloom_intensity, b.bloom_intensity),
        bloom_threshold: mix(a.bloom_threshold, b.bloom_threshold),
        chromatic_aberration: mix(a.chromatic_aberration, b.chromatic_aberration),
        film_grain: mix(a.film_grain, b.film_grain),
        temperature: mix(a.temperature, b.temperature),
        tint: mix(a.tint, b.tint),
        dof_focus_distance: mix(a.dof_focus_distance, b.dof_focus_distance),
        dof_aperture: mix(a.dof_aperture, b.dof_aperture),
        dof_max_blur: mix(a.dof_max_blur, b.dof_max_blur),
        lut_strength: 0.0,
    }
    .sanitized()
}

#[cfg(test)]
#[path = "post_process_tests.rs"]
mod tests;
