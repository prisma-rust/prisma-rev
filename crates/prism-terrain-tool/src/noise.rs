//! Noise functions for procedural heightmap generation.
//!
//! Provides value noise, FBM, domain warping (ridge noise), and utilities
//! inspired by Inigo Quilez's techniques used in the analyzed shaders.

use rayon::prelude::*;

/// Rotation matrix used in FBM octave accumulation (IQ's m2).
const ROT_MAT: [[f64; 2]; 2] = [[0.8, -0.6], [0.6, 0.8]];

// ---------------------------------------------------------------------------
// Hash helpers
// ---------------------------------------------------------------------------

/// Pseudo-random hash from a 2D coordinate, returning value in [0, 1).
#[inline]
fn hash2(p: [f64; 2]) -> f64 {
    let dot = p[0] * 12.9898 + p[1] * 78.233;
    let frac = (dot.sin() * 43758.5453).fract();
    frac.abs()
}

/// Integer-based hash for 1D input.
#[allow(dead_code)]
#[inline]
fn hash1(n: f64) -> f64 {
    let x = n * std::f64::consts::FRAC_1_PI;
    (n * 17.0 * x.fract()).fract().abs()
}

// ---------------------------------------------------------------------------
// 2D Value Noise with analytical derivatives
// ---------------------------------------------------------------------------

/// 2D value noise returning `(value, dx, dy)`.
pub fn noised_2d(p: [f64; 2]) -> [f64; 3] {
    let i = [p[0].floor(), p[1].floor()];
    let w = [p[0] - i[0], p[1] - i[1]];

    // Quintic blend for C2 continuity (IQ's preferred).
    let u = [
        w[0] * w[0] * w[0] * (w[0] * (w[0] * 6.0 - 15.0) + 10.0),
        w[1] * w[1] * w[1] * (w[1] * (w[1] * 6.0 - 15.0) + 10.0),
    ];
    let du = [
        30.0 * w[0] * w[0] * (w[0] * (w[0] - 2.0) + 1.0),
        30.0 * w[1] * w[1] * (w[1] * (w[1] - 2.0) + 1.0),
    ];

    let a = hash2(i);
    let b = hash2([i[0] + 1.0, i[1]]);
    let c = hash2([i[0], i[1] + 1.0]);
    let d = hash2([i[0] + 1.0, i[1] + 1.0]);

    let k0 = a;
    let k1 = b - a;
    let k2 = c - a;
    let k4 = a - b - c + d;

    let value = -1.0 + 2.0 * (k0 + k1 * u[0] + k2 * u[1] + k4 * u[0] * u[1]);
    let dx = 2.0 * du[0] * (k1 + k4 * u[1]);
    let dy = 2.0 * du[1] * (k2 + k4 * u[0]);

    [value, dx, dy]
}

/// 2D value noise without derivatives (slightly faster).
pub fn noise_2d(p: [f64; 2]) -> f64 {
    let i = [p[0].floor(), p[1].floor()];
    let w = [p[0] - i[0], p[1] - i[1]];
    let u = [
        w[0] * w[0] * (3.0 - 2.0 * w[0]),
        w[1] * w[1] * (3.0 - 2.0 * w[1]),
    ];

    let a = hash2(i);
    let b = hash2([i[0] + 1.0, i[1]]);
    let c = hash2([i[0], i[1] + 1.0]);
    let d = hash2([i[0] + 1.0, i[1] + 1.0]);

    -1.0 + 2.0 * (a + (b - a) * u[0] + (c - a) * u[1] + (a - b - c + d) * u[0] * u[1])
}

// ---------------------------------------------------------------------------
// FBM (Fractal Brownian Motion)
// ---------------------------------------------------------------------------

/// Standard FBM with IQ-style rotation matrix for detail decorrelation.
///
/// `p` — input position
/// `octaves` — number of layers
/// `gain` — amplitude multiplier per octave (typical: 0.5)
/// `lacunarity` — frequency multiplier per octave (typical: 2.0)
/// `use_derivs` — if true, use domain warping (ridge-generating) style
pub fn fbm_2d(p: [f64; 2], octaves: u32, gain: f64, lacunarity: f64) -> f64 {
    let mut p = p;
    let mut a = 0.0f64;
    let mut b = 1.0f64;
    for _ in 0..octaves {
        a += b * noise_2d(p);
        b *= gain;
        p = rotate(p, lacunarity);
    }
    a
}

/// FBM with analytical derivative accumulation (domain warping style).
///
/// Returns `(value, dx, dy)` — the accumulated noise value and its
/// gradient. The gradient can be used for `1/(1 + d·d)` ridge generation.
#[allow(dead_code)]
pub fn fbmd_2d(p: [f64; 2], octaves: u32, gain: f64, lacunarity: f64) -> [f64; 3] {
    let mut p = p;
    let mut a = 0.0f64;
    let mut b = 1.0f64;
    let mut d = [0.0f64; 2];
    // Identity rotation matrix for derivative chain rule.
    let mut m = [[1.0, 0.0], [0.0, 1.0]];

    for _ in 0..octaves {
        let n = noised_2d(p);
        a += b * n[0];
        // Accumulate derivatives through chain rule: d += b * M * n.yz
        d[0] += b * (m[0][0] * n[1] + m[0][1] * n[2]);
        d[1] += b * (m[1][0] * n[1] + m[1][1] * n[2]);

        b *= gain;
        p = rotate(p, lacunarity);
        // Update chain-rule matrix: M = lacunarity * ROT_MAT * M
        let new_m00 = lacunarity * (ROT_MAT[0][0] * m[0][0] + ROT_MAT[0][1] * m[1][0]);
        let new_m01 = lacunarity * (ROT_MAT[0][0] * m[0][1] + ROT_MAT[0][1] * m[1][1]);
        let new_m10 = lacunarity * (ROT_MAT[1][0] * m[0][0] + ROT_MAT[1][1] * m[1][0]);
        let new_m11 = lacunarity * (ROT_MAT[1][0] * m[0][1] + ROT_MAT[1][1] * m[1][1]);
        m = [[new_m00, new_m01], [new_m10, new_m11]];
    }

    [a, d[0], d[1]]
}

/// Ridge-style FBM with domain warping (`1/(1 + d·d)` technique).
///
/// This is the key technique from IQ's MdX3Rr shader that produces
/// sharp ridge-and-valley terrain features resembling erosion.
pub fn ridge_domain_warp(
    p: [f64; 2],
    octaves: u32,
    gain: f64,
    lacunarity: f64,
    warp_strength: f64,
) -> f64 {
    let mut p = p;
    let mut a = 0.0f64;
    let mut b = 1.0f64;
    let mut d = [0.0f64; 2];
    let mut m = [[1.0, 0.0], [0.0, 1.0]];

    for _ in 0..octaves {
        let n = noised_2d(p);
        d[0] += n[1];
        d[1] += n[2];
        // Core ridge trick: divide by 1+d·d to carve valleys where
        // gradient is steep, leaving sharp ridges.
        let denom = 1.0 + warp_strength * (d[0] * d[0] + d[1] * d[1]);
        a += b * n[0] / denom;

        b *= gain;
        p = rotate(p, lacunarity);
        let new_m00 = lacunarity * (ROT_MAT[0][0] * m[0][0] + ROT_MAT[0][1] * m[1][0]);
        let new_m01 = lacunarity * (ROT_MAT[0][0] * m[0][1] + ROT_MAT[0][1] * m[1][1]);
        let new_m10 = lacunarity * (ROT_MAT[1][0] * m[0][0] + ROT_MAT[1][1] * m[1][0]);
        let new_m11 = lacunarity * (ROT_MAT[1][0] * m[0][1] + ROT_MAT[1][1] * m[1][1]);
        m = [[new_m00, new_m01], [new_m10, new_m11]];
    }

    a
}

/// Classic ridge noise: `1 - |noise|`, amplified for sharp mountain ridges.
pub fn ridge_noise(p: [f64; 2], octaves: u32, gain: f64, lacunarity: f64) -> f64 {
    let mut p = p;
    let mut a = 0.0f64;
    let mut b = 1.0f64;
    for _ in 0..octaves {
        let n = 1.0 - noise_2d(p).abs();
        a += b * n * n; // Square for sharper ridges.
        b *= gain;
        p = rotate(p, lacunarity);
    }
    a
}

// ---------------------------------------------------------------------------
// Domain repetition helper (from 43cBzn style)
// ---------------------------------------------------------------------------

/// Repetition domain: map `p` into `[-r/2, r/2]` and return the cell ID.
#[allow(dead_code)]
pub fn domain_repeat(p: [f64; 2], r: f64) -> ([f64; 2], [i64; 2]) {
    let id = [(p[0] / r).floor() as i64, (p[1] / r).floor() as i64];
    let local = [
        p[0] - id[0] as f64 * r - r * 0.5,
        p[1] - id[1] as f64 * r - r * 0.5,
    ];
    (local, id)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Apply rotation matrix and scale to a 2D point (FBM frequency step).
#[inline]
fn rotate(p: [f64; 2], scale: f64) -> [f64; 2] {
    [
        scale * (ROT_MAT[0][0] * p[0] + ROT_MAT[0][1] * p[1]),
        scale * (ROT_MAT[1][0] * p[0] + ROT_MAT[1][1] * p[1]),
    ]
}

// ---------------------------------------------------------------------------
// Parallel heightmap generation helper
// ---------------------------------------------------------------------------

/// Generate a heightmap buffer by evaluating `f` at each pixel coordinate.
///
/// `f` receives noise-space coordinates `[x * frequency, y * frequency]`, where
/// `x`/`y` are pixel indices — so `frequency` is measured in **cycles per
/// pixel** (smaller = larger features). The default `0.003` therefore crosses
/// roughly three lattice cells across a 1024 px map.
///
/// Scaling the pixel index (not a normalized `[-1, 1]` UV) is deliberate: the
/// value-noise lattice is one unit wide, so multiplying a UV that spans only
/// two units by `0.003` keeps every sample inside cells `-1`/`0` and collapses
/// the map into a single smooth patch. It also keeps features isotropic on
/// non-square maps, which a subtractive aspect offset did not.
///
/// The function is called in parallel via `rayon`.
pub fn generate_parallel(
    width: u32,
    height: u32,
    frequency: f64,
    f: impl Fn([f64; 2]) -> f64 + Send + Sync,
) -> Vec<f32> {
    let mut data = vec![0.0f32; (width * height) as usize];

    data.par_chunks_mut(width as usize)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, cell) in row.iter_mut().enumerate() {
                *cell = f([x as f64 * frequency, y as f64 * frequency]) as f32;
            }
        });

    data
}

#[cfg(test)]
#[path = "noise_tests.rs"]
mod tests;
