//! 作者侧剪辑事件表（`*.skinning/notifies.json`）的视图与编辑（#560 Notify）。
//!
//! 文件形状与 cooker 的 `authored_notifies` 一致：
//!
//! ```json
//! { "clips": { "walk": [{ "time": 0.4, "name": "Footstep.L" }] } }
//! ```
//!
//! 键是 glTF 里的动画**原名**，不是 cook 后的 `.ranim` 文件名。这里只做形状层面的
//! 校验（名字非空、时间有限非负），「对上不对得上某个动画」仍由 cook 时 warn。

use serde_json::{json, Value};

/// 一条剪辑事件。
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyEntryView {
    pub time: f32,
    pub name: String,
}

/// 一个剪辑的事件列表。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipNotifiesView {
    pub clip: String,
    pub entries: Vec<NotifyEntryView>,
}

/// 整个事件表，按剪辑名排序。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NotifiesView {
    pub clips: Vec<ClipNotifiesView>,
}

impl NotifiesView {
    /// 所有事件总数（面板标题用）。
    pub fn len(&self) -> usize {
        self.clips.iter().map(|clip| clip.entries.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 解析事件表；缺 `clips` 当作空表。
pub fn parse_notifies_view(document: &Value) -> Result<NotifiesView, String> {
    let Some(clips) = document.get("clips") else {
        return Ok(NotifiesView::default());
    };
    let Some(clips) = clips.as_object() else {
        return Err("notify file needs a `clips` object".to_string());
    };
    let mut parsed = Vec::with_capacity(clips.len());
    for (clip, entries) in clips {
        let Some(entries) = entries.as_array() else {
            return Err(format!("clip '{clip}' needs an array of notifies"));
        };
        let mut view = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("clip '{clip}' notify #{index} needs a name"))?;
            view.push(NotifyEntryView {
                time: entry.get("time").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                name: name.to_string(),
            });
        }
        parsed.push(ClipNotifiesView {
            clip: clip.clone(),
            entries: view,
        });
    }
    parsed.sort_by(|a, b| a.clip.cmp(&b.clip));
    Ok(NotifiesView { clips: parsed })
}

fn validate(time: f32, name: &str) -> Result<(), String> {
    if !time.is_finite() || time < 0.0 {
        return Err("a notify time must be finite and >= 0".to_string());
    }
    if name.trim().is_empty() {
        return Err("a notify needs a non-empty name".to_string());
    }
    Ok(())
}

/// 给某个剪辑追加一条事件；剪辑键不存在时自动创建。
pub fn add_notify(document: &mut Value, clip: &str, time: f32, name: &str) -> Result<(), String> {
    let clip = clip.trim();
    let name = name.trim();
    if clip.is_empty() {
        return Err("a notify needs a clip name".to_string());
    }
    validate(time, name)?;
    if document.get("clips").is_none() {
        document["clips"] = json!({});
    }
    let Some(clips) = document.get_mut("clips").and_then(Value::as_object_mut) else {
        return Err("notify file needs a `clips` object".to_string());
    };
    let entries = clips.entry(clip.to_string()).or_insert_with(|| json!([]));
    let Some(entries) = entries.as_array_mut() else {
        return Err(format!("clip '{clip}' has an invalid notify list"));
    };
    entries.push(json!({ "time": time, "name": name }));
    Ok(())
}

/// 删除某个剪辑的第 `index` 条事件。
pub fn remove_notify(document: &mut Value, clip: &str, index: usize) -> Result<(), String> {
    let Some(entries) = find_clip_mut(document, clip)? else {
        return Err(format!("notify file has no clip '{clip}'"));
    };
    if index >= entries.len() {
        return Err(format!("clip '{clip}' has no notify #{index}"));
    }
    entries.remove(index);
    Ok(())
}

/// 改某条事件的时间与名字。
pub fn set_notify(
    document: &mut Value,
    clip: &str,
    index: usize,
    time: f32,
    name: &str,
) -> Result<(), String> {
    let name = name.trim();
    validate(time, name)?;
    let Some(entries) = find_clip_mut(document, clip)? else {
        return Err(format!("notify file has no clip '{clip}'"));
    };
    let Some(entry) = entries.get_mut(index) else {
        return Err(format!("clip '{clip}' has no notify #{index}"));
    };
    if !entry.is_object() {
        return Err(format!("clip '{clip}' notify #{index} is malformed"));
    }
    entry["time"] = json!(time);
    entry["name"] = json!(name);
    Ok(())
}

fn find_clip_mut<'a>(
    document: &'a mut Value,
    clip: &str,
) -> Result<Option<&'a mut Vec<Value>>, String> {
    let Some(clips) = document.get_mut("clips") else {
        return Ok(None);
    };
    let Some(clips) = clips.as_object_mut() else {
        return Err("notify file needs a `clips` object".to_string());
    };
    let Some(entries) = clips.get_mut(clip) else {
        return Ok(None);
    };
    let Some(entries) = entries.as_array_mut() else {
        return Err(format!("clip '{clip}' has an invalid notify list"));
    };
    Ok(Some(entries))
}

/// 时间轴上的 x 坐标（`left..left+width`）映射成秒，夹到 `0..=span` 并量化到 0.01 秒。
///
/// 面板拖拽和单元测试共用这一份换算，避免 UI 与测试各写一套夹取逻辑。
pub fn notify_time_at(x: f32, left: f32, width: f32, span: f32) -> f32 {
    if !x.is_finite() || !width.is_finite() || width <= 0.0 || !span.is_finite() || span <= 0.0 {
        return 0.0;
    }
    let fraction = ((x - left) / width).clamp(0.0, 1.0);
    ((fraction * span) * 100.0).round() / 100.0
}

/// 时间轴命中：返回离 `x` 最近、且像素距离不超过 `radius` 的事件下标。
pub fn nearest_notify_index(
    entries: &[NotifyEntryView],
    x: f32,
    left: f32,
    width: f32,
    span: f32,
    radius: f32,
) -> Option<usize> {
    if !x.is_finite() || !width.is_finite() || width <= 0.0 || !span.is_finite() || span <= 0.0 {
        return None;
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let marker = left + (entry.time / span).clamp(0.0, 1.0) * width;
            (index, (marker - x).abs())
        })
        .filter(|(_, distance)| *distance <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "clips": {
                "walk": [
                    { "time": 0.25, "name": "Footstep.L" },
                    { "time": 0.75, "name": "Footstep.R" }
                ],
                "run": [{ "time": 0.1, "name": "Footstep.L" }]
            }
        })
    }

    #[test]
    fn a_view_lists_clips_sorted_with_their_events() {
        let view = parse_notifies_view(&document()).expect("parses");
        assert_eq!(view.clips.len(), 2);
        assert_eq!(view.clips[0].clip, "run");
        assert_eq!(view.clips[1].clip, "walk");
        assert_eq!(view.clips[1].entries[1].name, "Footstep.R");
        assert_eq!(view.clips[1].entries[1].time, 0.75);
        assert_eq!(view.len(), 3);
    }

    #[test]
    fn a_missing_or_malformed_file_is_handled() {
        assert!(parse_notifies_view(&json!({})).expect("empty").is_empty());
        assert!(parse_notifies_view(&json!({ "clips": [] })).is_err());
        assert!(parse_notifies_view(&json!({ "clips": { "walk": 3 } })).is_err());
        assert!(
            parse_notifies_view(&json!({ "clips": { "walk": [{ "time": 0.1 }] } })).is_err(),
            "无名事件要报错"
        );
    }

    #[test]
    fn timeline_coordinates_map_to_clamped_centisecond_times() {
        // 100px 装 2 秒：中点 1 秒，两端夹住。
        assert_eq!(notify_time_at(50.0, 0.0, 100.0, 2.0), 1.0);
        assert_eq!(notify_time_at(-40.0, 0.0, 100.0, 2.0), 0.0);
        assert_eq!(notify_time_at(400.0, 0.0, 100.0, 2.0), 2.0);
        assert_eq!(notify_time_at(33.0, 0.0, 100.0, 2.0), 0.66);
        assert_eq!(notify_time_at(f32::NAN, 0.0, 100.0, 2.0), 0.0);
        assert_eq!(notify_time_at(10.0, 0.0, 0.0, 2.0), 0.0);
    }

    #[test]
    fn timeline_hit_testing_picks_the_nearest_marker_within_the_radius() {
        let entries = vec![
            NotifyEntryView {
                time: 0.5,
                name: "A".to_string(),
            },
            NotifyEntryView {
                time: 1.5,
                name: "B".to_string(),
            },
        ];
        // 0..200px 装 2 秒 → A 在 50px、B 在 150px。
        assert_eq!(
            nearest_notify_index(&entries, 52.0, 0.0, 200.0, 2.0, 8.0),
            Some(0)
        );
        assert_eq!(
            nearest_notify_index(&entries, 146.0, 0.0, 200.0, 2.0, 8.0),
            Some(1)
        );
        assert_eq!(
            nearest_notify_index(&entries, 100.0, 0.0, 200.0, 2.0, 8.0),
            None
        );
        assert_eq!(nearest_notify_index(&[], 50.0, 0.0, 200.0, 2.0, 8.0), None);
        assert_eq!(
            nearest_notify_index(&entries, f32::INFINITY, 0.0, 200.0, 2.0, 8.0),
            None
        );
    }

    #[test]
    fn events_can_be_added_removed_and_edited() {
        let mut doc = document();
        add_notify(&mut doc, "walk", 0.5, "Jump").expect("adds");
        assert_eq!(doc["clips"]["walk"].as_array().unwrap().len(), 3);
        add_notify(&mut doc, "idle", 0.0, "Breath").expect("creates clip");
        assert_eq!(doc["clips"]["idle"][0]["name"], "Breath");

        set_notify(&mut doc, "walk", 0, 0.3, " Step.L ").expect("edits");
        assert_eq!(
            doc["clips"]["walk"][0]["time"].as_f64(),
            Some(0.3f32 as f64)
        );
        assert_eq!(doc["clips"]["walk"][0]["name"], "Step.L");

        remove_notify(&mut doc, "walk", 0).expect("removes");
        assert_eq!(doc["clips"]["walk"][0]["name"], "Footstep.R");
    }

    #[test]
    fn invalid_events_are_rejected() {
        let mut doc = document();
        assert!(add_notify(&mut doc, "", 0.0, "Jump").is_err());
        assert!(add_notify(&mut doc, "walk", -1.0, "Jump").is_err());
        assert!(add_notify(&mut doc, "walk", f32::NAN, "Jump").is_err());
        assert!(add_notify(&mut doc, "walk", 0.0, "  ").is_err());
        assert!(remove_notify(&mut doc, "walk", 9).is_err());
        assert!(remove_notify(&mut doc, "missing", 0).is_err());
        assert!(set_notify(&mut doc, "walk", 0, -0.5, "Jump").is_err());
        assert!(set_notify(&mut doc, "walk", 0, 0.5, " ").is_err());
        assert!(set_notify(&mut doc, "walk", 9, 0.5, "Jump").is_err());
        assert!(set_notify(&mut doc, "missing", 0, 0.5, "Jump").is_err());
        assert!(add_notify(&mut json!({ "clips": [] }), "walk", 0.0, "Jump").is_err());
    }
}
