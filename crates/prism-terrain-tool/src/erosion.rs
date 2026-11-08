//! Erosion simulation for heightmap post-processing.
//!
//! Two erosion models:
//! - **Thermal erosion**: angle-of-repose relaxation (talus slope).
//! - **Hydraulic erosion**: particle-based water flow with sediment transport.

use crate::heightmap::Heightmap;
use rand::rngs::SmallRng;
use rand::{Rng, RngExt, SeedableRng};
use rayon::prelude::*;

/// Trait for erosion kernels — allows future GPU compute backends.
pub trait ErosionKernel {
    /// Apply erosion iterations to the heightmap in-place.
    fn erode(&self, hm: &mut Heightmap, iterations: u32);
}

// ---------------------------------------------------------------------------
// Thermal erosion
// ---------------------------------------------------------------------------

/// Thermal (talus) erosion parameters.
#[derive(Debug, Clone)]
pub struct ThermalErosion {
    /// Angle of repose in degrees. Slopes steeper than this will relax.
    pub talus_angle_degrees: f64,
    /// Fraction of excess height transferred per iteration (0-1).
    pub relaxation_rate: f64,
}

impl Default for ThermalErosion {
    fn default() -> Self {
        Self {
            talus_angle_degrees: 30.0,
            relaxation_rate: 0.5,
        }
    }
}

impl ThermalErosion {
    /// Convert talus angle to a slope (vertical/horizontal ratio).
    fn talus_slope(&self) -> f64 {
        (self.talus_angle_degrees * std::f64::consts::PI / 180.0).tan()
    }
}

impl ErosionKernel for ThermalErosion {
    fn erode(&self, hm: &mut Heightmap, iterations: u32) {
        if iterations == 0 || hm.width < 2 || hm.height < 2 {
            return;
        }

        let w = hm.width as usize;
        let h = hm.height as usize;
        let talus = self.talus_slope();
        let rate = self.relaxation_rate.clamp(0.0, 1.0);

        // Heights are normalized to [0, 1] over a unit-square map, so one
        // pixel spans `1 / w` horizontally and `1 / h` vertically. A talus
        // angle is a vertical/horizontal ratio, hence the largest drop still
        // at repose is `talus / w` (or `/ h`) per pixel. Using a flat
        // `talus` threshold made the default 30° unreachable on normalized
        // heightmaps.
        let max_drop_x = talus / w as f64;
        let max_drop_y = talus / h as f64;

        // Double-buffer approach: read from `src`, write to `dst`.
        let mut src: Vec<f64> = hm.data.iter().map(|&v| v as f64).collect();
        let mut dst = src.clone();

        for _iter in 0..iterations {
            // Parallel: process each pixel independently.
            dst.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                for (x, cell) in row.iter_mut().enumerate() {
                    let center_h = src[y * w + x];

                    // Net change: material leaving this cell to lower
                    // neighbours, plus the mirrored inflow from higher ones.
                    // Both sides of a pair evaluate the same `drop - max_drop`
                    // term, so the transfer is exactly conservative.
                    let mut net_change = 0.0;

                    for &(dx, dy, max_drop) in &[
                        (-1, 0, max_drop_x),
                        (1, 0, max_drop_x),
                        (0, -1, max_drop_y),
                        (0, 1, max_drop_y),
                    ] {
                        let nx = x as isize + dx;
                        let ny = y as isize + dy;
                        if nx < 0 || nx >= w as isize || ny < 0 || ny >= h as isize {
                            continue;
                        }

                        let neighbor_h = src[ny as usize * w + nx as usize];
                        let drop = center_h - neighbor_h;

                        if drop > max_drop {
                            // Material leaves this cell. The neighbour adds
                            // the same amount through its own inflow branch.
                            net_change -= (drop - max_drop) * rate * 0.25;
                        } else if -drop > max_drop {
                            // Material arrives from a higher neighbour.
                            net_change += (-drop - max_drop) * rate * 0.25;
                        }
                    }

                    *cell = center_h + net_change;
                }
            });

            std::mem::swap(&mut src, &mut dst);
        }

        for (dst, src) in hm.data.iter_mut().zip(src.iter()) {
            *dst = *src as f32;
        }
    }
}

// ---------------------------------------------------------------------------
// Hydraulic erosion (particle-based)
// ---------------------------------------------------------------------------

/// Hydraulic (water) erosion parameters.
#[derive(Debug, Clone)]
pub struct HydraulicErosion {
    /// Number of particles to spawn per iteration.
    pub particle_count: u32,
    /// Water volume per particle at spawn.
    pub rain_amount: f64,
    /// Sediment capacity factor (higher = more sediment carried).
    pub sediment_capacity: f64,
    /// Evaporation rate (fraction of water lost per step).
    pub evaporation_rate: f64,
    /// Deposition rate (fraction of excess sediment deposited per step).
    pub deposition_rate: f64,
    /// Erosion rate (fraction of bed material eroded per step, capped).
    pub erosion_rate: f64,
    /// Gravity strength.
    pub gravity: f64,
    /// Particle inertia (0-1, higher = smoother path).
    pub inertia: f64,
}

impl Default for HydraulicErosion {
    fn default() -> Self {
        Self {
            particle_count: 100_000,
            rain_amount: 1.0,
            sediment_capacity: 4.0,
            evaporation_rate: 0.01,
            deposition_rate: 0.3,
            erosion_rate: 0.3,
            gravity: 4.0,
            inertia: 0.3,
        }
    }
}

impl ErosionKernel for HydraulicErosion {
    fn erode(&self, hm: &mut Heightmap, iterations: u32) {
        let w = hm.width as i32;
        let h = hm.height as i32;

        for _iter in 0..iterations {
            // Generate and process particles in parallel.
            // Each particle is independent, so we can parallelize.
            let mut particles: Vec<Particle> = (0..self.particle_count)
                .into_par_iter()
                .map_init(
                    || SmallRng::from_rng(&mut rand::rng()),
                    |rng, _| Particle::new(rng, w, h, self.rain_amount),
                )
                .collect();

            // Process each particle's path.
            let particle_results: Vec<ParticleResult> = particles
                .par_iter_mut()
                .map_init(
                    || SmallRng::from_rng(&mut rand::rng()),
                    |rng, particle| self.simulate_particle(rng, &hm.data, w, h, particle),
                )
                .collect();

            // Apply erosion/deposition to heightmap.
            // Since particles can conflict, we use a simple accumulate-then-apply approach.
            // For a CLI tool this is fine; for production you'd want atomic floats or a grid lock.
            let mut erosion_map = vec![0.0f64; (w * h) as usize];
            let mut deposit_map = vec![0.0f64; (w * h) as usize];

            for result in &particle_results {
                for action in &result.actions {
                    let idx = (action.y * w + action.x) as usize;
                    if idx < erosion_map.len() {
                        match action.kind {
                            ActionKind::Erode => erosion_map[idx] += action.amount,
                            ActionKind::Deposit => deposit_map[idx] += action.amount,
                        }
                    }
                }
            }

            // Apply erosion + deposition (single-threaded, but just a pass).
            for ((h_val, &ero), &dep) in hm
                .data
                .iter_mut()
                .zip(erosion_map.iter())
                .zip(deposit_map.iter())
            {
                let change = dep as f32 - ero as f32;
                *h_val = (*h_val + change).max(0.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hydraulic erosion internals
// ---------------------------------------------------------------------------

struct Particle {
    x: f64,
    y: f64,
    water: f64,
    sediment: f64,
    speed: f64,
    vx: f64,
    vy: f64,
}

enum ActionKind {
    Erode,
    Deposit,
}

struct ParticleAction {
    x: i32,
    y: i32,
    kind: ActionKind,
    amount: f64,
}

struct ParticleResult {
    actions: Vec<ParticleAction>,
}

impl Particle {
    fn new(rng: &mut impl Rng, w: i32, h: i32, rain: f64) -> Self {
        Self {
            x: rng.random::<f64>() * (w - 1) as f64,
            y: rng.random::<f64>() * (h - 1) as f64,
            water: rain,
            sediment: 0.0,
            speed: 0.0,
            vx: 0.0,
            vy: 0.0,
        }
    }
}

impl HydraulicErosion {
    fn sample_height(data: &[f32], w: i32, _h: i32, x: f64, y: f64) -> f64 {
        // Clamp to valid range before sampling.
        let x = x.clamp(0.0, (w - 1) as f64);
        let y = y.clamp(0.0, (_h - 1) as f64);
        let x0 = (x.floor() as i32).max(0).min(w - 1);
        let y0 = (y.floor() as i32).max(0).min(_h - 1);
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(_h - 1);
        let fx = x - x0 as f64;
        let fy = y - y0 as f64;

        let a = data[(y0 * w + x0) as usize] as f64;
        let b = data[(y0 * w + x1) as usize] as f64;
        let c = data[(y1 * w + x0) as usize] as f64;
        let d = data[(y1 * w + x1) as usize] as f64;

        a * (1.0 - fx) * (1.0 - fy) + b * fx * (1.0 - fy) + c * (1.0 - fx) * fy + d * fx * fy
    }

    fn gradient_at(data: &[f32], w: i32, _h: i32, x: f64, y: f64) -> (f64, f64) {
        let eps = 0.5;
        let hx = Self::sample_height(data, w, _h, x + eps, y)
            - Self::sample_height(data, w, _h, x - eps, y);
        let hy = Self::sample_height(data, w, _h, x, y + eps)
            - Self::sample_height(data, w, _h, x, y - eps);
        (hx / (2.0 * eps), hy / (2.0 * eps))
    }

    fn simulate_particle(
        &self,
        rng: &mut impl Rng,
        data: &[f32],
        w: i32,
        h: i32,
        particle: &mut Particle,
    ) -> ParticleResult {
        let max_steps = 200;
        let mut actions = Vec::with_capacity(50);

        for _step in 0..max_steps {
            if particle.water <= 0.0 || particle.sediment < 0.0 {
                break;
            }

            // Clamp position to valid range.
            particle.x = particle.x.clamp(0.0, (w - 1) as f64);
            particle.y = particle.y.clamp(0.0, (h - 1) as f64);

            // Get height and gradient at current position.
            let height = Self::sample_height(data, w, h, particle.x, particle.y);
            let (gx, gy) = Self::gradient_at(data, w, h, particle.x, particle.y);

            // Update velocity with gradient + inertia.
            particle.vx = particle.vx * self.inertia + gx * self.gravity * (1.0 - self.inertia);
            particle.vy = particle.vy * self.inertia + gy * self.gravity * (1.0 - self.inertia);

            // Normalize velocity and move particle.
            let speed = (particle.vx * particle.vx + particle.vy * particle.vy).sqrt();
            if speed > 0.001 {
                let dir_x = particle.vx / speed;
                let dir_y = particle.vy / speed;
                let step_size = 1.0;
                particle.x += dir_x * step_size;
                particle.y += dir_y * step_size;

                // New height after moving.
                let new_height = Self::sample_height(data, w, h, particle.x, particle.y);
                let dh = height - new_height;

                // Sediment capacity (proportional to water volume and slope).
                let capacity = self.sediment_capacity * particle.water * speed.max(0.01);

                if dh > 0.0 {
                    // Erode: pick up sediment.
                    let to_erode = (dh * self.erosion_rate * particle.water).min(0.1);
                    let can_pickup = (capacity - particle.sediment).max(0.0).min(to_erode);

                    if can_pickup > 0.001 {
                        let xi = particle.x as i32;
                        let yi = particle.y as i32;
                        if xi >= 0 && xi < w && yi >= 0 && yi < h {
                            actions.push(ParticleAction {
                                x: xi,
                                y: yi,
                                kind: ActionKind::Erode,
                                amount: can_pickup,
                            });
                        }
                        particle.sediment += can_pickup;
                    }
                } else {
                    // Deposit: drop sediment when slope decreases.
                    let excess = (particle.sediment - capacity).max(0.0);
                    let to_deposit = excess * self.deposition_rate;
                    if to_deposit > 0.001 {
                        let xi = particle.x as i32;
                        let yi = particle.y as i32;
                        if xi >= 0 && xi < w && yi >= 0 && yi < h {
                            actions.push(ParticleAction {
                                x: xi,
                                y: yi,
                                kind: ActionKind::Deposit,
                                amount: to_deposit,
                            });
                        }
                        particle.sediment -= to_deposit;
                    }
                }

                // Evaporate.
                particle.water *= 1.0 - self.evaporation_rate;
                particle.speed = speed;
            } else {
                // Stuck: random perturbation.
                particle.vx += (rng.random::<f64>() - 0.5) * 0.1;
                particle.vy += (rng.random::<f64>() - 0.5) * 0.1;
            }
        }

        ParticleResult { actions }
    }
}

// ---------------------------------------------------------------------------
// Convenience
// ---------------------------------------------------------------------------

/// Apply both thermal and hydraulic erosion to a heightmap.
pub fn erode_both(
    hm: &mut Heightmap,
    thermal: &ThermalErosion,
    hydraulic: &HydraulicErosion,
    thermal_iters: u32,
    hydraulic_iters: u32,
) {
    if thermal_iters > 0 {
        log::info!("Thermal erosion: {thermal_iters} iterations...");
        thermal.erode(hm, thermal_iters);
    }
    if hydraulic_iters > 0 {
        log::info!("Hydraulic erosion: {hydraulic_iters} iterations...");
        hydraulic.erode(hm, hydraulic_iters);
    }
    hm.normalize();
}

#[cfg(test)]
#[path = "erosion_tests.rs"]
mod tests;
