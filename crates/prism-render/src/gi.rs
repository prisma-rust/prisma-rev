//! Baked probe-volume global illumination — representation + consumer layer.
//!
//! This module is the **producer-agnostic** core of the GI system (see
//! `docs/DESIGN.md` §6). It defines:
//!
//! * the *representation*: a regular grid of order-2 spherical-harmonic (SH)
//!   probes ([`ProbeVolumeInfo`] metadata + the 9-coefficient layout), and
//! * the *consumer*: world→grid mapping, trilinear probe blending, and
//!   [`eval_sh9`] irradiance reconstruction.
//!
//! Neither the data layout nor the evaluation changes between producers. The
//! offline baker (writes the grid once) and a future DDGI real-time pass
//! (updates the grid each frame) are interchangeable *producers* that fill the
//! exact same representation this module reads. Do **not** duplicate the
//! representation or consumer logic per producer.
//!
//! The Slang mirror lives in `shaders/slang/gi.slang` (`EvalSH9`,
//! `SampleProbeVolumeIrradiance`, `ProbeVolumeInfo`); the two must stay in
//! lock-step (basis ordering, constants, grid mapping).
//!
//! ## SH coefficient ordering (the baker contract)
//!
//! Order-2 real spherical harmonics, 9 coefficients, in this fixed order:
//!
//! | index | basis            | value (unit dir `n = (x,y,z)`)   |
//! |------:|------------------|----------------------------------|
//! | 0     | `Y_0^0`  (DC)    | `0.282095`                       |
//! | 1     | `Y_1^-1`         | `0.488603 * y`                   |
//! | 2     | `Y_1^0`          | `0.488603 * z`                   |
//! | 3     | `Y_1^1`          | `0.488603 * x`                   |
//! | 4     | `Y_2^-2`         | `1.092548 * x*y`                 |
//! | 5     | `Y_2^-1`         | `1.092548 * y*z`                 |
//! | 6     | `Y_2^0`          | `0.315392 * (3z^2 - 1)`          |
//! | 7     | `Y_2^1`          | `1.092548 * x*z`                 |
//! | 8     | `Y_2^2`          | `0.546274 * (x^2 - y^2)`         |
//!
//! The cosine-lobe convolution (the `1/pi` and zonal `A_l` factors) is assumed
//! to be **pre-applied by the baker**: coefficients already encode *irradiance*,
//! not raw radiance. [`eval_sh9`] therefore only reconstructs the function
//! value and does **not** multiply by albedo/π — the caller does that.

/// Number of spherical-harmonic coefficients for order 2 (bands 0, 1, 2):
/// `1 + 3 + 5 = 9`.
pub const SH_COEFF_COUNT: usize = 9;

// Order-2 real SH basis constants (Ramamoorthi & Hanrahan 2001, orthonormal).
const SH_C0: f32 = 0.282095; // 0.5 * sqrt(1/pi)
const SH_C1: f32 = 0.488603; // 0.5 * sqrt(3/pi)
const SH_C2: f32 = 1.092548; // 0.5 * sqrt(15/pi)
const SH_C3: f32 = 0.315392; // 0.25 * sqrt(5/pi)
const SH_C4: f32 = 0.546274; // 0.25 * sqrt(15/pi)

/// Probe-volume grid metadata.
///
/// Mirrors the Slang `ProbeVolumeInfo` struct in `shaders/slang/gi.slang`
/// byte-for-byte (std140: three `vec4` = 48 bytes, 16-byte aligned). Describes
/// a regular grid of SH probes in world space; producer-agnostic — the same
/// struct describes a baked grid or a DDGI real-time grid.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ProbeVolumeInfo {
    /// xyz = world position of probe `(0,0,0)`; w unused. offset 0.
    pub origin: [f32; 4],
    /// xyz = world distance between adjacent probes (per axis); w unused.
    /// offset 16.
    pub spacing: [f32; 4],
    /// xyz = probe count per axis (each `>= 1`); w unused. offset 32.
    pub dims: [u32; 4],
}

impl ProbeVolumeInfo {
    /// Convenience constructor from 3-component vectors (pads the unused `w`).
    pub fn new(origin: [f32; 3], spacing: [f32; 3], dims: [u32; 3]) -> Self {
        Self {
            origin: [origin[0], origin[1], origin[2], 0.0],
            spacing: [spacing[0], spacing[1], spacing[2], 0.0],
            dims: [dims[0], dims[1], dims[2], 0],
        }
    }
}

// -------------------------------------------------------------------
// Bake-time directional light (shared default for baker + runtime)
// -------------------------------------------------------------------
//
// The offline baker lives in `prism-build-pipeline` (`prism-build-pipeline
// bake-gi`), which links this crate for the probe math but cannot depend on
// `prism-engine` (the dependency runs engine -> render, so importing
// `engine::DirectionalLight` would form a cycle). To keep the baked
// indirect sun in sync with the runtime sun without that cycle, the canonical
// default light parameters are mirrored here. **These constants MUST stay in
// lock-step with `prism_engine::render_system::DirectionalLight::default()`**
// (euler_xyz, intensity, color). The runtime inserts that default into the ECS
// (`app.rs` `create_default_scene`), so as long as the two match, a bake's
// direct-sun bounce uses the same direction/color/intensity the player sees.
//
// The direction is stored as XYZ Euler angles (degrees) and converted with
// [`bake_euler_xyz_deg_to_dir`], a byte-identical copy of
// `prism_engine::render_system::euler_xyz_deg_to_dir` (see that function's
// docs for the right-handed Rx·Ry·Rz convention, +Y up, base vector +Z).

/// Default directional light Euler angles (degrees), matching
/// `DirectionalLight::default().euler_xyz` = `[45.0, -45.0, 0.0]`
/// (pitch=45°, yaw=-45° -> direction `[-1/√2, 1/√2, 0]`).
pub const BAKE_DEFAULT_LIGHT_EULER: [f32; 3] = [45.0, -45.0, 0.0];
/// Default directional light illuminance in **lux**, matching
/// `DirectionalLight::default().intensity` (100k lux = bright sunlight).
/// The runtime shader converts illuminance to radiance via `/ PI`; the baker
/// mirrors that division so the baked sun bounce uses the same effective
/// radiance the player sees.
pub const BAKE_DEFAULT_LIGHT_INTENSITY: f32 = 100_000.0;
/// Default directional light RGB color, matching
/// `DirectionalLight::default().color`.
pub const BAKE_DEFAULT_LIGHT_COLOR: [f32; 3] = [1.0, 1.0, 1.0];

/// Convert XYZ Euler angles (degrees) to a unit direction vector (direction
/// TO the light). Mirror of `prism_engine::render_system::euler_xyz_deg_to_dir`;
/// kept here so the baker (in `prism-render`) can reuse it without a crate
/// cycle. See the engine function for the full convention derivation.
pub fn bake_euler_xyz_deg_to_dir(e: [f32; 3]) -> [f32; 3] {
    let p = e[0].to_radians();
    let y = e[1].to_radians();
    // Roll (e[2]) does not affect a pure +Z base direction; intentionally unused.
    let (sp, cp) = p.sin_cos();
    let (sy, cy) = y.sin_cos();
    let x = cp * sy;
    let yy = sp;
    let z = cp * cy;
    let len = (x * x + yy * yy + z * z).sqrt().max(1e-8);
    [x / len, yy / len, z / len]
}

/// Order-2 real SH basis values for a unit direction `n = (x, y, z)`.
///
/// Returns the 9 basis values in the documented order. The direction is
/// assumed unit-length; callers should normalize before calling.
pub fn sh_basis(n: [f32; 3]) -> [f32; SH_COEFF_COUNT] {
    let (x, y, z) = (n[0], n[1], n[2]);
    [
        SH_C0,
        SH_C1 * y,
        SH_C1 * z,
        SH_C1 * x,
        SH_C2 * x * y,
        SH_C2 * y * z,
        SH_C3 * (3.0 * z * z - 1.0),
        SH_C2 * x * z,
        SH_C4 * (x * x - y * y),
    ]
}

/// Evaluate order-2 RADIANCE SH for a unit direction `n`.
///
/// Returns L(n) ≈ Σ c_lm * Y_lm(n) — the incident radiance reconstructed from
/// the radiance SH coefficients. No cosine convolution is applied; the baker
/// stores radiance SH (no cosine pre-convolution). This is the correct input
/// for the split-sum specular approximation.
///
/// For diffuse irradiance, use [`eval_sh9_irradiance`] which applies the
/// Ramamoorthi & Hanrahan A_l factors.
pub fn eval_sh9(n: [f32; 3], sh: &[[f32; 3]; SH_COEFF_COUNT]) -> [f32; 3] {
    let b = sh_basis(n);
    let mut out = [0.0f32; 3];
    for c in 0..SH_COEFF_COUNT {
        out[0] += sh[c][0] * b[c];
        out[1] += sh[c][1] * b[c];
        out[2] += sh[c][2] * b[c];
    }
    out
}

/// Ramamoorthi & Hanrahan SH cosine-convolution factors (orthonormal basis).
/// A_0 = π, A_1 = 2π/3, A_2 = π/4.
const A_L0: f32 = std::f32::consts::PI;
const A_L1: f32 = 2.0 * std::f32::consts::PI / 3.0;
const A_L2: f32 = std::f32::consts::PI / 4.0;

/// Evaluate order-2 IRRADIANCE SH for a surface normal `n`.
///
/// Applies the Ramamoorthi & Hanrahan cosine-convolution factors A_l to the
/// radiance SH coefficients, returning E(n) = Σ c_lm * A_l * Y_lm(n).
/// The caller divides by π for the Lambertian BRDF: diffuse = kd * E(n) * albedo / π.
pub fn eval_sh9_irradiance(n: [f32; 3], sh: &[[f32; 3]; SH_COEFF_COUNT]) -> [f32; 3] {
    let b = sh_basis(n);
    let mut out = [0.0f32; 3];
    // Band 0 (l=0, c=0): A_0 = π
    // Band 1 (l=1, c=1,2,3): A_1 = 2π/3
    // Band 2 (l=2, c=4..8): A_2 = π/4
    let al = [A_L0, A_L1, A_L1, A_L1, A_L2, A_L2, A_L2, A_L2, A_L2];
    for c in 0..SH_COEFF_COUNT {
        let a = al[c];
        out[0] += sh[c][0] * b[c] * a;
        out[1] += sh[c][1] * b[c] * a;
        out[2] += sh[c][2] * b[c] * a;
    }
    out
}

/// Map a world position to fractional probe-grid coordinates.
///
/// `coord = (world - origin) / spacing`. `coord == (0,0,0)` is probe `(0,0,0)`;
/// `coord == (dims-1)` is the last probe. The result may lie outside
/// `[0, dims-1]` for points beyond the volume — [`trilinear_weights`] clamps.
pub fn world_to_probe_coord(world: [f32; 3], info: &ProbeVolumeInfo) -> [f32; 3] {
    [
        (world[0] - info.origin[0]) / info.spacing[0],
        (world[1] - info.origin[1]) / info.spacing[1],
        (world[2] - info.origin[2]) / info.spacing[2],
    ]
}

/// Trilinear interpolation weights for a fractional grid coordinate.
///
/// Returns `(base, weights)` where `base` is the integer corner probe (clamped
/// so `base + 1` stays in-range) and `weights` are the 8 corner weights in
/// `(i, j, k)` binary order:
///
/// ```text
///   0 = (0,0,0)   1 = (1,0,0)   2 = (0,1,0)   3 = (1,1,0)
///   4 = (0,0,1)   5 = (1,0,1)   6 = (0,1,1)   7 = (1,1,1)
/// ```
///
/// The fractional coordinate is clamped to `[0, dims-1]`, so out-of-volume
/// points snap to the boundary probes. Handles `dims == 1` on any axis (single
/// probe → weight 0 = 1, no interpolation).
pub fn trilinear_weights(coord: [f32; 3], dims: [u32; 3]) -> ([i32; 3], [f32; 8]) {
    let max = [
        (dims[0].saturating_sub(1)) as f32,
        (dims[1].saturating_sub(1)) as f32,
        (dims[2].saturating_sub(1)) as f32,
    ];
    let c = [
        coord[0].clamp(0.0, max[0]),
        coord[1].clamp(0.0, max[1]),
        coord[2].clamp(0.0, max[2]),
    ];
    // Clamp base so base+1 <= dims-1 (i.e. base <= dims-2); the .max(0) keeps
    // dims==1 axes at base 0.
    let base = [
        (c[0].floor() as i32).clamp(0, (dims[0] as i32 - 2).max(0)),
        (c[1].floor() as i32).clamp(0, (dims[1] as i32 - 2).max(0)),
        (c[2].floor() as i32).clamp(0, (dims[2] as i32 - 2).max(0)),
    ];
    let t = [
        c[0] - base[0] as f32,
        c[1] - base[1] as f32,
        c[2] - base[2] as f32,
    ];
    let w = [
        (1.0 - t[0]) * (1.0 - t[1]) * (1.0 - t[2]),
        t[0] * (1.0 - t[1]) * (1.0 - t[2]),
        (1.0 - t[0]) * t[1] * (1.0 - t[2]),
        t[0] * t[1] * (1.0 - t[2]),
        (1.0 - t[0]) * (1.0 - t[1]) * t[2],
        t[0] * (1.0 - t[1]) * t[2],
        (1.0 - t[0]) * t[1] * t[2],
        t[0] * t[1] * t[2],
    ];
    (base, w)
}

/// Sample irradiance from a probe volume at a world position for a surface
/// normal.
///
/// `fetch(i, j, k, c)` returns the RGB SH coefficient `c` of probe `(i, j, k)`.
/// The 8 corner probes' 9 coefficients are trilinear-blended, then
/// [`eval_sh9_irradiance`] reconstructs the irradiance for `normal` using the
/// Ramamoorthi & Hanrahan A_l factors. Producer-agnostic: `fetch` can
/// read a baked 3D texture or a DDGI-updated one — the algorithm is identical.
///
/// The result is irradiance E(n) = ∫ L(ω) max(0, n·ω) dω; multiply by
/// `kd * albedo / π` at the call site for the Lambertian diffuse BRDF.
pub fn sample_probe_irradiance<F>(
    world: [f32; 3],
    normal: [f32; 3],
    info: &ProbeVolumeInfo,
    mut fetch: F,
) -> [f32; 3]
where
    F: FnMut(i32, i32, i32, usize) -> [f32; 3],
{
    let coord = world_to_probe_coord(world, info);
    let dims = [info.dims[0], info.dims[1], info.dims[2]];
    let (base, w) = trilinear_weights(coord, dims);

    // Trilinear-blend the 9 SH coefficients across the 8 corner probes.
    let mut sh = [[0.0f32; 3]; SH_COEFF_COUNT];
    for (idx, &weight) in w.iter().enumerate() {
        let di = (idx & 1) as i32;
        let dj = ((idx >> 1) & 1) as i32;
        let dk = ((idx >> 2) & 1) as i32;
        if weight == 0.0 {
            continue;
        }
        for (c, shc) in sh.iter_mut().enumerate() {
            let coeff = fetch(base[0] + di, base[1] + dj, base[2] + dk, c);
            shc[0] += coeff[0] * weight;
            shc[1] += coeff[1] * weight;
            shc[2] += coeff[2] * weight;
        }
    }
    eval_sh9_irradiance(normal, &sh)
}

#[cfg(test)]
#[path = "gi_tests.rs"]
mod tests;
