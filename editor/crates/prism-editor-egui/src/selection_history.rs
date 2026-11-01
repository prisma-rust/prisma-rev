//! 选择历史（Unity `SelectionHistory`，issue #417）。
//!
//! 记录「文档 + 选择集 + 活动对象」的快照，Edit 菜单的 Back / Forward 像浏览器
//! 一样回退选择；不是按实体下标循环。纯数据逻辑，和 egui / 引擎无关，方便单测。

/// 一次选择状态。`document` 是 `Workspace::active_index()`，因为选择下标是
/// 文档内相对位置，切换文档后必须一起还原。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionSnapshot {
    pub document: usize,
    pub selection: Vec<usize>,
    pub active: Option<usize>,
}

impl SelectionSnapshot {
    pub fn new(document: usize, mut selection: Vec<usize>, active: Option<usize>) -> Self {
        selection.sort_unstable();
        selection.dedup();
        Self {
            document,
            selection,
            active,
        }
    }
}

/// 最近一次记录 + 游标。`observe` 在每帧末尾调用，发现选择真的变了才压栈。
#[derive(Debug, Default)]
pub struct SelectionHistory {
    entries: Vec<SelectionSnapshot>,
    position: usize,
    /// 上一次观察到的状态：用于去重，也用于判断「回退后用户又点了新对象」。
    last: Option<SelectionSnapshot>,
}

impl SelectionHistory {
    /// 和 Unity 一样保留有限步数，避免长时间编辑后无限增长。
    pub const MAX_ENTRIES: usize = 100;

    /// 每帧观察当前选择。连续相同状态不重复入栈；回退后产生新选择时，
    /// 先丢掉前进分支（浏览器历史语义）。
    pub fn observe(&mut self, current: SelectionSnapshot) {
        if self.last.as_ref() == Some(&current) {
            return;
        }
        self.last = Some(current.clone());
        if self.entries.get(self.position) == Some(&current) {
            return;
        }
        if self.position + 1 < self.entries.len() {
            self.entries.truncate(self.position + 1);
        }
        self.entries.push(current);
        if self.entries.len() > Self::MAX_ENTRIES {
            let overflow = self.entries.len() - Self::MAX_ENTRIES;
            self.entries.drain(..overflow);
        }
        self.position = self.entries.len().saturating_sub(1);
    }

    /// 前进 / 后退 `delta` 步；越过两端返回 `None`（菜单项保持无操作）。
    pub fn navigate(&mut self, delta: isize) -> Option<SelectionSnapshot> {
        if self.entries.is_empty() {
            return None;
        }
        let target = self.position as isize + delta;
        if target < 0 || target >= self.entries.len() as isize {
            return None;
        }
        self.position = target as usize;
        let entry = self.entries[self.position].clone();
        // 导航本身不是「新选择」：下一帧 `observe` 看到相同状态会直接返回。
        self.last = Some(entry.clone());
        Some(entry)
    }

    pub fn can_forward(&self) -> bool {
        self.position + 1 < self.entries.len()
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(document: usize, selection: &[usize], active: Option<usize>) -> SelectionSnapshot {
        SelectionSnapshot::new(document, selection.to_vec(), active)
    }

    #[test]
    fn repeated_observations_do_not_grow_the_history() {
        let mut history = SelectionHistory::default();
        let first = snap(0, &[1, 2], Some(2));
        history.observe(first.clone());
        history.observe(first.clone());
        history.observe(first);
        assert_eq!(history.len(), 1);
        assert_eq!(history.position(), 0);
    }

    #[test]
    fn back_and_forward_restore_the_exact_selection_and_document() {
        let mut history = SelectionHistory::default();
        history.observe(snap(0, &[0], Some(0)));
        history.observe(snap(0, &[1, 2], Some(2)));
        history.observe(snap(1, &[3], Some(3)));
        assert_eq!(history.position(), 2);

        assert_eq!(history.navigate(-1), Some(snap(0, &[1, 2], Some(2))));
        assert_eq!(history.navigate(-1), Some(snap(0, &[0], Some(0))));
        assert_eq!(history.navigate(-1), None, "不能越过最旧一条");
        assert_eq!(history.navigate(1), Some(snap(0, &[1, 2], Some(2))));
        assert_eq!(history.navigate(1), Some(snap(1, &[3], Some(3))));
        assert_eq!(history.navigate(1), None, "不能越过最新一条");
    }

    #[test]
    fn a_new_selection_after_back_drops_the_forward_branch() {
        let mut history = SelectionHistory::default();
        history.observe(snap(0, &[0], Some(0)));
        history.observe(snap(0, &[1], Some(1)));
        history.observe(snap(0, &[2], Some(2)));
        assert!(history.navigate(-1).is_some());
        assert!(history.can_forward());

        history.observe(snap(0, &[9], Some(9)));
        assert!(!history.can_forward(), "新选择应截断前进分支");
        assert_eq!(history.navigate(-1), Some(snap(0, &[1], Some(1))));
    }

    #[test]
    fn navigating_does_not_add_a_duplicate_entry() {
        let mut history = SelectionHistory::default();
        history.observe(snap(0, &[0], Some(0)));
        history.observe(snap(0, &[1], Some(1)));
        let target = history.navigate(-1).unwrap();
        // 模拟下一帧末尾的 observe（看到的正是导航目标）。
        history.observe(target);
        assert_eq!(history.len(), 2);
        assert_eq!(history.position(), 0);
    }

    #[test]
    fn history_is_bounded() {
        let mut history = SelectionHistory::default();
        for index in 0..(SelectionHistory::MAX_ENTRIES + 20) {
            history.observe(snap(0, &[index], Some(index)));
        }
        assert_eq!(history.len(), SelectionHistory::MAX_ENTRIES);
        assert_eq!(history.position(), SelectionHistory::MAX_ENTRIES - 1);
        // 最旧的 20 条被丢掉，最早能回退到 index 20。
        let mut oldest = None;
        while let Some(entry) = history.navigate(-1) {
            oldest = Some(entry);
        }
        assert_eq!(oldest, Some(snap(0, &[20], Some(20))));
    }
}
