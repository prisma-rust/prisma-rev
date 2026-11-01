//! 资源目录定位——不依赖进程当前工作目录。
//!
//! 桌面端的启动方式不止一种：仓库根 `cargo run`、在 `projects/prisma/` 里运行、
//! 双击 `target/release/prisma.exe`、launcher 用绝对路径 spawn、快捷方式改过
//! 「起始位置」……而 `.pak` / `settings.ini` / `scenes.json` / 环境贴图此前
//! 都按 `assets/...` 直接相对 cwd 打开。cwd 不含 `assets/` 时它们全部静默失败，
//! 表现为黑屏或「没有任何场景」。
//!
//! 本模块定义**唯一**的资源根解析规则（[`asset_root`]），运行时各处共用：
//!
//! 1. `PRISMREV_ASSET_ROOT`（指向包含 `assets/` 的目录）——显式覆盖，便于测试/绿色版；
//! 2. 当前工作目录及其所有父目录；
//! 3. 可执行文件所在目录及其所有父目录（覆盖「从别处用绝对路径启动」）。
//!
//! 取第一个**确实存在 `assets/` 子目录**的候选，因此不会因为某个上级目录里
//! 恰好有个同名文件而误判。Android 不走这条链：`run_on_android` 会把 APK 资源
//! 解到 `<filesDir>/assets` 并 `set_current_dir`，第一个候选（cwd）即命中，
//! 行为与之前完全一致。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 显式指定资源根目录（**包含 `assets/` 的目录**）的环境变量。
pub const ASSET_ROOT_ENV: &str = "PRISMREV_ASSET_ROOT";

/// 资源子目录名，所有 `assets/...` 相对路径都挂在它下面。
const ASSET_DIR_NAME: &str = "assets";

/// 分包构建（#509）写出的可选包目录：`assets/optional/optional_*.pak`。
///
/// 自动挂载只扫描 `assets/` 顶层，因此这个目录里的包**不会**在启动时常驻；
/// 需要时显式调用 `GpuAssetResolver::load_optional_packages`。
pub const OPTIONAL_PAK_DIR_NAME: &str = "optional";

static WARNED_MISSING_ROOT: AtomicBool = AtomicBool::new(false);

/// 候选根目录，按优先级排序（见模块文档）。
///
/// 需要自行尝试多个相对路径的调用方（例如场景清单还要兼容旧格式
/// `crates/prism-engine/assets/`）可以直接遍历它。
pub fn search_roots() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let env_root = std::env::var_os(ASSET_ROOT_ENV).map(PathBuf::from);
    collect_search_roots(env_root.as_deref(), cwd.as_deref(), exe_dir.as_deref())
}

/// 定位包含 `assets/` 的目录（解析顺序见模块文档）。
///
/// 找不到时返回 `None`，并且**每个进程只警告一次**——调用点通常有两三个，
/// 重复警告只会让启动日志更吵。
pub fn asset_root() -> Option<PathBuf> {
    let root = find_asset_root(&search_roots());
    match &root {
        Some(root) => log::debug!("asset root: {}", root.display()),
        None => {
            if !WARNED_MISSING_ROOT.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "no `assets/` directory found from the current directory or the executable \
                     directory; resources (.pak / settings.ini / scenes.json) will not load \
                     (set {ASSET_ROOT_ENV} to override)"
                );
            }
        }
    }
    root
}

/// `<root>/assets`——所有资源相对路径的基目录。
pub fn asset_dir() -> Option<PathBuf> {
    asset_root().map(|root| root.join(ASSET_DIR_NAME))
}

/// 解析 `assets/` 下的相对路径，例如 `asset_path("scenes.pak")`
/// → `<root>/assets/scenes.pak`。
pub fn asset_path(relative: impl AsRef<Path>) -> Option<PathBuf> {
    asset_dir().map(|dir| dir.join(relative.as_ref()))
}

// ---------------------------------------------------------------------------
// Internals (纯函数，便于单测)
// ---------------------------------------------------------------------------

fn collect_search_roots(
    env_root: Option<&Path>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Some(root) = env_root {
        push_unique(&mut roots, root.to_path_buf());
    }

    // cwd 在前、exe 目录在后：开发时（cargo run / 编辑器）cwd 更贴合用户意图，
    // 部署后 cwd 往往只是「双击时的起始位置」，exe 目录才是可靠的那个。
    for start in [cwd, exe_dir].into_iter().flatten() {
        let mut current = Some(start);
        while let Some(dir) = current {
            push_unique(&mut roots, dir.to_path_buf());
            current = dir.parent();
        }
    }

    roots
}

fn push_unique(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !roots.contains(&candidate) {
        roots.push(candidate);
    }
}

fn find_asset_root(roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .find(|dir| dir.join(ASSET_DIR_NAME).is_dir())
        .cloned()
}

/// `dir` 下所有 `.pak` 文件，按文件名排序（#530 多包挂载）。
///
/// 打包器按内容分组写出多个包（场景包、资源包……），运行时不能再写死
/// `scenes.pak`。排序只为让启动日志与解析顺序稳定。
pub fn pak_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut paks: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pak"))
        })
        .collect();
    paks.sort();
    paks
}

/// `assets/` 下的所有 `.pak`（[`pak_files_in`] 的定位版本）。
pub fn pak_files() -> Vec<PathBuf> {
    asset_dir()
        .map(|dir| pak_files_in(&dir))
        .unwrap_or_default()
}

/// `assets/optional/`——分包构建（#509）的可选包目录。
pub fn optional_pak_dir() -> Option<PathBuf> {
    asset_dir().map(|dir| dir.join(OPTIONAL_PAK_DIR_NAME))
}

/// `assets/optional/` 下的所有 `.pak`，按文件名排序。
///
/// 这些包不在启动时自动挂载；需要它们的一方调用
/// `GpuAssetResolver::load_optional_packages` 显式登记。
pub fn optional_pak_files() -> Vec<PathBuf> {
    optional_pak_dir()
        .map(|dir| pak_files_in(&dir))
        .unwrap_or_default()
}

/// 松散资产（`.pak` 之外）是否存在。编辑器预览与用户项目的关卡 JSON、
/// 按时段回落的贴图都走这里，不要再自己拼 `assets/` 相对路径。
pub fn asset_exists(relative: impl AsRef<Path>) -> bool {
    asset_path(relative).is_some_and(|path| path.is_file())
}

/// 读取松散资产原始字节。
pub fn read_asset_file(relative: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let relative = relative.as_ref();
    let path = asset_path(relative).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("asset root not found for {}", relative.display()),
        )
    })?;
    std::fs::read(&path)
}

/// 读取松散文本资产（关卡配置、清单等）。
pub fn read_asset_text(relative: impl AsRef<Path>) -> std::io::Result<String> {
    let relative = relative.as_ref();
    let path = asset_path(relative).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("asset root not found for {}", relative.display()),
        )
    })?;
    std::fs::read_to_string(&path)
}

#[cfg(test)]
#[path = "asset_paths_tests.rs"]
mod tests;
