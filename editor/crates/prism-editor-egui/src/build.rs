//! 构建 / Cook：以**子进程**调用 `prism-build-pipeline`（与 Web 编辑器同一路径）。
//!
//! 编辑器不链接 cooker/build 流水线（AGENTS.md 的硬规则），所以这里只 spawn
//! `cargo run -p prism-build-pipeline -- …`，把 stdout/stderr 逐行送回 Console。
//! 编译/打包可能几十秒，全部跑在后台线程，UI 线程只 `try_recv`。
//!
//! 输出是**流式**的：stdout / stderr 各有一个读线程，`cargo` 每写一行就进 Console，
//! 不再等进程退出才一次性出现。`BuildJob::cancel()` 会杀掉子进程并让它正常收尾。
//! 一个任务可能包含多步命令（玩家构建先 cook 场景），每一步都走同一条流式路径。

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::diagnostics::{parse_cargo_line, Diagnostic, MAX_DIAGNOSTICS};
use prism_editor_core::build::{copy_artifacts, plan_build, BuildPlan};
use prism_editor_core::settings::ProjectSettings;

/// 一次构建任务。
pub struct BuildJob {
    rx: Receiver<String>,
    label: String,
    running: bool,
    cancel: Arc<AtomicBool>,
    /// `cargo build` 的 JSON 诊断；Console 仍然收到一行摘要，Problems 面板拿结构化数据。
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    /// 有没有步骤失败（非 0 退出 / 产物缺失 / spawn 失败）。
    failed: Arc<AtomicBool>,
}

pub enum BuildKind {
    /// `prism-build-pipeline build-scenes`（所有启用的场景 → `scenes.pak`）。
    ScenePack,
    /// `prism-build-pipeline cook <scene>`（单个 `.scene.json` → `.rscn`）。
    Cook(PathBuf),
    /// 构建用户项目（`File → Build Player`）：按 `[build]` 设置选 profile，
    /// 再 cook 场景包、按 `output_dir` 摆好 exe + `assets/`。
    Player,
    /// 同 [`BuildKind::Player`]，构建完直接跑 `output_dir/<exe>`（`Build And Run`）。
    PlayerRun,
    /// `cargo clean` 玩家工程（引擎的 Clean Build）。
    ///
    /// 玩家是**独立 workspace**（`projects/<p>/target`），清理它才是 Clean Build 的本意；
    /// 清仓库根的 `target/` 会连带删掉引擎和所有 crate 的产物，属于另一件事。
    Clean,
    /// `cargo clean` 仓库根 workspace（引擎 + 全部 crate 的编译缓存）。
    CleanEngine,
    /// `prism-build-pipeline bake-gi`：离线 GI 探针体烘焙（Offline Tools，#477）。
    BakeGi {
        output: String,
        rays: u32,
        bounces: u32,
    },
    /// `prism-build-pipeline heightmap`：程序化地形 + 侵蚀（Offline Tools，#477）。
    Heightmap {
        output: String,
        width: u32,
        height: u32,
        seed: u64,
        iterations: u32,
        sea_level: f32,
    },
    /// `prism-build-pipeline build-scenes --output … --compression …`：自定义打包（#477）。
    ScenePackCustom { output: String, compression: u32 },
}

pub(crate) fn game_manifest_path(root: &Path) -> PathBuf {
    let nested = root.join("projects/prisma/Cargo.toml");
    if nested.is_file() {
        nested
    } else {
        root.join("Cargo.toml")
    }
}

fn pipeline_manifest_path(root: &Path) -> Option<PathBuf> {
    let mut current = Some(root);
    while let Some(dir) = current {
        let manifest = dir.join("crates/prism-build-pipeline/Cargo.toml");
        if manifest.is_file() {
            return Some(dir.join("Cargo.toml"));
        }
        current = dir.parent();
    }
    None
}

/// 读 `[build]` 设置并生成玩家构建计划；设置文件坏了就用默认值（debug + `build/`）。
fn player_plan(root: &Path) -> BuildPlan {
    let settings = ProjectSettings::read(root).unwrap_or_default();
    plan_build(root, &settings)
}

impl BuildKind {
    pub fn label(&self) -> String {
        match self {
            BuildKind::ScenePack => "build-scenes".to_string(),
            BuildKind::Cook(path) => format!("cook {}", path.display()),
            BuildKind::Player => "player build".to_string(),
            BuildKind::PlayerRun => "player build + run".to_string(),
            BuildKind::Clean => "cargo clean (player)".to_string(),
            BuildKind::CleanEngine => "cargo clean (engine)".to_string(),
            BuildKind::BakeGi {
                output,
                rays,
                bounces,
            } => format!("bake-gi {rays} rays / {bounces} bounces → {output}"),
            BuildKind::Heightmap {
                output,
                width,
                height,
                ..
            } => format!("heightmap {width}x{height} → {output}"),
            BuildKind::ScenePackCustom { output, .. } => format!("build-scenes → {output}"),
        }
    }

    /// 玩家构建（需要 cook + 摆产物 + 可选运行）而不是单纯的 cargo 步骤。
    pub fn is_player_build(&self) -> bool {
        matches!(self, BuildKind::Player | BuildKind::PlayerRun)
    }

    /// 这个任务构建完要不要直接启动产物。
    pub fn runs_after_build(&self) -> bool {
        matches!(self, BuildKind::PlayerRun)
    }

    /// 这个任务要顺序执行的 cargo 命令。玩家构建先刷新 `scenes.pak`，
    /// 否则会跑上一次 cook 的场景。
    fn commands(&self, root: &Path) -> Result<Vec<Vec<String>>, String> {
        let pipeline = |extra: &[&str]| -> Result<Vec<String>, String> {
            let manifest = pipeline_manifest_path(root)
                .ok_or_else(|| "cannot find prism-build-pipeline workspace".to_string())?;
            let mut args = vec![
                "run".to_string(),
                "--manifest-path".to_string(),
                manifest.display().to_string(),
                "-p".to_string(),
                "prism-build-pipeline".to_string(),
                "--".to_string(),
            ];
            args.extend(extra.iter().map(|arg| arg.to_string()));
            Ok(args)
        };
        // 玩家构建复用数据层的 `BuildPlan`（profile / output_dir / 产物布局都在那里），
        // 这里只是把它的步骤翻译成 cargo 参数。
        let player_commands = || -> Result<Vec<Vec<String>>, String> {
            Ok(player_plan(root)
                .steps
                .into_iter()
                .map(|step| step.args)
                .collect())
        };
        Ok(match self {
            BuildKind::ScenePack => vec![pipeline(&["build-scenes"])?],
            BuildKind::Cook(path) => vec![pipeline(&["cook", &path.display().to_string()])?],
            BuildKind::Player | BuildKind::PlayerRun => player_commands()?,
            BuildKind::Clean => vec![vec![
                "clean".to_string(),
                "--manifest-path".to_string(),
                game_manifest_path(root).display().to_string(),
            ]],
            BuildKind::CleanEngine => vec![vec![
                "clean".to_string(),
                "--manifest-path".to_string(),
                root.join("Cargo.toml").display().to_string(),
            ]],
            BuildKind::BakeGi {
                output,
                rays,
                bounces,
            } => {
                let rays = rays.to_string();
                let bounces = bounces.to_string();
                vec![pipeline(&[
                    "bake-gi",
                    output,
                    "--rays",
                    &rays,
                    "--bounces",
                    &bounces,
                ])?]
            }
            BuildKind::Heightmap {
                output,
                width,
                height,
                seed,
                iterations,
                sea_level,
            } => {
                let (width, height) = (width.to_string(), height.to_string());
                let seed = format!("{seed}");
                let iterations = iterations.to_string();
                let sea_level = format!("{sea_level}");
                vec![pipeline(&[
                    "heightmap",
                    "--output",
                    output,
                    "--width",
                    &width,
                    "--height",
                    &height,
                    "--seed",
                    &seed,
                    "--iterations",
                    &iterations,
                    "--sea-level",
                    &sea_level,
                ])?]
            }
            BuildKind::ScenePackCustom {
                output,
                compression,
            } => {
                let compression = compression.to_string();
                vec![pipeline(&[
                    "build-scenes",
                    "--output",
                    output,
                    "--compression",
                    &compression,
                ])?]
            }
        })
    }
}

/// 把一个管道逐行转发到 channel；写端关闭时线程自然结束。
fn forward_lines(
    stream: impl Read + Send + 'static,
    tx: Sender<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    })
}

/// `cargo --message-format=json` 的 stdout：解析成诊断，
/// 同时在 Console 留一行人类可读的摘要（JSON 本身没人想看）。
fn forward_cargo_json(
    stream: impl Read + Send + 'static,
    tx: Sender<String>,
    root: PathBuf,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                break;
            };
            let Some(diagnostic) = parse_cargo_line(&line, &root) else {
                // cargo 偶尔混入非 JSON 行；JSON 事件本身已经由诊断/摘要覆盖。
                if !line.trim_start().starts_with('{') && tx.send(line).is_err() {
                    break;
                }
                continue;
            };
            let text = diagnostic.console_line();
            let stored = {
                let mut diagnostics = diagnostics.lock().unwrap();
                if diagnostics.len() < MAX_DIAGNOSTICS {
                    diagnostics.push(diagnostic);
                    true
                } else {
                    false
                }
            };
            if stored && tx.send(text).is_err() {
                break;
            }
        }
    })
}

impl BuildJob {
    /// 启动任务。`root` 是项目根（含 `assets/` 与根 `Cargo.toml`）。
    pub fn spawn(kind: BuildKind, root: &Path) -> Self {
        let label = kind.label();
        let (tx, rx) = channel();
        let root = root.to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let worker_diagnostics = Arc::clone(&diagnostics);
        let failed = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        std::thread::spawn(move || {
            let commands = match kind.commands(&root) {
                Ok(commands) => commands,
                Err(error) => {
                    let _ = tx.send(error);
                    worker_failed.store(true, Ordering::Relaxed);
                    return;
                }
            };
            for args in commands {
                if worker_cancel.load(Ordering::Relaxed) {
                    let _ = tx.send("── cancelled ──".to_string());
                    return;
                }
                let _ = tx.send(format!("$ cargo {}", args.join(" ")));
                let mut command = std::process::Command::new("cargo");
                command
                    .args(&args)
                    .current_dir(&root)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child: Child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = tx.send(format!("failed to spawn cargo: {error}"));
                        worker_failed.store(true, Ordering::Relaxed);
                        return;
                    }
                };

                // 这一步是不是结构化诊断模式（只有 `cargo build` 带这个参数）。
                let json = args.iter().any(|arg| arg == "json");
                if json {
                    worker_diagnostics.lock().unwrap().clear();
                }
                // 诊断里的 span 相对 workspace 根（`--manifest-path` 的父目录）。
                let span_root = args
                    .iter()
                    .position(|arg| arg == "--manifest-path")
                    .and_then(|index| args.get(index + 1))
                    .map(PathBuf::from)
                    .and_then(|manifest| manifest.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| root.clone());

                // stdout / stderr 各一个读线程，保证两路输出都不会互相饿死。
                let mut readers = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    readers.push(if json {
                        forward_cargo_json(
                            stdout,
                            tx.clone(),
                            span_root,
                            Arc::clone(&worker_diagnostics),
                        )
                    } else {
                        forward_lines(stdout, tx.clone())
                    });
                }
                if let Some(stderr) = child.stderr.take() {
                    readers.push(forward_lines(stderr, tx.clone()));
                }

                let mut cancelled = false;
                loop {
                    if worker_cancel.load(Ordering::Relaxed) {
                        // kill 只是发信号；下面的 wait() 负责回收，读线程也要等。
                        let _ = child.kill();
                        cancelled = true;
                        break;
                    }
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(error) => {
                            let _ = tx.send(format!("cannot wait for cargo: {error}"));
                            worker_failed.store(true, Ordering::Relaxed);
                            cancelled = true;
                            break;
                        }
                    }
                }

                let status = child.wait().ok();
                for reader in readers {
                    let _ = reader.join();
                }
                if cancelled {
                    let _ = tx.send("── cancelled ──".to_string());
                    return;
                }
                let code = status.and_then(|status| status.code()).unwrap_or(-1);
                let _ = tx.send(format!("── exit {code} ──"));
                // 前一步失败就不再跑后面的步骤（例如 cook 失败时不要打包旧场景）。
                if !status.map(|status| status.success()).unwrap_or(false) {
                    let _ = tx.send("build stopped: step failed".to_string());
                    worker_failed.store(true, Ordering::Relaxed);
                    return;
                }
            }

            // 玩家构建：把 exe / `assets/` 摆到 `[build].output_dir`，需要时直接运行产物。
            if kind.is_player_build() {
                let plan = player_plan(&root);
                let _ = tx.send(format!(
                    "staging artifacts into {}",
                    plan.output_dir.display()
                ));
                match copy_artifacts(&plan.copies) {
                    Ok(copied) => {
                        for line in copied {
                            let _ = tx.send(format!("copied {line}"));
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(format!("cannot stage artifacts: {error}"));
                        worker_failed.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                let missing: Vec<String> = plan
                    .required
                    .iter()
                    .filter(|path| !path.is_file())
                    .map(|path| path.display().to_string())
                    .collect();
                if !missing.is_empty() {
                    let _ = tx.send(format!(
                        "build stopped: missing artifact {}",
                        missing.join(", ")
                    ));
                    worker_failed.store(true, Ordering::Relaxed);
                    return;
                }
                if kind.runs_after_build() {
                    let exe = plan
                        .required
                        .first()
                        .cloned()
                        .unwrap_or_else(|| plan.output_dir.clone());
                    match std::process::Command::new(&exe)
                        .current_dir(&plan.output_dir)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            let _ =
                                tx.send(format!("launched {} (pid {})", exe.display(), child.id()));
                        }
                        Err(error) => {
                            let _ = tx.send(format!("cannot launch {}: {error}", exe.display()));
                            worker_failed.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        Self {
            rx,
            label,
            running: true,
            cancel,
            diagnostics,
            failed,
        }
    }

    /// 取走后台线程解析出的编译器诊断（Problems 面板用）。
    pub fn take_diagnostics(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut *self.diagnostics.lock().unwrap())
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// 请求中断：后台线程会 `kill()` 子进程并发送 `── cancelled ──`。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 这次任务是否被用户取消（`cancel()` 之后为 true）。
    pub fn was_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 有没有步骤真的失败（非 0 退出 / 产物缺失 / spawn 失败）。
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// 取走新输出（`None` = 任务已结束）。
    pub fn poll(&mut self) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }
        (lines, self.running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        let mut repo = std::env::current_dir().unwrap();
        while !repo
            .join("crates/prism-build-pipeline/Cargo.toml")
            .is_file()
        {
            repo = repo.parent().expect("repository root").to_path_buf();
        }
        repo
    }

    fn mentions(args: &[String], needle: &str) -> bool {
        args.iter().any(|arg| arg.contains(needle))
    }

    /// #477：Offline Tools 的面板只是把参数拼成 `prism-build-pipeline` 的命令行。
    #[test]
    fn offline_tool_commands_target_the_pipeline() {
        let root = repo_root();

        let bake = BuildKind::BakeGi {
            output: "assets/gi/out.bin".to_string(),
            rays: 128,
            bounces: 4,
        }
        .commands(&root)
        .unwrap();
        assert_eq!(bake.len(), 1);
        assert!(mentions(&bake[0], "bake-gi"));
        assert!(mentions(&bake[0], "assets/gi/out.bin"));
        assert!(bake[0]
            .windows(2)
            .any(|pair| pair[0] == "--rays" && pair[1] == "128"));
        assert!(bake[0]
            .windows(2)
            .any(|pair| pair[0] == "--bounces" && pair[1] == "4"));

        let heightmap = BuildKind::Heightmap {
            output: "assets/heightmaps/t.raw".to_string(),
            width: 256,
            height: 128,
            seed: 7,
            iterations: 9,
            sea_level: 0.4,
        }
        .commands(&root)
        .unwrap();
        assert_eq!(heightmap.len(), 1);
        let args = &heightmap[0];
        assert!(mentions(args, "heightmap"));
        for (flag, value) in [
            ("--output", "assets/heightmaps/t.raw"),
            ("--width", "256"),
            ("--height", "128"),
            ("--seed", "7"),
            ("--iterations", "9"),
            ("--sea-level", "0.4"),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == value),
                "{flag} {value} missing from {args:?}"
            );
        }

        let pack = BuildKind::ScenePackCustom {
            output: "assets/custom.pak".to_string(),
            compression: 7,
        }
        .commands(&root)
        .unwrap();
        assert!(mentions(&pack[0], "build-scenes"));
        assert!(pack[0]
            .windows(2)
            .any(|pair| pair[0] == "--output" && pair[1] == "assets/custom.pak"));
        assert!(pack[0]
            .windows(2)
            .any(|pair| pair[0] == "--compression" && pair[1] == "7"));
    }

    /// 建一个带 `assets/settings.ini` 的临时工程根（构建计划读它取 profile / output_dir）。
    fn project_root(name: &str, build_section: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("prism-egui-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(
            root.join("assets/settings.ini"),
            format!(
                "[build]
{build_section}
"
            ),
        )
        .unwrap();
        root
    }

    #[test]
    fn player_builds_refresh_the_scene_pack_first() {
        let root = project_root("player-steps", "profile = debug");
        for kind in [BuildKind::Player, BuildKind::PlayerRun] {
            let commands = kind.commands(&root).unwrap();
            assert_eq!(commands.len(), 2, "player builds need two steps");
            assert_eq!(commands[0][0], "build", "first step builds the player");
            assert!(
                commands[0].iter().any(|arg| arg.contains("Cargo.toml")),
                "the build step needs a manifest"
            );
            assert!(
                mentions(&commands[1], "build-scenes"),
                "the second step refreshes the scene pack"
            );
        }
    }

    /// `[build].profile` 决定 `--release`：以前 Build Player 永远构建 debug 版。
    #[test]
    fn the_release_profile_survives_the_trip_from_settings() {
        let debug_root = project_root("profile-debug", "profile = debug");
        let debug = BuildKind::Player.commands(&debug_root).unwrap();
        assert!(!mentions(&debug[0], "--release"));

        let release_root = project_root("profile-release", "profile = release");
        let release = BuildKind::Player.commands(&release_root).unwrap();
        assert!(
            mentions(&release[0], "--release"),
            "release profile must reach cargo: {:?}",
            release[0]
        );
    }

    #[test]
    fn player_build_targets_project_root_manifest() {
        let repo = repo_root();
        let root = project_root("manifest", "profile = debug");
        // 没有 `projects/prisma` 的用户工程回落到自己的 manifest。
        assert_eq!(
            BuildKind::Player.commands(&root).unwrap()[0],
            vec![
                "build".to_string(),
                "--manifest-path".to_string(),
                root.join("Cargo.toml").display().to_string(),
                "--message-format".to_string(),
                "json".to_string(),
            ]
        );
        assert_eq!(
            game_manifest_path(&repo),
            repo.join("projects/prisma/Cargo.toml")
        );
    }

    /// 玩家构建要 JSON 诊断（Problems 面板）；打包/cook 步骤不能加。
    #[test]
    fn only_the_player_build_asks_for_json_diagnostics() {
        let root = project_root("diagnostics", "profile = debug");
        for kind in [BuildKind::Player, BuildKind::PlayerRun] {
            let commands = kind.commands(&root).unwrap();
            assert!(mentions(&commands[0], "json"));
        }
        // 打包步骤走真实仓库根（临时根里没有 pipeline workspace）。
        let pack = BuildKind::ScenePack.commands(&repo_root()).unwrap();
        assert!(!mentions(&pack[0], "json"));
    }

    /// `Build And Run` 运行的是**摆好的产物**，不是 `cargo run` 的调试版。
    #[test]
    fn build_and_run_launches_the_staged_binary() {
        assert!(BuildKind::PlayerRun.is_player_build());
        assert!(BuildKind::PlayerRun.runs_after_build());
        assert!(BuildKind::Player.is_player_build());
        assert!(!BuildKind::Player.runs_after_build());
        let root = project_root(
            "staged",
            "profile = release
output_dir = build/win",
        );
        let plan = player_plan(&root);
        assert_eq!(plan.output_dir, root.join("build/win"));
        assert_eq!(
            plan.required,
            vec![plan.output_dir.join(prism_editor_core::build::exe_name())]
        );
        assert!(plan
            .copies
            .iter()
            .any(|copy| copy.to.ends_with("assets/scenes.pak")));
        let local = if cfg!(windows) {
            "prisma.exe"
        } else {
            "prisma"
        };
        assert_eq!(plan.required[0].file_name().unwrap().to_str(), Some(local));
    }

    #[test]
    fn scene_pack_cooks_in_one_step() {
        let root = repo_root();
        let commands = BuildKind::ScenePack.commands(&root).unwrap();
        assert_eq!(commands.len(), 1);
        assert!(mentions(&commands[0], "build-scenes"));
    }

    /// Clean 只清玩家 workspace，CleanEngine 才清仓库根。
    #[test]
    fn clean_targets_differ_between_player_and_engine() {
        let repo = repo_root();
        assert_eq!(
            BuildKind::Clean.commands(&repo).unwrap(),
            vec![vec![
                "clean".to_string(),
                "--manifest-path".to_string(),
                repo.join("projects/prisma/Cargo.toml")
                    .display()
                    .to_string()
            ]]
        );
        assert_eq!(
            BuildKind::CleanEngine.commands(&repo).unwrap(),
            vec![vec![
                "clean".to_string(),
                "--manifest-path".to_string(),
                repo.join("Cargo.toml").display().to_string()
            ]]
        );
    }

    #[test]
    fn pipeline_manifest_search_walks_to_repository_root() {
        let repo = repo_root();
        let root = repo.join("projects/fishing");
        assert_eq!(pipeline_manifest_path(&root), Some(repo.join("Cargo.toml")));
    }
}
