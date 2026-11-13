//! 桌面端拉起游戏二进制：定位 + 防重复启动。
//!
//! 旧实现用 `Command::new("template")` 交给 `PATH` 解析，结果取决于机器上装了
//! 什么：可能命中别处的同名程序，也可能在已经有一个游戏在跑时再起一个。
//! 这里改成按确定的布局找二进制，并用子进程句柄挡住重复启动。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// 直接指定游戏二进制的环境变量；设置后必须指向真实存在的文件。
pub const GAME_BIN_ENV: &str = "PRISMREV_GAME_BIN";

/// 游戏 crate 相对仓库根的位置。
const GAME_CRATE_DIR: &str = "projects/template";

/// 游戏二进制的文件名（带平台后缀）。
fn binary_name() -> String {
    format!("template{}", std::env::consts::EXE_SUFFIX)
}

/// 候选二进制路径，按优先级排列：
///
/// 1. 打包布局——与 launcher 可执行文件同目录；
/// 2. 开发布局——仓库根下的 `projects/template/target/<profile>/template[.exe]`，
///    仓库根由 `exe_dir` 逐级向上找 `projects/template/Cargo.toml` 得到。
pub fn candidates(exe_dir: &Path, profile: &str) -> Vec<PathBuf> {
    let mut out = vec![exe_dir.join(binary_name())];
    if let Some(root) = repo_root_from(exe_dir) {
        out.push(
            root.join(GAME_CRATE_DIR)
                .join("target")
                .join(profile)
                .join(binary_name()),
        );
    }
    out
}

pub(crate) fn repo_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(GAME_CRATE_DIR).join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// 解析要启动的游戏二进制。
///
/// `env_override` 有值时以它为准——这是用户显式指定，指向不存在的文件属于错误，
/// 不再回退（否则又会静默跑到别的 `template` 上）。没有覆盖值时才走
/// [`candidates`] 的顺序。
pub fn resolve(
    exe_dir: &Path,
    profile: &str,
    env_override: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(raw) = env_override {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{GAME_BIN_ENV} 指向的文件不存在：{}",
            path.display()
        ));
    }

    let tried = candidates(exe_dir, profile);
    tried
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            let list = tried
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "找不到游戏二进制 `{}`，已尝试：\n{list}\n\
                 开发时先在 projects/template 里 `cargo build`；\
                 也可以用 {GAME_BIN_ENV} 指定完整路径。",
                binary_name()
            )
        })
}

/// 已启动的游戏子进程；只用来挡住重复启动。
#[derive(Default)]
pub struct RunningGame(Mutex<Option<Child>>);

impl RunningGame {
    /// 启动游戏。已有存活子进程时返回错误；已退出的子进程会被顺手回收。
    pub fn launch(&self, config: Option<String>) -> Result<(), String> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| "游戏进程状态已损坏".to_string())?;
        if let Some(child) = slot.as_mut() {
            match child.try_wait() {
                // 还活着：不再起第二个。
                Ok(None) => return Err("游戏已经在运行".to_string()),
                // 已退出或查询失败：清掉句柄，允许重新启动。
                _ => *slot = None,
            }
        }

        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let binary = resolve(&exe_dir, profile, std::env::var_os(GAME_BIN_ENV).as_deref())?;

        let mut cmd = Command::new(&binary);
        if let Some(config) = config {
            cmd.env("PRISMREV_LAUNCH_CONFIG", config);
        }
        // 不吞游戏的输出：只把 stdin 接到 null，stdout/stderr 照常继承启动器的控制台。
        cmd.stdin(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|err| format!("启动 {} 失败：{err}", binary.display()))?;
        *slot = Some(child);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立的临时目录，测试结束不清理也无妨（位于系统临时目录下）。
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prismrev-launcher-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建父目录");
        }
        std::fs::write(path, b"fake").expect("写文件");
    }

    #[test]
    fn packaged_layout_uses_the_sibling_binary() {
        let root = scratch("packaged");
        let exe_dir = root.join("dist");
        touch(&exe_dir.join(binary_name()));

        let resolved = resolve(&exe_dir, "debug", None).expect("应命中同目录二进制");
        assert_eq!(resolved, exe_dir.join(binary_name()));
    }

    #[test]
    fn dev_layout_finds_the_game_crate_target() {
        let root = scratch("dev");
        // 仓库：<root>/repo/{Cargo.toml, projects/template/Cargo.toml}
        let repo = root.join("repo");
        touch(&repo.join("Cargo.toml"));
        touch(&repo.join(GAME_CRATE_DIR).join("Cargo.toml"));
        let game = repo
            .join(GAME_CRATE_DIR)
            .join("target")
            .join("debug")
            .join(binary_name());
        touch(&game);

        let exe_dir = repo
            .join("launcher")
            .join("src-tauri")
            .join("target")
            .join("debug");
        std::fs::create_dir_all(&exe_dir).expect("创建 exe 目录");

        let resolved = resolve(&exe_dir, "debug", None).expect("应命中 projects/template/target");
        assert_eq!(resolved, game);
    }

    #[test]
    fn env_override_wins_and_must_exist() {
        let root = scratch("env");
        let exe_dir = root.join("dist");
        touch(&exe_dir.join(binary_name()));
        let custom = root.join("custom").join("my-game");
        touch(&custom);

        let resolved = resolve(&exe_dir, "debug", Some(custom.as_os_str())).expect("覆盖值应优先");
        assert_eq!(resolved, custom);

        let missing = root.join("custom").join("nope");
        let err = resolve(&exe_dir, "debug", Some(missing.as_os_str()))
            .expect_err("覆盖值不存在应报错而不是回退");
        assert!(err.contains(GAME_BIN_ENV), "{err}");
    }

    #[test]
    fn missing_binary_lists_every_candidate() {
        let root = scratch("missing");
        let exe_dir = root.join("dist");
        std::fs::create_dir_all(&exe_dir).expect("创建 exe 目录");

        let err = resolve(&exe_dir, "release", None).expect_err("找不到二进制应报错");
        assert!(err.contains(&exe_dir.join(binary_name()).display().to_string()));
        assert!(err.contains("projects/template"), "{err}");
    }
}
