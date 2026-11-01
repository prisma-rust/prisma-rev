//! Scene package assembly: `scenes.json` + cooked RSCN → `scenes.pak`.
//!
//! Runtime scene loading resolves `scenes.json` paths through the `.pak` path
//! manifest, so the packaged layout must keep exactly the paths declared in
//! the manifest (`scenes/intro.rscn`, `scenes/default.rscn`, …). This module
//! owns that layout instead of asking users to copy files by hand.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use prism_cooker::cooker::scene::{parse_rscn_header, SceneCooker};
use prism_cooker::pack_directory;

/// 包内清单文件名——运行时按这个名字在 `.pak` 的路径清单里查找。
pub const SCENE_MANIFEST_NAME: &str = "scenes.json";

/// Summary of a completed scene-package build.
#[derive(Debug, Clone)]
pub struct ScenePackSummary {
    /// Number of scene entries cooked or copied into the package.
    pub scenes: usize,
    /// Entries skipped because they are not `.scene.json` / `.rscn` (e.g. a
    /// developer-local glTF path in `scenes.json`).
    pub skipped: usize,
    /// Summary of the underlying directory packer.
    pub pack: prism_cooker::PackSummary,
}

/// Cook every scene referenced by `manifest_path` into `staging`, rewrite the
/// manifest's authoring paths next to them, then pack `staging` into `output`.
///
/// `.scene.json` entries are cooked to the sibling `.rscn` path. `.rscn`
/// entries are re-cooked from a sibling `.scene.json` when possible, otherwise
/// copied verbatim. Non-scene entries (external glTF models) are skipped.
///
/// `staging` is a generated directory and is cleared before the build; it must
/// not contain the source manifest.
pub fn build_scene_package(
    manifest_path: impl AsRef<Path>,
    staging: impl AsRef<Path>,
    output: impl AsRef<Path>,
    compression: u32,
) -> Result<ScenePackSummary> {
    let manifest_path = manifest_path.as_ref();
    let staging = staging.as_ref();
    let output = output.as_ref();

    if !manifest_path.is_file() {
        bail!("scene manifest not found: {}", manifest_path.display());
    }
    if staging.components().all(|c| matches!(c, Component::CurDir)) {
        bail!("refusing to use the current directory as scene staging");
    }
    if manifest_path.starts_with(staging) {
        bail!(
            "scene staging {} must not contain the source manifest {}",
            staging.display(),
            manifest_path.display()
        );
    }

    // Clearing a generated staging directory is safe only when we created it.
    // The marker also survives a failed pack attempt, so the next run can retry.
    let marker = staging_marker(staging)?;
    if staging.exists() {
        let non_empty = fs::read_dir(staging)
            .with_context(|| format!("read scene staging {}", staging.display()))?
            .next()
            .is_some();
        if non_empty && !marker.is_file() {
            bail!(
                "scene staging {} is not empty and was not created by build-scenes; \
                 choose another --staging directory or remove it manually",
                staging.display()
            );
        }
    }

    let manifest_text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest = prism_engine::scene::SceneManifest::parse(&manifest_text)
        .map_err(|error| anyhow::anyhow!("parse {}: {error}", manifest_path.display()))?;
    let asset_root = manifest_path.parent().unwrap_or_else(|| Path::new("."));

    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("clear scene staging {}", staging.display()))?;
    }
    fs::create_dir_all(staging)
        .with_context(|| format!("create scene staging {}", staging.display()))?;

    let cooker = SceneCooker::default();
    let mut scenes = 0usize;
    let mut skipped = 0usize;
    for entry in &manifest.scenes {
        if !entry.enabled {
            skipped += 1;
            continue;
        }
        match stage_entry(&cooker, asset_root, staging, &entry.path)? {
            Some(_) => scenes += 1,
            None => skipped += 1,
        }
    }
    if scenes == 0 {
        bail!(
            "no cookable scenes in {} (expected .scene.json or .rscn entries)",
            manifest_path.display()
        );
    }

    // 场景引用的资源必须全部进包（#530）。任何一处解析不到的引用都会收集到
    // `missing`，构建结束时统一报错——否则打包产物看起来正常，运行起来却是
    // 空场景，只在日志里留一行 warn。
    let mut missing: Vec<String> = Vec::new();

    // 场景声明的环境贴图必须在包内，否则运行时 IBL 会退回空环境（黑屏）。
    stage_environment_assets(asset_root, staging, &manifest, &mut missing)?;

    // 2D 精灵贴图同理：`SpriteRenderer.sprite_path` 指向的 PNG 必须按
    // 运行时期望的 RTEX 格式烘进包内，否则精灵永远解析不到贴图。
    stage_sprite_assets(asset_root, staging, &manifest, &mut missing)?;
    // 3D 网格与材质（#530）：之前只打场景，导入的模型在打包后直接消失。
    stage_mesh_assets(asset_root, staging, &manifest, &mut missing)?;
    stage_material_assets(asset_root, staging, &manifest, &mut missing)?;
    // UI 组件的贴图 / 字体（#591 / #583）：`Style.image`、`Tilemap.tileset_path`、
    // `Text.font` 同样得进包，否则打包后图标、瓦片和工程字体全部消失。
    stage_ui_assets(asset_root, staging, &manifest, &mut missing)?;
    // 运行时生成的精灵（背景层、鱼、特效）不在场景 JSON 里，所以整个
    // `sprites/` 目录都要烘进包内——和引擎的「Assets 目录即内容」一致。
    stage_sprite_folder(asset_root, staging)?;

    report_missing_references(missing)?;

    // Runtime always looks up the manifest as `scenes.json` inside the package.
    // Rewrite authoring `.scene.json` paths to the cooked `.rscn` names.
    let mut packaged = manifest.clone();
    for entry in &mut packaged.scenes {
        if let Some(stem) = entry.path.strip_suffix(".scene.json") {
            entry.path = format!("{stem}.rscn");
        }
    }
    let packaged_manifest_path = staging.join(SCENE_MANIFEST_NAME);
    let packaged_manifest =
        serde_json::to_string_pretty(&packaged).context("serialize the packaged scene manifest")?;
    fs::write(&packaged_manifest_path, format!("{packaged_manifest}\n"))
        .with_context(|| format!("write {}", packaged_manifest_path.display()))?;

    fs::write(&marker, b"generated by prism-build-pipeline build-scenes\n")
        .with_context(|| format!("write {}", marker.display()))?;
    let pack = pack_directory(staging, output, compression)?;
    Ok(ScenePackSummary {
        scenes,
        skipped,
        pack,
    })
}

fn stage_entry(
    cooker: &SceneCooker,
    asset_root: &Path,
    staging: &Path,
    entry_path: &str,
) -> Result<Option<PathBuf>> {
    let Some(stem) = entry_path.strip_suffix(".scene.json") else {
        if entry_path.ends_with(".rscn") {
            let relative = safe_relative(entry_path)?;
            let source = asset_root.join(&relative);
            let source_json = source.with_extension("scene.json");
            let cooked = if source_json.is_file() {
                cook_file(cooker, &source_json)?
            } else {
                if !source.is_file() {
                    bail!(
                        "scene entry '{entry_path}' has neither {} nor {}",
                        source.display(),
                        source_json.display()
                    );
                }
                fs::read(&source).with_context(|| format!("read {}", source.display()))?
            };
            validate_rscn(entry_path, &cooked)?;
            write_staged(staging, &relative, &cooked)?;
            return Ok(Some(relative));
        }

        log::warn!("scene entry '{entry_path}' is not .scene.json/.rscn; skipping");
        return Ok(None);
    };

    safe_relative(entry_path)?;
    let source = asset_root.join(format!("{stem}.scene.json"));
    let cooked = cook_file(cooker, &source)?;
    let target = PathBuf::from(format!("{stem}.rscn"));
    validate_rscn(entry_path, &cooked)?;
    write_staged(staging, &target, &cooked)?;
    Ok(Some(target))
}

/// 把场景声明引用的环境贴图复制进 staging，路径与运行时解析结果一致。
///
/// 支持 `EnvironmentLighting.env_map` 与旧式 `Skybox.hdr_path`；路径可以是
/// 场景相对（`../valley.hdr`）或项目根相对（`assets/valley.hdr`）。
///
/// 有作者文档（`*.scene.json`）时以文档为准。只有 `.rscn` 的场景没有文档可读，
/// 此时从已暂存的 RSCN 里取 `Skybox.hdr_path`——否则这类场景打包后环境贴图
/// 不在包内，运行时 IBL 退回空环境（黑屏）。
fn stage_environment_assets(
    asset_root: &Path,
    staging: &Path,
    manifest: &prism_engine::scene::SceneManifest,
    missing: &mut Vec<String>,
) -> Result<usize> {
    let mut staged = std::collections::BTreeSet::new();

    for entry in &manifest.scenes {
        let Some(doc_rel) = scene_doc_rel(&entry.path) else {
            continue;
        };

        let mut env_values = Vec::new();
        let doc_path = asset_root.join(&doc_rel);
        if let Ok(text) = fs::read_to_string(&doc_path) {
            collect_env_values_from_doc(&text, &mut env_values);
        } else if let Ok(relative) = safe_relative(&entry.path) {
            if let Ok(cooked) = fs::read(staging.join(&relative)) {
                let path = prism_engine::scene::loader::read_env_path_from_rscn_bytes(&cooked);
                env_values.extend(path);
            }
        }

        for value in env_values {
            let Some(pak_path) = normalize_scene_relative(&doc_rel, &value) else {
                missing.push(format!(
                    "{doc_rel}: environment asset '{value}' is absolute or escapes assets/"
                ));
                continue;
            };
            if !staged.insert(pak_path.clone()) {
                continue;
            }

            let source = asset_root.join(&pak_path);
            if !source.is_file() {
                missing.push(format!(
                    "{doc_rel}: environment asset '{pak_path}' not found"
                ));
                continue;
            }
            let bytes = fs::read(&source)
                .with_context(|| format!("read environment {}", source.display()))?;
            write_staged(staging, Path::new(&pak_path), &bytes)?;
            log::info!("packaged environment asset: {pak_path}");
        }
    }

    Ok(staged.len())
}

/// 把场景里 `SpriteRenderer.sprite_path` 引用的 PNG 烘成 RTEX 并放进包内。
///
/// 运行时通过 `.pak` 路径清单按同一相对路径查找，所以 staging 里的文件名
/// 必须与组件里的 `sprite_path` 折叠结果一致（`sprites/boat.png`）。
fn stage_sprite_assets(
    asset_root: &Path,
    staging: &Path,
    manifest: &prism_engine::scene::SceneManifest,
    missing: &mut Vec<String>,
) -> Result<usize> {
    let mut staged = std::collections::BTreeSet::new();

    for entry in &manifest.scenes {
        let doc_rel = if let Some(stem) = entry.path.strip_suffix(".scene.json") {
            format!("{stem}.scene.json")
        } else if let Some(stem) = entry.path.strip_suffix(".rscn") {
            format!("{stem}.scene.json")
        } else {
            continue;
        };

        let doc_path = asset_root.join(&doc_rel);
        let Ok(text) = fs::read_to_string(&doc_path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(entities) = doc.get("entities").and_then(|value| value.as_array()) else {
            continue;
        };

        for entity in entities {
            let Some(components) = entity.get("components").and_then(|value| value.as_object())
            else {
                continue;
            };
            let Some(value) = components
                .get("prism_engine::scene::SpriteRenderer")
                .and_then(|payload| payload.get("sprite_path"))
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            let Some(pak_path) = resolve_asset_path(asset_root, &doc_rel, value) else {
                missing.push(format!(
                    "{doc_rel}: sprite_path '{value}' is absolute or escapes assets/"
                ));
                continue;
            };
            if !staged.insert(pak_path.clone()) {
                continue;
            }

            let source = asset_root.join(&pak_path);
            if !source.is_file() {
                missing.push(format!("{doc_rel}: sprite '{pak_path}' not found"));
                continue;
            }
            let cooked = cook_texture(&source)?;
            write_staged(staging, Path::new(&pak_path), &cooked)?;
            log::info!("packaged sprite asset: {pak_path}");
        }
    }

    Ok(staged.len())
}

/// 把 UI 组件引用的贴图与字体放进 staging（`#591` 背景图 / `#529` tileset / `#583` 字体）。
///
/// 贴图按运行时路径烘成 RTEX（与精灵一致），字体原样拷贝（fontdue 直接读字节）。
fn stage_ui_assets(
    asset_root: &Path,
    staging: &Path,
    manifest: &prism_engine::scene::SceneManifest,
    missing: &mut Vec<String>,
) -> Result<usize> {
    let mut textures: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut fonts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for entry in &manifest.scenes {
        let Some(doc_rel) = scene_doc_rel(&entry.path) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(asset_root.join(&doc_rel)) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(entities) = doc.get("entities").and_then(|value| value.as_array()) else {
            continue;
        };
        for entity in entities {
            let Some(components) = entity.get("components").and_then(|value| value.as_object())
            else {
                continue;
            };
            let string_field = |component: &str, field: &str| {
                components
                    .get(component)
                    .and_then(|payload| payload.get(field))
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
            };
            for (component, field) in [
                ("prism_engine::ui::Style", "image"),
                ("prism_engine::scene::Tilemap", "tileset_path"),
            ] {
                let Some(value) = string_field(component, field) else {
                    continue;
                };
                match resolve_asset_path(asset_root, &doc_rel, value) {
                    Some(pak_path)
                        if textures.insert(pak_path.clone())
                            && asset_root.join(&pak_path).is_file() =>
                    {
                        let cooked = cook_texture(&asset_root.join(&pak_path))?;
                        write_staged(staging, Path::new(&pak_path), &cooked)?;
                        log::info!("packaged UI texture: {pak_path}");
                    }
                    Some(pak_path) => {
                        if !asset_root.join(&pak_path).is_file() {
                            missing.push(format!("{doc_rel}: UI texture '{pak_path}' not found"));
                        }
                    }
                    None => missing.push(format!(
                        "{doc_rel}: UI texture '{value}' is absolute or escapes assets/"
                    )),
                }
            }
            if let Some(value) = string_field("prism_engine::ui::Text", "font") {
                match resolve_asset_path(asset_root, &doc_rel, value) {
                    Some(pak_path)
                        if fonts.insert(pak_path.clone())
                            && asset_root.join(&pak_path).is_file() =>
                    {
                        let bytes = fs::read(asset_root.join(&pak_path))
                            .with_context(|| format!("read font {pak_path}"))?;
                        write_staged(staging, Path::new(&pak_path), &bytes)?;
                        log::info!("packaged UI font: {pak_path}");
                    }
                    Some(pak_path) => {
                        if !asset_root.join(&pak_path).is_file() {
                            missing.push(format!("{doc_rel}: UI font '{pak_path}' not found"));
                        }
                    }
                    None => missing.push(format!(
                        "{doc_rel}: UI font '{value}' is absolute or escapes assets/"
                    )),
                }
            }
        }
    }

    Ok(textures.len() + fonts.len())
}

/// 把场景里 `MeshRenderer.mesh_path` 引用的模型烘成 RMES 并放进包内（#530）。
///
/// 内置基础网格（`prism_render::primitives`）不需要进包——运行时按名字直接
/// 生成。其余路径（`.gltf` / `.glb`）走与构建期相同的 importer/cooker，写进
/// staging 时**保持作者写的相对路径**，因为运行时就是按该路径在 pak 清单里查。
fn stage_mesh_assets(
    asset_root: &Path,
    staging: &Path,
    manifest: &prism_engine::scene::SceneManifest,
    missing: &mut Vec<String>,
) -> Result<usize> {
    let mut staged = std::collections::BTreeSet::new();

    for (doc_rel, components) in scene_component_maps(asset_root, manifest) {
        for entity in components {
            let Some(value) =
                component_string(&entity, "prism_engine::scene::MeshRenderer", "mesh_path")
            else {
                continue;
            };
            if prism_render::primitives::is_builtin(value) {
                continue;
            }
            let Some(pak_path) = resolve_asset_path(asset_root, &doc_rel, value) else {
                missing.push(format!(
                    "{doc_rel}: mesh_path '{value}' is absolute or escapes assets/"
                ));
                continue;
            };
            if !staged.insert(pak_path.clone()) {
                continue;
            }
            let source = asset_root.join(&pak_path);
            if !source.is_file() {
                missing.push(format!("{doc_rel}: mesh '{pak_path}' not found"));
                continue;
            }
            let cooked = cook_mesh(&source)?;
            write_staged(staging, Path::new(&pak_path), &cooked)?;
            log::info!("packaged mesh: {pak_path}");
        }
    }

    Ok(staged.len())
}

/// 把 `MeshRenderer.material_path` 的材质及其 5 张贴图放进包内（#530）。
///
/// 材质（`.mat.json` / `.mat`）里存的是贴图的 **`AssetId`**，所以贴图 ID 必须
/// 与打包器分配的一致：[`AssetId::from_path`] 按包内相对路径派生，材质 cook
/// 时把同样的 ID 写进 `RMAT`。这也是 [`pack_directory`] 改成路径派生 ID 的
/// 原因——顺序分配会让烘进去的 ID 在下一个文件出现时全部错位。
fn stage_material_assets(
    asset_root: &Path,
    staging: &Path,
    manifest: &prism_engine::scene::SceneManifest,
    missing: &mut Vec<String>,
) -> Result<usize> {
    use prism_asset::core::id::AssetId;
    use prism_asset::core::AssetType;
    use prism_cooker::cooker::profile::CookSettings;
    use prism_cooker::cooker::MaterialCooker;
    use prism_cooker::db::AssetDatabase;
    use prism_cooker::importer::MaterialImporter;
    use prism_cooker::importer::{ImportContext, Importer as _};
    use prism_cooker::Cooker as _;

    let mut staged = std::collections::BTreeSet::new();

    for (doc_rel, components) in scene_component_maps(asset_root, manifest) {
        for entity in components {
            let Some(value) = component_string(
                &entity,
                "prism_engine::scene::MeshRenderer",
                "material_path",
            ) else {
                continue;
            };
            let Some(pak_path) = resolve_asset_path(asset_root, &doc_rel, value) else {
                missing.push(format!(
                    "{doc_rel}: material_path '{value}' is absolute or escapes assets/"
                ));
                continue;
            };
            if !staged.insert(pak_path.clone()) {
                continue;
            }
            let source = asset_root.join(&pak_path);
            if !source.is_file() {
                missing.push(format!("{doc_rel}: material '{pak_path}' not found"));
                continue;
            }

            // 先把贴图路径与 ID 登记进数据库，importer 才能按槽位顺序解析依赖。
            let text =
                fs::read_to_string(&source).with_context(|| format!("read material {pak_path}"))?;
            let mut db = AssetDatabase::new();
            let mut texture_targets: Vec<String> = Vec::new();
            for raw in material_texture_paths(&text) {
                let Some(texture_pak) = resolve_asset_path(asset_root, &doc_rel, &raw) else {
                    missing.push(format!(
                        "{doc_rel}: material texture '{raw}' is absolute or escapes assets/"
                    ));
                    continue;
                };
                let texture_source = asset_root.join(&texture_pak);
                if !texture_source.is_file() {
                    missing.push(format!(
                        "{doc_rel}: material texture '{texture_pak}' not found"
                    ));
                    continue;
                }
                // 数据库键必须与 material importer 查询的字符串完全一致。
                let mut record = prism_cooker::AssetRecord::new(
                    AssetId::from_path(&texture_pak),
                    raw.replace('\\', "/"),
                    AssetType::Texture,
                    "texture-importer",
                );
                record.source_hash = texture_hash(&texture_source)?;
                let _ = db.insert(record);
                texture_targets.push(texture_pak);
            }
            let db = std::sync::Arc::new(db);

            let imported = MaterialImporter
                .import(&ImportContext {
                    source_path: source.clone(),
                    source_hash: texture_hash(&source)?,
                    settings: serde_json::json!({}),
                    db: db.clone(),
                })
                .with_context(|| format!("import material {pak_path}"))?;

            let mut record = prism_cooker::AssetRecord::new(
                AssetId::from_path(&pak_path),
                pak_path.clone(),
                AssetType::Material,
                "material-importer",
            );
            record.dependencies = imported.dependencies.clone();

            let cooked = MaterialCooker
                .cook(&prism_cooker::CookContext {
                    record: &record,
                    imported_data: &imported.output_data,
                    settings: &CookSettings::default(),
                })
                .with_context(|| format!("cook material {pak_path}"))?;
            write_staged(staging, Path::new(&pak_path), &cooked.cooked_data)?;
            log::info!("packaged material: {pak_path}");

            for texture_pak in texture_targets {
                let cooked = cook_texture(&asset_root.join(&texture_pak))?;
                write_staged(staging, Path::new(&texture_pak), &cooked)?;
                log::info!("packaged material texture: {texture_pak}");
            }
        }
    }

    Ok(staged.len())
}

/// 遍历清单里所有场景文档的组件表（`*.scene.json` 才有作者数据）。
fn scene_component_maps(
    asset_root: &Path,
    manifest: &prism_engine::scene::SceneManifest,
) -> Vec<(String, Vec<serde_json::Map<String, serde_json::Value>>)> {
    let mut out = Vec::new();
    for entry in &manifest.scenes {
        if !entry.enabled {
            continue;
        }
        let Some(doc_rel) = scene_doc_rel(&entry.path) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(asset_root.join(&doc_rel)) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let components = doc
            .get("entities")
            .and_then(|value| value.as_array())
            .map(|entities| {
                entities
                    .iter()
                    .filter_map(|entity| {
                        entity
                            .get("components")
                            .and_then(|value| value.as_object())
                            .cloned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push((doc_rel, components));
    }
    out
}

fn component_string<'a>(
    components: &'a serde_json::Map<String, serde_json::Value>,
    component: &str,
    field: &str,
) -> Option<&'a str> {
    components
        .get(component)
        .and_then(|payload| payload.get(field))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
}

/// 材质 JSON 里 5 个贴图槽（与 `MATERIAL_TEX_SLOTS` 顺序一致）。
fn material_texture_paths(text: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    [
        "albedo_tex",
        "normal_tex",
        "metallic_roughness_tex",
        "emissive_tex",
        "occlusion_tex",
    ]
    .iter()
    .filter_map(|key| value.get(*key).and_then(|slot| slot.as_str()))
    .filter(|path| !path.is_empty())
    .map(str::to_string)
    .collect()
}

fn texture_hash(path: &Path) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(hasher.finish())
}

/// 用 `GltfImporter` + `MeshCooker` 把 glTF/GLB 烘成 RMES 字节。
fn cook_mesh(source: &Path) -> Result<Vec<u8>> {
    use prism_asset::core::{AssetId, AssetType};
    use prism_cooker::cooker::profile::CookSettings;
    use prism_cooker::cooker::MeshCooker;
    use prism_cooker::importer::{GltfImporter, ImportContext, Importer as _};
    use prism_cooker::Cooker as _;

    let imported = GltfImporter
        .import(&ImportContext {
            source_path: source.to_path_buf(),
            source_hash: texture_hash(source)?,
            settings: serde_json::json!({}),
            db: std::sync::Arc::new(prism_cooker::AssetDatabase::new()),
        })
        .with_context(|| format!("import mesh {}", source.display()))?;

    let record = prism_cooker::AssetRecord::new(
        AssetId::generate(),
        source.to_string_lossy().into_owned(),
        AssetType::Mesh,
        "mesh-importer",
    );
    let cooked = MeshCooker
        .cook(&prism_cooker::CookContext {
            record: &record,
            imported_data: &imported.output_data,
            settings: &CookSettings::default(),
        })
        .with_context(|| format!("cook mesh {}", source.display()))?;
    Ok(cooked.cooked_data)
}

/// 引用了不存在 / 无法进包的资源时让构建失败（#530）。
///
/// 打包产物少一张贴图或一个模型时，运行期只会表现为「物体不出现」，没有任何
/// 报错——必须在构建期拦住。
fn report_missing_references(mut missing: Vec<String>) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    missing.dedup();
    let shown: Vec<String> = missing
        .iter()
        .take(20)
        .map(|entry| format!("  - {entry}"))
        .collect();
    let more = missing.len().saturating_sub(shown.len());
    let suffix = if more > 0 {
        format!("\n  … and {more} more")
    } else {
        String::new()
    };
    bail!(
        "{} asset reference(s) cannot be packaged; packaged builds would silently \
         miss them (fix the paths or remove the entities):\n{}{suffix}",
        missing.len(),
        shown.join("\n")
    )
}

/// 用 cooker 的 `TextureImporter` + `TextureCooker` 把源图烘成 RTEX 字节。
fn cook_texture(source: &Path) -> Result<Vec<u8>> {
    use std::hash::{Hash, Hasher};

    use prism_asset::core::AssetType;
    use prism_cooker::cooker::profile::CookSettings;
    use prism_cooker::cooker::TextureCooker;
    use prism_cooker::importer::{ImportContext, Importer as _, TextureImporter};
    use prism_cooker::Cooker as _;

    let bytes = fs::read(source).with_context(|| format!("read sprite {}", source.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let source_hash = hasher.finish();

    let imported = TextureImporter
        .import(&ImportContext {
            source_path: source.to_path_buf(),
            source_hash,
            settings: serde_json::json!({}),
            db: std::sync::Arc::new(prism_cooker::AssetDatabase::new()),
        })
        .with_context(|| format!("import sprite {}", source.display()))?;

    let record = prism_cooker::AssetRecord::new(
        prism_asset::core::AssetId::generate(),
        source.to_string_lossy().into_owned(),
        AssetType::Texture,
        "texture-importer",
    );
    // 每资产导入设置（#514）：`<asset>.import.json` 覆盖 profile 的 max_size / mips。
    let mut settings = CookSettings::default();
    prism_cooker::ImportSettings::load(source).apply(&mut settings);
    let cooked = TextureCooker
        .cook(&prism_cooker::CookContext {
            record: &record,
            imported_data: &imported.output_data,
            settings: &settings,
        })
        .with_context(|| format!("cook sprite {}", source.display()))?;
    Ok(cooked.cooked_data)
}

/// 把 `assets/sprites/` 下的所有图片烘成 RTEX 并写进 staging。
///
/// 运行时 spawn 的精灵（分层背景、鱼、水花）不经过场景 JSON，因此不能只靠
/// [`stage_sprite_assets`] 的引用扫描；整个目录都进包，路径保持一致。
fn stage_sprite_folder(asset_root: &Path, staging: &Path) -> Result<usize> {
    let root = asset_root.join("sprites");
    if !root.is_dir() {
        return Ok(0);
    }

    let mut staged = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("read sprite dir {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "tga" | "bmp") {
                continue;
            }
            let rel = path
                .strip_prefix(asset_root)
                .with_context(|| format!("sprite {} escapes assets/", path.display()))?;
            let cooked = cook_texture(&path)?;
            write_staged(staging, rel, &cooked)?;
            staged += 1;
        }
    }
    log::info!("packaged {staged} sprite file(s) from {}", root.display());
    Ok(staged)
}

/// 场景对应的作者文档路径（`.rscn` 条目映射回同名 `.scene.json`）。
fn scene_doc_rel(entry_path: &str) -> Option<String> {
    if let Some(stem) = entry_path.strip_suffix(".scene.json") {
        Some(format!("{stem}.scene.json"))
    } else {
        entry_path
            .strip_suffix(".rscn")
            .map(|stem| format!("{stem}.scene.json"))
    }
}

/// 收集作者文档里每个实体声明的环境贴图路径。
fn collect_env_values_from_doc(text: &str, out: &mut Vec<String>) {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(entities) = doc.get("entities").and_then(|value| value.as_array()) else {
        return;
    };

    for entity in entities {
        let Some(components) = entity.get("components").and_then(|value| value.as_object()) else {
            continue;
        };
        for (component, field) in [
            ("prism_engine::scene::EnvironmentLighting", "env_map"),
            ("prism_engine::scene::Skybox", "hdr_path"),
        ] {
            if let Some(value) = components
                .get(component)
                .and_then(|payload| payload.get(field))
                .and_then(|value| value.as_str())
            {
                out.push(value.to_string());
            }
        }
    }
}

/// 把 `scene_rel` 所对应场景里的相对路径折叠成 `assets/` 内路径。
///
/// 两种写法都接受，按顺序尝试：
/// 1. **场景相对**（`../sprites/boat.png`）——与 `Skybox.hdr_path` 的旧约定一致；
/// 2. **资源包相对**（`sprites/boat.png` 或 `assets/sprites/boat.png`）。
///
/// 场景相对的候选在磁盘上不存在时回落到资源包相对路径，这样组件里可以
/// 直接写包内路径（运行时 `id_by_path` 用的就是同一个字符串）。
fn normalize_scene_relative(scene_rel: &str, value: &str) -> Option<String> {
    let value = value.replace('\\', "/");
    if value.is_empty() || Path::new(&value).is_absolute() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let project_style = value.starts_with("assets/");
    if !project_style {
        if let Some((dir, _)) = scene_rel.rsplit_once('/') {
            parts.extend(
                dir.split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_string),
            );
        }
    }

    let relative = value.strip_prefix("assets/").unwrap_or(&value);
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other.to_string()),
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// 与 [`normalize_scene_relative`] 相同，但当场景相对候选在 `asset_root`
/// 下不存在时返回资源包相对路径。
fn resolve_asset_path(asset_root: &Path, scene_rel: &str, value: &str) -> Option<String> {
    let scene_relative = normalize_scene_relative(scene_rel, value)?;
    if asset_root.join(&scene_relative).is_file() {
        return Some(scene_relative);
    }
    let normalized = value.replace('\\', "/");
    let root_relative = normalized
        .strip_prefix("assets/")
        .unwrap_or(&normalized)
        .to_string();
    if !root_relative.is_empty() && asset_root.join(&root_relative).is_file() {
        return Some(root_relative);
    }
    // 两个候选都不存在：返回场景相对路径，让调用方打印原始目标。
    Some(scene_relative)
}

fn cook_file(cooker: &SceneCooker, path: &Path) -> Result<Vec<u8>> {
    let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    cooker
        .cook_json_bytes(&data)
        .with_context(|| format!("cook {}", path.display()))
}

fn validate_rscn(entry_path: &str, data: &[u8]) -> Result<()> {
    if parse_rscn_header(data).is_none() {
        bail!("scene entry '{entry_path}' did not produce a valid RSCN v4 blob");
    }
    Ok(())
}

fn write_staged(staging: &Path, relative: &Path, data: &[u8]) -> Result<()> {
    let target = staging.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    fs::write(&target, data).with_context(|| format!("write {}", target.display()))
}

fn staging_marker(staging: &Path) -> Result<PathBuf> {
    let name = staging
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("scene staging has no file name: {}", staging.display()))?;
    let mut marker_name = OsString::from(".");
    marker_name.push(name);
    marker_name.push(".prism-scene-staging");
    Ok(staging.with_file_name(marker_name))
}

fn safe_relative(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("scene entry path must be relative and stay inside the asset root: {path:?}");
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_asset::runtime::ResourceManager;

    fn write_minimal_scene(path: &Path) {
        let json = br#"{
            "version": 1,
            "entities": [{
                "name": "Cam",
                "parent": null,
                "transform": {},
                "components": {
                    "prism_engine::scene::Camera": { "fov_y_degrees": 60.0 }
                }
            }]
        }"#;
        fs::write(path, json).unwrap();
    }

    /// 把 JSON 文本写进 `assets/scenes.json`。
    fn write_manifest(assets: &Path, json: &str) {
        fs::write(assets.join("scenes.json"), json).unwrap();
    }

    /// 1×1 透明 PNG：够 cooker 解出一张 RTEX，不需要 image 依赖。
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// #591 / #583 / #529：UI 贴图、tileset 与字体都要进包。
    #[test]
    fn ui_images_tilesets_and_fonts_are_packaged() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::create_dir_all(assets.join("textures")).unwrap();
        fs::create_dir_all(assets.join("fonts")).unwrap();
        fs::write(assets.join("textures/icon.png"), TINY_PNG).unwrap();
        fs::write(assets.join("textures/tiles.png"), TINY_PNG).unwrap();
        fs::write(assets.join("fonts/hud.ttf"), b"not-a-real-ttf").unwrap();
        fs::write(
            assets.join("scenes/hud.scene.json"),
            br#"{
                "version": 1,
                "entities": [{
                    "name": "HUD",
                    "parent": null,
                    "transform": {},
                    "components": {
                        "prism_engine::ui::Node": {},
                        "prism_engine::ui::Style": { "image": "textures/icon.png" },
                        "prism_engine::ui::Text": {
                            "content": "HUD", "font": "fonts/hud.ttf"
                        },
                        "prism_engine::scene::Tilemap": {
                            "tileset_path": "textures/tiles.png"
                        }
                    }
                }]
            }"#,
        )
        .unwrap();
        write_manifest(
            &assets,
            r#"{"scenes":[{"name":"hud","path":"scenes/hud.scene.json","enabled":true}]}"#,
        );

        let output = temp.path().join("scenes.pak");
        let staging = temp.path().join("cooked");
        let summary =
            build_scene_package(assets.join("scenes.json"), &staging, &output, 0).unwrap();

        let meta_text = fs::read_to_string(summary.pack.manifest_path.clone()).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_text).unwrap();
        let paths: Vec<_> = meta["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["path"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "textures/icon.png",
            "textures/tiles.png",
            "fonts/hud.ttf",
            "scenes/hud.rscn",
        ] {
            assert!(
                paths.iter().any(|path| path == expected),
                "{expected} missing from pak paths: {paths:?}"
            );
        }

        // 字体原样进包（fontdue 直接读字节），贴图被烘成 RTEX。
        let mut rm = ResourceManager::new();
        rm.load_package_bytes(&fs::read(&output).unwrap()).unwrap();
        rm.load_path_manifest_from_str(&meta_text).unwrap();
        let font_id = rm.id_by_path("fonts/hud.ttf").expect("font path");
        assert_eq!(rm.load_with_deps_raw(font_id).unwrap(), b"not-a-real-ttf");
        let texture_id = rm.id_by_path("textures/icon.png").expect("texture path");
        let cooked = rm.load_with_deps_raw(texture_id).unwrap();
        assert_ne!(cooked, TINY_PNG, "贴图必须过 cooker");
    }

    /// 一个只有一个三角形的 glTF（buffer 内嵌为 data URI）：够 importer 解析。
    const TINY_GLTF: &str = r#"{
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0, 0, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962},
            {"buffer": 0, "byteOffset": 36, "byteLength": 6, "target": 34963}
        ],
        "buffers": [{
            "byteLength": 42,
            "uri": "data:application/octet-stream;base64,AAAAAAAAAAAAAAAAAACAPwAAAAAAAAAAAAAAAAAAgD8AAAAAAAABAAIA"
        }]
    }"#;

    /// #530：场景引用的模型、材质和材质贴图都要进包，且材质里烘的贴图 ID
    /// 必须等于打包器给同一路径分配的 ID。
    #[test]
    fn meshes_and_materials_are_packaged_with_matching_texture_ids() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::create_dir_all(assets.join("models")).unwrap();
        fs::create_dir_all(assets.join("materials")).unwrap();
        fs::create_dir_all(assets.join("textures")).unwrap();
        fs::write(assets.join("models/tri.gltf"), TINY_GLTF).unwrap();
        fs::write(assets.join("textures/marble.png"), TINY_PNG).unwrap();
        fs::write(
            assets.join("materials/marble.mat.json"),
            br#"{
                "base_color": [0.9, 0.85, 0.8, 1.0],
                "metallic": 0.0,
                "roughness": 0.4,
                "albedo_tex": "textures/marble.png"
            }"#,
        )
        .unwrap();
        fs::write(
            assets.join("scenes/room.scene.json"),
            br#"{
                "version": 1,
                "entities": [{
                    "name": "Tri",
                    "parent": null,
                    "transform": { "position": [0, 0, 0] },
                    "components": {
                        "prism_engine::scene::MeshRenderer": {
                            "mesh_path": "models/tri.gltf",
                            "material_path": "materials/marble.mat.json"
                        }
                    }
                }]
            }"#,
        )
        .unwrap();
        write_manifest(
            &assets,
            r#"{"scenes":[{"name":"room","path":"scenes/room.scene.json","enabled":true}]}"#,
        );

        let output = temp.path().join("scenes.pak");
        let staging = temp.path().join("cooked");
        let summary =
            build_scene_package(assets.join("scenes.json"), &staging, &output, 0).unwrap();

        let meta_text = fs::read_to_string(summary.pack.manifest_path.clone()).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_text).unwrap();
        let paths: Vec<String> = meta["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["path"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "models/tri.gltf",
            "materials/marble.mat.json",
            "textures/marble.png",
            "scenes/room.rscn",
        ] {
            assert!(
                paths.iter().any(|path| path == expected),
                "{expected} missing from pak paths: {paths:?}"
            );
        }

        let mut rm = ResourceManager::new();
        rm.load_package_bytes(&fs::read(&output).unwrap()).unwrap();
        rm.load_path_manifest_from_str(&meta_text).unwrap();

        let mesh_id = rm.id_by_path("models/tri.gltf").expect("mesh path");
        let mesh = rm.load_with_deps_raw(mesh_id).unwrap();
        assert_eq!(&mesh[..4], b"RMES", "网格必须被 cook 成 RMES");

        let material_id = rm
            .id_by_path("materials/marble.mat.json")
            .expect("material path");
        let material = rm.load_with_deps_raw(material_id).unwrap();
        let decoded = prism_cooker::cooker::decode_rmat(&material).expect("RMAT blob");
        let expected_texture = prism_asset::core::id::AssetId::from_path("textures/marble.png");
        assert_eq!(
            decoded.texture_ids[0],
            Some(expected_texture),
            "材质里烘的 albedo 贴图 ID 必须与包内分配一致"
        );
    }

    /// #530：引用不到的资源必须让构建失败，而不是打出一个缺资源的包。
    #[test]
    fn a_missing_mesh_reference_fails_the_build() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::write(
            assets.join("scenes/room.scene.json"),
            br#"{
                "version": 1,
                "entities": [{
                    "name": "Tri",
                    "parent": null,
                    "transform": {},
                    "components": {
                        "prism_engine::scene::MeshRenderer": {
                            "mesh_path": "models/gone.gltf"
                        }
                    }
                }]
            }"#,
        )
        .unwrap();
        write_manifest(
            &assets,
            r#"{"scenes":[{"name":"room","path":"scenes/room.scene.json","enabled":true}]}"#,
        );

        let err = build_scene_package(
            assets.join("scenes.json"),
            temp.path().join("cooked"),
            temp.path().join("scenes.pak"),
            0,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot be packaged") && message.contains("models/gone.gltf"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn builds_package_with_paths_declared_by_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        write_minimal_scene(&assets.join("scenes/intro.scene.json"));
        write_manifest(
            &assets,
            r#"{
                "scenes": [
                    {"name": "intro", "path": "scenes/intro.scene.json", "enabled": true},
                    {"name": "external", "path": "D:/external/model.gltf", "enabled": true}
                ]
            }"#,
        );

        let output = temp.path().join("scenes.pak");
        let staging = temp.path().join("cooked");
        let summary =
            build_scene_package(assets.join("scenes.json"), &staging, &output, 0).unwrap();

        assert_eq!(summary.scenes, 1);
        assert_eq!(summary.skipped, 1);
        assert!(staging.join("scenes.json").is_file());
        assert!(staging.join("scenes/intro.rscn").is_file());

        let pak = fs::read(&output).unwrap();
        let meta_text = fs::read_to_string(summary.pack.manifest_path.clone()).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_text).unwrap();
        let paths: Vec<_> = meta["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["scenes.json", "scenes/intro.rscn"]);

        let mut rm = ResourceManager::new();
        rm.load_package_bytes(&pak).unwrap();
        rm.load_path_manifest_from_str(&meta_text).unwrap();
        assert!(rm.id_by_path("scenes/intro.rscn").is_some());

        // The runtime reads this manifest from inside the package; every
        // cooked scene path it declares must resolve through the same package.
        let manifest_id = rm.id_by_path("scenes.json").expect("packaged manifest");
        let manifest_text = String::from_utf8(rm.load_with_deps_raw(manifest_id).unwrap()).unwrap();
        let packaged = prism_engine::scene::SceneManifest::parse(&manifest_text).unwrap();
        // 作者写的是 `.scene.json`，包里必须改写成 cook 后的 `.rscn`。
        assert_eq!(packaged.scenes[0].path, "scenes/intro.rscn");
        for entry in packaged
            .scenes
            .iter()
            .filter(|entry| entry.path.ends_with(".rscn"))
        {
            assert!(
                rm.id_by_path(&entry.path).is_some(),
                "scene path {} missing from package",
                entry.path
            );
        }

        // End-to-end: the packaged RSCN bytes must be loadable by the runtime
        // decoder with the same component registry used by the game.
        let scene_id = rm.id_by_path("scenes/intro.rscn").unwrap();
        let scene_bytes = rm.load_with_deps_raw(scene_id).unwrap();
        let mut registry = prism_engine::scene::ComponentRegistry::new();
        prism_engine::scene::register_builtin_components(&mut registry);
        let mut world = prism_ecs::World::new();
        let mut loader = prism_engine::scene::loader::SceneLoader::new();
        let instance = loader
            .load_and_spawn(
                &mut world,
                prism_engine::scene::loader::SceneSource::RawCooked(scene_bytes),
                &registry,
            )
            .unwrap();
        assert_eq!(instance.all_entities.len(), 1);
    }

    #[test]
    fn only_clears_staging_created_by_build_scenes() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        write_minimal_scene(&assets.join("scenes/intro.scene.json"));
        let manifest = assets.join("scenes.json");
        write_manifest(
            &assets,
            r#"{"scenes": [{"name": "intro", "path": "scenes/intro.rscn"}]}"#,
        );

        let staging = temp.path().join("cooked");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("unrelated.txt"), b"keep me").unwrap();
        let err =
            build_scene_package(&manifest, &staging, temp.path().join("a.pak"), 0).unwrap_err();
        assert!(err.to_string().contains("was not created by build-scenes"));

        fs::remove_file(staging.join("unrelated.txt")).unwrap();
        build_scene_package(&manifest, &staging, temp.path().join("a.pak"), 0).unwrap();
        assert!(staging_marker(&staging).unwrap().is_file());

        // A subsequent run may clear its own generated files.
        fs::write(staging.join("stale.rscn"), b"stale").unwrap();
        build_scene_package(&manifest, &staging, temp.path().join("b.pak"), 0).unwrap();
        assert!(!staging.join("stale.rscn").exists());
    }

    #[test]
    fn rejects_staging_that_contains_the_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("scenes.json");
        fs::write(&manifest, "{\"scenes\": []}\n").unwrap();
        let err = build_scene_package(&manifest, temp.path(), temp.path().join("scenes.pak"), 0)
            .unwrap_err();
        assert!(err.to_string().contains("must not contain"));
    }

    fn skybox_scene_json(hdr_path: &str) -> String {
        format!(
            r#"{{
                "version": 1,
                "entities": [{{
                    "name": "Sky",
                    "parent": null,
                    "transform": {{}},
                    "components": {{
                        "prism_engine::scene::Skybox": {{ "hdr_path": "{hdr_path}" }}
                    }}
                }}]
            }}"#
        )
    }

    /// A `.rscn`-only manifest entry has no authoring `.scene.json` to read, but
    /// its environment map still has to end up inside the package.
    #[test]
    fn stages_environment_for_rscn_only_scenes() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::create_dir_all(assets.join("env")).unwrap();
        fs::write(assets.join("env/valley.hdr"), b"hdr-bytes").unwrap();

        // Cook, then drop the authoring doc so only the `.rscn` remains.
        let authoring = assets.join("scenes/valley.scene.json");
        fs::write(&authoring, skybox_scene_json("../env/valley.hdr")).unwrap();
        let cooked = SceneCooker::default()
            .cook_json_bytes(&fs::read(&authoring).unwrap())
            .unwrap();
        fs::remove_file(&authoring).unwrap();
        fs::write(assets.join("scenes/valley.rscn"), &cooked).unwrap();

        write_manifest(
            &assets,
            r#"{"scenes": [{"name": "valley", "path": "scenes/valley.rscn"}]}"#,
        );

        let staging = temp.path().join("cooked");
        let output = temp.path().join("scenes.pak");
        let summary =
            build_scene_package(assets.join("scenes.json"), &staging, &output, 0).unwrap();

        assert!(
            staging.join("env/valley.hdr").is_file(),
            "environment map of a .rscn-only scene must be staged"
        );

        let meta_text = fs::read_to_string(summary.pack.manifest_path.clone()).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_text).unwrap();
        let paths: Vec<_> = meta["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"env/valley.hdr"), "paths = {paths:?}");
    }

    /// `.scene.json` entries keep working, and each HDR is staged once.
    #[test]
    fn stages_environment_declared_by_the_authoring_doc() {
        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir_all(assets.join("scenes")).unwrap();
        fs::create_dir_all(assets.join("env")).unwrap();
        fs::write(assets.join("env/sky.hdr"), b"hdr-bytes").unwrap();
        fs::write(
            assets.join("scenes/main.scene.json"),
            skybox_scene_json("../env/sky.hdr"),
        )
        .unwrap();
        write_manifest(
            &assets,
            r#"{"scenes": [{"name": "main", "path": "scenes/main.scene.json"}]}"#,
        );

        let staging = temp.path().join("cooked");
        build_scene_package(
            assets.join("scenes.json"),
            &staging,
            temp.path().join("scenes.pak"),
            0,
        )
        .unwrap();

        assert!(staging.join("env/sky.hdr").is_file());
    }
}
