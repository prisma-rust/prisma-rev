//! 资产 JSON 编辑的撤销 / 重做（#560 动画图 / 状态机 / Notify 面板）。
//!
//! 场景编辑走 [`crate::undo`]，但图、状态机、事件表这些 sidecar 资产不在 `SceneDoc`
//! 里，每次改动都直接写盘，所以需要单独一条历史。这里存的是**整份 JSON 快照**：
//! 文件只有几 KB，比给每种编辑写求逆命令稳得多，也能覆盖「面板之外的字段保持原样」。
//!
//! 连续拖拽（DragValue / 时间轴）每帧都会产生一次改动，同一个文件、间隔在
//! `coalesce_ms` 内、且新一步的 `before` 正好等于上一步的 `after` 时合并成一步，
//! 否则撤销一次只退一帧。

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_LIMIT: usize = 100;
const DEFAULT_COALESCE_MS: u64 = 800;

/// 一步资产编辑：文件路径 + 改动前后的完整 JSON。
#[derive(Debug, Clone, PartialEq)]
pub struct AssetEdit {
    pub path: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone)]
struct Entry {
    edit: AssetEdit,
    at_ms: u64,
}

/// 资产编辑历史；`undo` / `redo` 返回要写回的状态，由调用方落盘。
#[derive(Debug, Clone)]
pub struct AssetEditHistory {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    limit: usize,
    coalesce_ms: u64,
}

impl Default for AssetEditHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetEditHistory {
    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: DEFAULT_LIMIT,
            coalesce_ms: DEFAULT_COALESCE_MS,
        }
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    pub fn set_coalesce_ms(&mut self, ms: u64) {
        self.coalesce_ms = ms;
    }

    /// 记录一次编辑；`before == after` 不算改动。
    pub fn record(&mut self, path: &str, before: &Value, after: &Value) {
        self.record_at(path, before, after, now_ms());
    }

    fn record_at(&mut self, path: &str, before: &Value, after: &Value, at_ms: u64) {
        if before == after {
            return;
        }
        self.redo.clear();
        if let Some(last) = self.undo.last_mut() {
            let same_file = last.edit.path == path;
            let in_window = at_ms.saturating_sub(last.at_ms) <= self.coalesce_ms;
            let continues = last.edit.after == *before;
            if same_file && in_window && continues {
                last.edit.after = after.clone();
                last.at_ms = at_ms;
                return;
            }
        }
        self.undo.push(Entry {
            edit: AssetEdit {
                path: path.to_string(),
                before: before.clone(),
                after: after.clone(),
            },
            at_ms,
        });
        if self.undo.len() > self.limit {
            let overflow = self.undo.len() - self.limit;
            self.undo.drain(0..overflow);
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 下一次撤销会改的文件。
    pub fn undo_path(&self) -> Option<&str> {
        self.undo.last().map(|entry| entry.edit.path.as_str())
    }

    /// 下一次重做会改的文件。
    pub fn redo_path(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.edit.path.as_str())
    }

    /// 撤销一步，返回要写回的状态（`before`）。
    pub fn undo(&mut self) -> Option<AssetEdit> {
        let entry = self.undo.pop()?;
        let outcome = entry.edit.clone();
        self.redo.push(entry);
        Some(outcome)
    }

    /// 重做一步，返回要写回的状态（`after`）。
    pub fn redo(&mut self) -> Option<AssetEdit> {
        let entry = self.redo.pop()?;
        let outcome = entry.edit.clone();
        self.undo.push(entry);
        Some(outcome)
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn undo_and_redo_walk_the_history_in_order() {
        let mut history = AssetEditHistory::new();
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 0);
        history.record_at("a.json", &json!({ "x": 1 }), &json!({ "x": 2 }), 10_000);
        assert!(history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo_path(), Some("a.json"));

        let first = history.undo().expect("undo");
        assert_eq!(first.before, json!({ "x": 1 }));
        let second = history.undo().expect("undo again");
        assert_eq!(second.before, json!({ "x": 0 }));
        assert!(!history.can_undo());
        assert!(history.can_redo());

        let redo = history.redo().expect("redo");
        assert_eq!(redo.after, json!({ "x": 1 }));
        let redo = history.redo().expect("redo again");
        assert_eq!(redo.after, json!({ "x": 2 }));
        assert!(!history.can_redo());
    }

    #[test]
    fn a_new_edit_drops_the_redo_tail() {
        let mut history = AssetEditHistory::new();
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 0);
        history.record_at("a.json", &json!({ "x": 1 }), &json!({ "x": 2 }), 10_000);
        history.undo();
        history.record_at("a.json", &json!({ "x": 1 }), &json!({ "x": 9 }), 20_000);
        assert!(!history.can_redo());
        assert_eq!(history.undo().expect("undo").before, json!({ "x": 1 }));
    }

    #[test]
    fn continuous_edits_of_one_file_coalesce_into_one_step() {
        let mut history = AssetEditHistory::new();
        history.set_coalesce_ms(800);
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 1_000);
        history.record_at("a.json", &json!({ "x": 1 }), &json!({ "x": 2 }), 1_200);
        history.record_at("a.json", &json!({ "x": 2 }), &json!({ "x": 3 }), 1_400);
        let undo = history.undo().expect("one step");
        assert_eq!(undo.before, json!({ "x": 0 }));
        assert_eq!(undo.after, json!({ "x": 3 }));
        assert!(!history.can_undo(), "拖拽只该占一步");
    }

    #[test]
    fn a_gap_a_different_file_or_a_broken_chain_starts_a_new_step() {
        let mut history = AssetEditHistory::new();
        history.set_coalesce_ms(800);
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 0);
        history.record_at("a.json", &json!({ "x": 1 }), &json!({ "x": 2 }), 5_000);
        assert_eq!(history.undo().expect("gap").before, json!({ "x": 1 }));

        let mut history = AssetEditHistory::new();
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 0);
        history.record_at("b.json", &json!({ "x": 1 }), &json!({ "x": 2 }), 10);
        assert_eq!(history.undo().expect("file").before, json!({ "x": 1 }));

        let mut history = AssetEditHistory::new();
        history.record_at("a.json", &json!({ "x": 0 }), &json!({ "x": 1 }), 0);
        history.record_at("a.json", &json!({ "x": 7 }), &json!({ "x": 8 }), 10);
        assert_eq!(history.undo().expect("chain").before, json!({ "x": 7 }));
    }

    #[test]
    fn identical_states_and_empty_histories_are_ignored() {
        let mut history = AssetEditHistory::new();
        history.record("a.json", &json!({ "x": 1 }), &json!({ "x": 1 }));
        assert!(!history.can_undo());
        assert!(history.undo().is_none());
        assert!(history.redo().is_none());
        history.record("a.json", &json!({}), &json!({ "x": 1 }));
        history.clear();
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }

    #[test]
    fn the_limit_drops_the_oldest_steps() {
        let mut history = AssetEditHistory::new();
        history.set_limit(2);
        for x in 1..=3 {
            history.record_at(
                "a.json",
                &json!({ "x": x - 1 }),
                &json!({ "x": x }),
                x as u64 * 10_000,
            );
        }
        assert_eq!(history.undo().expect("step 3").before, json!({ "x": 2 }));
        assert_eq!(history.undo().expect("step 2").before, json!({ "x": 1 }));
        assert!(!history.can_undo(), "最老的一步被挤掉");
    }
}
