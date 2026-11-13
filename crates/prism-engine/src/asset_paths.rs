//! 资源目录定位——不依赖进程当前工作目录。
//!
//! 桌面端的启动方式不止一种：仓库根 `cargo run`、在 `projects/template/` 里运行、
//! 双击 `target/release/template.exe`、launcher 用绝对路径 spawn、快捷方式改过
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
        // 每次资产查询都会走到这里（一次会话几十次），所以常驻日志只留在
        // `trace`；启动时 `prism-app` 会单独打一行 `asset root:`。
        Some(root) => log::trace!("asset root: {}", root.display()),
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
///
/// 松散文件优先；找不到时再查 `assets/*.pak` 的路径清单（#1049），所以
/// Android / 发布版里打进包的关卡数据同样可见。
pub fn asset_exists(relative: impl AsRef<Path>) -> bool {
    let relative = relative.as_ref();
    if asset_path(relative).is_some_and(|path| path.is_file()) {
        return true;
    }
    packaged_asset_exists(&package_key(relative))
}

/// 读取松散资产原始字节；松散文件不存在时回退到 `.pak`（#1049）。
pub fn read_asset_file(relative: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let relative = relative.as_ref();
    if let Some(path) = asset_path(relative) {
        if path.is_file() {
            return std::fs::read(&path);
        }
    }
    if let Some(bytes) = packaged_asset_bytes(&package_key(relative)) {
        return Ok(bytes);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "asset not found (loose or packaged): {}",
            relative.display()
        ),
    ))
}

/// 读取松散文本资产（关卡配置、清单等）；`read_asset_file` 的 UTF-8 版本。
pub fn read_asset_text(relative: impl AsRef<Path>) -> std::io::Result<String> {
    let bytes = read_asset_file(relative)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

// ---------------------------------------------------------------------------
// 打包资产回退（#1049）
// ---------------------------------------------------------------------------

/// 已打开的 `.pak` 读取器缓存条目。
///
/// `PackageReader::open` 会校验整包 checksum，所以不能每次查询都重开；
/// 用 `(mtime, len)` 判断包是否被重建（与热重载同一套判据，Android 的
/// FUSE 时间戳粒度是 1 秒，单看 mtime 会漏）。
struct CachedPak {
    path: PathBuf,
    stamp: (u64, u64),
    reader: std::sync::Arc<prism_asset::package::PackageReader>,
}

static PAK_CACHE: std::sync::OnceLock<std::sync::Mutex<Vec<CachedPak>>> =
    std::sync::OnceLock::new();

/// `.pak` 里的路径键：正斜杠、去掉开头的 `./`（`AssetId::from_path` 的输入）。
fn package_key(relative: &Path) -> String {
    relative
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn pak_stamp(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    Some((modified, metadata.len()))
}

/// 用 `assets/` 下当前的 `.pak` 列表刷新缓存：保留未变化的读取器，重开
/// 刚被重建的包，丢掉已删除的包。
fn refresh_pak_cache(cache: &mut Vec<CachedPak>) {
    let mut previous = std::mem::take(cache);
    let mut next = Vec::with_capacity(previous.len());
    for path in pak_files() {
        let Some(stamp) = pak_stamp(&path) else {
            continue;
        };
        if let Some(index) = previous
            .iter()
            .position(|cached| cached.path == path && cached.stamp == stamp)
        {
            next.push(previous.swap_remove(index));
            continue;
        }
        match prism_asset::package::PackageReader::open(&path) {
            Ok(reader) => next.push(CachedPak {
                path,
                stamp,
                reader: std::sync::Arc::new(reader),
            }),
            // 包里正在热重载时可能恰好在写；下一次查询会重试。
            Err(error) => log::debug!(
                "cannot open {} for packaged-asset lookup: {error}",
                path.display()
            ),
        }
    }
    *cache = next;
}

fn with_pak_cache<R>(f: impl FnOnce(&[CachedPak]) -> R) -> R {
    let cache = PAK_CACHE.get_or_init(Default::default);
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    refresh_pak_cache(&mut guard);
    f(&guard)
}

fn packaged_asset_exists(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let id = prism_asset::AssetId::from_path(key);
    with_pak_cache(|paks| paks.iter().any(|pak| pak.reader.find_record(id).is_some()))
}

fn packaged_asset_bytes(key: &str) -> Option<Vec<u8>> {
    if key.is_empty() {
        return None;
    }
    let id = prism_asset::AssetId::from_path(key);
    with_pak_cache(|paks| {
        for pak in paks {
            let Some(record) = pak.reader.find_record(id) else {
                continue;
            };
            match pak.reader.read_asset_record_data(record) {
                Ok(bytes) => return Some(bytes),
                Err(error) => {
                    log::warn!("read packaged '{key}' from {}: {error}", pak.path.display())
                }
            }
        }
        None
    })
}

#[cfg(test)]
#[path = "asset_paths_tests.rs"]
mod tests;
