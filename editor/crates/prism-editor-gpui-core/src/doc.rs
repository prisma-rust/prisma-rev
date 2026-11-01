//! 场景/预制体文档模型（`.scene.json` 的 Rust 镜像）。
//!
//! 与 `editor/src/types.ts` 的 `SceneDoc` / `EntityJson` 一一对应，字段名保持
//! camelCase；`parent` 指向实体的稳定 `id`（v3 起不再用数组下标），因此层级
//! 重排、删除、预制体实例化都不会让父子关系漂移。

use crate::components::ComponentMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 预制体实例链接组件名（编辑器元数据，引擎运行时不查询）。
pub const PREFAB_LINK_COMPONENT: &str = "prism_engine::scene::PrefabInstance";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransformJson {
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Default for TransformJson {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

impl TransformJson {
    /// 平移增量（欧拉角不作为真理源；编辑器只写 position/rotation/scale）。
    pub fn translated(mut self, delta: [f32; 3]) -> Self {
        for (axis, value) in delta.iter().enumerate() {
            self.position[axis] += value;
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    /// 父实体的 `id`（不是数组下标）。
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub transform: TransformJson,
    /// 组件表，顺序 = 检查器里的显示顺序 = 作者添加 / 排布的顺序（#512）。
    #[serde(default)]
    pub components: ComponentMap,
    /// 编辑器不认识的实体级键（`_note`、工具元数据等）。保存时原样写回，
    /// 不能因为一次 Save 就静默丢数据（#615）。
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for EntityJson {
    fn default() -> Self {
        Self {
            id: None,
            name: None,
            parent: None,
            transform: TransformJson::default(),
            components: ComponentMap::new(),
            extra: serde_json::Map::new(),
        }
    }
}

impl EntityJson {
    pub fn display_name(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => "GameObject".to_string(),
        }
    }

    pub fn component(&self, name: &str) -> Option<&Value> {
        self.components.get(name)
    }

    pub fn has_component(&self, name: &str) -> bool {
        self.components.contains_key(name)
    }

    pub fn active(&self) -> bool {
        match self.components.get("prism_engine::scene::Active") {
            Some(Value::Object(map)) => map
                .get("value")
                .and_then(|value| value.as_bool())
                .or_else(|| map.get("active").and_then(|value| value.as_bool()))
                .unwrap_or(true),
            _ => true,
        }
    }

    pub fn prefab_link(&self) -> Option<&str> {
        self.components
            .get(PREFAB_LINK_COMPONENT)
            .and_then(|value| value.get("prefab"))
            .and_then(|value| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneDoc {
    pub version: u32,
    #[serde(default)]
    pub entities: Vec<EntityJson>,
    /// 文档级未知键（`_comment`、`x_meta` 等）。原样保留并在 Save 时写回（#615）。
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub const DOC_VERSION: u32 = 3;

impl Default for SceneDoc {
    fn default() -> Self {
        Self {
            version: DOC_VERSION,
            entities: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }
}

impl SceneDoc {
    pub fn from_json_str(text: &str) -> Result<Self, serde_json::Error> {
        let mut doc: SceneDoc = serde_json::from_str(text)?;
        doc.normalize();
        Ok(doc)
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 给缺失 id 的实体按数组顺序补 id，并把 `parent`（旧文档里可能是下标）
    /// 迁移成 id。与 `prism_scene::migrate_scene_doc` 的意图一致。
    pub fn normalize(&mut self) {
        if self.version < DOC_VERSION {
            // v1/v2：parent 是下标。
            let index_to_id: Vec<u32> = (0..self.entities.len()).map(|i| i as u32 + 1).collect();
            for (index, entity) in self.entities.iter_mut().enumerate() {
                if entity.id.is_none() {
                    entity.id = Some(index_to_id[index]);
                }
                entity.parent = entity
                    .parent
                    .and_then(|parent| index_to_id.get(parent as usize).copied());
            }
            self.version = DOC_VERSION;
        }

        let mut next = self
            .entities
            .iter()
            .filter_map(|entity| entity.id)
            .max()
            .unwrap_or(0)
            + 1;
        for entity in &mut self.entities {
            if entity.id.is_none() {
                entity.id = Some(next);
                next += 1;
            }
        }

        // 丢掉指向不存在 id 的 parent（防环在 reparent 时单独检查）。
        let ids: HashSet<u32> = self.entities.iter().filter_map(|e| e.id).collect();
        for entity in &mut self.entities {
            if let Some(parent) = entity.parent {
                if !ids.contains(&parent) {
                    entity.parent = None;
                }
            }
        }
    }

    pub fn next_id(&self) -> u32 {
        self.entities
            .iter()
            .filter_map(|entity| entity.id)
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn index_of_id(&self, id: u32) -> Option<usize> {
        self.entities
            .iter()
            .position(|entity| entity.id == Some(id))
    }

    pub fn id_of_index(&self, index: usize) -> Option<u32> {
        self.entities.get(index).and_then(|entity| entity.id)
    }

    pub fn roots(&self) -> Vec<usize> {
        self.entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| entity.parent.is_none())
            .map(|(index, _)| index)
            .collect()
    }

    /// 直接子实体（数组顺序）。
    pub fn children(&self, index: usize) -> Vec<usize> {
        let Some(id) = self.id_of_index(index) else {
            return Vec::new();
        };
        self.entities
            .iter()
            .enumerate()
            .filter(|(_, entity)| entity.parent == Some(id))
            .map(|(index, _)| index)
            .collect()
    }

    /// 深度优先的可见行（`expanded` 决定是否展开某个实体的子级）。
    pub fn visible_rows(&self, expanded: &HashSet<u32>) -> Vec<(usize, u32)> {
        fn walk(
            doc: &SceneDoc,
            indices: Vec<usize>,
            depth: u32,
            expanded: &HashSet<u32>,
            out: &mut Vec<(usize, u32)>,
        ) {
            for index in indices {
                out.push((index, depth));
                if let Some(id) = doc.id_of_index(index) {
                    if expanded.contains(&id) {
                        walk(doc, doc.children(index), depth + 1, expanded, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(self, self.roots(), 0, expanded, &mut out);
        out
    }

    pub fn is_expandable(&self, index: usize) -> bool {
        !self.children(index).is_empty()
    }

    /// 面包屑路径（根 → 实体）。
    pub fn path_of(&self, index: usize) -> Vec<usize> {
        let mut path = Vec::new();
        let mut current = Some(index);
        while let Some(index) = current {
            path.push(index);
            current = self
                .entities
                .get(index)
                .and_then(|entity| entity.parent.and_then(|parent| self.index_of_id(parent)));
        }
        path.reverse();
        path
    }

    pub fn descendants(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = self.children(index);
        while let Some(index) = stack.pop() {
            out.push(index);
            stack.extend(self.children(index));
        }
        out
    }

    pub fn add_entity(&mut self, entity: EntityJson) -> usize {
        let mut entity = entity;
        entity.id = Some(self.next_id());
        self.entities.push(entity);
        self.entities.len() - 1
    }

    pub fn add_named_child(&mut self, parent: Option<usize>, name: &str) -> usize {
        let parent_id = parent.and_then(|index| self.id_of_index(index));
        let entity = EntityJson {
            name: Some(name.to_string()),
            parent: parent_id,
            ..Default::default()
        };
        self.add_entity(entity)
    }

    pub fn remove_entities(&mut self, indices: &[usize]) -> Vec<usize> {
        let mut removed: Vec<usize> = indices
            .iter()
            .copied()
            .flat_map(|index| {
                let mut all = vec![index];
                all.extend(self.descendants(index));
                all
            })
            .collect();
        removed.sort_unstable();
        removed.dedup();

        let removed_ids: HashSet<u32> = removed
            .iter()
            .filter_map(|index| self.id_of_index(*index))
            .collect();
        let removed_set: HashSet<usize> = removed.iter().copied().collect();

        // 孤儿子级变根。
        for entity in &mut self.entities {
            if let Some(parent) = entity.parent {
                if removed_ids.contains(&parent) {
                    entity.parent = None;
                }
            }
        }

        let mut kept = Vec::with_capacity(self.entities.len());
        for (index, entity) in self.entities.drain(..).enumerate() {
            if !removed_set.contains(&index) {
                kept.push(entity);
            }
        }
        self.entities = kept;
        removed
    }

    pub fn rename(&mut self, index: usize, name: &str) {
        if let Some(entity) = self.entities.get_mut(index) {
            entity.name = Some(name.to_string());
        }
    }

    /// 把 `index` 挂到 `new_parent` 下（`None` = 根），并插到 `position`。
    /// 拒绝自父与成环。
    pub fn reparent(
        &mut self,
        index: usize,
        new_parent: Option<usize>,
        position: Option<usize>,
    ) -> Result<(), &'static str> {
        if self.entities.get(index).is_none() {
            return Err("实体不存在");
        }
        if let Some(parent) = new_parent {
            if parent == index {
                return Err("不能把实体挂到自己下面");
            }
            if self.descendants(index).contains(&parent) {
                return Err("不能把实体挂到自己的子级下面");
            }
        }

        let parent_id = match new_parent {
            Some(parent) => self.id_of_index(parent).ok_or("父实体不存在")?,
            None => {
                self.entities[index].parent = None;
                if let Some(position) = position {
                    self.move_entity(index, position)?;
                }
                return Ok(());
            }
        };
        self.entities[index].parent = Some(parent_id);
        if let Some(position) = position {
            self.move_entity(index, position)?;
        }
        Ok(())
    }

    /// 移动数组位置（层级树的拖拽排序）；父子关系已由调用方设置。
    /// 多选拖拽改父级：只改 parent，不调整兄弟顺序；拒绝挂到选中项自身或其后代下。
    pub fn reparent_many(
        &mut self,
        roots: &[usize],
        new_parent: Option<usize>,
    ) -> Result<usize, &'static str> {
        let mut roots: Vec<usize> = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();
        if roots.is_empty() {
            return Ok(0);
        }
        let parent_id = match new_parent {
            Some(parent) => {
                if roots.contains(&parent) {
                    return Err("不能把实体挂到自己下面");
                }
                for root in &roots {
                    if self.entities.get(*root).is_none() {
                        continue;
                    }
                    if self.descendants(*root).contains(&parent) {
                        return Err("不能把实体挂到自己的子级下面");
                    }
                }
                Some(self.id_of_index(parent).ok_or("父实体不存在")?)
            }
            None => None,
        };
        let mut moved = 0;
        for root in roots {
            if let Some(entity) = self.entities.get_mut(root) {
                entity.parent = parent_id;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// 把一组实体（含各自的子树）插到 `target` 的前/后，保持组内相对顺序（#525）。
    ///
    /// 与 [`SceneDoc::reparent_many`] 的区别：那个只改父级、不改兄弟顺序；层级拖拽的
    /// 「插入到上一行 / 下一行」需要真正的顺序变化。组内每个根改挂到目标的父级，
    /// 子树内部的父子关系不变。
    pub fn reparent_many_at(
        &mut self,
        roots: &[usize],
        target: usize,
        before: bool,
    ) -> Result<usize, &'static str> {
        let mut roots: Vec<usize> = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();
        roots.retain(|index| *index < self.entities.len());
        if roots.is_empty() {
            return Ok(0);
        }
        if target >= self.entities.len() {
            return Err("目标实体不存在");
        }
        if roots.contains(&target) {
            return Err("不能把实体拖到它自己身上");
        }
        for root in &roots {
            if self.descendants(*root).contains(&target) {
                return Err("不能把实体拖到自己的子级里");
            }
        }

        // 搬的是整棵子树：根 + 后代，保持数组顺序（父级先于子级）。
        let mut moving: Vec<usize> = Vec::new();
        for root in &roots {
            moving.push(*root);
            moving.extend(self.descendants(*root));
        }
        moving.sort_unstable();
        moving.dedup();
        let is_root: Vec<bool> = moving.iter().map(|index| roots.contains(index)).collect();

        // 目标被搬走后会前移的格数 = 它前面有多少个被搬走的实体。
        let shift = moving.iter().filter(|index| **index < target).count();
        let mut insert_at = target - shift;
        if !before {
            insert_at += 1;
        }
        let parent_id = self.entities.get(target).and_then(|entity| entity.parent);

        let mut pulled: Vec<(EntityJson, bool)> = Vec::with_capacity(moving.len());
        for (position, index) in moving.iter().enumerate().rev() {
            let entity = self.entities.remove(*index);
            pulled.push((entity, is_root[position]));
        }
        pulled.reverse();
        for (entity, is_root) in pulled.iter_mut() {
            if *is_root {
                entity.parent = parent_id;
            }
        }

        let tail = self.entities.split_off(insert_at.min(self.entities.len()));
        self.entities
            .extend(pulled.into_iter().map(|(entity, _)| entity));
        self.entities.extend(tail);
        Ok(roots.len())
    }

    pub fn move_entity(&mut self, from: usize, to: usize) -> Result<usize, &'static str> {
        if from >= self.entities.len() {
            return Err("实体不存在");
        }
        let to = to.min(self.entities.len() - 1);
        if from == to {
            return Ok(to);
        }
        let entity = self.entities.remove(from);
        self.entities.insert(to, entity);
        Ok(to)
    }

    /// 复制整棵子树（引擎的 Ctrl+D）：内部 parent 重映射到新 id，根保持原父级。
    pub fn duplicate_subtree(&mut self, index: usize) -> Option<usize> {
        self.entities.get(index)?;
        // 收集子树（根 + 后代），保持数组顺序，保证父级先于子级。
        let mut selected = vec![index];
        selected.extend(self.descendants(index));
        selected.sort_unstable();

        let base = self.next_id();
        let remap: HashMap<u32, u32> = selected
            .iter()
            .filter_map(|index| self.id_of_index(*index))
            .enumerate()
            .map(|(offset, id)| (id, base + offset as u32))
            .collect();
        let root_id = self.id_of_index(index);

        let start = self.entities.len();
        for old_index in selected {
            let Some(source) = self.entities.get(old_index).cloned() else {
                continue;
            };
            let mut copy = source.clone();
            let old_id = source.id;
            copy.id = old_id.and_then(|id| remap.get(&id).copied());
            copy.parent = if old_id == root_id {
                source.parent
            } else {
                source.parent.and_then(|parent| remap.get(&parent).copied())
            };
            if old_id == root_id {
                copy.name = Some(self.unique_name(&source.display_name(), source.parent));
            }
            self.entities.push(copy);
        }
        if self.entities.len() == start {
            return None;
        }
        Some(start)
    }

    pub fn duplicate(&mut self, index: usize) -> Option<usize> {
        let source = self.entities.get(index)?.clone();
        let mut copy = source.clone();
        copy.id = Some(self.next_id());
        copy.name = Some(self.unique_name(&source.display_name(), source.parent));
        let inserted = self.entities.len();
        self.entities.push(copy);
        Some(inserted)
    }

    pub fn set_active(&mut self, index: usize, active: bool) {
        let Some(entity) = self.entities.get_mut(index) else {
            return;
        };
        const ACTIVE: &str = "prism_engine::scene::Active";
        if !entity.components.contains_key(ACTIVE) {
            entity
                .components
                .insert(ACTIVE.to_string(), serde_json::json!({ "value": true }));
        }
        let Some(entry) = entity.components.get_mut(ACTIVE) else {
            return;
        };
        if let Value::Object(map) = entry {
            map.insert("value".to_string(), Value::Bool(active));
        }
    }

    pub fn add_component(&mut self, index: usize, name: &str, value: Value) -> bool {
        let Some(entity) = self.entities.get_mut(index) else {
            return false;
        };
        if entity.components.contains_key(name) {
            return false;
        }
        entity.components.insert(name.to_string(), value);
        true
    }

    pub fn remove_component(&mut self, index: usize, name: &str) -> Option<Value> {
        let entity = self.entities.get_mut(index)?;
        if name == "Transform" {
            return None;
        }
        entity.components.remove(name)
    }

    pub fn component_names(&self, index: usize) -> Vec<String> {
        self.entities
            .get(index)
            .map(|entity| entity.components.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// 提取选中实体子树为独立文档（「存为预制体」）。
    /// 根上的预制体实例链接会被剥掉，避免预制体引用自己。
    pub fn extract_subtree(&self, roots: &[usize]) -> SceneDoc {
        self.extract_subtree_inner(roots, true)
    }

    /// 复制 / 剪切用：与 `extract_subtree` 相同，但保留根上的预制体链接，
    /// 这样粘贴出来的仍是同源实例（引擎的复制预制体实例行为）。
    pub fn extract_subtree_preserving_links(&self, roots: &[usize]) -> SceneDoc {
        self.extract_subtree_inner(roots, false)
    }

    fn extract_subtree_inner(&self, roots: &[usize], strip_root_links: bool) -> SceneDoc {
        let mut selected: Vec<usize> = Vec::new();
        for root in roots {
            select_subtree(self, *root, &mut selected);
        }
        selected.sort_unstable();
        selected.dedup();

        let id_map: HashMap<u32, u32> = selected
            .iter()
            .filter_map(|index| self.id_of_index(*index))
            .enumerate()
            .map(|(new_index, id)| (id, new_index as u32 + 1))
            .collect();

        let root_set: HashSet<u32> = roots
            .iter()
            .filter_map(|index| self.id_of_index(*index))
            .collect();

        let mut doc = SceneDoc::default();
        for index in selected {
            let Some(source) = self.entities.get(index) else {
                continue;
            };
            let mut entity = source.clone();
            let old_id = entity.id;
            entity.id = old_id.and_then(|id| id_map.get(&id).copied());
            entity.parent = entity
                .parent
                .and_then(|parent| id_map.get(&parent).copied())
                .filter(|_parent| {
                    // 根实体不能保留指向外部父级的 parent。
                    old_id.map(|id| !root_set.contains(&id)).unwrap_or(false)
                });
            let is_root = old_id.map(|id| root_set.contains(&id)).unwrap_or(false);
            if strip_root_links && is_root {
                entity.components.remove(PREFAB_LINK_COMPONENT);
            }
            doc.entities.push(entity);
        }
        doc.normalize();
        doc
    }

    /// 预制体实例化：追加到场景末尾、内部父 id 平移、根挂到 `parent`、名字去重。
    pub fn instantiate_prefab(
        &mut self,
        prefab: &SceneDoc,
        parent: Option<usize>,
        prefab_path: &str,
    ) -> Vec<usize> {
        let mut result = Vec::new();
        if prefab.entities.is_empty() {
            return result;
        }

        let base = self.next_id();
        let parent_id = parent.and_then(|index| self.id_of_index(index));
        let root_old_id = prefab.id_of_index(0);

        let start = self.entities.len();
        for (position, source) in prefab.entities.iter().enumerate() {
            let mut entity = source.clone();
            let old_id = entity.id.unwrap_or(position as u32 + 1);
            entity.id = Some(base + old_id);
            entity.parent = if Some(old_id) == root_old_id {
                parent_id
            } else {
                entity.parent.map(|parent| base + parent)
            };
            if Some(old_id) == root_old_id {
                entity.name = Some(self.unique_name(&entity.display_name(), parent_id));
                if !prefab_path.is_empty() {
                    entity.components.insert(
                        PREFAB_LINK_COMPONENT.to_string(),
                        serde_json::json!({ "prefab": prefab_path }),
                    );
                }
            }
            self.entities.push(entity);
            result.push(start + position);
        }
        self.normalize();
        result
    }

    /// 引擎的 `GetUniqueNameForSibling`：剥掉末尾的 ` (数字)` 得到基名，
    /// 再在同级里找最小的未占用序号（第一个同名实体不加后缀）。
    pub fn unique_name(&self, base: &str, parent: Option<u32>) -> String {
        let base = strip_name_suffix(base);
        let taken: HashSet<String> = self
            .entities
            .iter()
            .filter(|entity| entity.parent == parent)
            .map(|entity| entity.display_name())
            .collect();
        if !taken.contains(base) {
            return base.to_string();
        }
        let mut index = 1;
        loop {
            let candidate = format!("{base} ({index})");
            if !taken.contains(&candidate) {
                return candidate;
            }
            index += 1;
        }
    }

    /// 资产重命名 / 移动时改写本文档里的引用（#465）。
    ///
    /// 与旧的「任何字符串精确相等就改」不同，这里只碰**资产路径字段**（
    /// [`crate::assets::is_asset_path_field`]：`*_path` 后缀 + `env_map` /
    /// `volume` / `prefab`，外加 `materials` / `textures` 这类路径数组），
    /// 所以实体名、`Text::content` 里恰好写着同一段路径的字符串保持原样。
    ///
    /// `doc_dir` 是**本文档所在的目录**（`assets/scenes` 这种），用来解析
    /// `../textures/wood.png` 这种相对写法；改写后仍按作者原来的风格写回，
    /// 不会把相对的 `../` 变成绝对的 `assets/...`。
    pub fn rewrite_asset_references(
        &mut self,
        doc_dir: &str,
        from: &str,
        to: &str,
    ) -> ReferenceRewrite {
        let mut out = ReferenceRewrite::default();
        for entity in &mut self.entities {
            for (component, value) in entity.components.iter_mut() {
                rewrite_references_in(value, "", component, doc_dir, from, to, &mut out);
            }
        }
        out
    }

    pub fn prefab_links(&self) -> Vec<(usize, String)> {
        self.entities
            .iter()
            .enumerate()
            .filter_map(|(index, entity)| {
                entity
                    .prefab_link()
                    .map(|prefab| (index, prefab.to_string()))
            })
            .collect()
    }

    /// 供资产管理器/引用检索用：扫描所有字符串字段里引用了 `path` 的位置。
    /// 所有「资产路径字段」的取值：`(实体下标, "组件.字段", 值)`。
    ///
    /// 与 [`Self::field_references`] 相反：那个是「给定路径找引用」，这个是
    /// 「把文档里所有引用列出来」，用来验证它们是否还指向存在的文件（#467）。
    /// 字段白名单与重命名改写共用 [`is_reference_path`]，不会一边改一边漏。
    pub fn reference_values(&self) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        for (index, entity) in self.entities.iter().enumerate() {
            for (component, value) in entity.components.iter() {
                collect_reference_values(value, "", component, &mut |field, text| {
                    out.push((index, format!("{component}.{field}"), text.to_string()));
                });
            }
        }
        out
    }

    pub fn field_references(&self, path: &str) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        for (index, entity) in self.entities.iter().enumerate() {
            for (component, value) in entity.components.iter() {
                collect_references(value, path, &mut |field, value| {
                    out.push((index, format!("{component}.{field}"), value.to_string()));
                });
            }
        }
        out
    }
}

/// 资产重命名 / 移动时改写了多少处引用，以及哪些写法解析不出来。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReferenceRewrite {
    /// 被改写的引用数量。
    pub rewritten: usize,
    /// 文件名与旧路径相同、但解析不出完整路径的写法（绝对路径、URL 等）。
    ///
    /// 这些引用会**留着旧的路径**，所以要报出来，而不是假装一切正常。
    pub ambiguous: Vec<String>,
}

/// 这个字段是不是「资产路径」。
///
/// 取路径的第一段（去掉数组下标）判断，`materials[0]` / `materials[1]` 都算。
fn is_reference_path(component: &str, path: &str) -> bool {
    let leaf = path.split('.').next().unwrap_or(path);
    let leaf = leaf.split('[').next().unwrap_or(leaf);
    crate::assets::is_asset_path_field(component, leaf)
        || matches!(leaf, "materials" | "textures" | "material_paths")
}

fn rewrite_references_in(
    value: &mut Value,
    path: &str,
    component: &str,
    doc_dir: &str,
    from: &str,
    to: &str,
    out: &mut ReferenceRewrite,
) {
    match value {
        Value::String(text) => {
            if !is_reference_path(component, path) {
                return;
            }
            match rewrite_reference_string(text, doc_dir, from, to) {
                RewriteOutcome::Unchanged => {}
                RewriteOutcome::Rewritten(next) => {
                    *text = next;
                    out.rewritten += 1;
                }
                RewriteOutcome::Ambiguous => out.ambiguous.push(text.clone()),
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                rewrite_references_in(
                    item,
                    &format!("{path}[{index}]"),
                    component,
                    doc_dir,
                    from,
                    to,
                    out,
                );
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let next = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                rewrite_references_in(item, &next, component, doc_dir, from, to, out);
            }
        }
        _ => {}
    }
}

enum RewriteOutcome {
    Unchanged,
    Rewritten(String),
    Ambiguous,
}

/// 把一个引用字符串按「它自己那种写法」改到 `to`。
fn rewrite_reference_string(text: &str, doc_dir: &str, from: &str, to: &str) -> RewriteOutcome {
    if text.trim().is_empty() {
        return RewriteOutcome::Unchanged;
    }
    // 纯文件名：和旧文件的文件名比对，改完仍是纯文件名。
    if !text.contains('/') {
        return if text == file_name(from) {
            RewriteOutcome::Rewritten(file_name(to).to_string())
        } else {
            RewriteOutcome::Unchanged
        };
    }
    let Some(resolved) = resolve_asset_reference(text, doc_dir) else {
        // 解析不出来：只有「看起来就是这个文件」才值得提醒。
        return if file_name(text) == file_name(from) {
            RewriteOutcome::Ambiguous
        } else {
            RewriteOutcome::Unchanged
        };
    };
    if resolved != from {
        return RewriteOutcome::Unchanged;
    }
    // 按作者原来的风格写回：`assets/...` 保持绝对写法，`../` 保持相对写法。
    if text.starts_with("assets/") {
        return RewriteOutcome::Rewritten(to.to_string());
    }
    match relative_from(doc_dir, to) {
        Some(relative) if text.starts_with("./") && !relative.starts_with("../") => {
            RewriteOutcome::Rewritten(format!("./{relative}"))
        }
        Some(relative) => RewriteOutcome::Rewritten(relative),
        None => RewriteOutcome::Rewritten(to.to_string()),
    }
}

/// 把引用解析成「相对项目根」的路径（`assets/...`）。
///
/// 纯文件名（不含 `/`）与绝对路径返回 `None` —— 前者由调用方按文件名处理，
/// 后者没法安全改写。
fn resolve_asset_reference(text: &str, doc_dir: &str) -> Option<String> {
    if text.contains(':') || text.starts_with('/') {
        return None;
    }
    if text.starts_with("assets/") {
        return normalize_relative(text);
    }
    if text.starts_with("./") || text.starts_with("../") {
        return normalize_relative(&format!("{doc_dir}/{text}"));
    }
    None
}

/// 消解 `.` / `..`；越过项目根（`..` 多于已有的段）返回 `None`。
fn normalize_relative(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// `doc_dir` 里的文档该怎样写 `target`（相对写法，如 `../../textures/a.png`）。
///
/// 两者没有公共前缀（不同根 / 不同盘）时返回 `None`。
fn relative_from(doc_dir: &str, target: &str) -> Option<String> {
    let from: Vec<&str> = doc_dir.split('/').filter(|part| !part.is_empty()).collect();
    let to: Vec<&str> = target.split('/').filter(|part| !part.is_empty()).collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return None;
    }
    let mut parts: Vec<String> = from[common..].iter().map(|_| "..".to_string()).collect();
    parts.extend(to[common..].iter().map(|part| part.to_string()));
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 收集一条组件值里所有「资产路径字段」的字符串。
fn collect_reference_values(
    value: &Value,
    path: &str,
    component: &str,
    out: &mut impl FnMut(&str, &str),
) {
    match value {
        Value::String(text) => {
            if is_reference_path(component, path) && !text.is_empty() {
                out(path, text);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_reference_values(item, &format!("{path}[{index}]"), component, out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let next = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_reference_values(item, &next, component, out);
            }
        }
        _ => {}
    }
}

fn collect_references(value: &Value, needle: &str, out: &mut impl FnMut(String, &str)) {
    fn walk(value: &Value, needle: &str, path: &str, out: &mut impl FnMut(String, &str)) {
        match value {
            Value::String(text) => {
                if text.contains(needle) {
                    out(path.to_string(), text);
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, needle, &format!("{path}[{index}]"), out);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    let next = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(item, needle, &next, out);
                }
            }
            _ => {}
        }
    }
    walk(value, needle, "", out);
}

fn select_subtree(doc: &SceneDoc, index: usize, out: &mut Vec<usize>) {
    if out.contains(&index) {
        return;
    }
    out.push(index);
    for child in doc.children(index) {
        select_subtree(doc, child, out);
    }
}

/// 剥掉 既有的 ` (数字)` 后缀：`Cube (3)` → `Cube`。
fn strip_name_suffix(name: &str) -> &str {
    let trimmed = name.trim_end();
    if !trimmed.ends_with(')') {
        return trimmed;
    }
    let Some(open) = trimmed.rfind(" (") else {
        return trimmed;
    };
    let inner = &trimmed[open + 2..trimmed.len() - 1];
    if !inner.is_empty() && inner.bytes().all(|byte| byte.is_ascii_digit()) {
        &trimmed[..open]
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_tree() -> SceneDoc {
        let mut doc = SceneDoc::default();
        let root = doc.add_named_child(None, "Root");
        let child = doc.add_named_child(Some(root), "Child");
        let _grand = doc.add_named_child(Some(child), "Grand");
        let _sibling = doc.add_named_child(None, "Sibling");
        doc
    }

    #[test]
    fn normalize_assigns_stable_ids_and_migrates_index_parents() {
        let mut doc: SceneDoc = serde_json::from_str(
            r#"{"version":1,"entities":[
                {"name":"A","transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}},
                {"name":"B","parent":0,"transform":{"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}}
            ]}"#,
        )
        .unwrap();
        doc.normalize();
        assert_eq!(doc.version, DOC_VERSION);
        assert_eq!(doc.entities[0].id, Some(1));
        assert_eq!(doc.entities[1].id, Some(2));
        assert_eq!(doc.entities[1].parent, Some(1), "下标 0 迁移为 id 1");
    }

    #[test]
    fn unknown_scene_and_entity_keys_survive_roundtrip() {
        let text = r#"{
            "version": 3,
            "_comment": "hand note",
            "x_meta": { "note": "keep me", "v": 3 },
            "entities": [{
                "id": 1,
                "name": "A",
                "parent": null,
                "transform": {"position":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]},
                "components": {},
                "_note": "entity note"
            }]
        }"#;
        let doc = SceneDoc::from_json_str(text).unwrap();
        assert_eq!(doc.extra["_comment"], serde_json::json!("hand note"));
        assert_eq!(doc.extra["x_meta"]["v"], serde_json::json!(3));
        assert_eq!(
            doc.entities[0].extra["_note"],
            serde_json::json!("entity note")
        );

        let saved = doc.to_json_string();
        assert!(saved.contains("\"_comment\""), "{saved}");
        assert!(saved.contains("\"x_meta\""), "{saved}");
        assert!(saved.contains("\"_note\""), "{saved}");
        let round = SceneDoc::from_json_str(&saved).unwrap();
        assert_eq!(round.extra, doc.extra);
        assert_eq!(round.entities[0].extra, doc.entities[0].extra);
    }

    #[test]
    fn visible_rows_follow_expansion() {
        let doc = doc_with_tree();
        let mut expanded = HashSet::new();
        let rows = doc.visible_rows(&expanded);
        assert_eq!(rows.len(), 2, "默认只显示两个根");

        let root_id = doc.entities[0].id.unwrap();
        expanded.insert(root_id);
        let rows = doc.visible_rows(&expanded);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].1, 1, "子级缩进一层");
    }

    #[test]
    fn reparent_rejects_self_and_cycles() {
        let mut doc = doc_with_tree();
        assert!(doc.reparent(0, Some(0), None).is_err());
        assert!(doc.reparent(0, Some(1), None).is_err(), "不能挂到子级");
        assert!(doc.reparent(0, Some(3), None).is_ok());
        assert_eq!(doc.entities[0].parent, doc.entities[3].id);
    }

    #[test]
    fn remove_entities_promotes_orphans_and_keeps_ids() {
        let mut doc = doc_with_tree();
        let child_id = doc.entities[1].id.unwrap();
        doc.remove_entities(&[1]);
        // Grand 是 Child 的子级，应一起删除。
        assert_eq!(doc.entities.len(), 2);
        assert!(!doc.entities.iter().any(|e| e.id == Some(child_id)));

        let mut doc = doc_with_tree();
        doc.remove_entities(&[0]);
        // Root 的子级 Child/Grand 一起删除，只剩 Sibling。
        assert_eq!(doc.entities.len(), 1);
        assert_eq!(doc.entities[0].display_name(), "Sibling");
    }

    #[test]
    fn delete_parent_with_child_only_keeps_unrelated() {
        let mut doc = SceneDoc::default();
        let a = doc.add_named_child(None, "A");
        let b = doc.add_named_child(Some(a), "B");
        doc.remove_entities(&[a]);
        assert!(doc.entities.is_empty());
        assert_ne!(b, usize::MAX);
    }

    #[test]
    fn duplicate_subtree_remaps_children_and_keeps_root_parent() {
        let mut doc = doc_with_tree();
        let duplicated = doc.duplicate_subtree(0).expect("duplicate");
        assert_eq!(doc.entities.len(), 7, "3 个原实体 + 3 个复制 + sibling");
        assert_eq!(doc.entities[duplicated].display_name(), "Root (1)");
        assert!(doc.entities[duplicated].parent.is_none());
        let root_id = doc.entities[duplicated].id.unwrap();
        let children: Vec<&EntityJson> = doc
            .entities
            .iter()
            .filter(|entity| entity.parent == Some(root_id))
            .collect();
        assert_eq!(children.len(), 1, "直接子级指向新根");
        assert_eq!(children[0].display_name(), "Child");
        let child_id = children[0].id.unwrap();
        assert!(doc
            .entities
            .iter()
            .any(|entity| entity.parent == Some(child_id)));
        // id 全局唯一。
        let ids: HashSet<u32> = doc.entities.iter().filter_map(|e| e.id).collect();
        assert_eq!(ids.len(), doc.entities.len());
    }

    #[test]
    fn move_entity_reorders_array() {
        let mut doc = doc_with_tree();
        let moved = doc.move_entity(3, 0).unwrap();
        assert_eq!(moved, 0);
        assert_eq!(doc.entities[0].display_name(), "Sibling");
        assert_eq!(doc.entities.len(), 4);
    }

    #[test]
    fn extract_subtree_strips_prefab_link_and_remapps_ids() {
        let mut doc = doc_with_tree();
        doc.entities[0].components.insert(
            PREFAB_LINK_COMPONENT.to_string(),
            serde_json::json!({"prefab": "assets/prefabs/a.prefab.json"}),
        );
        let prefab = doc.extract_subtree(&[0]);
        assert_eq!(prefab.entities.len(), 3);
        assert_eq!(prefab.entities[0].id, Some(1));
        assert!(prefab.entities[0].parent.is_none());
        assert!(prefab.entities[0]
            .components
            .get(PREFAB_LINK_COMPONENT)
            .is_none());
        assert_eq!(prefab.entities[1].parent, Some(1));
    }

    #[test]
    fn extract_subtree_keeps_nested_instance_links() {
        let mut doc = doc_with_tree();
        doc.entities[1].components.insert(
            PREFAB_LINK_COMPONENT.to_string(),
            serde_json::json!({"prefab": "assets/prefabs/child.prefab.json"}),
        );
        let prefab = doc.extract_subtree(&[0]);
        assert_eq!(
            prefab.entities[1].prefab_link(),
            Some("assets/prefabs/child.prefab.json")
        );
        assert!(prefab.entities[0].prefab_link().is_none());
    }

    #[test]
    fn extract_subtree_preserving_links_keeps_root_link() {
        let mut doc = doc_with_tree();
        doc.entities[0].components.insert(
            PREFAB_LINK_COMPONENT.to_string(),
            serde_json::json!({"prefab": "assets/prefabs/root.prefab.json"}),
        );
        let copy = doc.extract_subtree_preserving_links(&[0]);
        assert_eq!(
            copy.entities[0].prefab_link(),
            Some("assets/prefabs/root.prefab.json")
        );
        let stripped = doc.extract_subtree(&[0]);
        assert!(stripped.entities[0].prefab_link().is_none());
    }

    #[test]
    fn instantiate_prefab_remaps_ids_and_dedupes_names() {
        let mut scene = doc_with_tree();
        let prefab = scene.extract_subtree(&[0]);
        let inserted = scene.instantiate_prefab(&prefab, None, "assets/prefabs/root.prefab.json");
        assert_eq!(inserted.len(), 3);
        let root = &scene.entities[inserted[0]];
        assert_eq!(root.display_name(), "Root (1)");
        assert_eq!(root.prefab_link(), Some("assets/prefabs/root.prefab.json"));
        // 新的 id 不与旧的重叠。
        let ids: HashSet<u32> = scene.entities.iter().filter_map(|e| e.id).collect();
        assert_eq!(ids.len(), scene.entities.len());
    }

    #[test]
    fn unique_name_strips_suffix_and_uses_smallest_free_slot() {
        let mut doc = SceneDoc::default();
        let root = doc.add_named_child(None, "Cube");
        assert_eq!(doc.unique_name("Cube", None), "Cube (1)");
        doc.add_named_child(None, "Cube (1)");
        assert_eq!(doc.unique_name("Cube (1)", None), "Cube (2)");
        // 同级范围：子级可以复用根级的名字。
        assert_eq!(doc.unique_name("Cube", doc.id_of_index(root)), "Cube");
        // 剥后缀后再找空位。
        assert_eq!(doc.unique_name("Cube (9)", None), "Cube (2)");
        // 非数字后缀属于名字本身。
        assert_eq!(doc.unique_name("Cube (left)", None), "Cube (left)");
    }

    #[test]
    fn repeated_duplicates_get_increasing_names() {
        let mut doc = SceneDoc::default();
        let first = doc.add_named_child(None, "Cube");
        let a = doc.duplicate_subtree(first).expect("duplicate subtree");
        assert_eq!(doc.entities[a].display_name(), "Cube (1)");
        let b = doc.duplicate_subtree(a).expect("duplicate copy");
        assert_eq!(doc.entities[b].display_name(), "Cube (2)");
        let c = doc.duplicate(first).expect("duplicate root");
        assert_eq!(doc.entities[c].display_name(), "Cube (3)");
    }

    #[test]
    fn field_references_walk_nested_values() {
        let mut doc = SceneDoc::default();
        let index = doc.add_named_child(None, "Mesh");
        doc.add_component(
            index,
            "prism_engine::scene::MeshRenderer",
            serde_json::json!({
                "mesh_path": "assets/meshes/a.rmes",
                "materials": ["assets/materials/m.rtex"]
            }),
        );
        let refs = doc.field_references("assets/meshes/a.rmes");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].1.ends_with("mesh_path"));
        assert_eq!(doc.field_references("assets/materials/m.rtex").len(), 1);
        assert_eq!(doc.field_references("nope").len(), 0);
    }

    #[test]
    fn rewrite_asset_references_replaces_exact_paths() {
        let mut doc = SceneDoc::default();
        let index = doc.add_named_child(None, "Mesh");
        doc.add_component(
            index,
            "prism_engine::scene::MeshRenderer",
            serde_json::json!({
                "mesh_path": "assets/meshes/a.rmes",
                "materials": ["assets/materials/m.rtex", "assets/materials/a.rmes.extra"]
            }),
        );
        let rewrite = doc.rewrite_asset_references(
            "assets/scenes",
            "assets/meshes/a.rmes",
            "assets/meshes/b.rmes",
        );
        assert_eq!(rewrite.rewritten, 1, "只替换精确匹配，不做子串替换");
        assert!(rewrite.ambiguous.is_empty());
        assert_eq!(
            doc.entities[0].components["prism_engine::scene::MeshRenderer"]["mesh_path"],
            serde_json::json!("assets/meshes/b.rmes")
        );
        let rewrite = doc.rewrite_asset_references(
            "assets/scenes",
            "assets/materials/m.rtex",
            "assets/materials/n.rtex",
        );
        assert_eq!(rewrite.rewritten, 1);
    }

    #[test]
    fn renaming_only_touches_asset_path_fields() {
        // #465：实体名和 UI 文本里恰好写着同一段路径时不能跟着改。
        let mut doc = SceneDoc::default();
        let mesh = doc.add_named_child(None, "assets/textures/wood.png");
        doc.add_component(
            mesh,
            "prism_engine::scene::SpriteRenderer",
            serde_json::json!({ "sprite_path": "assets/textures/wood.png" }),
        );
        doc.add_component(
            mesh,
            "prism_engine::ui::Text",
            serde_json::json!({ "content": "assets/textures/wood.png" }),
        );

        let rewrite = doc.rewrite_asset_references(
            "assets/scenes",
            "assets/textures/wood.png",
            "assets/textures/oak.png",
        );

        assert_eq!(rewrite.rewritten, 1);
        assert_eq!(
            doc.entities[mesh].name.as_deref(),
            Some("assets/textures/wood.png"),
            "实体名不是资产引用"
        );
        assert_eq!(
            doc.entities[mesh].components["prism_engine::ui::Text"]["content"],
            serde_json::json!("assets/textures/wood.png"),
            "UI 文本不是资产引用"
        );
        assert_eq!(
            doc.entities[mesh].components["prism_engine::scene::SpriteRenderer"]["sprite_path"],
            serde_json::json!("assets/textures/oak.png")
        );
    }

    #[test]
    fn relative_and_bare_references_keep_their_own_style() {
        let mut doc = SceneDoc::default();
        let deep = doc.add_named_child(None, "Deep");
        doc.add_component(
            deep,
            "prism_engine::scene::MeshRenderer",
            serde_json::json!({
                "mesh_path": "../../../textures/wood.png",
                "materials": ["wood.png", "assets/textures/wood.png"]
            }),
        );

        let rewrite = doc.rewrite_asset_references(
            "assets/scenes/deep/nested",
            "assets/textures/wood.png",
            "assets/textures/oak.png",
        );

        assert_eq!(rewrite.rewritten, 3);
        let component = &doc.entities[deep].components["prism_engine::scene::MeshRenderer"];
        // 相对写法按**本文档目录**算回去，仍是相对的。
        assert_eq!(
            component["mesh_path"],
            serde_json::json!("../../../textures/oak.png")
        );
        assert_eq!(component["materials"][0], serde_json::json!("oak.png"));
        assert_eq!(
            component["materials"][1],
            serde_json::json!("assets/textures/oak.png")
        );
    }

    #[test]
    fn an_unparseable_reference_is_reported_not_ignored() {
        let mut doc = SceneDoc::default();
        let entity = doc.add_named_child(None, "Sprite");
        doc.add_component(
            entity,
            "prism_engine::scene::SpriteRenderer",
            serde_json::json!({
                "sprite_path": "C:/art/textures/wood.png",
                "atlas_path": "assets/textures/other.png"
            }),
        );

        let rewrite = doc.rewrite_asset_references(
            "assets/scenes",
            "assets/textures/wood.png",
            "assets/textures/oak.png",
        );

        assert_eq!(rewrite.rewritten, 0);
        assert_eq!(
            rewrite.ambiguous,
            vec!["C:/art/textures/wood.png".to_string()]
        );
        assert_eq!(
            doc.entities[entity].components["prism_engine::scene::SpriteRenderer"]["sprite_path"],
            serde_json::json!("C:/art/textures/wood.png"),
            "改写不了就不要动它"
        );
    }

    #[test]
    fn active_flag_toggles_component() {
        let mut doc = doc_with_tree();
        assert!(doc.entities[0].active());
        doc.set_active(0, false);
        assert!(!doc.entities[0].active());
        doc.set_active(0, true);
        assert!(doc.entities[0].active());
    }
}
