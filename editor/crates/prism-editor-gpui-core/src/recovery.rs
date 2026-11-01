//! 未保存文档的退出快照（#619）。
//!
//! egui-ash 在 winit 的 `CloseRequested` 里先调用了 `event_loop.exit()`，所以编辑器
//! 里那套 `CancelClose` + “Unsaved Changes” 对话框在实际关窗时来不及生效。与其把
//! 防止丢数据的希望寄托在依赖仓库的事件循环上，这里在 `AppEvent::LoopExiting`
//! （事件循环已经结束、但 app 仍可访问）把 dirty 文档写进项目内的恢复文件。
//!
//! 快照**不覆盖**原场景 / 预制体：下次启动时由 UI 询问 Recover / Discard，恢复出来
//! 的文档仍是 dirty 状态，用户确认后才落盘。文件放在 `target/` 下，不会进版本库。

use crate::doc::SceneDoc;
use crate::workspace::DocKind;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 恢复快照相对项目根的位置。
pub const RECOVERY_DIR: &str = "target/editor-recovery";
/// 快照文件名；所有文档装在同一个 JSON 数组里，写入是原子的。
pub const RECOVERY_FILE: &str = "recovery.json";

/// 一条待恢复的文档。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredDocument {
    /// 原文档相对项目根的路径；`None` = 从未保存过的 Untitled 文档。
    pub path: Option<String>,
    pub kind: DocKind,
    pub document: SceneDoc,
}

fn recovery_path(root: &Path) -> PathBuf {
    root.join(RECOVERY_DIR).join(RECOVERY_FILE)
}

/// 原子写入快照。`documents` 为空时什么都不做（不删除旧快照——删除只能由
/// Recover / Discard 显式触发，避免用户忽略提示后被一次普通保存清掉）。
pub fn write(root: &Path, documents: &[RecoveredDocument]) -> io::Result<usize> {
    if documents.is_empty() {
        return Ok(0);
    }
    let dir = root.join(RECOVERY_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(RECOVERY_FILE);
    let tmp = dir.join(format!("{RECOVERY_FILE}.tmp"));
    let mut text = serde_json::to_string_pretty(documents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    text.push('\n');
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        // 快照就是最后一道保险，必须真正落盘后再 rename。
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(documents.len())
}

/// 读取快照；没有快照时返回空列表。
pub fn read(root: &Path) -> io::Result<Vec<RecoveredDocument>> {
    let path = recovery_path(root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// 删除快照（连同空的恢复目录）。
pub fn clear(root: &Path) -> io::Result<()> {
    let path = recovery_path(root);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // 目录里可能还有用户 / 其他进程放的东西，只在空的时候删。
    let _ = fs::remove_dir(root.join(RECOVERY_DIR));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::SceneDoc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "prism-editor-recovery-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("assets/scenes")).unwrap();
            Self { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn doc(name: &str) -> SceneDoc {
        let mut doc = SceneDoc::default();
        doc.add_named_child(None, name);
        doc
    }

    #[test]
    fn round_trips_documents_and_clears() {
        let root = TempRoot::new();
        let entries = vec![
            RecoveredDocument {
                path: Some("assets/scenes/a.scene.json".to_string()),
                kind: DocKind::Scene,
                document: doc("a"),
            },
            RecoveredDocument {
                path: None,
                kind: DocKind::Prefab,
                document: doc("untitled"),
            },
        ];
        assert_eq!(write(&root.path, &entries).unwrap(), 2);
        assert!(recovery_path(&root.path).exists());

        let read_back = read(&root.path).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(
            read_back[0].path.as_deref(),
            Some("assets/scenes/a.scene.json")
        );
        assert_eq!(read_back[0].kind, DocKind::Scene);
        assert_eq!(read_back[0].document, entries[0].document);
        assert_eq!(read_back[1].path, None);
        assert_eq!(read_back[1].kind, DocKind::Prefab);

        clear(&root.path).unwrap();
        assert!(!recovery_path(&root.path).exists());
        assert!(read(&root.path).unwrap().is_empty());
    }

    #[test]
    fn writing_no_documents_keeps_an_existing_snapshot() {
        let root = TempRoot::new();
        let entries = vec![RecoveredDocument {
            path: Some("assets/scenes/a.scene.json".to_string()),
            kind: DocKind::Scene,
            document: doc("a"),
        }];
        write(&root.path, &entries).unwrap();

        assert_eq!(write(&root.path, &[]).unwrap(), 0);
        assert_eq!(read(&root.path).unwrap().len(), 1);
    }

    #[test]
    fn missing_snapshot_reads_as_empty() {
        let root = TempRoot::new();
        assert!(read(&root.path).unwrap().is_empty());
        clear(&root.path).unwrap();
    }

    #[test]
    fn malformed_snapshot_is_an_error_not_a_panic() {
        let root = TempRoot::new();
        fs::create_dir_all(root.path.join(RECOVERY_DIR)).unwrap();
        fs::write(recovery_path(&root.path), "{ not json").unwrap();
        let error = read(&root.path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
