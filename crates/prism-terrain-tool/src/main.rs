//! `prism-editor-tool` — Editor tools for PrismaRev.
//!
//! Subcommands:
//!   heightmap   — procedural heightmap generator with optional erosion

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use prism_editor_tool::erosion::{self, ErosionKernel};
use prism_editor_tool::export;
use prism_editor_tool::heightmap;

#[derive(Parser)]
#[command(name = "prism-editor-tool", about = "PrismaRev editor tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a procedural heightmap with optional erosion simulation.
    Heightmap {
        /// Output file path (.png or .exr)
        #[arg(short, long)]
        output: PathBuf,

        /// Width of the output heightmap in pixels
        #[arg(long, default_value_t = 1024)]
        width: u32,

        /// Height of the output heightmap in pixels
        #[arg(long, default_value_t = 1024)]
        height: u32,

        /// FBM octaves (layers of noise, more = more detail)
        #[arg(long, default_value_t = 8)]
        octaves: u32,

        /// Base frequency in cycles per pixel (smaller = larger features)
        #[arg(long, default_value_t = 0.003)]
        frequency: f64,

        /// FBM gain (amplitude decay per octave)
        #[arg(long, default_value_t = 0.5)]
        gain: f64,

        /// FBM lacunarity (frequency multiplier per octave)
        #[arg(long, default_value_t = 2.0)]
        lacunarity: f64,

        /// Use domain-warp ridge noise (MdX3Rr-style, sharp valleys + ridges)
        #[arg(long)]
        ridge: bool,

        /// Strength of domain warping ridge effect
        #[arg(long, default_value_t = 2.0)]
        warp_strength: f64,

        /// Use classic ridge noise (1 - |noise|) instead of domain warp
        #[arg(long)]
        ridge_classic: bool,

        /// Add cliff enhancement (vertical rock face band)
        #[arg(long)]
        cliff: bool,

        /// Cliff center height (0-1, normalized)
        #[arg(long, default_value_t = 0.6)]
        cliff_center: f32,

        /// Cliff transition width
        #[arg(long, default_value_t = 0.05)]
        cliff_width: f32,

        /// Cliff height addition
        #[arg(long, default_value_t = 0.15)]
        cliff_amount: f32,

        /// Erosion type: none, thermal, hydraulic, both
        #[arg(long, default_value = "none")]
        erosion: String,

        /// Thermal erosion iterations
        #[arg(long, default_value_t = 100)]
        thermal_iters: u32,

        /// Hydraulic erosion iterations
        #[arg(long, default_value_t = 5)]
        hydraulic_iters: u32,

        /// Thermal erosion talus angle in degrees
        #[arg(long, default_value_t = 30.0)]
        talus_angle: f64,

        /// Hydraulic erosion particle count per iteration
        #[arg(long, default_value_t = 100_000)]
        particles: u32,

        /// Hydraulic erosion rain amount per particle
        #[arg(long, default_value_t = 1.0)]
        rain_amount: f64,

        /// Random seed (0 = time-based)
        #[arg(long, default_value_t = 0)]
        seed: u64,

        /// Number of threads for parallel processing (0 = auto)
        #[arg(long, default_value_t = 0)]
        threads: usize,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Command::Heightmap {
            output,
            width,
            height,
            octaves,
            frequency,
            gain,
            lacunarity,
            ridge,
            warp_strength,
            ridge_classic,
            cliff,
            cliff_center,
            cliff_width,
            cliff_amount,
            erosion,
            thermal_iters,
            hydraulic_iters,
            talus_angle,
            particles,
            rain_amount,
            seed,
            threads,
        } => cmd_heightmap(
            output,
            width,
            height,
            octaves,
            frequency,
            gain,
            lacunarity,
            ridge,
            warp_strength,
            ridge_classic,
            cliff,
            cliff_center,
            cliff_width,
            cliff_amount,
            &erosion,
            thermal_iters,
            hydraulic_iters,
            talus_angle,
            particles,
            rain_amount,
            seed,
            threads,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_heightmap(
    output: PathBuf,
    width: u32,
    height: u32,
    octaves: u32,
    frequency: f64,
    gain: f64,
    lacunarity: f64,
    ridge: bool,
    warp_strength: f64,
    ridge_classic: bool,
    cliff: bool,
    cliff_center: f32,
    cliff_width: f32,
    cliff_amount: f32,
    erosion_type: &str,
    thermal_iters: u32,
    hydraulic_iters: u32,
    talus_angle: f64,
    particles: u32,
    rain_amount: f64,
    seed: u64,
    threads: usize,
) -> Result<()> {
    // Configure thread pool.
    if threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .ok();
    }

    let seed = if seed == 0 { None } else { Some(seed) };

    // Build generation config.
    let gen_cfg = heightmap::HeightmapConfig {
        width,
        height,
        octaves,
        frequency,
        gain,
        lacunarity,
        ridge,
        warp_strength,
        ridge_classic,
        cliff,
        cliff_center,
        cliff_width,
        cliff_amount,
        seed,
    };

    log::info!(
        "Generating heightmap: {}x{}, {} octaves, ridge={}, cliff={}",
        width,
        height,
        octaves,
        ridge,
        cliff
    );
    let mut hm = heightmap::generate_heightmap(&gen_cfg);
    log::info!(
        "Heightmap generated (range: {:.4}–{:.4})",
        hm.data.iter().copied().fold(f32::MAX, f32::min),
        hm.data.iter().copied().fold(f32::MIN, f32::max),
    );

    // Erosion.
    match erosion_type.to_lowercase().as_str() {
        "none" => {}
        "thermal" => {
            let thermal = erosion::ThermalErosion {
                talus_angle_degrees: talus_angle,
                ..Default::default()
            };
            thermal.erode(&mut hm, thermal_iters);
            log::info!("Thermal erosion complete ({thermal_iters} iters)");
        }
        "hydraulic" => {
            let hydraulic = erosion::HydraulicErosion {
                particle_count: particles,
                rain_amount,
                ..Default::default()
            };
            hydraulic.erode(&mut hm, hydraulic_iters);
            log::info!(
                "Hydraulic erosion complete ({hydraulic_iters} iters, {particles} particles)"
            );
        }
        "both" => {
            let thermal = erosion::ThermalErosion {
                talus_angle_degrees: talus_angle,
                ..Default::default()
            };
            let hydraulic = erosion::HydraulicErosion {
                particle_count: particles,
                rain_amount,
                ..Default::default()
            };
            erosion::erode_both(
                &mut hm,
                &thermal,
                &hydraulic,
                thermal_iters,
                hydraulic_iters,
            );
            log::info!(
                "Both erosion complete (thermal={thermal_iters}, hydraulic={hydraulic_iters})"
            );
        }
        other => anyhow::bail!(
            "unknown erosion type '{other}', expected: none, thermal, hydraulic, both"
        ),
    }

    // Detect format from extension, fall back to EXR.
    let fmt = export::format_from_extension(&output).unwrap_or(export::ExportFormat::Exr);

    export::export_heightmap(&hm, &output, fmt)?;

    log::info!("Done! → {output:?}");
    Ok(())
}
