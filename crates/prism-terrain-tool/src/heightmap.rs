//! Heightmap data structure and generation pipeline.
//!
//! The pipeline:
//! ```text
//!   Base FBM → Domain Warp / Ridge → Cliff Enhance → Normalize → Heightmap
//!                                                                    ↓
//!                                                               Erosion (optional)
//! ```

use crate::noise;

/// A 2D heightmap stored as a contiguous `f32` array in row-major order.
///
/// Provides sampling, normalization, gradient computation, and remapping.
#[derive(Debug, Clone)]
pub struct Heightmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

#[allow(dead_code)]
impl Heightmap {
    /// Create a new heightmap from raw data.
    pub fn new(width: u32, height: u32, data: Vec<f32>) -> Self {
        assert_eq!(data.len(), (width * height) as usize);
        Self {
            width,
            height,
            data,
        }
    }

    /// Create an empty heightmap (zero-initialized).
    pub fn zero(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0.0; (width * height) as usize],
        }
    }

    /// Get value at pixel coordinates.
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> f32 {
        self.data[(y * self.width + x) as usize]
    }

    /// Get mutable reference to pixel.
    #[inline]
    pub fn get_mut(&mut self, x: u32, y: u32) -> &mut f32 {
        &mut self.data[(y * self.width + x) as usize]
    }

    /// Sample with bilinear interpolation at normalized coordinates `(u, v)` in [0, 1].
    pub fn sample(&self, u: f64, v: f64) -> f32 {
        let x = u * (self.width as f64 - 1.0);
        let y = v * (self.height as f64 - 1.0);
        let x0 = (x.floor() as u32).min(self.width - 1);
        let y0 = (y.floor() as u32).min(self.height - 1);
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - x0 as f64;
        let fy = y - y0 as f64;

        let a = self.get(x0, y0) as f64;
        let b = self.get(x1, y0) as f64;
        let c = self.get(x0, y1) as f64;
        let d = self.get(x1, y1) as f64;

        let top = a * (1.0 - fx) + b * fx;
        let bot = c * (1.0 - fx) + d * fx;
        (top * (1.0 - fy) + bot * fy) as f32
    }

    /// Normalize the heightmap to [0, 1] range (in-place).
    pub fn normalize(&mut self) {
        let mut min_val = f32::MAX;
        let mut max_val = f32::MIN;
        for &v in &self.data {
            if v < min_val {
                min_val = v;
            }
            if v > max_val {
                max_val = v;
            }
        }
        let range = max_val - min_val;
        if range > 1e-10 {
            for v in &mut self.data {
                *v = (*v - min_val) / range;
            }
        }
    }

    /// Remap to [0, 1] with optional exponent (gamma) curve.
    pub fn remap(&mut self, gamma: f32) {
        self.normalize();
        if (gamma - 1.0).abs() > 1e-6 {
            for v in &mut self.data {
                *v = v.powf(gamma);
            }
        }
    }

    /// Compute gradient (slope) at pixel `(x, y)`, returning `(dx, dy)`.
    pub fn gradient(&self, x: u32, y: u32) -> (f32, f32) {
        let w = self.width;
        let h = self.height;
        let left = if x == 0 {
            self.get(0, y)
        } else {
            self.get(x - 1, y)
        };
        let right = if x >= w - 1 {
            self.get(w - 1, y)
        } else {
            self.get(x + 1, y)
        };
        let up = if y == 0 {
            self.get(x, 0)
        } else {
            self.get(x, y - 1)
        };
        let down = if y >= h - 1 {
            self.get(x, h - 1)
        } else {
            self.get(x, y + 1)
        };
        ((right - left) * 0.5, (down - up) * 0.5)
    }

    /// Compute slope magnitude at `(x, y)`.
    #[allow(dead_code)]
    pub fn slope(&self, x: u32, y: u32) -> f32 {
        let (gx, gy) = self.gradient(x, y);
        (gx * gx + gy * gy).sqrt()
    }
}

// ---------------------------------------------------------------------------
// Generation parameters
// ---------------------------------------------------------------------------

/// Configuration for the heightmap generation pipeline.
#[derive(Debug, Clone)]
pub struct HeightmapConfig {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// FBM octaves (layers of noise).
    pub octaves: u32,
    /// Base frequency in cycles per pixel (smaller = larger features).
    ///
    /// Tuned for 1024 px maps: `0.003` yields roughly three base-noise
    /// features across the whole map.
    pub frequency: f64,
    /// FBM gain (amplitude decay per octave, typical: 0.5).
    pub gain: f64,
    /// FBM lacunarity (frequency multiplier, typical: 2.0).
    pub lacunarity: f64,
    /// Enable ridge / domain-warp style (MdX3Rr-style `1/(1+d·d)`).
    pub ridge: bool,
    /// Strength of domain warping ridge effect.
    pub warp_strength: f64,
    /// If true, use classic ridge noise (`1 - |noise|`) instead of domain warp.
    pub ridge_classic: bool,
    /// Enable cliff enhancement (adds vertical rock faces at certain heights).
    pub cliff: bool,
    /// Cliff enhancement center height (normalized 0-1 after generation).
    pub cliff_center: f32,
    /// Cliff enhancement width.
    pub cliff_width: f32,
    /// Cliff height addition (as fraction of total range).
    pub cliff_amount: f32,
    /// Random seed (None = use fixed default pattern).
    pub seed: Option<u64>,
}

impl Default for HeightmapConfig {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 1024,
            octaves: 8,
            frequency: 0.003,
            gain: 0.5,
            lacunarity: 2.0,
            ridge: false,
            warp_strength: 2.0,
            ridge_classic: false,
            cliff: false,
            cliff_center: 0.6,
            cliff_width: 0.05,
            cliff_amount: 0.15,
            seed: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline runner
// ---------------------------------------------------------------------------

/// Generate a heightmap from the given configuration.
///
/// Pipeline:
/// 1. Generate base FBM field (parallel)
/// 2. Apply ridge / domain warping if requested
/// 3. Apply cliff enhancement if requested
/// 4. Normalize to [0, 1]
pub fn generate_heightmap(cfg: &HeightmapConfig) -> Heightmap {
    let data: Vec<f32> = if cfg.ridge {
        // Domain-warp ridge style (MdX3Rr).
        let oct = cfg.octaves;
        let warp = cfg.warp_strength;
        let gain = cfg.gain;
        let lac = cfg.lacunarity;
        let freq = cfg.frequency;
        let seed_offset = cfg.seed.unwrap_or(0) as f64 * 1000.0;
        noise::generate_parallel(cfg.width, cfg.height, freq, move |p| {
            let p = [p[0] + seed_offset, p[1] + seed_offset];
            noise::ridge_domain_warp(p, oct, gain, lac, warp)
        })
    } else if cfg.ridge_classic {
        let oct = cfg.octaves;
        let gain = cfg.gain;
        let lac = cfg.lacunarity;
        let freq = cfg.frequency;
        let seed_offset = cfg.seed.unwrap_or(0) as f64 * 1000.0;
        noise::generate_parallel(cfg.width, cfg.height, freq, move |p| {
            let p = [p[0] + seed_offset, p[1] + seed_offset];
            noise::ridge_noise(p, oct, gain, lac)
        })
    } else {
        // Standard FBM.
        let oct = cfg.octaves;
        let gain = cfg.gain;
        let lac = cfg.lacunarity;
        let freq = cfg.frequency;
        let seed_offset = cfg.seed.unwrap_or(0) as f64 * 1000.0;
        noise::generate_parallel(cfg.width, cfg.height, freq, move |p| {
            let p = [p[0] + seed_offset, p[1] + seed_offset];
            noise::fbm_2d(p, oct, gain, lac)
        })
    };

    let mut hm = Heightmap::new(cfg.width, cfg.height, data);

    // Cliff enhancement (applied before normalization).
    if cfg.cliff {
        hm.normalize();
        let center = cfg.cliff_center;
        let width = cfg.cliff_width;
        let amount = cfg.cliff_amount;
        for v in &mut hm.data {
            let t = (*v - center) / width;
            let smooth = if t < -1.0 {
                0.0
            } else if t > 1.0 {
                1.0
            } else {
                t * 0.5 + 0.5
            };
            let cliff = if smooth <= 0.0 {
                0.0
            } else if smooth >= 1.0 {
                1.0
            } else {
                smooth * smooth * (3.0 - 2.0 * smooth)
            };
            *v += amount * cliff;
        }
    }

    hm.normalize();
    hm
}

#[cfg(test)]
#[path = "heightmap_tests.rs"]
mod tests;
