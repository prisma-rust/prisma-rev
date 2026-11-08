//! BRDF 校准球——一排参考材质，用于目测 PBR 管线是否正确。
//!
//! 启动时沿 +X 轴放置六个球体，以便将每个经典 PBR 参考材质
//! 与其预期外观（白色/金色/铝/塑料/石头/黑色）并排比较。
//! 这些球共享同一个 UV 球体网格，仅材质参数不同，
//! 因此它们之间的任何视觉差异归因于 BRDF，而非几何体。
//!
//! 预期结果（在正确的 BRDF + 线性 HDR 管线下）：
//!   white - 平坦中灰色，无过曝高光，柔和镜面反射
//!   black - 非常暗，仅有紧凑的镜面高光可见
//!   gold - 暖黄色金属，有色镜面反射，无漫反射
//!   aluminum - 明亮中性金属，锐利镜面反射，无漫反射
//!   plastic - 哑光漫反射 + 弱而紧的镜面反射（电介质 F0 ~0.04）
//!   stone   - rough diffuse, no visible specular highlight
//!
//! The spheres are spawned as ECS entities with new scene components
//! (MeshRef + MaterialRef + LocalTransform + LocalToWorld), so they live in
//! the ECS world alongside other geometry. They use the same bindless PBR path;
//! no texture slots are bound (u32::MAX) so the scalar `base_color` /
//! `metallic` / `roughness` drive the BRDF directly.

use prism_ecs::World;
use prism_render::managers::{MaterialUploadInput, MeshHandle, MeshUploadInput};
use prism_render::GraphRenderer;

use crate::scene::components::{LocalToWorld, LocalTransform, MaterialRef, MeshRef, SceneAssetId};

/// Spacing between sphere centres along the X axis (world units).
const SPHERE_SPACING: f32 = 2.2;

/// A single calibration material: name + the PBR scalars that define it.
struct CalibMaterial {
    name: &'static str,
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
}

/// The six reference materials. Values follow the standard PBR calibration
/// chart (see the BRDF baseline spec): gold/aluminum use real measured RGB
/// reflectance at perpendicular incidence; dielectrics use a neutral base.
const CALIB_MATERIALS: &[CalibMaterial] = &[
    CalibMaterial {
        name: "white",
        base_color: [0.8, 0.8, 0.8, 1.0],
        metallic: 0.0,
        roughness: 0.5,
    },
    CalibMaterial {
        name: "black",
        base_color: [0.04, 0.04, 0.04, 1.0],
        metallic: 0.0,
        roughness: 0.5,
    },
    CalibMaterial {
        name: "gold",
        // Measured gold albedo (F0): (1.0, 0.766, 0.336).
        base_color: [1.0, 0.766, 0.336, 1.0],
        metallic: 1.0,
        roughness: 0.3,
    },
    CalibMaterial {
        name: "aluminum",
        // Measured aluminum albedo (F0): (0.91, 0.92, 0.92).
        base_color: [0.91, 0.92, 0.92, 1.0],
        metallic: 1.0,
        roughness: 0.25,
    },
    CalibMaterial {
        name: "plastic",
        base_color: [0.5, 0.5, 0.5, 1.0],
        metallic: 0.0,
        roughness: 0.3,
    },
    CalibMaterial {
        name: "stone",
        base_color: [0.5, 0.5, 0.5, 1.0],
        metallic: 0.0,
        roughness: 0.8,
    },
];

/// Build a UV-sphere mesh (radius 1) as a `MeshUploadInput`.
///
/// `segments` = longitude slices (around Y), `rings` = latitude slices (pole
/// to pole). Normals are the normalized positions; tangents point along the
/// longitude (dP/dphi) so the TBN basis is well-formed for the (unused here)
/// normal-map path. UVs wrap [0,1] x [0,1].
fn uv_sphere(segments: u32, rings: u32) -> MeshUploadInput {
    // rings = latitude divisions; we need rings+1 vertices pole-to-pole.
    let lat_steps = rings + 1;
    let lon_steps = segments;

    let vert_count = (lat_steps * lon_steps) as usize;
    let mut positions = Vec::with_capacity(vert_count);
    let mut normals = Vec::with_capacity(vert_count);
    let mut uvs = Vec::with_capacity(vert_count);
    let mut tangents = Vec::with_capacity(vert_count);

    for i in 0..lat_steps {
        // theta: 0 at +Y pole, PI at -Y pole.
        let theta = std::f32::consts::PI * (i as f32) / (rings as f32);
        let sin_t = theta.sin();
        let cos_t = theta.cos();
        for j in 0..lon_steps {
            // phi: 0..2PI around Y.
            let phi = 2.0 * std::f32::consts::PI * (j as f32) / (lon_steps as f32);
            let sin_p = phi.sin();
            let cos_p = phi.cos();

            // Position on unit sphere.
            let x = sin_t * cos_p;
            let y = cos_t;
            let z = sin_t * sin_p;
            positions.push([x, y, z]);
            // Normal = normalized position (unit sphere -> already unit).
            normals.push([x, y, z]);
            // uv u wraps with longitude, v goes pole-to-pole.
            uvs.push([j as f32 / lon_steps as f32, i as f32 / rings as f32]);
            // Tangent: dP/dphi = (-sin_t*sin_p, 0, sin_t*cos_p), normalized.
            // Degenerate at the poles (sin_t -> 0); fall back to +X there.
            // w = handedness +1 (UVs are not mirrored on a UV sphere).
            let tx = -sin_p;
            let tz = cos_p;
            let tlen = (tx * tx + tz * tz).sqrt();
            if tlen > 1e-6 {
                tangents.push([tx / tlen, 0.0, tz / tlen, 1.0]);
            } else {
                tangents.push([1.0, 0.0, 0.0, 1.0]);
            }
        }
    }

    // Indices: two triangles per quad, winding CCW when viewed from outside.
    let mut indices = Vec::with_capacity((lat_steps * lon_steps * 6) as usize);
    for i in 0..rings {
        for j in 0..lon_steps {
            let p00 = i * lon_steps + j;
            let p01 = i * lon_steps + ((j + 1) % lon_steps);
            let p10 = (i + 1) * lon_steps + j;
            let p11 = (i + 1) * lon_steps + ((j + 1) % lon_steps);
            indices.extend_from_slice(&[p00, p10, p01, p01, p10, p11]);
        }
    }

    MeshUploadInput {
        positions,
        normals,
        colors: vec![],
        uvs,
        tangents,
        indices,
    }
}

/// Build a column-major 4x4 translation matrix (no rotation/scale).
fn translation_matrix(x: f32, y: f32, z: f32) -> glam::Mat4 {
    glam::Mat4::from_translation(glam::Vec3::new(x, y, z))
}

/// Register a single UV-sphere mesh + six calibration materials with the
/// renderer, and spawn one ECS entity per sphere with a [`RenderInstance`]
/// component.
///
/// Spheres are placed along the X axis starting at `origin_x`, spaced
/// `SPHERE_SPACING` apart, sitting on `y=1.0` (radius 1) so they rest on the
/// ground plane (`y=0`). The sphere mesh is uploaded via the synchronous
/// `register_mesh` path (not batched) since this runs after the scene's batched
/// upload has already flushed.
pub fn spawn_calibration_spheres(
    renderer: &mut GraphRenderer,
    world: &mut World,
    origin_x: f32,
    origin_y: f32,
    origin_z: f32,
) -> anyhow::Result<()> {
    // One shared sphere mesh (32x16 is smooth enough for BRDF inspection).
    let sphere = uv_sphere(32, 16);
    let mesh: MeshHandle = renderer.register_mesh(&sphere)?;

    // Register each calibration material and emit a draw item.
    for (i, mat) in CALIB_MATERIALS.iter().enumerate() {
        let input = MaterialUploadInput {
            base_color: mat.base_color,
            metallic: mat.metallic,
            roughness: mat.roughness,
            emissive: [0.0; 3],
            albedo_tex: None,
            normal_tex: None,
            metallic_roughness_tex: None,
            emissive_tex: None,
            occlusion_tex: None,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            transmission: 0.0,
            ior: 1.5,
            translucency: 0.0,
            anisotropy: 0.0,
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
            emissive_strength: 1.0,
        };
        let handle = renderer.register_material(input)?;
        let slot = renderer
            .material_slot(handle)
            .ok_or_else(|| anyhow::anyhow!("calibration sphere {}: no material slot", mat.name))?;

        let x = origin_x + i as f32 * SPHERE_SPACING;
        let entity = world.spawn();
        world.insert(
            entity,
            MeshRef {
                asset_id: SceneAssetId::generate(),
                render_handle: mesh,
                generation: 1,
            },
        );
        world.insert(
            entity,
            MaterialRef {
                asset_id: SceneAssetId::generate(),
                material_slot: slot,
                generation: 1,
            },
        );
        world.insert(
            entity,
            LocalTransform {
                position: glam::Vec3::new(x, origin_y, origin_z),
                ..Default::default()
            },
        );
        world.insert(
            entity,
            LocalToWorld(translation_matrix(x, origin_y, origin_z)),
        );
        log::debug!(
            "calibration sphere[{}] '{}': bc={:?} m={} r={} -> slot {}",
            i,
            mat.name,
            mat.base_color,
            mat.metallic,
            mat.roughness,
            slot
        );
    }

    // Flush the new material SSBO entries so the GPU sees them before the next
    // draw. The scene path calls flush_materials() once after its own
    // registrations; the spheres are added afterward so they need their own
    // flush.
    renderer.flush_materials()?;
    log::info!(
        "calibration spheres: registered {} spheres at origin ({}, {}, {})",
        CALIB_MATERIALS.len(),
        origin_x,
        origin_y,
        origin_z
    );
    Ok(())
}
