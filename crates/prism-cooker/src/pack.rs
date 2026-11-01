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
use std::path::{Path, PathBuf};

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
}
