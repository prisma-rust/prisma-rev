//! 作者侧 Montage 段表（`*.skinning/montages.json`）的视图与编辑（#560 P4）。
//!
//! 文件形状与 cooker 的 `authored_montages` 一致：
//!
//! ```json
//! {
//!   "clips": {
//!     "vault": {
//!       "sections": [
//!         { "name": "Start", "start": 0.0, "end": 0.3 },
//!         { "name": "Vault", "start": 0.3, "end": 0.8 }
//!       ]
//!     }
//!   }
//! }
//! ```
//!
//! 键是 glTF 里的动画**原名**，不是 cook 后的 `.ranim` 文件名。这里只做形状层面
//! 的校验（名字非空 / 唯一、时间有限且 `0 <= start <= end`），「对上不对得上某个
//! 动画」仍由 cook 时 warn。

use serde_json::{json, Value};

/// 一段 Montage 区间。
#[derive(Debug, Clone, PartialEq)]
pub struct MontageSectionView {
    pub name: String,
    /// 起点（秒）。
    pub start: f32,
    /// 终点（秒）；可以等于 `start`。
    pub end: f32,
}

/// 一个剪辑的 Montage 段列表。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipMontageView {
    pub clip: String,
    pub sections: Vec<MontageSectionView>,
}

/// 整个 Montage 段表，按剪辑名排序。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MontagesView {
    pub clips: Vec<ClipMontageView>,
}

impl MontagesView {
    /// 所有段总数（面板标题用）。
    pub fn len(&self) -> usize {
        self.clips.iter().map(|clip| clip.sections.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn validate(name: &str, start: f32, end: f32) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("a montage section needs a non-empty name".to_string());
    }
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end < start {
        return Err("a montage section needs finite 0 <= start <= end".to_string());
    }
    Ok(())
}

/// 解析 Montage 段表；缺 `clips` 当作空表。
pub fn parse_montages_view(document: &Value) -> Result<MontagesView, String> {
    let Some(clips) = document.get("clips") else {
        return Ok(MontagesView::default());
    };
    let Some(clips) = clips.as_object() else {
        return Err("montage file needs a `clips` object".to_string());
    };
    let mut parsed = Vec::with_capacity(clips.len());
    for (clip, entry) in clips {
        let Some(entry) = entry.as_object() else {
            return Err(format!("clip '{clip}' needs an object with `sections`"));
        };
        let Some(sections) = entry.get("sections") else {
            parsed.push(ClipMontageView {
                clip: clip.clone(),
                sections: Vec::new(),
            });
            continue;
        };
        let Some(sections) = sections.as_array() else {
            return Err(format!("clip '{clip}' needs a `sections` array"));
        };
        let mut view = Vec::with_capacity(sections.len());
        for (index, section) in sections.iter().enumerate() {
            let name = section
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("clip '{clip}' section #{index} needs a name"))?;
            let start = section.get("start").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let end = section
                .get("end")
                .and_then(Value::as_f64)
                .unwrap_or(start as f64) as f32;
            validate(name, start, end)
                .map_err(|error| format!("clip '{clip}' section #{index}: {error}"))?;
            if view
                .iter()
                .any(|existing: &MontageSectionView| existing.name == name)
            {
                return Err(format!(
                    "clip '{clip}' has duplicate montage section '{name}'"
                ));
            }
            view.push(MontageSectionView {
                name: name.to_string(),
                start,
                end,
            });
        }
        view.sort_by(|a, b| {
            a.start
                .total_cmp(&b.start)
                .then_with(|| a.name.cmp(&b.name))
        });
        parsed.push(ClipMontageView {
            clip: clip.clone(),
            sections: view,
        });
    }
    parsed.sort_by(|a, b| a.clip.cmp(&b.clip));
    Ok(MontagesView { clips: parsed })
}

fn find_clip_mut<'a>(document: &'a mut Value, clip: &str) -> Result<Option<&'a mut Value>, String> {
    let Some(clips) = document.get_mut("clips") else {
        return Ok(None);
    };
    let Some(clips) = clips.as_object_mut() else {
        return Err("montage file needs a `clips` object".to_string());
    };
    Ok(clips.get_mut(clip))
}

fn ensure_clip<'a>(document: &'a mut Value, clip: &str) -> Result<&'a mut Value, String> {
    if document.get("clips").is_none() {
        document["clips"] = json!({});
    }
    let Some(clips) = document.get_mut("clips").and_then(Value::as_object_mut) else {
        return Err("montage file needs a `clips` object".to_string());
    };
    let entry = clips
        .entry(clip.to_string())
        .or_insert_with(|| json!({ "sections": [] }));
    if !entry.is_object() {
        return Err(format!("clip '{clip}' needs an object with `sections`"));
    }
    if entry.get("sections").is_none() {
        entry["sections"] = json!([]);
    }
    Ok(entry)
}

/// 给某个剪辑追加一段；剪辑键不存在时自动创建。
pub fn add_montage_section(
    document: &mut Value,
    clip: &str,
    name: &str,
    start: f32,
    end: f32,
) -> Result<(), String> {
    let clip = clip.trim();
    let name = name.trim();
    if clip.is_empty() {
        return Err("a montage section needs a clip name".to_string());
    }
    validate(name, start, end)?;
    let entry = ensure_clip(document, clip)?;
    let Some(sections) = entry.get_mut("sections").and_then(Value::as_array_mut) else {
        return Err(format!("clip '{clip}' needs a `sections` array"));
    };
    if sections
        .iter()
        .any(|section| section.get("name").and_then(Value::as_str).map(str::trim) == Some(name))
    {
        return Err(format!(
            "clip '{clip}' already has the montage section '{name}'"
        ));
    }
    sections.push(json!({ "name": name, "start": start, "end": end }));
    Ok(())
}

/// 删除某个剪辑的第 `index` 段。
pub fn remove_montage_section(
    document: &mut Value,
    clip: &str,
    index: usize,
) -> Result<(), String> {
    let Some(entry) = find_clip_mut(document, clip)? else {
        return Err(format!("montage file has no clip '{clip}'"));
    };
    let Some(sections) = entry.get_mut("sections").and_then(Value::as_array_mut) else {
        return Err(format!("clip '{clip}' needs a `sections` array"));
    };
    if index >= sections.len() {
        return Err(format!("clip '{clip}' has no montage section #{index}"));
    }
    sections.remove(index);
    Ok(())
}

/// 改某一段的名字 / 时间；同一剪辑内段名不能重复。
pub fn set_montage_section(
    document: &mut Value,
    clip: &str,
    index: usize,
    name: &str,
    start: f32,
    end: f32,
) -> Result<(), String> {
    let name = name.trim();
    validate(name, start, end)?;
    let Some(entry) = find_clip_mut(document, clip)? else {
        return Err(format!("montage file has no clip '{clip}'"));
    };
    let Some(sections) = entry.get_mut("sections").and_then(Value::as_array_mut) else {
        return Err(format!("clip '{clip}' needs a `sections` array"));
    };
    if index >= sections.len() {
        return Err(format!("clip '{clip}' has no montage section #{index}"));
    }
    if sections.iter().enumerate().any(|(other_index, section)| {
        other_index != index
            && section.get("name").and_then(Value::as_str).map(str::trim) == Some(name)
    }) {
        return Err(format!(
            "clip '{clip}' already has the montage section '{name}'"
        ));
    }
    let Some(section) = sections.get_mut(index) else {
        return Err(format!("clip '{clip}' has no montage section #{index}"));
    };
    if !section.is_object() {
        return Err(format!("clip '{clip}' section #{index} is malformed"));
    }
    section["name"] = json!(name);
    section["start"] = json!(start);
    section["end"] = json!(end);
    Ok(())
}

/// 时间点落在哪一段（左闭右开；零长度段命中起点）。
pub fn montage_section_at(time: f32, sections: &[MontageSectionView]) -> Option<usize> {
    if !time.is_finite() {
        return None;
    }
    sections.iter().position(|section| {
        if !section.start.is_finite() || !section.end.is_finite() {
            return false;
        }
        if section.end <= section.start {
            (time - section.start).abs() <= f32::EPSILON
        } else {
            time >= section.start && time < section.end
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Value {
        json!({
            "clips": {
                "vault": {
                    "sections": [
                        { "name": "Land", "start": 0.8, "end": 1.0 },
                        { "name": "Start", "start": 0.0, "end": 0.3 }
                    ]
                }
            }
        })
    }

    #[test]
    fn sections_parse_sorted_and_empty_files_are_allowed() {
        let view = parse_montages_view(&document()).expect("parse");
        assert_eq!(view.clips.len(), 1);
        assert_eq!(view.clips[0].clip, "vault");
        let names: Vec<&str> = view.clips[0]
            .sections
            .iter()
            .map(|section| section.name.as_str())
            .collect();
        assert_eq!(names, vec!["Start", "Land"]);
        assert_eq!(view.len(), 2);

        assert!(parse_montages_view(&json!({})).unwrap().is_empty());
        assert!(parse_montages_view(&json!({ "clips": {} }))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_sections_are_rejected() {
        assert!(parse_montages_view(&json!({ "clips": [] })).is_err());
        assert!(parse_montages_view(&json!({ "clips": { "a": [] } })).is_err());
        assert!(parse_montages_view(&json!({
            "clips": { "a": { "sections": [{ "name": "", "start": 0.0, "end": 1.0 }] } }
        }))
        .is_err());
        assert!(parse_montages_view(&json!({
            "clips": { "a": { "sections": [{ "name": "x", "start": 1.0, "end": 0.5 }] } }
        }))
        .is_err());
        assert!(parse_montages_view(&json!({
            "clips": { "a": { "sections": [
                { "name": "x", "start": 0.0, "end": 1.0 },
                { "name": "x", "start": 1.0, "end": 2.0 }
            ] } }
        }))
        .is_err());
    }

    #[test]
    fn sections_can_be_added_changed_and_removed() {
        let mut doc = json!({});
        add_montage_section(&mut doc, "vault", "Start", 0.0, 0.3).expect("add");
        add_montage_section(&mut doc, "vault", "Land", 0.8, 1.0).expect("add");
        assert_eq!(doc["clips"]["vault"]["sections"][0]["name"], "Start");
        assert!(add_montage_section(&mut doc, "vault", "Start", 0.0, 0.2).is_err());

        set_montage_section(&mut doc, "vault", 0, "Begin", 0.0, 0.4).expect("set");
        assert_eq!(doc["clips"]["vault"]["sections"][0]["name"], "Begin");
        assert!(set_montage_section(&mut doc, "vault", 0, "Land", 0.0, 0.4).is_err());

        remove_montage_section(&mut doc, "vault", 0).expect("remove");
        assert_eq!(
            doc["clips"]["vault"]["sections"].as_array().unwrap().len(),
            1
        );
        assert!(remove_montage_section(&mut doc, "vault", 5).is_err());
        assert!(remove_montage_section(&mut doc, "missing", 0).is_err());
    }

    #[test]
    fn section_lookup_uses_half_open_ranges() {
        let view = parse_montages_view(&document()).unwrap();
        let sections = &view.clips[0].sections;
        assert_eq!(montage_section_at(0.0, sections), Some(0));
        assert_eq!(montage_section_at(0.3, sections), None);
        assert_eq!(montage_section_at(0.8, sections), Some(1));
        assert_eq!(montage_section_at(1.0, sections), None);
        assert_eq!(montage_section_at(f32::NAN, sections), None);
    }
}
