//! 有序组件表（#512）。
//!
//! `.scene.json` 的 `components` 是一个 JSON 对象，对象本身没有顺序语义，但
//! **编辑器把它当序列化数据**：作者排好的顺序要能存盘、重开、进撤销，并且在
//! 检查器里按原样渲染。`serde_json::Map` 背后是 `BTreeMap`（没有开
//! `preserve_order`），插入顺序在第一次 Roundtrip 后就没了，所以这里用一个
//! 手写的有序表替掉它。
//!
//! 之所以不是给 `serde_json` 打开 `preserve_order`：那会改掉全仓库每一个
//! `Map`（包括组件字段值、manifest、`json!` 宏的输出顺序），影响面远超本需求。
//! 存放顺序也不进入运行时：引擎 / cooker 仍然把组件表当无序集合读，RSCN 按
//! `TypeDesc` 字段顺序编码。
//!
//! 对外接口刻意贴着 `serde_json::Map` 的常用子集（`get` / `insert` / `remove` /
//! `keys` / `iter` / `Index<&str>`），因此调用点基本不用改。

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;
use std::ops::{Index, IndexMut};

/// 组件名 → 组件值，**保留声明顺序**。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComponentMap(Vec<(String, Value)>);

impl ComponentMap {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// 组件在表里的位置（检查器的渲染顺序）。
    pub fn position(&self, name: &str) -> Option<usize> {
        self.0.iter().position(|(key, _)| key == name)
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.0
            .iter_mut()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// 写入一个组件；已存在时**原地替换值**，位置不变。
    ///
    /// 新组件追加到末尾，所以检查器里的顺序就是添加顺序。
    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        if let Some(existing) = self.0.iter_mut().find(|(key, _)| *key == name) {
            return Some(std::mem::replace(&mut existing.1, value));
        }
        self.0.push((name, value));
        None
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let index = self.position(name)?;
        Some(self.0.remove(index).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(name, value)| (name, value))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&mut String, &mut Value)> {
        self.0.iter_mut().map(|(name, value)| (name, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(name, _)| name)
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.iter().map(|(_, value)| value)
    }

    /// 当前的组件顺序（组件名）。
    pub fn order(&self) -> Vec<String> {
        self.0.iter().map(|(name, _)| name.clone()).collect()
    }

    /// 把 `name` 沿检查器顺序移动 `delta` 格；到头不动。
    ///
    /// 返回是否真的移动过 —— 调用方用它决定要不要开一个撤销步。
    pub fn move_by(&mut self, name: &str, delta: isize) -> bool {
        let Some(index) = self.position(name) else {
            return false;
        };
        let target = index as isize + delta;
        if target < 0 || target >= self.0.len() as isize || target == index as isize {
            return false;
        }
        let entry = self.0.remove(index);
        self.0.insert(target as usize, entry);
        true
    }
}

impl Index<&str> for ComponentMap {
    type Output = Value;

    fn index(&self, name: &str) -> &Value {
        self.get(name)
            .unwrap_or_else(|| panic!("no component named `{name}`"))
    }
}

impl IndexMut<&str> for ComponentMap {
    fn index_mut(&mut self, name: &str) -> &mut Value {
        self.get_mut(name)
            .unwrap_or_else(|| panic!("no component named `{name}`"))
    }
}

impl Serialize for ComponentMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ComponentMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ComponentMapVisitor;

        impl<'de> Visitor<'de> for ComponentMapVisitor {
            type Value = ComponentMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map of component name to component value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                // serde 按文件里的顺序把条目喂进来，所以这里天然还原了作者顺序。
                let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((name, value)) = access.next_entry::<String, Value>()? {
                    entries.push((name, value));
                }
                Ok(ComponentMap(entries))
            }
        }

        deserializer.deserialize_map(ComponentMapVisitor)
    }
}

impl FromIterator<(String, Value)> for ComponentMap {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (name, value) in iter {
            map.insert(name, value);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> ComponentMap {
        let mut map = ComponentMap::new();
        map.insert("z_camera".into(), json!({ "fov": 60.0 }));
        map.insert("a_light".into(), json!({ "intensity": 1.0 }));
        map.insert("m_renderer".into(), json!({ "mesh": "cube" }));
        map
    }

    #[test]
    fn insertion_order_is_kept_not_sorted() {
        let map = sample();
        assert_eq!(map.order(), vec!["z_camera", "a_light", "m_renderer"]);
        assert_eq!(map.position("a_light"), Some(1));
    }

    #[test]
    fn replacing_a_component_keeps_its_position() {
        let mut map = sample();
        let replaced = map.insert("z_camera".into(), json!({ "fov": 90.0 }));
        assert!(replaced.is_some());
        assert_eq!(map.order(), vec!["z_camera", "a_light", "m_renderer"]);
        assert_eq!(map["z_camera"]["fov"], 90.0);
    }

    #[test]
    fn move_by_swaps_within_bounds_only() {
        let mut map = sample();
        assert!(map.move_by("m_renderer", -1));
        assert_eq!(map.order(), vec!["z_camera", "m_renderer", "a_light"]);
        assert!(map.move_by("z_camera", 1));
        assert_eq!(map.order(), vec!["m_renderer", "z_camera", "a_light"]);
        // 到头 / 到尾 / 不存在的名字都不动。
        assert!(!map.move_by("m_renderer", -1));
        assert!(!map.move_by("a_light", 1));
        assert!(!map.move_by("nope", 1));
        assert_eq!(map.order(), vec!["m_renderer", "z_camera", "a_light"]);
    }

    #[test]
    fn json_roundtrip_keeps_the_declared_order() {
        let mut entity = crate::doc::EntityJson::default();
        entity.components.insert("z_last".into(), json!({ "v": 1 }));
        entity
            .components
            .insert("a_first".into(), json!({ "v": 2 }));

        let text = serde_json::to_string(&entity).unwrap();
        assert!(
            text.find("z_last").unwrap() < text.find("a_first").unwrap(),
            "serialized order must follow the map: {text}"
        );

        let parsed: crate::doc::EntityJson = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.components.order(), vec!["z_last", "a_first"]);
        assert_eq!(parsed, entity);
    }
}
