//! 作者侧剪辑同步标记表（`*.skinning/sync_markers.json`）的视图与编辑（#560）。
//!
//! 文件形状与 cooker 的 `authored_sync_markers` 一致：
//!
//! ```json
//! { "clips": { "walk": [0.0, 0.5], "run": [0.05, 0.55] } }
//! ```
//!
//! 键是 glTF 里的动画**原名**，值是归一化相位。写入时把相位归一化到 `[0, 1)`，
//! 重复相位（误差 `1e-6`）直接拒绝；数组顺序保持作者写入顺序，cook 时再排序去重。
//! 「对上不对得上某个动画」仍由 cook 时 warn。

use serde_json::{json, Value};

/// 一个剪辑的同步标记。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncMarkerClipView {
    pub clip: String,
    pub phases: Vec<f32>,
}

/// 整个同步标记表，按剪辑名排序。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SyncMarkersView {
    pub clips: Vec<SyncMarkerClipView>,
}

impl SyncMarkersView {
    /// 所有标记总数（面板标题用）。
    pub fn len(&self) -> usize {
        self.clips.iter().map(|clip| clip.phases.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 解析同步标记表；缺 `clips` 当作空表。
pub fn parse_sync_markers_view(document: &Value) -> Result<SyncMarkersView, String> {
    let Some(clips) = document.get("clips") else {
        return Ok(SyncMarkersView::default());
    };
    let Some(clips) = clips.as_object() else {
        return Err("sync marker file needs a `clips` object".to_string());
    };
    let mut parsed = Vec::with_capacity(clips.len());
    for (clip, phases) in clips {
        let Some(phases) = phases.as_array() else {
            return Err(format!("clip '{clip}' needs an array of phases"));
        };
        let mut view = Vec::with_capacity(phases.len());
        for (index, phase) in phases.iter().enumerate() {
            let phase = phase
                .as_f64()
                .ok_or_else(|| format!("clip '{clip}' marker #{index} must be a number"))?
                as f32;
            view.push(phase);
        }
        parsed.push(SyncMarkerClipView {
            clip: clip.clone(),
            phases: view,
        });
    }
    parsed.sort_by(|a, b| a.clip.cmp(&b.clip));
    Ok(SyncMarkersView { clips: parsed })
}

fn normalize(phase: f32) -> Result<f32, String> {
    if !phase.is_finite() {
        return Err("a sync marker phase must be finite".to_string());
    }
    Ok(phase.rem_euclid(1.0))
}

fn phase_of(value: &Value) -> f32 {
    value.as_f64().unwrap_or(0.0) as f32
}

fn has_phase(entries: &[Value], phase: f32, skip: Option<usize>) -> bool {
    entries
        .iter()
        .enumerate()
        .any(|(index, entry)| Some(index) != skip && (phase_of(entry) - phase).abs() <= 1e-6)
}

/// 给某个剪辑追加一个同步标记；剪辑键不存在时自动创建。
///
/// 相位先归一化到 `[0, 1)`；同一剪辑里已有相同相位（误差 `1e-6`）时拒绝，
/// 避免「加了一条但 cook 后消失」。
pub fn add_sync_marker(document: &mut Value, clip: &str, phase: f32) -> Result<(), String> {
    let clip = clip.trim();
    if clip.is_empty() {
        return Err("a sync marker needs a clip name".to_string());
    }
    let phase = normalize(phase)?;
    if document.get("clips").is_none() {
        document["clips"] = json!({});
    }
    let Some(clips) = document.get_mut("clips").and_then(Value::as_object_mut) else {
        return Err("sync marker file needs a `clips` object".to_string());
    };
    let entries = clips.entry(clip.to_string()).or_insert_with(|| json!([]));
    let Some(entries) = entries.as_array_mut() else {
        return Err(format!("clip '{clip}' has an invalid marker list"));
    };
    if has_phase(entries, phase, None) {
        return Err(format!(
            "clip '{clip}' already has a sync marker at {phase:.3}"
        ));
    }
    entries.push(json!(phase));
    Ok(())
}

/// 删除某个剪辑的第 `index` 个同步标记。
pub fn remove_sync_marker(document: &mut Value, clip: &str, index: usize) -> Result<(), String> {
    let Some(entries) = find_clip_mut(document, clip)? else {
        return Err(format!("sync marker file has no clip '{clip}'"));
    };
    if index >= entries.len() {
        return Err(format!("clip '{clip}' has no marker #{index}"));
    }
    entries.remove(index);
    Ok(())
}

/// 改某个剪辑第 `index` 个同步标记的相位。
pub fn set_sync_marker(
    document: &mut Value,
    clip: &str,
    index: usize,
    phase: f32,
) -> Result<(), String> {
    let phase = normalize(phase)?;
    let Some(entries) = find_clip_mut(document, clip)? else {
        return Err(format!("sync marker file has no clip '{clip}'"));
    };
    if index >= entries.len() {
        return Err(format!("clip '{clip}' has no marker #{index}"));
    }
    if has_phase(entries, phase, Some(index)) {
        return Err(format!(
            "clip '{clip}' already has a sync marker at {phase:.3}"
        ));
    }
    entries[index] = json!(phase);
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
        return Err("sync marker file needs a `clips` object".to_string());
    };
    let Some(entries) = clips.get_mut(clip) else {
        return Ok(None);
    };
    let Some(entries) = entries.as_array_mut() else {
        return Err(format!("clip '{clip}' has an invalid marker list"));
    };
    Ok(Some(entries))
}

/// 时间轴上的 x 坐标映射成 `[0, 1)` 相位，量化到 0.001。
///
/// 面板拖拽和单元测试共用这一份换算，避免 UI 与测试各写一套夹取逻辑。
pub fn sync_marker_phase_at(x: f32, left: f32, width: f32) -> f32 {
    if !x.is_finite() || !left.is_finite() || !width.is_finite() || width <= 0.0 {
        return 0.0;
    }
    let fraction = ((x - left) / width).clamp(0.0, 1.0);
    let phase = (fraction * 1000.0).round() / 1000.0;
    phase.rem_euclid(1.0)
}

/// 时间轴命中：返回离 `x` 最近、且像素距离不超过 `radius` 的标记下标。
pub fn nearest_sync_marker_index(
    phases: &[f32],
    x: f32,
    left: f32,
    width: f32,
    radius: f32,
) -> Option<usize> {
    if !x.is_finite() || !width.is_finite() || width <= 0.0 || !radius.is_finite() {
        return None;
    }
    phases
        .iter()
        .enumerate()
        .map(|(index, phase)| {
            let marker = left + phase.clamp(0.0, 1.0) * width;
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
                "walk": [0.0, 0.5],
                "run": [0.05, 0.55]
            }
        })
    }

    #[test]
    fn a_view_lists_clips_sorted_with_their_markers() {
        let view = parse_sync_markers_view(&document()).expect("parses");
        assert_eq!(view.clips.len(), 2);
        assert_eq!(view.clips[0].clip, "run");
        assert_eq!(view.clips[0].phases, [0.05, 0.55]);
        assert_eq!(view.clips[1].phases, [0.0, 0.5]);
        assert_eq!(view.len(), 4);
    }

    #[test]
    fn a_missing_or_malformed_file_is_handled() {
        assert!(parse_sync_markers_view(&json!({}))
            .expect("empty")
            .is_empty());
        assert!(parse_sync_markers_view(&json!({ "clips": [] })).is_err());
        assert!(parse_sync_markers_view(&json!({ "clips": { "walk": 3 } })).is_err());
        assert!(parse_sync_markers_view(&json!({ "clips": { "walk": ["x"] } })).is_err());
    }

    #[test]
    fn markers_are_normalized_and_keep_author_order() {
        let mut doc = document();
        add_sync_marker(&mut doc, "walk", 1.25).expect("adds");
        assert_eq!(doc["clips"]["walk"], json!([0.0, 0.5, 0.25]));
        add_sync_marker(&mut doc, "idle", -0.25).expect("creates clip");
        assert_eq!(doc["clips"]["idle"], json!([0.75]));
        assert!(add_sync_marker(&mut doc, "walk", 0.5).is_err(), "重复相位");
    }

    #[test]
    fn markers_can_be_edited_and_removed() {
        let mut doc = document();
        set_sync_marker(&mut doc, "walk", 0, 0.75).expect("sets");
        assert_eq!(doc["clips"]["walk"], json!([0.75, 0.5]));
        remove_sync_marker(&mut doc, "walk", 0).expect("removes");
        assert_eq!(doc["clips"]["walk"], json!([0.5]));
        set_sync_marker(&mut doc, "run", 0, 0.3).expect("moves");
        assert_eq!(doc["clips"]["run"], json!([0.3f32 as f64, 0.55]));
        assert!(
            set_sync_marker(&mut doc, "run", 0, 0.55).is_err(),
            "不能移动到已有相位"
        );
    }

    #[test]
    fn invalid_markers_are_rejected() {
        let mut doc = document();
        assert!(add_sync_marker(&mut doc, "", 0.0).is_err());
        assert!(add_sync_marker(&mut doc, "walk", f32::NAN).is_err());
        assert!(add_sync_marker(&mut doc, "walk", f32::INFINITY).is_err());
        assert!(add_sync_marker(&mut json!({ "clips": [] }), "walk", 0.0).is_err());
        assert!(remove_sync_marker(&mut doc, "walk", 9).is_err());
        assert!(remove_sync_marker(&mut doc, "missing", 0).is_err());
        assert!(set_sync_marker(&mut doc, "walk", 9, 0.5).is_err());
        assert!(set_sync_marker(&mut doc, "walk", 0, f32::NAN).is_err());
        assert!(set_sync_marker(&mut doc, "missing", 0, 0.5).is_err());
        assert!(
            set_sync_marker(&mut doc, "walk", 0, 0.5).is_err(),
            "重复相位"
        );
    }

    #[test]
    fn timeline_coordinates_map_to_clamped_phases() {
        assert_eq!(sync_marker_phase_at(50.0, 0.0, 100.0), 0.5);
        assert_eq!(sync_marker_phase_at(-40.0, 0.0, 100.0), 0.0);
        assert_eq!(sync_marker_phase_at(140.0, 0.0, 100.0), 0.0);
        assert_eq!(sync_marker_phase_at(33.0, 0.0, 100.0), 0.33);
        assert_eq!(sync_marker_phase_at(f32::NAN, 0.0, 100.0), 0.0);
        assert_eq!(sync_marker_phase_at(10.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn timeline_hit_testing_picks_the_nearest_marker_within_the_radius() {
        let phases = [0.25, 0.75];
        // 0..200px → 0.25 在 50px、0.75 在 150px。
        assert_eq!(
            nearest_sync_marker_index(&phases, 52.0, 0.0, 200.0, 8.0),
            Some(0)
        );
        assert_eq!(
            nearest_sync_marker_index(&phases, 146.0, 0.0, 200.0, 8.0),
            Some(1)
        );
        assert_eq!(
            nearest_sync_marker_index(&phases, 100.0, 0.0, 200.0, 8.0),
            None
        );
        assert_eq!(nearest_sync_marker_index(&[], 50.0, 0.0, 200.0, 8.0), None);
        assert_eq!(
            nearest_sync_marker_index(&phases, f32::INFINITY, 0.0, 200.0, 8.0),
            None
        );
    }
}
