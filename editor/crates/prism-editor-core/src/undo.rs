//! 撤销 / 重做栈。
//!
//! 两类命令：
//! - `SetField`：检查器里的一次字段写入。同一个实体 + 组件 + 路径在
//!   `coalesce_ms` 内连续改动会合并成一步（拖动滑杆只产生一次撤销）。
//! - `Snapshot`：结构性编辑（增删实体 / 重排父子 / 预制体实例化 / 粘贴组件）。
//!   文档很小，整文档快照比逐操作求逆更不容易出错。

use crate::doc::{SceneDoc, TransformJson};
use crate::schema::set_at_path;
use serde_json::Value;

const DEFAULT_LIMIT: usize = 200;
const DEFAULT_COALESCE_MS: u64 = 800;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetField {
        entity_id: u32,
        component: String,
        path: String,
        before: Value,
        after: Value,
    },
    /// Transform 是作者 JSON 的结构字段（不在 `components` 里），单独一类命令。
    Transform {
        entity_id: u32,
        before: TransformJson,
        after: TransformJson,
    },
    /// 多选 gizmo 拖动：同一帧只产生一个撤销步，拖动中按 key 合并。
    TransformBatch {
        changes: Vec<(u32, TransformJson, TransformJson)>,
    },
    Snapshot {
        before: Box<SceneDoc>,
        after: Box<SceneDoc>,
    },
}

impl Command {
    pub fn label(&self) -> &'static str {
        match self {
            Command::SetField { .. } => "修改字段",
            Command::Transform { .. } => "修改 Transform",
            Command::TransformBatch { .. } => "修改 Transform",
            Command::Snapshot { .. } => "编辑场景",
        }
    }
}

fn apply_field(
    doc: &mut SceneDoc,
    entity_id: u32,
    component: &str,
    path: &str,
    value: &Value,
) -> bool {
    let Some(index) = doc.index_of_id(entity_id) else {
        return false;
    };
    let Some(root) = doc.entities[index].components.get_mut(component) else {
        return false;
    };
    set_at_path(root, path, value.clone())
}

/// 一步结构性编辑前后的选中实体（按 `EntityJson.id`，不是数组下标）。
///
/// 撤销 / 重做会连选择一起还原，见 #513：删掉 → 撤销后原实体重新被选中，
/// 复制 → 撤销后回到复制前的选择。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectionSnapshot {
    pub before: Vec<u32>,
    pub after: Vec<u32>,
}

/// [`UndoStack::undo`] / [`UndoStack::redo`] 的结果：标签 + 要还原的选择（如果有）。
#[derive(Debug, Clone, PartialEq)]
pub struct UndoOutcome {
    pub label: String,
    /// 该步记录的选中实体；`None` = 这一步没有记录选择（字段类命令），调用方保持现状。
    pub selection: Option<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub command: Command,
    pub label: String,
    coalesce: Option<String>,
    at_ms: u64,
    /// 结构性编辑的选中快照；字段 / Transform 命令为 `None`（实体集合没变）。
    selection: Option<SelectionSnapshot>,
    /// 执行这一步 / 撤销这一步之后所处的文档状态编号。
    ///
    /// 编号单调递增且全局唯一，所以「撤销回保存时的位置」和「撤销后又在同一深度
    /// 做了别的编辑」可以区分开 —— 只比较撤销栈深度会把后一种误判成未修改（#992）。
    before_state: u64,
    after_state: u64,
}

impl Entry {
    pub fn is_field_edit(&self) -> bool {
        matches!(
            self.command,
            Command::SetField { .. } | Command::Transform { .. } | Command::TransformBatch { .. }
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct UndoStack {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    limit: usize,
    coalesce_ms: u64,
    /// 交互进行中（拖动 gizmo / 拖拽输入框）：忽略合并时间窗口，
    /// 整段交互始终压成一步。
    sticky: bool,
    /// 下一个可用状态编号。
    next_state: u64,
    /// 文档当前所处的状态编号。
    current_state: u64,
    /// 最后一次保存时的状态编号；`None` = 当前状态不可达（保存点已被丢弃）。
    saved_state: Option<u64>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: DEFAULT_LIMIT,
            coalesce_ms: DEFAULT_COALESCE_MS,
            sticky: false,
            next_state: 1,
            current_state: 0,
            saved_state: Some(0),
        }
    }

    fn allocate_state(&mut self) -> u64 {
        let state = self.next_state;
        self.next_state += 1;
        state
    }

    /// 文档当前是否和最后一次 `mark_saved` 的状态不同。
    ///
    /// 只看撤销栈的游标 / 深度是不够的：撤销回到保存点后，`undo` 与 `redo`
    /// 的长度会回到保存时的样子；但如果之后在同深度做了别的编辑，深度同样
    /// 对得上，内容却变了。状态编号能同时覆盖这两种情况。
    pub fn is_dirty(&self) -> bool {
        self.saved_state != Some(self.current_state)
    }

    /// 把当前状态记为「已保存」的基线。
    pub fn mark_saved(&mut self) {
        self.saved_state = Some(self.current_state);
    }

    /// 开始一次交互：此后的改动不再受 `coalesce_ms` 时间窗口限制。
    pub fn begin_interaction(&mut self) {
        self.sticky = true;
    }

    /// 结束一次交互：清掉最后一步的合并标记，让下一次修改必然开新步。
    pub fn commit(&mut self) {
        self.sticky = false;
        if let Some(last) = self.undo.last_mut() {
            last.coalesce = None;
        }
    }

    /// 当前是否处于交互中（供测试 / 调试）。
    pub fn interaction_active(&self) -> bool {
        self.sticky
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    pub fn set_coalesce_ms(&mut self, ms: u64) {
        self.coalesce_ms = ms;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|entry| entry.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.label.as_str())
    }

    /// 最近到最早的操作标签（「历史」面板）。
    pub fn history(&self) -> Vec<&str> {
        self.undo
            .iter()
            .rev()
            .map(|entry| entry.label.as_str())
            .collect()
    }

    /// 完整历史（最早 → 最新），已撤销的步骤附在末尾并标记 `undone`。
    ///
    /// 和 `history()` 不同，这里包含 redo 栈：引擎的撤销历史窗口会把撤销过的
    /// 步骤继续列出来，点回去就是 redo，而不是让它们凭空消失。
    pub fn history_entries(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .undo
            .iter()
            .map(|entry| (entry.label.clone(), false))
            .collect();
        // redo 栈的最后一个元素是下一次 redo 要执行的步骤，所以整体反转后
        // 才是「按时间顺序」接在 undo 之后的步骤。
        out.extend(
            self.redo
                .iter()
                .rev()
                .map(|entry| (entry.label.clone(), true)),
        );
        out
    }

    /// 当前状态在 `history_entries()` 中的位置（= 已执行步骤数）。
    pub fn history_cursor(&self) -> usize {
        self.undo.len()
    }

    pub fn clear(&mut self) {
        let was_dirty = self.is_dirty();
        self.undo.clear();
        self.redo.clear();
        self.current_state = self.allocate_state();
        // 清空历史不应顺手清掉脏标记：干净的文档继续保持干净，脏的文档也
        // 不会因为「历史没了」而看起来像没改过。
        self.saved_state = if was_dirty {
            None
        } else {
            Some(self.current_state)
        };
    }

    /// 一次字段写入。`entity_id + component + path` 相同且在合并窗口内时，
    /// 直接把上一步的 `after` 更新成新值（保留最初的 `before`）。
    #[allow(clippy::too_many_arguments)]
    pub fn push_field(
        &mut self,
        doc: &mut SceneDoc,
        entity_id: u32,
        component: &str,
        path: &str,
        before: Value,
        after: Value,
        now_ms: u64,
    ) {
        if before == after {
            return;
        }
        let coalesce = format!("{entity_id}/{component}/{path}");
        let state = self.allocate_state();
        if let Some(last) = self.undo.last_mut() {
            if last.coalesce.as_deref() == Some(coalesce.as_str())
                && (self.sticky || now_ms.saturating_sub(last.at_ms) <= self.coalesce_ms)
            {
                if let Command::SetField {
                    after: last_after, ..
                } = &mut last.command
                {
                    *last_after = after.clone();
                    last.at_ms = now_ms;
                    last.after_state = state;
                    self.current_state = state;
                    self.redo.clear();
                    apply_field(doc, entity_id, component, path, &after);
                    return;
                }
            }
        }

        let command = Command::SetField {
            entity_id,
            component: component.to_string(),
            path: path.to_string(),
            before,
            after: after.clone(),
        };
        apply_field(doc, entity_id, component, path, &after);
        self.undo.push(Entry {
            label: format!("{} · {}", short_component(component), path),
            command,
            coalesce: Some(coalesce),
            at_ms: now_ms,
            selection: None,
            before_state: self.current_state,
            after_state: state,
        });
        self.current_state = state;
        self.redo.clear();
        self.trim();
    }

    /// Transform 的一次拖动：同一实体的连续改动合并成一步。
    pub fn push_transform(
        &mut self,
        doc: &mut SceneDoc,
        entity_id: u32,
        before: TransformJson,
        after: TransformJson,
        now_ms: u64,
    ) {
        if before == after {
            return;
        }
        let coalesce = format!("transform/{entity_id}");
        let state = self.allocate_state();
        if let Some(last) = self.undo.last_mut() {
            if last.coalesce.as_deref() == Some(coalesce.as_str())
                && (self.sticky || now_ms.saturating_sub(last.at_ms) <= self.coalesce_ms)
            {
                if let Command::Transform {
                    after: last_after, ..
                } = &mut last.command
                {
                    *last_after = after;
                    last.at_ms = now_ms;
                    last.after_state = state;
                    self.current_state = state;
                    self.redo.clear();
                    apply_transform(doc, entity_id, &after);
                    return;
                }
            }
        }
        apply_transform(doc, entity_id, &after);
        self.undo.push(Entry {
            label: "修改 Transform".to_string(),
            command: Command::Transform {
                entity_id,
                before,
                after,
            },
            coalesce: Some(coalesce),
            at_ms: now_ms,
            selection: None,
            before_state: self.current_state,
            after_state: state,
        });
        self.current_state = state;
        self.redo.clear();
        self.trim();
    }

    /// 多选 Transform 的一次拖动：`changes` 是 (实体 id, before, after)，
    /// 同一 `coalesce` key 的连续拖动合并成一步（整组一起撤销）。
    pub fn push_transform_batch(
        &mut self,
        doc: &mut SceneDoc,
        changes: Vec<(u32, TransformJson, TransformJson)>,
        coalesce: String,
        now_ms: u64,
    ) {
        let changes: Vec<_> = changes
            .into_iter()
            .filter(|(_, before, after)| before != after)
            .collect();
        if changes.is_empty() {
            return;
        }
        let state = self.allocate_state();
        if let Some(last) = self.undo.last_mut() {
            if last.coalesce.as_deref() == Some(coalesce.as_str())
                && (self.sticky || now_ms.saturating_sub(last.at_ms) <= self.coalesce_ms)
            {
                if let Command::TransformBatch {
                    changes: last_changes,
                } = &mut last.command
                {
                    for (entity_id, before, after) in &changes {
                        match last_changes
                            .iter_mut()
                            .find(|(existing, _, _)| existing == entity_id)
                        {
                            Some(slot) => slot.2 = *after,
                            None => last_changes.push((*entity_id, *before, *after)),
                        }
                        apply_transform(doc, *entity_id, after);
                    }
                    last.at_ms = now_ms;
                    last.after_state = state;
                    self.current_state = state;
                    self.redo.clear();
                    return;
                }
            }
        }
        for (entity_id, _, after) in &changes {
            apply_transform(doc, *entity_id, after);
        }
        self.undo.push(Entry {
            label: "修改 Transform".to_string(),
            command: Command::TransformBatch { changes },
            coalesce: Some(coalesce),
            at_ms: now_ms,
            selection: None,
            before_state: self.current_state,
            after_state: state,
        });
        self.current_state = state;
        self.redo.clear();
        self.trim();
    }

    /// 结构性编辑：把 `before` 快照与编辑后的文档整体记成一步。
    ///
    /// 不带选择快照 —— 撤销 / 重做时选择集保持现状（调用方自己校正）。
    pub fn push_snapshot(&mut self, label: impl Into<String>, before: SceneDoc, after: &SceneDoc) {
        self.push_snapshot_with_selection(label, before, None, after);
    }

    /// 结构性编辑 + 选中实体快照（#513）。
    ///
    /// `selection` 里的 id 在撤销 / 重做后按 `EntityJson.id` 映射回下标，找不到的
    /// 直接丢弃，所以「删掉再撤销」能准确回到原来的实体，而不是某个碰巧同下标的对象。
    pub fn push_snapshot_with_selection(
        &mut self,
        label: impl Into<String>,
        before: SceneDoc,
        selection: Option<SelectionSnapshot>,
        after: &SceneDoc,
    ) {
        if before == *after {
            return;
        }
        let state = self.allocate_state();
        self.undo.push(Entry {
            label: label.into(),
            command: Command::Snapshot {
                before: Box::new(before),
                after: Box::new(after.clone()),
            },
            coalesce: None,
            at_ms: 0,
            selection,
            before_state: self.current_state,
            after_state: state,
        });
        self.current_state = state;
        self.redo.clear();
        self.trim();
    }

    pub fn undo(&mut self, doc: &mut SceneDoc) -> Option<UndoOutcome> {
        let mut entry = self.undo.pop()?;
        match &entry.command {
            Command::SetField {
                entity_id,
                component,
                path,
                before,
                ..
            } => {
                apply_field(doc, *entity_id, component, path, before);
            }
            Command::Transform {
                entity_id, before, ..
            } => {
                apply_transform(doc, *entity_id, before);
            }
            Command::TransformBatch { changes } => {
                for (entity_id, before, _) in changes {
                    apply_transform(doc, *entity_id, before);
                }
            }
            Command::Snapshot { before, .. } => {
                *doc = (**before).clone();
            }
        }
        let selection = entry
            .selection
            .as_ref()
            .map(|snapshot| snapshot.before.clone());
        entry.coalesce = None;
        self.current_state = entry.before_state;
        let label = entry.label.clone();
        self.redo.push(entry);
        Some(UndoOutcome { label, selection })
    }

    pub fn redo(&mut self, doc: &mut SceneDoc) -> Option<UndoOutcome> {
        let mut entry = self.redo.pop()?;
        match &entry.command {
            Command::SetField {
                entity_id,
                component,
                path,
                after,
                ..
            } => {
                apply_field(doc, *entity_id, component, path, after);
            }
            Command::Transform {
                entity_id, after, ..
            } => {
                apply_transform(doc, *entity_id, after);
            }
            Command::TransformBatch { changes } => {
                for (entity_id, _, after) in changes {
                    apply_transform(doc, *entity_id, after);
                }
            }
            Command::Snapshot { after, .. } => {
                *doc = (**after).clone();
            }
        }
        let selection = entry
            .selection
            .as_ref()
            .map(|snapshot| snapshot.after.clone());
        entry.coalesce = None;
        self.current_state = entry.after_state;
        let label = entry.label.clone();
        self.undo.push(entry);
        Some(UndoOutcome { label, selection })
    }

    fn trim(&mut self) {
        let limit = self.limit;
        if self.undo.len() > limit {
            let excess = self.undo.len() - limit;
            self.undo.drain(0..excess);
        }
    }
}

fn apply_transform(doc: &mut SceneDoc, entity_id: u32, transform: &TransformJson) {
    if let Some(index) = doc.index_of_id(entity_id) {
        doc.entities[index].transform = *transform;
    }
}

fn short_component(component: &str) -> &str {
    component.rsplit("::").next().unwrap_or(component)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{SceneDoc, TransformJson};
    use serde_json::json;

    fn doc_with_camera() -> (SceneDoc, u32) {
        let mut doc = SceneDoc::default();
        let index = doc.add_named_child(None, "Camera");
        doc.add_component(
            index,
            "prism_engine::scene::Camera",
            json!({ "fov_y_degrees": 60.0, "near": 0.1 }),
        );
        let id = doc.entities[index].id.unwrap();
        (doc, id)
    }

    #[test]
    fn field_edits_coalesce_within_window() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(60.0),
            json!(61.0),
            0,
        );
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(61.0),
            json!(70.0),
            100,
        );
        assert_eq!(stack.history().len(), 1, "同一字段连续输入合并成一步");
        assert_eq!(stack.history()[0], "Camera · fov_y_degrees");

        // 撤销回到最初的 60。
        stack.undo(&mut doc);
        let camera = doc.entities[0]
            .components
            .get("prism_engine::scene::Camera")
            .unwrap();
        assert_eq!(camera["fov_y_degrees"], json!(60.0));
        assert!(stack.can_redo());

        stack.redo(&mut doc);
        let camera = doc.entities[0]
            .components
            .get("prism_engine::scene::Camera")
            .unwrap();
        assert_eq!(camera["fov_y_degrees"], json!(70.0));
    }

    #[test]
    fn different_fields_or_stale_edits_create_new_steps() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.2),
            0,
        );
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(60.0),
            json!(70.0),
            10,
        );
        assert_eq!(stack.history().len(), 2);

        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(70.0),
            json!(80.0),
            10_000,
        );
        assert_eq!(stack.history().len(), 3, "超出合并窗口算新一步");
    }

    #[test]
    fn new_edit_clears_redo() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.2),
            0,
        );
        stack.undo(&mut doc);
        assert!(stack.can_redo());
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.3),
            5_000,
        );
        assert!(!stack.can_redo(), "新编辑应清空 redo");
    }

    #[test]
    fn snapshot_undo_redo_restores_whole_doc() {
        let (mut doc, _) = doc_with_camera();
        let before = doc.clone();
        doc.add_named_child(None, "Cube");
        let mut stack = UndoStack::new();
        stack.push_snapshot("新建实体", before.clone(), &doc);
        assert_eq!(doc.entities.len(), 2);

        stack.undo(&mut doc);
        assert_eq!(doc.entities.len(), 1);
        stack.redo(&mut doc);
        assert_eq!(doc.entities.len(), 2);
    }

    #[test]
    fn equal_snapshots_are_ignored() {
        let (doc, _) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_snapshot("无变化", doc.clone(), &doc);
        assert!(!stack.can_undo());
    }

    #[test]
    fn limit_drops_oldest_entries() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.set_limit(3);
        for step in 0..5 {
            stack.push_field(
                &mut doc,
                id,
                "prism_engine::scene::Camera",
                "near",
                json!(step as f64),
                json!(step as f64 + 1.0),
                step as u64 * 10_000,
            );
        }
        assert_eq!(stack.history().len(), 3);
    }

    #[test]
    fn transform_edits_coalesce_and_undo() {
        let (mut doc, id) = doc_with_camera();
        let before = doc.entities[0].transform;
        let mut after = before;
        after.position[0] = 5.0;
        let mut stack = UndoStack::new();
        stack.push_transform(&mut doc, id, before, after, 0);
        assert_eq!(doc.entities[0].transform.position[0], 5.0);

        let mut second = after;
        second.position[0] = 7.0;
        stack.push_transform(&mut doc, id, after, second, 100);
        assert_eq!(stack.history().len(), 1, "同一实体的拖动合并");
        assert_eq!(doc.entities[0].transform.position[0], 7.0);

        stack.undo(&mut doc);
        assert_eq!(doc.entities[0].transform.position[0], 0.0);
        stack.redo(&mut doc);
        assert_eq!(doc.entities[0].transform.position[0], 7.0);
    }

    #[test]
    fn interaction_boundary_holds_and_closes_undo_steps() {
        let (mut doc, id) = doc_with_camera();
        let before = doc.entities[0].transform;
        let mut stack = UndoStack::new();

        // 交互中：超过 800ms 的停顿仍然并成一步。
        stack.begin_interaction();
        let mut step = before;
        step.position[0] = 1.0;
        stack.push_transform(&mut doc, id, before, step, 0);
        let mut step2 = step;
        step2.position[0] = 2.0;
        stack.push_transform(&mut doc, id, step, step2, 5_000);
        assert_eq!(stack.history().len(), 1, "一次拖动 = 一步");
        assert!(stack.interaction_active());

        // 松手后立刻再拖：必须开新步，即使间隔远小于 800ms。
        stack.commit();
        assert!(!stack.interaction_active());
        let mut step3 = step2;
        step3.position[0] = 3.0;
        stack.push_transform(&mut doc, id, step2, step3, 5_010);
        assert_eq!(stack.history().len(), 2, "两次拖动 = 两步");

        stack.undo(&mut doc);
        assert_eq!(doc.entities[0].transform.position[0], 2.0);
        stack.undo(&mut doc);
        assert_eq!(doc.entities[0].transform.position[0], 0.0);
    }

    #[test]
    fn missing_entity_is_silently_skipped() {
        let (mut doc, _) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(&mut doc, 999, "c", "p", json!(1), json!(2), 0);
        stack.undo(&mut doc);
        assert_eq!(doc.entities.len(), 1);
    }

    #[test]
    fn undo_back_to_the_saved_state_clears_dirty() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        assert!(!stack.is_dirty(), "新文档是干净的");

        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(60.0),
            json!(70.0),
            0,
        );
        assert!(stack.is_dirty());

        stack.undo(&mut doc);
        assert!(!stack.is_dirty(), "撤销回保存点后应该是干净的");

        stack.redo(&mut doc);
        assert!(stack.is_dirty(), "重做后又离开保存点");
    }

    #[test]
    fn a_new_edit_at_the_saved_depth_is_dirty() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.2),
            0,
        );
        stack.undo(&mut doc);
        assert!(!stack.is_dirty());

        // 撤销后在同一深度做别的编辑：栈长度会重新变成 1，但内容不是保存点。
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.3),
            10_000,
        );
        assert!(stack.is_dirty(), "同深度但内容不同仍是未保存");
    }

    #[test]
    fn mark_saved_moves_the_baseline() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "fov_y_degrees",
            json!(60.0),
            json!(70.0),
            0,
        );
        stack.mark_saved();
        assert!(!stack.is_dirty(), "保存后基线跟着前移");

        stack.undo(&mut doc);
        assert!(stack.is_dirty(), "保存后再撤销同样算未保存");
        stack.redo(&mut doc);
        assert!(!stack.is_dirty(), "重做回保存点又干净了");
    }

    #[test]
    fn clearing_history_keeps_the_dirty_state() {
        let (mut doc, id) = doc_with_camera();
        let mut stack = UndoStack::new();
        stack.clear();
        assert!(!stack.is_dirty(), "干净文档清历史后还是干净");

        stack.push_field(
            &mut doc,
            id,
            "prism_engine::scene::Camera",
            "near",
            json!(0.1),
            json!(0.2),
            0,
        );
        stack.clear();
        assert!(stack.is_dirty(), "清历史不能把未保存改动洗白");
    }
}
