//! 动画剪辑时间轴与根运动的纯数据换算（#560 P0 编辑器）。
//!
//! `.ranim` 本身是 cooked 二进制，编辑器只做**只读检查**：时间轴坐标、Notify /
//! 同步标记位置、根关节首尾位移。真正的编辑仍走作者侧 `notifies.json` /
//! `sync_markers.json` 侧车。

/// 时间轴上的 x 坐标映射成秒，夹到 `0..=duration` 并量化到 0.01 秒。
pub fn timeline_time_at(x: f32, left: f32, width: f32, duration: f32) -> f32 {
    if !x.is_finite()
        || !left.is_finite()
        || !width.is_finite()
        || width <= 0.0
        || !duration.is_finite()
        || duration <= 0.0
    {
        return 0.0;
    }
    let fraction = ((x - left) / width).clamp(0.0, 1.0);
    ((fraction * duration) * 100.0).round() / 100.0
}

/// 秒映射回时间轴 x 坐标；超出剪辑范围夹到两端。
pub fn timeline_x(time: f32, duration: f32, left: f32, width: f32) -> f32 {
    if !time.is_finite()
        || !duration.is_finite()
        || duration <= 0.0
        || !left.is_finite()
        || !width.is_finite()
        || width <= 0.0
    {
        return left;
    }
    left + (time / duration).clamp(0.0, 1.0) * width
}

/// 根关节首尾位移与绕 Y 的偏航角（度）。
///
/// PrismaRev 的 forward 是 `-Z`，所以 `atan2(dx, -dz)`：向 -Z 走是 0°，
/// 向 +X 走是 +90°。垂直位移不参与偏航。
pub fn root_motion_delta(start: [f32; 3], end: [f32; 3]) -> ([f32; 3], f32) {
    let delta = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
    let yaw = delta[0].atan2(-delta[2]).to_degrees();
    (delta, yaw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_coordinates_map_both_ways_and_clamp() {
        assert_eq!(timeline_time_at(50.0, 0.0, 100.0, 2.0), 1.0);
        assert_eq!(timeline_time_at(-40.0, 0.0, 100.0, 2.0), 0.0);
        assert_eq!(timeline_time_at(400.0, 0.0, 100.0, 2.0), 2.0);
        assert_eq!(timeline_time_at(33.0, 0.0, 100.0, 2.0), 0.66);
        assert_eq!(timeline_time_at(f32::NAN, 0.0, 100.0, 2.0), 0.0);
        assert_eq!(timeline_time_at(10.0, 0.0, 0.0, 2.0), 0.0);
        assert_eq!(timeline_time_at(10.0, 0.0, 100.0, 0.0), 0.0);

        assert_eq!(timeline_x(1.0, 2.0, 0.0, 100.0), 50.0);
        assert_eq!(timeline_x(-1.0, 2.0, 0.0, 100.0), 0.0);
        assert_eq!(timeline_x(9.0, 2.0, 0.0, 100.0), 100.0);
        assert_eq!(timeline_x(f32::NAN, 2.0, 0.0, 100.0), 0.0);
        assert_eq!(timeline_x(1.0, 0.0, 0.0, 100.0), 0.0);
    }

    #[test]
    fn root_motion_reports_delta_and_yaw_with_minus_z_forward() {
        let (delta, yaw) = root_motion_delta([0.0, 0.0, 0.0], [0.0, 0.0, -2.0]);
        assert_eq!(delta, [0.0, 0.0, -2.0]);
        assert!(yaw.abs() < 1e-4, "向 -Z 走是 0°");

        let (_, yaw) = root_motion_delta([0.0, 0.0, 0.0], [2.0, 0.0, 0.0]);
        assert!((yaw - 90.0).abs() < 1e-4, "向 +X 走是 +90°");

        let (delta, yaw) = root_motion_delta([1.0, 2.0, 3.0], [4.0, 6.0, 5.0]);
        assert_eq!(delta, [3.0, 4.0, 2.0]);
        assert!(yaw.is_finite(), "垂直位移不应产生 NaN");
    }
}
