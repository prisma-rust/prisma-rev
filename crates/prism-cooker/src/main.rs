//! # prism-cook
//!
//! Command-line 接口 for the PrismaRev 资源 管线
//!
//! ## 用法
//!
//! ```bash
//! # Initialize a new project
//! prism-cook init
//!
//! # Scan the Assets/ directory
//! prism-cook scan
//!
//! # 导入 all assets (run importers + cache)
//! prism-cook 导入
//!
//! # 构建 a .pak for distribution
//! prism-cook 构建 --output game.pak --compression 3
//!
//! # Validate an existing .pak
//! prism-cook validate game.pak
//!
//! # 列表 registered assets
//! prism-cook 列表
//!
//! # Inspect a specific 资源
//! prism-cook inspect 10001
//! ```

use prism_asset::core::{AssetId, AssetType};
use prism_asset::package::PackageBuilder;
use prism_cooker::cooker::{default_cooker_registry, profile, CookPipeline};
use prism_cooker::db::{AssetDatabase, ImportCache};
use prism_cooker::importer::{default_importer_registry, ImportPipeline};

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// 格式 a byte count for human-readable display.
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Pad a 字符串 to a 目标 terminal display 宽度 accounting for CJK
/// characters (which occupy 2 columns). If `s` is wider than 宽度 no
/// 填充 is added.
fn pad_display(s: &str, width: usize) -> String {
    let sw = UnicodeWidthStr::width(s);
    if sw >= width {
        s.to_owned()
    } else {
        let pad = " ".repeat(width - sw);
        format!("{s}{pad}")
    }
}

/// Print a bilingual key-value pair with the `/` aligned.
/// `key_en_width` is the display 宽度 to pad the EN text to, so that
/// ` / ` starts at the same 列 across all rows.
fn kv(key_en: &str, key_cn: &str, value: &str, key_en_width: usize) {
    let label = format!("{} / {}", pad_display(key_en, key_en_width), key_cn);
    println!("  {}  {}", label, value);
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "prism-cook",
    about = "PrismaRev Resource Pipeline CLI\n资源管线命令行工具"
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// .pak file to inspect / validate (shorthand for `validate`).
    /// 直接传入 .pak 路径来验证（`validate` 的快捷方式）
    #[arg(index = 1)]
    pak: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Project root (defaults to cwd).
    /// 项目根目录（默认为当前目录）
    #[arg(long, global = true)]
    project: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new project structure. 初始化新项目
    Init {
        /// Target directory (defaults to cwd). 目标目录（默认当前目录）
        dir: Option<PathBuf>,
    },

    /// Scan the Assets/ directory and 更新 the database.
    /// 扫描 Assets/ 目录并更新数据库
    Scan,

    /// Import assets (run importers). 导入资源（执行导入器）
    Import {
        /// Force re-import even if cached. 强制重新导入（跳过缓存）
        #[arg(long, short)]
        force: bool,
    },

    /// Build a .pak package. 构建 .pak 资源包
    Build {
        /// Output .pak file path. 输出 .pak 文件路径
        #[arg(long, short, default_value = "game.pak")]
        output: PathBuf,

        /// Compression level (0-22). Overrides the profile level when given.
        /// 压缩级别（0-22）；不填则使用 profile 的 level
        #[arg(long, short = 'l')]
        compression: Option<u32>,

        /// Platform target; selects the built-in profile of the same name.
        /// 目标平台（选中同名内置 profile）
        #[arg(long)]
        platform: Option<String>,

        /// Profile name to resolve (defaults to `--platform`, then `desktop`).
        /// 要解析的 profile 名（默认取 --platform，再退回 desktop）
        #[arg(long)]
        profile: Option<String>,

        /// Mark assets at or above this many uncompressed bytes as streamed.
        /// 按未压缩大小把资源标记为 streamed（覆盖 profile）
        #[arg(long)]
        stream_threshold: Option<u64>,
    },

    /// Validate a .pak file. 验证 .pak 文件
    Validate {
        /// Path to the .pak file. .pak 文件路径
        pak: PathBuf,
    },

    /// List all registered assets. 列出所有已注册资源
    List,

    /// Inspect a single asset. 查看单个资源详情
    Inspect {
        /// Asset ID or path. 资源 ID 或路径
        id: String,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn project_root(cli: &Cli) -> PathBuf {
    cli.project
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn assets_dir(root: &Path) -> PathBuf {
    root.join("Assets")
}

fn library_dir(root: &Path) -> PathBuf {
    root.join("Library")
}

fn db_path(root: &Path) -> PathBuf {
    library_dir(root).join("AssetDatabase.json")
}

fn cache_path(root: &Path) -> PathBuf {
    library_dir(root).join("import_cache.json")
}

fn intermediate_dir(root: &Path) -> PathBuf {
    library_dir(root).join("Intermediate")
}

fn intermediate_path(root: &Path, id: AssetId) -> PathBuf {
    intermediate_dir(root).join(format!("{:016x}.bin", id.0))
}

/// Resolve the profile/platform + CLI overrides into final cook settings.
///
/// Profile lookup order: explicit `--profile`, then `--platform`, then
/// `desktop`. User profiles are read from `<root>/Library/Profiles/<name>.json`;
/// when the name is neither a user file nor a built-in, cooking falls back to
/// the defaults for that platform (and says so) instead of failing the build.
fn resolve_build_settings(
    root: &Path,
    platform: Option<&str>,
    profile_name: Option<&str>,
    compression: Option<u32>,
    stream_threshold: Option<u64>,
) -> anyhow::Result<(String, profile::CookSettings)> {
    let profiles_dir = library_dir(root).join("Profiles");
    let mut profiles = profile::ProfileManager::new(&profiles_dir);
    let name = profile_name.or(platform).unwrap_or("desktop").to_owned();

    let mut settings = match profiles.resolve(&name) {
        Ok(settings) => settings,
        Err(profile::ProfileError::NotFound(missing)) => {
            eprintln!("  ⚠  unknown profile / 未知 profile: {missing} (using defaults)");
            profile::CookSettings {
                platform: missing,
                ..Default::default()
            }
        }
        Err(err) => return Err(err.into()),
    };

    let overrides = profile::CliOverrides {
        platform: platform.map(str::to_owned),
        compression_level: compression.map(|level| level.min(22) as i32),
        streaming_threshold_bytes: stream_threshold,
        ..Default::default()
    };
    profile::ProfileManager::apply_cli_overrides(&mut settings, &overrides)?;

    Ok((name, settings))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_init(dir: &Path) -> anyhow::Result<()> {
    let assets = assets_dir(dir);
    let library = library_dir(dir);
    let intermediate = intermediate_dir(dir);

    std::fs::create_dir_all(&assets)?;
    std::fs::create_dir_all(&library)?;
    std::fs::create_dir_all(&intermediate)?;

    let db = AssetDatabase::new();
    db.save(db_path(dir))?;

    let cache = ImportCache::new();
    cache.save(cache_path(dir))?;

    println!(
        "✅  Resource pipeline initialized / 资源管线已初始化: {}",
        dir.display()
    );
    println!("   Assets/    –  Place source files here / 放置源文件");
    println!("   Library/   –  Database & import cache / 数据库和导入缓存");
    Ok(())
}

fn cmd_scan(root: &Path, _cli: &Cli) -> anyhow::Result<()> {
    let assets = assets_dir(root);
    if !assets.exists() {
        anyhow::bail!("Assets/ directory not found / 目录不存在. Run 'prism-cook init' first.");
    }

    let mut db = AssetDatabase::load(db_path(root))?;
    let mut scanned = 0u32;

    walk_directory(&assets, &mut |path| {
        let relative = path
            .strip_prefix(&assets)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Skip if already known.
        if db.get_by_path(&relative).is_some() {
            return;
        }

        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let asset_type = AssetType::from_extension(extension);

        let id = db.generate_id();
        let record = prism_cooker::db::AssetRecord::new(id, relative, asset_type, "raw");
        db.insert(record).unwrap();
        scanned += 1;
    });

    db.save(db_path(root))?;
    println!(
        "✅  Scan complete / 扫描完成: {} new/新增, {} total/总计",
        scanned,
        db.len()
    );
    Ok(())
}

fn cmd_import(root: &Path, force: bool, _cli: &Cli) -> anyhow::Result<()> {
    let assets = assets_dir(root);
    if !assets.exists() {
        anyhow::bail!("Assets/ directory not found / 目录不存在. Run 'prism-cook init' first.");
    }

    let mut db = AssetDatabase::load(db_path(root))?;
    let mut cache = ImportCache::load(cache_path(root))?;

    let registry = Arc::new(default_importer_registry());
    let pipeline = ImportPipeline::new(registry);

    // 第一个 register all 源 files in the DB so we have records.
    walk_directory(&assets, &mut |path| {
        let relative = path
            .strip_prefix(&assets)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if db.get_by_path(&relative).is_none() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let asset_type = AssetType::from_extension(ext);
            let id = db.generate_id();
            let record = prism_cooker::db::AssetRecord::new(id, relative, asset_type, "raw");
            db.insert(record).ok();
        }
    });

    // Now 导入 each file.
    let count = db.len();
    let mut imported = 0u32;
    let mut cached = 0u32;

    let inter_dir = intermediate_dir(root);
    std::fs::create_dir_all(&inter_dir)?;

    // Collect records as vec since import_file borrows db mutably.
    let paths: Vec<String> = db.records().map(|r| r.path.clone()).collect();

    for relative in &paths {
        let source_path = assets.join(relative);
        if !source_path.exists() {
            continue;
        }
        let id_opt = db.id_by_path(relative);
        let force_this = force
            || id_opt
                .map(|id| !intermediate_path(root, id).exists())
                .unwrap_or(true);

        match pipeline.import_file_with_key(
            &source_path,
            relative,
            &mut db,
            &mut cache,
            None,
            force_this,
        ) {
            Ok(r) => {
                if r.was_imported {
                    imported += 1;
                    if let Some(data) = r.intermediate_data {
                        if let Some(id) = db.id_by_path(relative) {
                            let path = intermediate_path(root, id);
                            if let Err(e) = std::fs::write(&path, &data) {
                                eprintln!(
                                    "  ⚠  cannot write intermediate / 无法写入中间文件 {}: {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                } else {
                    cached += 1;
                }
            }
            Err(e) => eprintln!("  ⚠  {}: {e}", relative),
        }
    }

    db.save(db_path(root))?;
    cache.save(cache_path(root))?;

    println!(
        "✅  Import complete / 导入完成: {imported} imported/导入, {cached} cached/缓存, {count} total/总计"
    );
    Ok(())
}

fn cmd_build(
    root: &Path,
    output: &Path,
    compression: Option<u32>,
    platform: &Option<String>,
    profile_name: Option<&str>,
    stream_threshold: Option<u64>,
    _cli: &Cli,
) -> anyhow::Result<()> {
    if !db_path(root).exists() {
        anyhow::bail!("No AssetDatabase.json found / 未找到数据库. Run 'prism-cook scan' first.");
    }

    let db = AssetDatabase::load(db_path(root))?;
    if db.is_empty() {
        anyhow::bail!("No assets to build / 没有可构建的资源. Run 'prism-cook scan' first.");
    }

    // Resolve the platform/profile settings *before* cooking: the texture and
    // mesh settings used by the cookers come from here, not from hardcoded
    // defaults.
    let (resolved_profile, settings) = resolve_build_settings(
        root,
        platform.as_deref(),
        profile_name,
        compression,
        stream_threshold,
    )?;
    println!(
        "  profile / 配置: {} (platform {}, compression level {})",
        resolved_profile, settings.platform, settings.compression.level
    );

    // 加载 imported data: 从 Library/Intermediate/ 读取中间数据；
    // 若尚未导入则按需执行 importer 生成中间数据，避免直接将原始文件（如 PNG）
    // 传给期望 RTXI/RMXI 的 cooker。
    let assets = assets_dir(root);
    let inter_dir = intermediate_dir(root);
    std::fs::create_dir_all(&inter_dir)?;

    let mut db_mut = db.clone();
    let mut cache = ImportCache::load(cache_path(root)).unwrap_or_default();
    let mut dirty = false;

    let importer_reg = Arc::new(default_importer_registry());
    let import_pipeline = ImportPipeline::new(importer_reg);

    let mut asset_data: std::collections::HashMap<prism_asset::core::AssetId, Vec<u8>> =
        std::collections::HashMap::new();
    for record in db.records() {
        let inter_path = intermediate_path(root, record.id);
        if inter_path.exists() {
            if let Ok(data) = std::fs::read(&inter_path) {
                asset_data.insert(record.id, data);
                continue;
            }
        }

        let source_path = assets.join(&record.path);
        if !source_path.exists() {
            eprintln!("  ⚠  cannot read / 无法读取 {}", record.path);
            continue;
        }

        match import_pipeline.import_file_with_key(
            &source_path,
            &record.path,
            &mut db_mut,
            &mut cache,
            None,
            false,
        ) {
            Ok(r) => {
                dirty = true;
                if let Some(data) = r.intermediate_data {
                    let _ = std::fs::write(&inter_path, &data);
                    asset_data.insert(record.id, data);
                } else if let Ok(data) = std::fs::read(&inter_path) {
                    asset_data.insert(record.id, data);
                } else if let Ok(data) = std::fs::read(&source_path) {
                    asset_data.insert(record.id, data);
                }
            }
            Err(e) => {
                eprintln!("  ⚠  import failed / 导入失败 {}: {e}", record.path);
                if let Ok(data) = std::fs::read(&source_path) {
                    asset_data.insert(record.id, data);
                }
            }
        }
    }

    if dirty {
        let _ = db_mut.save(db_path(root));
        let _ = cache.save(cache_path(root));
    }
    let db = db_mut;

    let cooker_reg = default_cooker_registry();
    let pipeline = CookPipeline::new(cooker_reg);

    let mut builder = PackageBuilder::new();
    builder.set_compression(settings.compression.level.max(0));

    let summary = pipeline.cook_all(&db, &asset_data, &mut builder, &settings)?;

    let pak = builder.build()?;
    std::fs::write(output, &pak)?;

    // ── 写入 a human-readable .meta.json alongside the .pak ──────────
    //
    // This file is purely for inspection (validate / pak-info).  It
    // pairs the AssetDatabase records (which have paths, types,
    // importer names) with the per-asset sizes from the actual .pak.
    // The 运行时 never reads this file.
    let meta_path = output.with_extension("pak.meta.json");
    if let Ok(reader) = prism_asset::package::PackageReader::from_bytes(&pak) {
        let mut assets = Vec::new();
        for record in db.records() {
            let size = reader
                .find_record(record.id)
                .map(|r| (r.size, r.compressed_size))
                .unwrap_or((0, 0));
            let compressed = size.1 > 0;
            assets.push(serde_json::json!({
                "id": format!("{:#x}", record.id.into_raw()),
                "path": record.path,
                "type": record.asset_type.label(),
                "importer": record.importer_name,
                "size": size.0,
                "compressed_size": if compressed {
                    serde_json::Value::Number(size.1.into())
                } else {
                    serde_json::Value::Null
                },
                "compression_ratio": if compressed && size.0 > 0 {
                    let r = size.1 as f64 / size.0 as f64;
                    serde_json::Value::Number(serde_json::Number::from_f64(
                        (r * 100.0).round() / 100.0
                    ).unwrap_or(serde_json::Number::from(0)))
                } else {
                    serde_json::Value::Null
                },
            }));
        }
        let manifest = serde_json::json!({
            "pak": output.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
            "format": std::str::from_utf8(&reader.header().magic).unwrap_or("?"),
            "version": reader.header().version,
            "asset_count": reader.asset_count(),
            "total_size": pak.len(),
            "assets": assets,
        });
        let meta_json =
            serde_json::to_string_pretty(&manifest).map_err(|e| anyhow::anyhow!("{e}"))?;
        std::fs::write(&meta_path, &meta_json)?;
        println!("   📋  Manifest / 清单:  {}", meta_path.display());
    } else {
        eprintln!("   ⚠  Could not read back .pak for manifest / 无法回读 .pak");
    }

    println!(
        "✅  Build complete / 构建完成: {} cooked/已烹饪, {} skipped/跳过 → {} ({})",
        summary.cooked,
        summary.skipped,
        output.display(),
        format_bytes(pak.len() as u64),
    );
    Ok(())
}

fn cmd_validate(pak_path: &Path) -> anyhow::Result<()> {
    use prism_asset::package::PackageReader;

    let pak = std::fs::read(pak_path)?;
    let reader = PackageReader::from_bytes(&pak).map_err(|e| anyhow::anyhow!("{e}"))?;

    let magic_str = std::str::from_utf8(&reader.header().magic).unwrap_or("????");
    kv("📦  Magic", "标识", magic_str, 12);
    kv(
        "🔢  Version",
        "版本",
        &format!("{}", reader.header().version),
        12,
    );
    kv(
        "📊  Assets",
        "资源",
        &format!("{}", reader.asset_count()),
        12,
    );
    kv(
        "📏  Size",
        "大小",
        &format!("{} ({} bytes)", format_bytes(pak.len() as u64), pak.len()),
        12,
    );
    kv("✅  Checksum", "校验", "PASS", 12);

    // Optionally enrich with the .pak.meta.json if it 存在 alongside.
    let meta_path = pak_path.with_extension("pak.meta.json");
    let meta_assets: std::collections::HashMap<String, serde_json::Value> = (|| {
        let text = std::fs::read_to_string(&meta_path).ok()?;
        let root: serde_json::Value = serde_json::from_str(&text).ok()?;
        let arr = root.get("assets")?.as_array()?;
        let mut map = std::collections::HashMap::new();
        for entry in arr {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                map.insert(id.to_owned(), entry.clone());
            }
        }
        Some(map)
    })()
    .unwrap_or_default();

    println!();
    println!("   Assets / 资源清单:");
    println!();
    // 列 widths in display columns.
    const COL_ID: usize = 12;
    const COL_TYPE: usize = 12;
    const COL_SIZE: usize = 12;
    const COL_CSIZE: usize = 12;
    const COL_RATIO: usize = 12;
    // Header 行 — bilingual, EN padded to 5 so `/` aligns.
    let hdr =
        |en: &str, cn: &str, w: usize| pad_display(&format!("{} / {}", pad_display(en, 5), cn), w);
    println!(
        "  {}  {}  {}  {}  {}  Name / 名称",
        hdr("ID", "标识", COL_ID),
        hdr("Type", "类型", COL_TYPE),
        hdr("Size", "大小", COL_SIZE),
        hdr("Comp", "压缩", COL_CSIZE),
        hdr("Ratio", "比率", COL_RATIO),
    );
    // Separator — all columns same 宽度
    println!(
        "  {}  {}  {}  {}  {}  {}",
        "-".repeat(COL_ID),
        "-".repeat(COL_TYPE),
        "-".repeat(COL_SIZE),
        "-".repeat(COL_CSIZE),
        "-".repeat(COL_RATIO),
        "-".repeat(COL_ID),
    );
    for record in reader.records() {
        let id_hex = format!("{:#x}", record.id);
        let meta = meta_assets.get(&id_hex);

        // Best-effort 标签 from metadata, or fall 后 to 类型 name + raw ID.
        let label: String = meta
            .and_then(|m| m.get("path").and_then(|p| p.as_str()))
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(p)
                    .to_owned()
            })
            .unwrap_or_else(|| {
                let type_name = AssetType::from_u32(record.type_id).label();
                format!("({} asset {id_hex})", pad_display(type_name, 7))
            });
        let type_label = meta
            .and_then(|m| m.get("type").and_then(|t| t.as_str()))
            .unwrap_or_else(|| AssetType::from_u32(record.type_id).label());

        let ratio_str = if record.compressed_size > 0 && record.size > 0 {
            let ratio = record.compressed_size as f64 / record.size as f64;
            let pct = (ratio * 100.0).round();
            // Only show 压缩 比率 when it actually saved 空间 (< 95%).
            if pct < 95.0 {
                format!("{pct:.0}%")
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let formatted_size = format_bytes(record.size);
        let formatted_compressed = if record.compressed_size > 0 {
            format_bytes(record.compressed_size)
        } else {
            "-".to_owned()
        };

        // All columns left-aligned using Unicode-aware 填充
        println!(
            "  {}  {}  {}  {}  {}  {}",
            pad_display(&id_hex, COL_ID),
            pad_display(type_label, COL_TYPE),
            pad_display(&formatted_size, COL_SIZE),
            pad_display(&formatted_compressed, COL_CSIZE),
            pad_display(&ratio_str, COL_RATIO),
            label,
        );
    }
    // Hint about metadata when the .meta.json was absent.
    if meta_assets.is_empty() {
        println!();
        println!("💡  Tip / 提示:");
        println!("     Rebuild with current prism-cook to generate .pak.meta.json");
        println!("     重新构建以生成可读的资产清单");
    }
    Ok(())
}

fn cmd_list(root: &Path, _cli: &Cli) -> anyhow::Result<()> {
    if !db_path(root).exists() {
        anyhow::bail!("No AssetDatabase.json found / 未找到数据库. Run 'prism-cook scan' first.");
    }

    let db = AssetDatabase::load(db_path(root))?;
    println!("📋  Assets / 资源 ({} records / 条):", db.len());
    for record in db.records() {
        let type_label = format!("{:?}", record.asset_type);
        println!(
            "  {:>10}  {}  {}",
            record.id,
            pad_display(&type_label, 10),
            record.path,
        );
    }
    Ok(())
}

fn cmd_inspect(root: &Path, id_or_path: &str) -> anyhow::Result<()> {
    if !db_path(root).exists() {
        anyhow::bail!("No AssetDatabase.json found / 未找到数据库.");
    }

    let db = AssetDatabase::load(db_path(root))?;

    // Try to parse as numeric ID 第一个
    let record = if let Ok(num) = id_or_path.parse::<u64>() {
        db.get(prism_asset::core::AssetId::from_raw(num))
    } else {
        db.get_by_path(id_or_path)
    };

    match record {
        Some(r) => {
            println!("── Asset / 资源 ──────────────────────────────────");
            kv("ID", "编号", &r.id.to_string(), 11);
            kv("Path", "路径", &r.path, 11);
            kv("Type", "类型", &format!("{:?}", r.asset_type), 11);
            kv("Importer", "导入器", &r.importer_name, 11);
            kv("State", "状态", &format!("{:?}", r.state), 11);
            kv("Version", "版本", &r.version.to_string(), 11);
            kv("Source Hash", "哈希", &format!("{:#x}", r.source_hash), 11);
            kv(
                "Deps",
                "依赖",
                &format!(
                    "{} assets / {} 个资源",
                    r.dependencies.len(),
                    r.dependencies.len()
                ),
                11,
            );
            for dep in &r.dependencies {
                if let Some(dep_record) = db.get(*dep) {
                    println!("  → {}  {}", dep, dep_record.path);
                } else {
                    println!("  → {}  (missing / 缺失)", dep);
                }
            }
        }
        None => anyhow::bail!("Asset not found / 未找到资源: {id_or_path}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let root = project_root(&cli);

    match &cli.command {
        Some(Commands::Init { dir }) => cmd_init(dir.as_deref().unwrap_or(&root)),
        Some(Commands::Scan) => cmd_scan(&root, &cli),
        Some(Commands::Import { force }) => cmd_import(&root, *force, &cli),
        Some(Commands::Build {
            output,
            compression,
            platform,
            profile,
            stream_threshold,
        }) => cmd_build(
            &root,
            output,
            *compression,
            platform,
            profile.as_deref(),
            *stream_threshold,
            &cli,
        ),
        Some(Commands::Validate { pak }) => cmd_validate(pak),
        Some(Commands::List) => cmd_list(&root, &cli),
        Some(Commands::Inspect { id }) => cmd_inspect(&root, id),
        None => {
            if let Some(pak) = &cli.pak {
                cmd_validate(pak)
            } else {
                // No subcommand and no .pak → show help.
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn walk_directory(dir: &Path, cb: &mut impl FnMut(PathBuf)) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_directory(&path, cb);
            } else if path.is_file() {
                cb(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_cooker::cooker::profile::TextureCompression;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("prism-cook-{tag}-{}", std::process::id()))
    }

    #[test]
    fn build_uses_the_platform_profile() {
        // Regression for #81: `build --platform android` used to cook with
        // `CookSettings::default()` (i.e. desktop values).
        let (name, settings) = resolve_build_settings(
            &temp_root("build-android"),
            Some("android"),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(name, "android");
        assert_eq!(settings.platform, "android");
        assert_eq!(settings.texture.compression, TextureCompression::Astc8x8);
        assert_eq!(settings.texture.max_size, 2048);
        assert!(settings.mesh.vertex_compression);
        assert!(settings.streaming);
        assert_eq!(settings.compression.level, 3);
    }

    #[test]
    fn build_cli_compression_overrides_the_profile_level() {
        let (_, settings) = resolve_build_settings(
            &temp_root("build-embedded"),
            Some("embedded"),
            None,
            Some(9),
            None,
        )
        .unwrap();

        assert_eq!(settings.compression.level, 9);
        assert_eq!(settings.texture.max_size, 1024, "profile still applies");
    }

    #[test]
    fn build_profile_flag_wins_over_platform() {
        let (name, settings) = resolve_build_settings(
            &temp_root("build-profile-flag"),
            Some("android"),
            Some("desktop"),
            None,
            None,
        )
        .unwrap();

        assert_eq!(name, "desktop");
        assert_eq!(settings.texture.max_size, 4096);
        // The platform flag is still authoritative for the recorded platform.
        assert_eq!(settings.platform, "android");
    }

    #[test]
    fn build_falls_back_to_defaults_for_unknown_profiles() {
        let (name, settings) =
            resolve_build_settings(&temp_root("build-unknown"), Some("haiku"), None, None, None)
                .unwrap();

        assert_eq!(name, "haiku");
        assert_eq!(settings.platform, "haiku");
        assert_eq!(settings.texture, Default::default());
    }

    #[test]
    fn build_reads_user_profiles_from_library() {
        let root = temp_root("build-user-profile");
        let profiles_dir = library_dir(&root).join("Profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        std::fs::write(
            profiles_dir.join("hq.json"),
            serde_json::json!({
                "base": "desktop",
                "texture": { "quality": 100 }
            })
            .to_string(),
        )
        .unwrap();

        let (name, settings) = resolve_build_settings(&root, None, Some("hq"), None, None).unwrap();

        assert_eq!(name, "hq");
        assert_eq!(settings.texture.quality, 100);
        assert_eq!(settings.texture.max_size, 4096, "inherited from desktop");

        let _ = std::fs::remove_dir_all(&root);
    }
}
