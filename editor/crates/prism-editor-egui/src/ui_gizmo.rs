//! Scene 视图里的 UI 锚点手柄（#429）。
//!
//! 只做纯几何：把归一化锚点换算成视口里的手柄位置、命中最近的 handle，
//! 以及把拖动中的光标换算回新的锚点。文档写入在
//! `Workspace::set_ui_anchors_preserving_rect` 里完成，方便单测。

use egui::{Pos2, Rect};

/// 手柄的视觉半径（正方形半边长）。
pub const HANDLE_RADIUS: f32 = 4.0;

/// 命中半径：比视觉稍大，触屏 / 高 DPI 下也好点。
pub const HIT_RADIUS: f32 = 9.0;

/// 锚点矩形上的一个可拖动手柄。
///
/// 点锚点只出一个 `Point`（min == max），拉伸轴按被拉伸的方向出边 / 角手柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiAnchorHandle {
    Point,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// 在当前父矩形里列出所有手柄。坐标是 egui points。
pub fn anchor_handles(anchors: [f32; 4], parent: Rect) -> Vec<(UiAnchorHandle, Pos2)> {
    let [min_x, min_y, max_x, max_y] = anchors;
    let x0 = parent.min.x + min_x * parent.width();
    let x1 = parent.min.x + max_x * parent.width();
    let y0 = parent.min.y + min_y * parent.height();
    let y1 = parent.min.y + max_y * parent.height();
    let x_point = (max_x - min_x).abs() <= 1e-5;
    let y_point = (max_y - min_y).abs() <= 1e-5;

    match (x_point, y_point) {
        (true, true) => vec![(UiAnchorHandle::Point, egui::pos2(x0, y0))],
        (false, true) => vec![
            (UiAnchorHandle::Left, egui::pos2(x0, y0)),
            (UiAnchorHandle::Right, egui::pos2(x1, y0)),
        ],
        (true, false) => vec![
            (UiAnchorHandle::Top, egui::pos2(x0, y0)),
            (UiAnchorHandle::Bottom, egui::pos2(x0, y1)),
        ],
        (false, false) => {
            let xm = (x0 + x1) * 0.5;
            let ym = (y0 + y1) * 0.5;
            vec![
                (UiAnchorHandle::TopLeft, egui::pos2(x0, y0)),
                (UiAnchorHandle::Top, egui::pos2(xm, y0)),
                (UiAnchorHandle::TopRight, egui::pos2(x1, y0)),
                (UiAnchorHandle::Left, egui::pos2(x0, ym)),
                (UiAnchorHandle::Right, egui::pos2(x1, ym)),
                (UiAnchorHandle::BottomLeft, egui::pos2(x0, y1)),
                (UiAnchorHandle::Bottom, egui::pos2(xm, y1)),
                (UiAnchorHandle::BottomRight, egui::pos2(x1, y1)),
            ]
        }
    }
}

/// 离光标最近、且在命中半径内的手柄。
pub fn hit_handle(handles: &[(UiAnchorHandle, Pos2)], pointer: Pos2) -> Option<UiAnchorHandle> {
    handles
        .iter()
        .map(|(handle, pos)| (*handle, pointer.distance(*pos)))
        .filter(|(_, distance)| *distance <= HIT_RADIUS)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(handle, _)| handle)
}

/// 拖动光标 → 新锚点。
///
/// `snap` 时把归一化坐标吸到 5% 的整数倍，和编辑器里 Ctrl 吸附的手感一致。
/// 轴被拖成点锚点时 min == max，保持点锚点；越过另一条边时夹住，不交换。
pub fn anchors_from_drag(
    handle: UiAnchorHandle,
    pointer: Pos2,
    parent: Rect,
    anchors: [f32; 4],
    snap: bool,
) -> [f32; 4] {
    let normalize = |value: f32, origin: f32, size: f32| {
        if size <= 0.0 {
            return 0.0;
        }
        let mut normalized = (value - origin) / size;
        if snap {
            normalized = (normalized * 20.0).round() / 20.0;
        }
        normalized.clamp(0.0, 1.0)
    };
    let nx = normalize(pointer.x, parent.min.x, parent.width());
    let ny = normalize(pointer.y, parent.min.y, parent.height());

    let [mut min_x, mut min_y, mut max_x, mut max_y] = anchors;
    match handle {
        UiAnchorHandle::Point => {
            min_x = nx;
            max_x = nx;
            min_y = ny;
            max_y = ny;
        }
        UiAnchorHandle::Left => min_x = nx.min(max_x),
        UiAnchorHandle::Right => max_x = nx.max(min_x),
        UiAnchorHandle::Top => min_y = ny.min(max_y),
        UiAnchorHandle::Bottom => max_y = ny.max(min_y),
        UiAnchorHandle::TopLeft => {
            min_x = nx.min(max_x);
            min_y = ny.min(max_y);
        }
        UiAnchorHandle::TopRight => {
            max_x = nx.max(min_x);
            min_y = ny.min(max_y);
        }
        UiAnchorHandle::BottomLeft => {
            min_x = nx.min(max_x);
            max_y = ny.max(min_y);
        }
        UiAnchorHandle::BottomRight => {
            max_x = nx.max(min_x);
            max_y = ny.max(min_y);
        }
    }
    [min_x, min_y, max_x, max_y]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Rect {
        Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0))
    }

    #[test]
    fn point_anchor_has_one_handle_and_moves_both_edges() {
        let handles = anchor_handles([0.5, 0.5, 0.5, 0.5], parent());
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].1, egui::pos2(200.0, 100.0));

        let moved = anchors_from_drag(
            UiAnchorHandle::Point,
            egui::pos2(250.0, 125.0),
            parent(),
            [0.5, 0.5, 0.5, 0.5],
            false,
        );
        assert_eq!(moved, [0.75, 0.75, 0.75, 0.75]);
    }

    #[test]
    fn stretched_anchors_expose_corners_and_edges() {
        let handles = anchor_handles([0.0, 0.0, 1.0, 1.0], parent());
        assert_eq!(handles.len(), 8);
        assert!(handles
            .iter()
            .any(|(handle, _)| *handle == UiAnchorHandle::TopLeft));
        assert!(handles
            .iter()
            .any(|(handle, _)| *handle == UiAnchorHandle::Right));
    }

    #[test]
    fn hit_handle_prefers_the_nearest_one() {
        let handles = anchor_handles([0.5, 0.5, 0.5, 0.5], parent());
        assert_eq!(
            hit_handle(&handles, egui::pos2(203.0, 103.0)),
            Some(UiAnchorHandle::Point)
        );
        assert_eq!(hit_handle(&handles, egui::pos2(220.0, 100.0)), None);
    }

    #[test]
    fn snapping_rounds_to_five_percent_and_clamps() {
        let snapped = anchors_from_drag(
            UiAnchorHandle::Point,
            egui::pos2(238.0, 57.0),
            parent(),
            [0.5, 0.5, 0.5, 0.5],
            true,
        );
        assert_eq!(snapped, [0.7, 0.05, 0.7, 0.05]);

        let clamped = anchors_from_drag(
            UiAnchorHandle::Right,
            egui::pos2(0.0, 0.0),
            parent(),
            [0.2, 0.2, 0.8, 0.8],
            false,
        );
        assert_eq!(clamped[2], 0.2, "右边界不能拖到左边界左边");
    }

    #[test]
    fn left_edge_only_changes_min_x() {
        let moved = anchors_from_drag(
            UiAnchorHandle::Left,
            egui::pos2(150.0, 80.0),
            parent(),
            [0.0, 0.0, 1.0, 1.0],
            false,
        );
        assert_eq!(moved, [0.25, 0.0, 1.0, 1.0]);
    }
}
