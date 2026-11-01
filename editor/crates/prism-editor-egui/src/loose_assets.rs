//! 松散资产热重载（#495 第 ② 条）：`assets/` 下没进包的文件变了，就自动重打场景包。
//!
//! 运行时只从 `.pak` 读资产，所以改一张贴图 / 换一个 HDR / 新导入的网格，必须重新
//! cook 打包才会进内存。编辑器在这里做最小闭环：
//!
//! 1. 轮询 `assets/` 下**作者资产**的版本戳（mtime + 长度，排除 `.pak` / `.rscn` / `.spv` 等生成物，
//!    以及 `.scene.json` / `.prefab.json` —— 它们由文档自动保存写盘，重打包没有意义）；
//! 2. 变化后静默 `DEBOUNCE`，避免保存大文件时连续触发；
//! 3. 触发一次现有的 `BuildKind::ScenePack`，打完包后 `EngineHost::poll_asset_changes`
//!    的现有链路会重开 `.pak` 并 `invalidate_assets`，不用退出 Play。
//!
//! 扫描是纯文件系统操作，测试用临时目录直接验证；时间由调用方注入，方便测 debounce。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use prism_editor_core::project::{file_stamp_at, FileStamp};

/// 变化后等待这么久再打包（覆盖一次多文件保存 / 大文件分块写入）。
pub const DEBOUNCE: Duration = Duration::from_millis(800);

/// 生成物 / 编辑器文档：不参与松散资产扫描。
///
/// - `.pak` / `.pak.meta.json`：打包输出本身，包含进来会无限触发；
/// - `.rscn`：`cook` 的单场景产物；
/// - `.spv`：shader 编译产物（改 shader 需要重新 compile，不是资产打包）；
/// - `.scene.json` / `.prefab.json`：文档自动保存写盘，重打包没有收益。
fn is_watched(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".pak")
        || name.ends_with(".pak.meta.json")
        || name.ends_with(".rscn")
        || name.ends_with(".spv")
        || name.ends_with(".scene.json")
        || name.ends_with(".prefab.json")
    {
        return false;
    }
    // 隐藏文件 / 临时文件（vim、VSCode 的原子保存、系统缩略图缓存）。
    if name.starts_with('.') || name.ends_with('~') || name.ends_with(".swp") {
        return false;
    }
    true
}

fn skip_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules" | ".svn" | ".hg")
}

/// 扫描 `assets/` 下所有被监视文件的 `(路径, mtime)`。
///
/// 路径是**相对项目根**的（与 `.pak` / manifest 里的写法一致），这样换机器 / 换
/// 绝对路径不会让快照整体失效。
fn scan(root: &Path) -> BTreeMap<PathBuf, FileStamp> {
    let mut out = BTreeMap::new();
    let assets = root.join("assets");
    collect(&assets, root, &mut out);
    out
}

fn collect(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, FileStamp>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            if !skip_dir(&name.to_string_lossy()) {
                collect(&path, root, out);
            }
            continue;
        }
        if !file_type.is_file() || !is_watched(&path) {
            continue;
        }
        let Some(stamp) = file_stamp_at(&path) else {
            continue;
        };
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        out.insert(relative, stamp);
    }
}

/// 松散资产轮询器：快照 + debounce 状态。
#[derive(Debug, Default)]
pub struct LooseAssetWatcher {
    snapshot: BTreeMap<PathBuf, FileStamp>,
    /// 最近一次检测到变化的时间；安静满 [`DEBOUNCE`] 才触发。
    pending: Option<Instant>,
}

impl LooseAssetWatcher {
    pub fn new(root: &Path) -> Self {
        Self {
            snapshot: scan(root),
            pending: None,
        }
    }

    /// 轮询一次；变化后安静满 [`DEBOUNCE`] 时返回 `true`（只返回一次）。
    ///
    /// `now` 由调用方注入，测试可以精确控制 debounce 时序。
    pub fn poll(&mut self, root: &Path, now: Instant) -> bool {
        let current = scan(root);
        if current != self.snapshot {
            self.snapshot = current;
            self.pending = Some(now);
            return false;
        }
        match self.pending {
            Some(at) if now.saturating_duration_since(at) >= DEBOUNCE => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }

    /// 重新对齐快照（构建刚写完包 / 切换工程时用），丢弃未触发的 debounce。
    pub fn resync(&mut self, root: &Path) {
        self.snapshot = scan(root);
        self.pending = None;
    }

    /// 当前是否在等待 debounce 结束（测试用）。
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prism-loose-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("assets/sprites")).unwrap();
        dir
    }

    fn touch(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn generated_artifacts_and_documents_are_not_watched() {
        let root = temp_dir("filter");
        for name in [
            "scenes.pak",
            "scenes.pak.meta.json",
            "scenes/a.rscn",
            "shaders/post.frag.spv",
            "scenes/lake.scene.json",
            "prefabs/boat.prefab.json",
            ".hidden.png",
            "sprites/boat.png~",
        ] {
            touch(&root.join("assets").join(name), b"x");
        }
        touch(&root.join("assets/sprites/boat.png"), b"x");
        touch(&root.join("assets/models/boat.gltf"), b"x");

        let scan = scan(&root);
        let mut names: Vec<String> = scan
            .keys()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["assets/models/boat.gltf", "assets/sprites/boat.png"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_change_fires_once_after_the_debounce() {
        let root = temp_dir("debounce");
        touch(&root.join("assets/sprites/boat.png"), b"v1");
        let mut watcher = LooseAssetWatcher::new(&root);
        let start = Instant::now();

        // 没变化：永远不触发。
        assert!(!watcher.poll(&root, start));
        assert!(!watcher.poll(&root, start + DEBOUNCE * 2));

        // 改文件 → 只记录 pending，不立刻触发。
        touch(&root.join("assets/sprites/boat.png"), b"v2-longer");
        assert!(!watcher.poll(&root, start));
        assert!(watcher.is_pending());
        assert!(!watcher.poll(&root, start + DEBOUNCE / 2));

        // 安静满 DEBOUNCE → 触发一次，然后回到空闲。
        assert!(watcher.poll(&root, start + DEBOUNCE + Duration::from_millis(1)));
        assert!(!watcher.is_pending());
        assert!(!watcher.poll(&root, start + DEBOUNCE * 3));

        // 连续写入会把 debounce 重新计时。
        touch(&root.join("assets/sprites/boat.png"), b"v3-a");
        assert!(!watcher.poll(&root, start));
        touch(&root.join("assets/sprites/boat.png"), b"v4-longer");
        assert!(!watcher.poll(&root, start + Duration::from_millis(600)));
        assert!(!watcher.poll(&root, start + DEBOUNCE));
        assert!(watcher.poll(&root, start + Duration::from_millis(600) + DEBOUNCE));

        // 新文件 / 删除也算变化。
        touch(&root.join("assets/models/new.gltf"), b"x");
        assert!(!watcher.poll(&root, start));
        assert!(watcher.poll(&root, start + DEBOUNCE + Duration::from_millis(2)));
        std::fs::remove_file(root.join("assets/models/new.gltf")).unwrap();
        assert!(!watcher.poll(&root, start));
        assert!(watcher.poll(&root, start + DEBOUNCE + Duration::from_millis(3)));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resync_discards_a_pending_trigger() {
        let root = temp_dir("resync");
        touch(&root.join("assets/sprites/boat.png"), b"v1");
        let mut watcher = LooseAssetWatcher::new(&root);
        let start = Instant::now();
        touch(&root.join("assets/sprites/boat.png"), b"v2-longer");
        assert!(!watcher.poll(&root, start));
        assert!(watcher.is_pending());
        watcher.resync(&root);
        assert!(!watcher.is_pending());
        assert!(!watcher.poll(&root, start + DEBOUNCE * 2));
        std::fs::remove_dir_all(&root).ok();
    }
}
