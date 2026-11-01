//! CLI entry point for `prism-build-pipeline`.
//!
//! Subcommands:
//!   bake-gi    — offline GI probe-volume baker (GPU ray-query, multi-bounce path tracing)
//!   cook       — cook a `.scene.json` to RSCN v4 (delegates to prism-cooker)
//!   build-scenes — cook `scenes.json` entries and pack `scenes.pak`
//!   pack       — pack a cooked directory into a `.pak` + path manifest
//!   heightmap  — generate + erode a procedural heightmap, write raw f32

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "prism-build-pipeline", about = "Prisma build pipeline CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Offline GI probe-volume baker (GPU ray-query, multi-bounce path tracing)
    BakeGi {
        /// Output .bin path (default: assets/gi/probe_volume.bin)
        output: Option<PathBuf>,
        /// Path to .pak resource package (default: assets/scenes.pak)
        pak: Option<PathBuf>,
        /// Path to .rscn cooked scene file (default: target/scene_pack/scenes/default.rscn)
        rscn: Option<PathBuf>,
        /// Rays per probe [default: 64]
        #[arg(long, default_value_t = 64)]
        rays: u32,
        /// Max bounces [default: 3]
        #[arg(long, default_value_t = 3)]
        bounces: u32,
    },
    /// Cook a scene/asset file into cooked format
    Cook {
        /// Input file path
        input: PathBuf,
        /// Output directory (default: assets/cooked)
        #[arg(long, default_value = "assets/cooked")]
        output: PathBuf,
    },
    /// Cook all scenes in `scenes.json` and build the runtime `scenes.pak`
    ///
    /// Also migrates a legacy project (`settings.toml` / `scenes.toml`) on the
    /// first run after upgrading.
    BuildScenes {
        /// Scene manifest to read (default: assets/scenes.json)
        #[arg(long, default_value = "assets/scenes.json")]
        manifest: PathBuf,
        /// Generated staging directory (default: target/scene_pack)
        #[arg(long, default_value = "target/scene_pack")]
        staging: PathBuf,
        /// Output .pak path (default: assets/scenes.pak)
        #[arg(long, default_value = "assets/scenes.pak")]
        output: PathBuf,
        /// zstd compression level (0-22, 0 = no compression)
        #[arg(long, short = 'l', default_value_t = 3)]
        compression: u32,
        /// Mark assets at or above this many uncompressed bytes as streamed
        #[arg(long)]
        stream_threshold: Option<u64>,
        /// Split this staged directory into `optional/optional_<name>.pak`.
        /// Repeatable; uses the packaged layout (`textures`, `env`, …).
        #[arg(long = "optional-dir", value_name = "DIR")]
        optional_dirs: Vec<String>,
    },
    /// Build a .pak resource package from a directory of cooked files
    Pack {
        /// Input directory containing cooked assets
        input: PathBuf,
        /// Output .pak path (default: assets/packed/game.pak)
        #[arg(long, default_value = "assets/packed/game.pak")]
        output: PathBuf,
        /// zstd compression level (0-22, 0 = no compression)
        #[arg(long, short = 'l', default_value_t = 3)]
        compression: u32,
        /// Mark files at or above this many uncompressed bytes as streamed
        #[arg(long)]
        stream_threshold: Option<u64>,
        /// Split this input-relative directory into `optional/optional_<name>.pak`
        /// instead of the base package. Repeatable; longest prefix wins.
        #[arg(long = "optional-dir", value_name = "DIR")]
        optional_dirs: Vec<String>,
    },
    /// Generate a procedural heightmap (fbm terrain + hydraulic/thermal erosion)
    ///
    /// Writes raw f32 little-endian samples (width × height) — load in
    /// Python/numpy, Blender, or any tool that reads raw height data.
    Heightmap {
        /// Output .raw path (default: assets/heightmaps/terrain.raw)
        #[arg(long, default_value = "assets/heightmaps/terrain.raw")]
        output: PathBuf,
        /// Terrain width in texels [default: 512]
        #[arg(long, default_value_t = 512)]
        width: usize,
        /// Terrain height in texels [default: 512]
        #[arg(long, default_value_t = 512)]
        height: usize,
        /// RNG seed (same seed → same terrain) [default: 0x5EED]
        #[arg(long, default_value_t = 0x5EED)]
        seed: u64,
        /// Minimum elevation in meters (default: −11 000 m, Marianas)
        #[arg(long, default_value_t = -11_000.0)]
        min_elevation: f64,
        /// Maximum elevation in meters (default: +8 850 m, Everest)
        #[arg(long, default_value_t = 8_850.0)]
        max_elevation: f64,
        /// Erosion iterations [default: 100]
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        /// Hydraulic particle count [default: 200 000]
        #[arg(long, default_value_t = 200_000)]
        particles: usize,
        /// Thermal talus angle in degrees [default: 35]
        #[arg(long, default_value_t = 35.0)]
        talus_angle: f64,
        /// Sea level as fraction of the relative height range (0.0 = all land,
        /// 1.0 = all ocean; 0.7 ≈ Earth's 71% ocean) [default: 0.7]
        #[arg(long, default_value_t = 0.7)]
        sea_level: f64,
        /// Terrain scale in meters per pixel (default 1.0 = 512px covers 512 m).
        /// Larger values spread features over a bigger physical area.
        #[arg(long, default_value_t = 1.0)]
        terrain_scale: f64,
        /// Skip erosion entirely; output the raw generated terrain.
        #[arg(long)]
        no_erode: bool,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Command::BakeGi {
            output,
            pak,
            rscn,
            rays,
            bounces,
        } => {
            let cfg = prism_build_pipeline::BakeGiConfig {
                output_path: output.unwrap_or_else(|| PathBuf::from("assets/gi/probe_volume.bin")),
                pak_path: pak.unwrap_or_else(|| PathBuf::from("assets/scenes.pak")),
                rscn_path: rscn
                    .unwrap_or_else(|| PathBuf::from("target/scene_pack/scenes/default.rscn")),
                num_rays: rays,
                max_bounce: bounces,
                ..Default::default()
            };
            prism_build_pipeline::bake_gi(&cfg)
        }
        Command::Cook { input, output } => {
            log::info!("cook: {input:?} -> {output:?}");
            let data = std::fs::read(&input)
                .map_err(|e| anyhow::anyhow!("cook: failed to read {}: {e}", input.display()))?;

            let is_scene_json = input
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(".scene.json"))
                .unwrap_or(false);
            if !is_scene_json {
                anyhow::bail!(
                    "cook: {} is not a .scene.json; use `prism-cook` for generic assets",
                    input.display()
                );
            }

            let cooked =
                prism_cooker::cooker::scene::SceneCooker::default().cook_json_bytes(&data)?;
            std::fs::create_dir_all(&output)?;
            let stem = input
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("scene.scene.json")
                .trim_end_matches(".scene.json");
            let out_path = output.join(format!("{stem}.rscn"));
            std::fs::write(&out_path, cooked).map_err(|e| {
                anyhow::anyhow!("cook: failed to write {}: {e}", out_path.display())
            })?;
            log::info!("cook: wrote {}", out_path.display());
            Ok(())
        }
        Command::BuildScenes {
            manifest,
            staging,
            output,
            compression,
            stream_threshold,
            optional_dirs,
        } => {
            let manifest = prism_build_pipeline::resolve_scene_manifest(&manifest)?;
            log::info!(
                "build-scenes: {} -> {} -> {}",
                manifest.display(),
                staging.display(),
                output.display()
            );
            let summary = prism_build_pipeline::build_scene_package_split(
                &manifest,
                &staging,
                &output,
                prism_cooker::PackOptions {
                    compression,
                    stream_threshold_bytes: stream_threshold,
                },
                &optional_dirs,
            )?;
            log::info!(
                "build-scenes: {} scene(s), {} skipped; {} files ({} streamed), {} bytes -> {}; manifest {}",
                summary.scenes,
                summary.skipped,
                summary.pack.asset_count,
                summary.pack.streamed_assets,
                summary.pack.total_bytes,
                summary.pack.pak_bytes,
                summary.pack.manifest_path.display()
            );
            for optional in &summary.optional {
                log::info!(
                    "build-scenes: optional '{}' -> {} ({} files, {} bytes)",
                    optional.group,
                    optional.pak_path.display(),
                    optional.pack.asset_count,
                    optional.pack.pak_bytes
                );
            }
            Ok(())
        }
        Command::Pack {
            input,
            output,
            compression,
            stream_threshold,
            optional_dirs,
        } => {
            log::info!("pack: {input:?} -> {output:?}");
            let options = prism_cooker::PackOptions {
                compression,
                stream_threshold_bytes: stream_threshold,
            };
            if optional_dirs.is_empty() {
                let summary = prism_cooker::pack_directory_with_options(&input, &output, options)?;
                log::info!(
                    "pack: {} files ({} streamed), {} bytes uncompressed -> {} bytes; manifest {}",
                    summary.asset_count,
                    summary.streamed_assets,
                    summary.total_bytes,
                    summary.pak_bytes,
                    summary.manifest_path.display()
                );
            } else {
                let summary =
                    prism_cooker::pack_directory_split(&input, &output, options, &optional_dirs)?;
                log::info!(
                    "pack: base {} files ({} streamed), {} bytes; manifest {}",
                    summary.base.asset_count,
                    summary.base.streamed_assets,
                    summary.base.pak_bytes,
                    summary.base.manifest_path.display()
                );
                for optional in &summary.optional {
                    log::info!(
                        "pack: optional '{}' -> {} ({} files, {} bytes)",
                        optional.group,
                        optional.pak_path.display(),
                        optional.pack.asset_count,
                        optional.pack.pak_bytes
                    );
                }
            }
            Ok(())
        }
        Command::Heightmap {
            output,
            width,
            height,
            seed,
            min_elevation,
            max_elevation,
            iterations,
            particles,
            talus_angle,
            sea_level,
            terrain_scale,
            no_erode,
        } => {
            let t0 = std::time::Instant::now();

            // 1. 生成初始地形：频率按物理地块尺度标定
            let scale_m = width as f64 * terrain_scale;
            let terrain = prism_build_pipeline::generate_terrain(width, height, seed, scale_m);
            log::info!(
                "terrain: {}x{} seed={seed:#x} range=[{:.0}, {:.0}]",
                width,
                height,
                terrain.min_height,
                terrain.max_height
            );

            // 2. 水力 + 热力侵蚀（--no-erode 时跳过）
            let mut params = prism_build_pipeline::ErosionParams {
                iterations,
                particle_count: particles,
                talus_angle,
                seed,
                ..Default::default()
            };
            if no_erode {
                params.iterations = 0;
                params.thermal_pre_iterations = 0;
            }
            let hm = prism_build_pipeline::generate_eroded_heightmap(terrain, &params);
            log::info!(
                "eroded in {:?}: min={:.1} max={:.1} sea_level={:.1}{}",
                t0.elapsed(),
                hm.min_height,
                hm.max_height,
                hm.sea_level,
                if no_erode { " (erosion skipped)" } else { "" }
            );

            // 3. 映射到真实高程范围（如 −11 km ~ +8.85 km）
            //    侵蚀后相对高度并非 [0,1] 且分布偏斜（水力削平谷底），
            //    先归一化，再用「分位数」定位海平面——对归一化数据排序，
            //    取第 sea_frac 百分位的值作为海平面，保证海底/陆地比例
            //    严格等于 sea_level 设定（0 m 恰为海平面）。
            //    陆地段加 gamma 拉伸（<1 压低矮处展开）避免 99% 陆地挤在低处。
            let sea_frac = sea_level.clamp(0.001, 0.999);
            const LAND_GAMMA: f64 = 0.5;
            let mut abs = hm;
            abs.normalize();
            let mut sorted: Vec<f64> = abs.data.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let sea_threshold = sorted[((sorted.len() - 1) as f64 * sea_frac) as usize];
            for v in abs.data.iter_mut() {
                if *v <= sea_threshold {
                    *v = min_elevation + (*v / sea_threshold) * (0.0 - min_elevation);
                } else {
                    let t = ((*v - sea_threshold) / (1.0 - sea_threshold)).powf(LAND_GAMMA);
                    *v = t * max_elevation;
                }
            }
            abs.min_height = min_elevation;
            abs.max_height = max_elevation;
            abs.sea_level = 0.0; // 绝对高程下海平面 = 0 m
            log::info!(
                "absolute elevation: min={:.1} m, max={:.1} m, sea_level={:.1} m (sea fraction {:.0}%)",
                abs.min_height,
                abs.max_height,
                abs.sea_level,
                sea_frac * 100.0
            );

            // 4. 写出 f32 LE raw
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut buf = Vec::with_capacity(abs.data.len() * 4);
            for &v in &abs.data {
                buf.extend_from_slice(&(v as f32).to_le_bytes());
            }
            std::fs::write(&output, buf)?;
            log::info!(
                "wrote {} samples ({} bytes) to {}",
                abs.data.len(),
                abs.data.len() * 4,
                output.display()
            );
            Ok(())
        }
    }
}
