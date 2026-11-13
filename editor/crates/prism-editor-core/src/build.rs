//! 构建计划：把「构建项目」翻译成可执行的子进程步骤与产物拷贝。
//!
//! 与 Web 编辑器的 `editor_build_project` 同语义：先构建游戏 crate，再 cook
//! `assets/scenes.json`，最后把 exe / `scenes.pak` / `settings.ini` 拷到
//! `[build].output_dir`。这里只产出纯数据，真正 spawn 由 UI 层做。

use crate::settings::ProjectSettings;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    /// 运行目录（通常是项目根，`cargo run -p ...` 需要根 workspace）。
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCopy {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    pub steps: Vec<BuildStep>,
    pub copies: Vec<ArtifactCopy>,
    pub output_dir: PathBuf,
    /// 没有它就算构建失败（可执行文件）；`copies` 里缺 `scenes.pak` 之类只跳过。
    pub required: Vec<PathBuf>,
}

/// 游戏 crate 的 manifest：仓库布局优先 `projects/template`，否则项目根本身。
pub fn game_manifest_path(root: &Path) -> PathBuf {
    let nested = root.join("projects/template/Cargo.toml");
    if nested.is_file() {
        nested
    } else {
        root.join("Cargo.toml")
    }
}

fn target_profile(settings: &ProjectSettings) -> &'static str {
    if settings.build.profile.eq_ignore_ascii_case("release") {
        "release"
    } else {
        "debug"
    }
}

/// 可执行文件名：Windows 带 `.exe`。
pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "template.exe"
    } else {
        "template"
    }
}

/// 从用户工程的 `Cargo.toml` 里读 `[package] name`（读不到就退回 `template`）。
///
/// 玩家 crate 的名字不一定叫 `template`（`projects/fishing` 就不是），产物路径要跟着它走。
pub fn game_binary_name(manifest: &Path) -> String {
    let stem = std::fs::read_to_string(manifest)
        .ok()
        .and_then(|text| {
            let mut in_package = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_package = line == "[package]";
                    continue;
                }
                if !in_package {
                    continue;
                }
                let Some(rest) = line.strip_prefix("name") else {
                    continue;
                };
                let rest = rest.trim_start();
                let Some(value) = rest.strip_prefix('=') else {
                    continue;
                };
                let value = value.trim().trim_matches(['"', '\'']);
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
            None
        })
        .unwrap_or_else(|| "template".to_string());
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem
    }
}

/// 运行时需要的 `assets/` 产物：`.pak` 包、路径清单、场景清单、`settings.ini`
/// 以及工程自己在运行时读的顶层数据文件（`*.json`）。
///
/// 子目录里的工程数据（`assets/data/`）由 `build-scenes` 打进 `.pak`（#1049），
/// 这里只需要处理顶层松散文件。
///
/// 只拷**产物**，不拷作者文档（`*.scene.json` / `*.prefab.json`）与散装资产：
/// 场景内容已经在 `.pak` 里，散装资产由 cook 决定，拷目录会带走一堆源文件。
///
/// `.pak.meta.json` 是运行时解析 `AssetId` 的路径清单，缺了它所有贴图都会被判成
/// `broken asset reference`；`scenes.json` 是启动场景清单，缺了它一个场景都不加载
/// （两者都是「静默失败」，见 #989）。
fn asset_artifacts(root: &Path, output_dir: &Path) -> Vec<ArtifactCopy> {
    let assets = root.join("assets");
    let Ok(entries) = std::fs::read_dir(&assets) else {
        return Vec::new();
    };
    let mut copies: Vec<ArtifactCopy> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        let wanted = name.ends_with(".pak")
            || name.ends_with(".pak.meta.json")
            || name == "settings.ini"
            || (name.ends_with(".json")
                && !name.ends_with(".scene.json")
                && !name.ends_with(".prefab.json"));
        if !wanted {
            continue;
        }
        let to = match path.file_name() {
            Some(file) => output_dir.join("assets").join(file),
            None => continue,
        };
        copies.push(ArtifactCopy { from: path, to });
    }
    // 计划要可复现：按文件名排序，别让 `read_dir` 的顺序影响构建产物的顺序。
    copies.sort_by(|a, b| a.from.cmp(&b.from));
    copies
}

/// 找含 `crates/prism-build-pipeline` 的 workspace 根（从项目根向上找）。
///
/// 用户工程通常是**独立 workspace**（`projects/fishing/Cargo.toml` 里没有管线 crate），
/// 直接在项目根 `cargo run -p prism-build-pipeline` 会报
/// `package(s) prism-build-pipeline not found in workspace`。cook 又必须在项目根跑
/// （它按 cwd 找 `assets/`），所以要用 `--manifest-path` 指回引擎仓库。
pub fn pipeline_manifest_path(root: &Path) -> Option<PathBuf> {
    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join("crates/prism-build-pipeline/Cargo.toml").is_file() {
            return Some(dir.join("Cargo.toml"));
        }
        current = dir.parent();
    }
    None
}

fn cook_step(root: &Path) -> BuildStep {
    let mut args: Vec<String> = vec!["run".into()];
    if let Some(manifest) = pipeline_manifest_path(root) {
        args.push("--manifest-path".into());
        args.push(manifest.to_string_lossy().into_owned());
    }
    args.extend(
        ["-p", "prism-build-pipeline", "--", "build-scenes"]
            .into_iter()
            .map(str::to_string),
    );
    BuildStep {
        label: "build-scenes（cook 场景包）".into(),
        program: "cargo".into(),
        args,
        // cwd 必须是项目根：`build-scenes` 按 cwd 找 `assets/scenes.json`。
        cwd: root.to_path_buf(),
    }
}

/// 按项目根 + `[build]` 设置生成构建计划。
pub fn plan_build(root: &Path, settings: &ProjectSettings) -> BuildPlan {
    let manifest = game_manifest_path(root);
    let release = target_profile(settings) == "release";
    let mut build_args = vec![
        "build".to_string(),
        "--manifest-path".to_string(),
        manifest.to_string_lossy().into_owned(),
        // 编辑器把这两路输出解析成 Problems 面板条目；`cargo run` 不加（会污染游戏 stdout）。
        "--message-format".to_string(),
        "json".to_string(),
    ];
    if release {
        build_args.push("--release".to_string());
    }

    let output_dir = {
        let configured = PathBuf::from(settings.build.output_dir.trim());
        // 留空不能退化成「项目根」——那会把 assets/ 拷到它自己身上。
        let configured = if configured.as_os_str().is_empty() {
            PathBuf::from("build/player")
        } else {
            configured
        };
        if configured.is_absolute() {
            configured
        } else {
            root.join(configured)
        }
    };

    let game_target = manifest
        .parent()
        .unwrap_or(root)
        .join("target")
        .join(target_profile(settings));
    // 产物布局必须让 `asset_paths` 认得出 `assets/`：可执行文件放在 `output_dir/`，
    // 资源放在 `output_dir/assets/`（和 Web 编辑器、以及游戏目录的打包布局一致）。
    let binary = game_binary_name(&manifest);
    let exe = output_dir.join(&binary);
    let mut copies = vec![ArtifactCopy {
        from: game_target.join(&binary),
        to: exe.clone(),
    }];
    copies.extend(asset_artifacts(root, &output_dir));

    BuildPlan {
        steps: vec![
            BuildStep {
                label: format!("cargo build（{}）", target_profile(settings)),
                program: "cargo".into(),
                args: build_args,
                cwd: root.to_path_buf(),
            },
            cook_step(root),
        ],
        copies,
        output_dir,
        required: vec![exe],
    }
}

/// 两个路径是否指向同一个文件（按 canonicalize 比较；任一侧不存在就不算）。
fn same_file(from: &Path, to: &Path) -> bool {
    match (from.canonicalize(), to.canonicalize()) {
        (Ok(from), Ok(to)) => from == to,
        _ => from == to,
    }
}

/// 拷贝存在的产物，返回每个实际拷贝的 `from -> to`；缺失的产物跳过（不算失败）。
pub fn copy_artifacts(copies: &[ArtifactCopy]) -> io::Result<Vec<String>> {
    let mut copied = Vec::new();
    for copy in copies {
        if !copy.from.is_file() {
            continue;
        }
        // 目标就是源（`output_dir` 指回项目根）时拷自己会报错，跳过即可。
        if same_file(&copy.from, &copy.to) {
            continue;
        }
        if let Some(parent) = copy.to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&copy.from, &copy.to)?;
        copied.push(format!("{} -> {}", copy.from.display(), copy.to.display()));
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(profile: &str, output: &str) -> ProjectSettings {
        let mut settings = ProjectSettings::default();
        settings.build.profile = profile.into();
        settings.build.output_dir = output.into();
        settings
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prism-editor-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn manifest_prefers_nested_game_crate() {
        let root = temp_dir("manifest-nested");
        std::fs::create_dir_all(root.join("projects/template")).unwrap();
        std::fs::write(root.join("projects/template/Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            game_manifest_path(&root),
            root.join("projects/template/Cargo.toml")
        );

        let root = temp_dir("manifest-root");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(game_manifest_path(&root), root.join("Cargo.toml"));
    }

    /// 用户工程是独立 workspace 时，cook 步要用 `--manifest-path` 指回引擎仓库，
    /// 否则 `cargo run -p prism-build-pipeline` 在工程根直接报
    /// "package(s) `prism-build-pipeline` not found in workspace"。
    #[test]
    fn cook_step_points_back_at_the_engine_workspace() {
        let root = temp_dir("cook-standalone");
        let project = root.join("projects/fishing");
        std::fs::create_dir_all(&project).unwrap();
        let plan = plan_build(&project, &settings("debug", "build/windows"));
        assert_eq!(plan.steps[1].cwd, project);
        assert!(
            !plan.steps[1]
                .args
                .iter()
                .any(|arg| arg == "--manifest-path"),
            "没有引擎仓库时不应凭空编一个 manifest: {:?}",
            plan.steps[1].args
        );

        std::fs::create_dir_all(root.join("crates/prism-build-pipeline")).unwrap();
        std::fs::write(root.join("crates/prism-build-pipeline/Cargo.toml"), "").unwrap();
        let plan = plan_build(&project, &settings("debug", "build/windows"));
        let args = &plan.steps[1].args;
        let index = args
            .iter()
            .position(|arg| arg == "--manifest-path")
            .expect("引擎仓库在上层时应该带上 --manifest-path");
        assert_eq!(args[index + 1], root.join("Cargo.toml").to_string_lossy());
        // cook 仍然在项目根跑，否则会 cook 到仓库根自己的 assets/。
        assert_eq!(plan.steps[1].cwd, project);
        assert_eq!(args.last().map(String::as_str), Some("build-scenes"));
    }

    #[test]
    fn debug_plan_has_build_and_cook_steps() {
        let root = temp_dir("plan-debug");
        let plan = plan_build(&root, &settings("debug", "build/windows"));
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].program, "cargo");
        assert_eq!(plan.steps[0].args[0], "build");
        assert!(!plan.steps[0].args.iter().any(|arg| arg == "--release"));
        assert_eq!(
            plan.steps[1].args.last().map(String::as_str),
            Some("build-scenes")
        );
        assert_eq!(plan.steps[0].cwd, root);
        assert_eq!(plan.output_dir, root.join("build/windows"));
        // 临时目录里没有 `assets/`，所以只有 exe 一个拷贝项。
        assert_eq!(plan.copies.len(), 1);
        assert_eq!(
            plan.copies[0].from,
            root.join("target/debug").join(exe_name())
        );
        assert_eq!(plan.required, vec![plan.copies[0].to.clone()]);
    }

    /// 产物必须落在 `output_dir/assets/`：引擎只认 `<root>/assets/`，
    /// 平铺成 `output_dir/scenes.pak` 的话双击 exe 会报 "no assets directory"。
    #[test]
    fn assets_are_staged_into_an_assets_subdirectory() {
        let root = temp_dir("layout");
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("scenes")).unwrap();
        for name in [
            "scenes.pak",
            "scenes.pak.meta.json",
            "settings.ini",
            "scenes.json",
            "project_data.json",
        ] {
            std::fs::write(assets.join(name), b"x").unwrap();
        }
        // 作者文档与散装资产不该被拷进发布目录。
        std::fs::write(assets.join("scenes/lake.scene.json"), b"{}").unwrap();
        std::fs::write(assets.join("boat.png"), b"png").unwrap();

        let plan = plan_build(&root, &settings("debug", "build/windows"));
        let out = root.join("build/windows");
        assert_eq!(plan.copies[0].to, out.join(exe_name()));
        let staged: Vec<String> = plan.copies[1..]
            .iter()
            .map(|copy| copy.to.display().to_string())
            .collect();
        for name in [
            "scenes.pak",
            "scenes.pak.meta.json",
            "settings.ini",
            "scenes.json",
            "project_data.json",
        ] {
            assert!(
                staged.contains(&out.join("assets").join(name).display().to_string()),
                "{name} 没有进发布目录：{staged:?}"
            );
        }
        assert!(!staged.iter().any(|path| path.ends_with(".scene.json")));
        assert!(!staged.iter().any(|path| path.ends_with(".png")));
    }

    #[test]
    fn the_binary_name_follows_the_game_crate() {
        let root = temp_dir("binary-name");
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let manifest = root.join("projects/Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]
name = \"fishing\"
version = \"0.1.0\"
",
        )
        .unwrap();
        let expected = if cfg!(windows) {
            "fishing.exe"
        } else {
            "fishing"
        };
        assert_eq!(game_binary_name(&manifest), expected);
        assert_eq!(
            game_binary_name(&root.join("nope/Cargo.toml")),
            exe_name(),
            "读不到清单就退回 template"
        );
    }

    #[test]
    fn release_plan_uses_release_target_and_absolute_output() {
        let root = temp_dir("plan-release");
        std::fs::create_dir_all(root.join("projects/template")).unwrap();
        std::fs::write(root.join("projects/template/Cargo.toml"), "[package]\n").unwrap();
        let out = std::env::temp_dir().join("prism-editor-build-abs-out");
        let plan = plan_build(&root, &settings("release", out.to_str().unwrap()));
        assert!(plan.steps[0].args.iter().any(|arg| arg == "--release"));
        assert_eq!(
            plan.copies[0].from,
            root.join("projects/template/target/release")
                .join(exe_name())
        );
        assert_eq!(plan.output_dir, out);
    }

    #[test]
    fn a_blank_output_dir_does_not_stage_into_the_project_root() {
        let root = temp_dir("blank-output");
        let plan = plan_build(&root, &settings("debug", "   "));
        assert_eq!(plan.output_dir, root.join("build/player"));
    }

    #[test]
    fn copying_a_file_onto_itself_is_skipped() {
        let root = temp_dir("same-file");
        let from = root.join("assets/scenes.pak");
        std::fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::fs::write(&from, b"pak").unwrap();
        let copies = vec![ArtifactCopy {
            from: from.clone(),
            to: from.clone(),
        }];
        assert!(copy_artifacts(&copies).unwrap().is_empty());
        assert_eq!(std::fs::read(&from).unwrap(), b"pak");
    }

    #[test]
    fn copy_artifacts_skips_missing_and_copies_existing() {
        let root = temp_dir("copy");
        let from = root.join("assets/scenes.pak");
        std::fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::fs::write(&from, b"pak").unwrap();
        let out = root.join("build");
        let copies = vec![
            ArtifactCopy {
                from: from.clone(),
                to: out.join("scenes.pak"),
            },
            ArtifactCopy {
                from: root.join("missing.exe"),
                to: out.join("missing.exe"),
            },
        ];
        let copied = copy_artifacts(&copies).unwrap();
        assert_eq!(copied.len(), 1);
        assert!(copied[0].contains("scenes.pak"));
        assert_eq!(std::fs::read(out.join("scenes.pak")).unwrap(), b"pak");
        assert!(!out.join("missing.exe").exists());
    }
}
