//! Directory → `.pak` packer.
//!
//! This is the low-level “already cooked files” path used by
//! `prism-build-pipeline pack`. It does **not** import or cook; callers must
//! pass a directory of runtime-ready files (`.rscn`, `.rtex`, `.rmes`, …).
//!
//! IDs are derived from each file's relative path (FNV-1a, see
//! [`AssetId::from_path`]), so adding a file to the input never renumbers the
//! assets that were already there. That matters for cooked blobs that embed
//! dependency IDs (a material stores its texture `AssetId`s), which can only
//! match the package if the packer's allocation is reproducible without
//! knowing the final file list.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use prism_asset::core::id::AssetId;
use prism_asset::core::AssetType;
use prism_asset::package::{PackageBuilder, PackageReader};

/// Summary returned by [`pack_directory`].
#[derive(Debug, Clone)]
pub struct PackSummary {
    /// Number of files packed.
    pub asset_count: usize,
    /// Number of files marked `FLAG_STREAMED`.
    pub streamed_assets: usize,
    /// Sum of the uncompressed asset sizes.
    pub total_bytes: u64,
    /// Size of the generated `.pak`.
    pub pak_bytes: u64,
    /// Path of the human-readable manifest written alongside the `.pak`.
    pub manifest_path: PathBuf,
}

/// Subdirectory (next to the base package) that receives `optional_*.pak`.
pub const OPTIONAL_PAK_DIR: &str = "optional";

/// One `optional_*.pak` produced by [`pack_directory_split`].
#[derive(Debug, Clone)]
pub struct OptionalPackSummary {
    /// Input-relative directory this package was split from.
    pub group: String,
    /// Path of the written `.pak`.
    pub pak_path: PathBuf,
    /// Summary of the packer for this package.
    pub pack: PackSummary,
}

/// Summary returned by [`pack_directory_split`].
#[derive(Debug, Clone)]
pub struct SplitPackSummary {
    /// The base package, written to the requested output path.
    pub base: PackSummary,
    /// Optional packages under `optional/`, sorted by group name.
    pub optional: Vec<OptionalPackSummary>,
}

/// Packing options for [`pack_directory_with_options`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PackOptions {
    /// zstd compression level (0–22; 0 = none).
    pub compression: u32,
    /// Mark files at or above this uncompressed size as `FLAG_STREAMED`.
    ///
    /// Streamed files are stored uncompressed so a reader can serve them
    /// directly from their offset range. `None` disables the cutoff.
    pub stream_threshold_bytes: Option<u64>,
}

/// Pack every file under `input` into `output` (a `.pak` file).
///
/// File names are sorted before ID allocation so repeated builds produce
/// byte-identical packages. Existing `.pak` files and `.pak.meta.json`
/// manifests under `input` are ignored, which makes it safe to point the
/// packer at an output directory.
pub fn pack_directory(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    compression: u32,
) -> Result<PackSummary> {
    pack_directory_with_options(
        input,
        output,
        PackOptions {
            compression,
            stream_threshold_bytes: None,
        },
    )
}

/// Like [`pack_directory`], but lets the caller mark large files as streamed
/// (#509). Marked files skip compression and carry `FLAG_STREAMED` in their
/// runtime record and `"streamed": true` in the manifest.
pub fn pack_directory_with_options(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: PackOptions,
) -> Result<PackSummary> {
    let input = input.as_ref();
    let output = output.as_ref();

    if !input.is_dir() {
        bail!("pack input is not a directory: {}", input.display());
    }

    let mut files = Vec::new();
    collect_files(input, input, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.is_empty() {
        bail!("pack input contains no files: {}", input.display());
    }

    pack_files(files, output, options)
}

/// Split a cooked directory into a base package plus `optional_*.pak` files
/// (#509).
///
/// `optional_dirs` are paths relative to `input` (for example `"audio/music"`
/// or `"textures/4k"`). Every file under one of those directories goes into
/// `optional/optional_<group>.pak` next to the base package; everything else
/// goes into `output`. Directories are matched longest-first, so `textures/4k`
/// can carve a subdirectory out of an optional `textures`.
///
/// The base package's manifest lists the optional packages under
/// `"optional_paks"`. Asset IDs are derived from the packaged relative path, so
/// splitting a directory never renumbers assets — a scene cooked against the
/// single-package layout keeps resolving after the split.
///
/// Optional packages deliberately live in an `optional/` subdirectory: the
/// runtime's automatic package scan only reads the top level, so shipping
/// `assets/optional/` does **not** mount those bytes at startup. Callers opt in
/// explicitly (for example `GpuAssetResolver::load_optional_packages`).
pub fn pack_directory_split(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: PackOptions,
    optional_dirs: &[String],
) -> Result<SplitPackSummary> {
    let input = input.as_ref();
    let output = output.as_ref();

    if !input.is_dir() {
        bail!("pack input is not a directory: {}", input.display());
    }
    if optional_dirs.is_empty() {
        bail!("pack_directory_split needs at least one optional directory");
    }

    // 长的前缀先匹配：`textures` 与 `textures/4k` 同时给出时，4k 资源只进后者。
    let mut dirs: Vec<String> = optional_dirs
        .iter()
        .map(|dir| normalize_optional_dir(dir))
        .collect::<Result<_>>()?;
    dirs.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    dirs.dedup();

    let mut files = Vec::new();
    collect_files(input, input, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.is_empty() {
        bail!("pack input contains no files: {}", input.display());
    }

    let mut base = Vec::new();
    let mut groups: Vec<(String, Vec<(String, PathBuf)>)> = Vec::new();
    for (relative, absolute) in files {
        match dirs.iter().find(|dir| path_is_under(&relative, dir)) {
            Some(dir) => match groups.iter_mut().find(|(name, _)| name == dir) {
                Some((_, group)) => group.push((relative, absolute)),
                None => groups.push((dir.clone(), vec![(relative, absolute)])),
            },
            None => base.push((relative, absolute)),
        }
    }

    if base.is_empty() {
        bail!(
            "every file under {} matched an optional directory; keep at least one file in the base package",
            input.display()
        );
    }

    let optional_dir = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(OPTIONAL_PAK_DIR);

    // 稳定顺序 + 先写可选包：base 的 manifest 要引用它们，最后写 base 才能保证
    // 清单不会指向一个还没生成 / 写失败的包。
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let mut optional = Vec::with_capacity(groups.len());
    let mut manifest_entries = Vec::with_capacity(groups.len());
    let mut seen_names = std::collections::HashSet::new();

    for (group, files) in groups {
        let stem = sanitize_group_name(&group);
        if !seen_names.insert(stem.clone()) {
            bail!(
                "optional directories collapse to the same package name '{stem}'; rename one of them"
            );
        }
        let pak_path = optional_dir.join(format!("optional_{stem}.pak"));
        let pack = pack_files(files, &pak_path, options)?;
        manifest_entries.push(serde_json::json!({
            "pak": format!("{OPTIONAL_PAK_DIR}/optional_{stem}.pak"),
            "group": group,
            "asset_count": pack.asset_count,
            "total_size": pack.pak_bytes,
            "manifest": format!("{OPTIONAL_PAK_DIR}/optional_{stem}.pak.meta.json"),
        }));
        optional.push(OptionalPackSummary {
            group,
            pak_path,
            pack,
        });
    }

    let base_pack = pack_files(base, output, options)?;

    // pack_files 已写完并读回校验基础包；这里再把可选包清单合进同一个
    // manifest。失败时基础包仍然可用，下一次构建会重写清单。
    let manifest_path = base_pack.manifest_path.clone();
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    manifest["optional_paks"] = serde_json::Value::Array(manifest_entries);
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .context("serialize .pak manifest with optional packages")?;
    fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    Ok(SplitPackSummary {
        base: base_pack,
        optional,
    })
}

/// Pack an already collected (and sorted) file list into one `.pak`.
fn pack_files(
    files: Vec<(String, PathBuf)>,
    output: &Path,
    options: PackOptions,
) -> Result<PackSummary> {
    let compression = options.compression.min(22) as i32;
    let mut builder = PackageBuilder::new();
    builder.set_compression(compression);
    let mut entries = Vec::with_capacity(files.len());
    // 32 位 serial 空间里撞 ID 的概率很小，但一旦撞了就是「两个文件共用一个
    // 句柄」，运行时会静默读到错资源 —— 宁可在构建期报错。
    let mut seen_ids: std::collections::HashMap<AssetId, String> = std::collections::HashMap::new();
    let mut total_bytes = 0u64;
    let mut streamed_assets = 0usize;

    for (relative, absolute) in files {
        let data =
            fs::read(&absolute).with_context(|| format!("read asset {}", absolute.display()))?;
        total_bytes += data.len() as u64;
        let streamed = options
            .stream_threshold_bytes
            .is_some_and(|limit| data.len() as u64 >= limit);

        let extension = absolute
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        let asset_type = AssetType::from_extension(extension);
        let id = AssetId::from_path(&relative);
        if let Some(existing) = seen_ids.insert(id, relative.clone()) {
            bail!(
                "asset id collision between '{existing}' and '{relative}' ({}); rename one of them",
                AssetId::from_path(&relative)
            );
        }
        if streamed {
            builder.add_streamed_asset(id, asset_type, data, &[]);
            streamed_assets += 1;
        } else {
            builder.add_asset(id, asset_type, data, &[]);
        }
        entries.push((id, relative, asset_type, streamed));
    }

    let pak = builder.build().context("build .pak")?;
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
    }
    // 先写同目录的临时文件再改名：`#509` 之后运行时不再把整个 `.pak` 读进内存，
    // 而是按偏移读同一个文件，就地截断+重写会让正在运行的编辑器或游戏读到半截包。
    // 同目录 rename 是原子的，旧句柄继续指向旧文件（Windows 也允许被共享删除的
    // 文件被替换）。
    let staging = pak_staging_path(output);
    fs::write(&staging, &pak).with_context(|| format!("write {}", staging.display()))?;
    if let Err(error) = fs::rename(&staging, output) {
        let _ = fs::remove_file(&staging);
        return Err(error).with_context(|| format!("replace {}", output.display()));
    }

    let reader = PackageReader::from_bytes(&pak).context("read back generated .pak")?;
    let assets: Vec<_> = entries
        .iter()
        .map(|(id, relative, asset_type, streamed)| {
            let record = reader.find_record(*id);
            let size = record.map(|record| record.size).unwrap_or(0);
            let compressed_size = record
                .map(|record| record.compressed_size)
                .filter(|&size| size > 0);
            serde_json::json!({
                "id": format!("{:#x}", id.into_raw()),
                "path": relative,
                "type": asset_type.label(),
                "size": size,
                "compressed_size": compressed_size,
                "streamed": streamed,
            })
        })
        .collect();

    let manifest = serde_json::json!({
        "pak": output.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
        "format": std::str::from_utf8(&reader.header().magic).unwrap_or("?"),
        "version": reader.header().version,
        "asset_count": reader.asset_count(),
        "streamed_count": streamed_assets,
        "total_size": pak.len(),
        "assets": assets,
    });
    let manifest_path = output.with_extension("pak.meta.json");
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("serialize .pak manifest")?;
    fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    Ok(PackSummary {
        asset_count: entries.len(),
        streamed_assets,
        total_bytes,
        pak_bytes: pak.len() as u64,
        manifest_path,
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read directory {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;

        if file_type.is_dir() {
            collect_files(root, &path, out)?;
        } else if file_type.is_file() && !is_package_artifact(&path) {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
            let relative = relative
                .to_str()
                .with_context(|| format!("asset path is not valid UTF-8: {}", path.display()))?
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
    Ok(())
}

/// Validate + normalize an optional directory prefix (`a\b\` → `a/b`).
fn normalize_optional_dir(dir: &str) -> Result<String> {
    let normalized = dir.trim().replace('\\', "/");
    // `/absolute` 在 Windows 上不算 `is_absolute()`（没有盘符），但它仍是根路径；
    // `C:` / 相对盘符也是一样 —— 三种都得在每个平台上拒绝，否则同一个参数
    // 在 CI（Linux）上被拒、在开发机上被接受。
    let rooted = normalized.starts_with('/')
        || normalized.get(1..2) == Some(":")
        || Path::new(&normalized).is_absolute();
    if rooted {
        bail!("optional directory must be relative to the input, got '{dir}'");
    }
    let normalized = normalized.trim_matches('/');
    if normalized.is_empty() || normalized == "." {
        bail!("optional directory must name a subdirectory of the input, got '{dir}'");
    }
    for component in Path::new(normalized).components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("optional directory must stay inside the input: '{dir}'");
        }
    }
    Ok(normalized.to_string())
}

/// `relative` is the directory `dir` itself or a file below it.
fn path_is_under(relative: &str, dir: &str) -> bool {
    relative
        .strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Turn an input-relative directory into a package-name-safe stem.
fn sanitize_group_name(dir: &str) -> String {
    let mut name = String::with_capacity(dir.len());
    for ch in dir.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            name.push(ch);
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    let name = name.trim_matches('_').to_ascii_lowercase();
    if name.is_empty() {
        "group".to_string()
    } else {
        name
    }
}

fn is_package_artifact(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("pak")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".pak.meta.json"))
}

/// 写入用的临时文件名：同目录、带进程号，正常路径上会在 `rename` 后消失。
fn pak_staging_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "package.pak".to_string());
    output.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_directory_replaces_the_pak_without_leaving_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(&input).unwrap();
        fs::write(input.join("a.bin"), b"first").unwrap();
        let out = temp.path().join("game.pak");

        pack_directory(&input, &out, 0).unwrap();
        let first = fs::read(&out).unwrap();

        // Overwrite an existing package: the rename must replace it in place.
        fs::write(input.join("a.bin"), b"second generation").unwrap();
        pack_directory(&input, &out, 0).unwrap();
        let second = fs::read(&out).unwrap();
        assert_ne!(first, second);

        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".pak.tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    /// #530：多一个文件不能改动已有资源的 ID，否则材质里烘进去的贴图 ID 会指错。
    #[test]
    fn adding_a_file_does_not_renumber_existing_assets() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(input.join("textures")).unwrap();
        fs::write(input.join("textures/marble.png"), b"png").unwrap();
        fs::write(input.join("materials/marble.mat"), b"mat").unwrap_err();

        fs::create_dir_all(input.join("materials")).unwrap();
        fs::write(input.join("materials/marble.mat"), b"mat").unwrap();

        let first = pack_directory(&input, temp.path().join("a.pak"), 0).unwrap();
        let before = fs::read_to_string(temp.path().join("a.pak.meta.json")).unwrap();
        let marble = AssetId::from_path("textures/marble.png");
        assert!(
            before.contains(&format!("{:#x}", marble.into_raw())),
            "manifest must use the path-derived id: {before}"
        );

        // 加一个按字典序排在前面、会推后原顺序的文件。
        fs::write(input.join("000-first.bin"), b"x").unwrap();
        let second = pack_directory(&input, temp.path().join("b.pak"), 0).unwrap();
        let after = fs::read_to_string(temp.path().join("b.pak.meta.json")).unwrap();
        assert!(after.contains(&format!("{:#x}", marble.into_raw())));
        assert_eq!(second.asset_count, first.asset_count + 1);

        // 同一份输入重打仍然字节一致。
        let again = pack_directory(&input, temp.path().join("c.pak"), 0).unwrap();
        assert_eq!(
            fs::read(temp.path().join("b.pak")).unwrap(),
            fs::read(temp.path().join("c.pak")).unwrap()
        );
        assert_eq!(again.asset_count, second.asset_count);
    }

    /// #509：超过阈值的文件带 `FLAG_STREAMED`，且即使开了压缩也保持未压缩，
    /// 这样读取端可以直接从偏移区间流式读取。
    #[test]
    fn stream_threshold_marks_large_files_and_keeps_them_uncompressed() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(&input).unwrap();
        fs::write(input.join("small.bin"), b"tiny").unwrap();
        fs::write(input.join("large.bin"), vec![7u8; 32]).unwrap();

        let output = temp.path().join("game.pak");
        let summary = pack_directory_with_options(
            &input,
            &output,
            PackOptions {
                compression: 9,
                stream_threshold_bytes: Some(16),
            },
        )
        .unwrap();

        assert_eq!(summary.asset_count, 2);
        assert_eq!(summary.streamed_assets, 1);

        let reader = PackageReader::from_bytes(&fs::read(&output).unwrap()).unwrap();
        let small = AssetId::from_path("small.bin");
        let large = AssetId::from_path("large.bin");
        assert!(!reader.is_asset_streamed(small));
        assert!(reader.is_asset_streamed(large));

        let large_record = reader.find_record(large).unwrap();
        assert!(!large_record.is_compressed());
        assert_eq!(large_record.compressed_size, 0);
        assert_eq!(large_record.size, 32);
        // A streamed asset must still round-trip through the normal read path.
        assert_eq!(
            reader.read_asset_data(large).unwrap().unwrap(),
            vec![7u8; 32]
        );

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(summary.manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["streamed_count"], 1);
        let assets = manifest["assets"].as_array().unwrap();
        let streamed: Vec<_> = assets
            .iter()
            .filter(|asset| asset["streamed"] == true)
            .map(|asset| asset["path"].as_str().unwrap())
            .collect();
        assert_eq!(streamed, vec!["large.bin"]);
    }

    #[test]
    fn pack_directory_is_deterministic_and_writes_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(input.join("scenes")).unwrap();
        fs::write(input.join("scenes/default.rscn"), b"RSCNdata").unwrap();
        fs::write(input.join("notes.bin"), b"hello").unwrap();

        let out_a = temp.path().join("a.pak");
        let out_b = temp.path().join("nested/b.pak");
        let summary = pack_directory(&input, &out_a, 0).unwrap();
        pack_directory(&input, &out_b, 0).unwrap();

        assert_eq!(summary.asset_count, 2);
        assert_eq!(summary.total_bytes, 8 + 5);
        assert!(summary.manifest_path.exists());
        assert_eq!(fs::read(&out_a).unwrap(), fs::read(&out_b).unwrap());

        let reader = PackageReader::from_bytes(&fs::read(&out_a).unwrap()).unwrap();
        assert_eq!(reader.asset_count(), 2);
        let scene = reader
            .records()
            .iter()
            .find(|record| record.type_id == AssetType::Scene.to_u32())
            .expect("rscn should be typed as Scene");
        assert_eq!(scene.size, 8);

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&summary.manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["asset_count"], 2);
        assert_eq!(manifest["assets"][0]["path"], "notes.bin");
        assert_eq!(manifest["assets"][1]["path"], "scenes/default.rscn");
    }

    #[test]
    fn pack_directory_rejects_empty_input() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("empty");
        fs::create_dir_all(&input).unwrap();
        assert!(pack_directory(&input, temp.path().join("x.pak"), 0).is_err());
    }

    /// #509：`--optional-dir` 把指定目录拆到 `optional/optional_*.pak`，基础包
    /// 只保留其余文件；最长前缀优先，嵌套目录不会同时落进两个包。
    #[test]
    fn split_pack_moves_optional_dirs_into_separate_packages() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        for (relative, bytes) in [
            ("scenes/default.rscn", b"RSCNdata".as_slice()),
            ("textures/small.rtex", b"small".as_slice()),
            ("textures/4k/big.rtex", b"big".as_slice()),
            ("audio/beep.ogg", b"beep".as_slice()),
            ("audio/music/song.ogg", b"song".as_slice()),
        ] {
            let path = input.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        let output = temp.path().join("dist/assets/game.pak");
        let summary = pack_directory_split(
            &input,
            &output,
            PackOptions {
                compression: 0,
                stream_threshold_bytes: None,
            },
            &[
                "textures".to_string(),
                "textures/4k".to_string(),
                "audio/music".to_string(),
            ],
        )
        .unwrap();

        // audio/beep 与 scenes/default 留在基础包；`textures/4k` 归属最长的前缀，
        // 不会再同时落进 `textures`。
        assert_eq!(summary.base.asset_count, 2);
        assert_eq!(summary.optional.len(), 3);
        let groups: Vec<_> = summary.optional.iter().map(|o| o.group.as_str()).collect();
        assert_eq!(groups, vec!["audio/music", "textures", "textures/4k"]);

        let audio = &summary.optional[0];
        assert_eq!(
            audio.pak_path,
            temp.path()
                .join("dist/assets/optional/optional_audio_music.pak")
        );
        assert_eq!(audio.pack.asset_count, 1);
        assert_eq!(summary.optional[1].pack.asset_count, 1);
        let big = &summary.optional[2];
        assert_eq!(
            big.pak_path,
            temp.path()
                .join("dist/assets/optional/optional_textures_4k.pak")
        );
        assert_eq!(big.pack.asset_count, 1);

        // 拆包不改资产 ID：ID 仍由包内相对路径派生。
        let big_id = AssetId::from_path("textures/4k/big.rtex");
        let base_reader = PackageReader::open(&output).unwrap();
        assert!(base_reader.find_record(big_id).is_none());
        let big_reader = PackageReader::open(&big.pak_path).unwrap();
        assert!(big_reader.find_record(big_id).is_some());

        // 基础包清单列出可选包，供按需挂载方按 group 找到文件。
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&summary.base.manifest_path).unwrap())
                .unwrap();
        let listed = manifest["optional_paks"].as_array().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0]["group"], "audio/music");
        assert_eq!(listed[0]["pak"], "optional/optional_audio_music.pak");
        assert_eq!(listed[0]["asset_count"], 1);
        assert_eq!(listed[1]["group"], "textures");
        assert_eq!(listed[1]["pak"], "optional/optional_textures.pak");
        assert_eq!(listed[2]["group"], "textures/4k");
        assert_eq!(listed[2]["pak"], "optional/optional_textures_4k.pak");
        assert_eq!(listed[2]["asset_count"], 1);
    }

    /// 拆包后每个包仍然各自应用流式阈值；基础包与可选包都不能丢 `FLAG_STREAMED`。
    #[test]
    fn split_pack_applies_stream_threshold_per_package() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(input.join("hdri")).unwrap();
        fs::write(input.join("small.bin"), b"tiny").unwrap();
        fs::write(input.join("hdri/large.hdr"), vec![9u8; 64]).unwrap();

        let output = temp.path().join("game.pak");
        let summary = pack_directory_split(
            &input,
            &output,
            PackOptions {
                compression: 9,
                stream_threshold_bytes: Some(16),
            },
            &["hdri".to_string()],
        )
        .unwrap();

        assert_eq!(summary.base.streamed_assets, 0);
        assert_eq!(summary.optional[0].pack.streamed_assets, 1);

        let reader = PackageReader::open(&summary.optional[0].pak_path).unwrap();
        let id = AssetId::from_path("hdri/large.hdr");
        assert!(reader.is_asset_streamed(id));
        let record = reader.find_record(id).unwrap();
        assert!(!record.is_compressed());
    }

    #[test]
    fn split_pack_rejects_splitting_everything_out_of_the_base() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(input.join("audio")).unwrap();
        fs::write(input.join("audio/beep.ogg"), b"beep").unwrap();

        let error = pack_directory_split(
            &input,
            temp.path().join("game.pak"),
            PackOptions::default(),
            &["audio".to_string()],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("keep at least one file"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn split_pack_rejects_optional_dirs_outside_the_input() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("cooked");
        fs::create_dir_all(&input).unwrap();
        fs::write(input.join("a.bin"), b"a").unwrap();

        for dir in ["", "..", "../outside", "/absolute"] {
            assert!(
                pack_directory_split(
                    &input,
                    temp.path().join("game.pak"),
                    PackOptions::default(),
                    &[dir.to_string()],
                )
                .is_err(),
                "'{dir}' should be rejected"
            );
        }
    }
}
