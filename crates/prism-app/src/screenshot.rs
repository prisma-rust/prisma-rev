//! 内置截图：用环境变量在某一帧把画面写成 PNG。
//!
//! 这是给自动化（CI 截图、编辑器 / MCP 抓图、美术调参）用的最小接口，
//! 不引入新的公开 API：
//!
//! - `PRISMREV_SCREENSHOT=<path>` 启用，写出 PNG（父目录需已存在）。
//! - `PRISMREV_SCREENSHOT_FRAME=<n>` 在第 n 帧抓（默认 90，给资源上传和动画留时间）。
//! - `PRISMREV_SCREENSHOT_SIZE=1920x1080` 指定输出分辨率。
//! - `PRISMREV_SCREENSHOT_SCALE=2` 按窗口分辨率的倍数放大（`SIZE` 优先）。
//!
//! 缩放是最近邻，像素画放大不会糊；内容本身仍是窗口那一帧，不重渲染。

use std::path::PathBuf;

/// 一次待执行的截图。
#[derive(Debug, Clone)]
pub struct ScreenshotPlan {
    pub path: PathBuf,
    pub frame: u64,
    pub size: Option<[u32; 2]>,
    pub scale: Option<f32>,
}

impl ScreenshotPlan {
    /// 从环境变量读取；没有 `PRISMREV_SCREENSHOT` 时返回 `None`。
    pub fn from_env() -> Option<Self> {
        let raw_path = std::env::var("PRISMREV_SCREENSHOT").ok()?;
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = PathBuf::from(trimmed);
        let frame = std::env::var("PRISMREV_SCREENSHOT_FRAME")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|frame| *frame > 0)
            .unwrap_or(90);
        let size = std::env::var("PRISMREV_SCREENSHOT_SIZE")
            .ok()
            .and_then(|value| parse_size(&value));
        let scale = std::env::var("PRISMREV_SCREENSHOT_SCALE")
            .ok()
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|scale| *scale > 0.0);
        if size.is_none() && scale.is_none() {
            log::debug!("screenshot: no size/scale requested, using the window resolution");
        }
        Some(Self {
            path,
            frame,
            size,
            scale,
        })
    }

    /// 这一帧是否达到或超过设定帧数（一旦满足且内容就绪即可抓图）。
    pub fn fires_on(&self, frame_count: u64) -> bool {
        frame_count >= self.frame
    }
}

/// 解析 `1920x1080` / `1920X1080` / `1280,720`。
fn parse_size(raw: &str) -> Option<[u32; 2]> {
    let normalized = raw.trim().replace(['X', ',', '*'], "x");
    let (width, height) = normalized.split_once('x')?;
    let width = width.trim().parse::<u32>().ok()?;
    let height = height.trim().parse::<u32>().ok()?;
    (width > 0 && height > 0).then_some([width, height])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_size_spellings() {
        assert_eq!(parse_size("1920x1080"), Some([1920, 1080]));
        assert_eq!(parse_size(" 1280X720 "), Some([1280, 720]));
        assert_eq!(parse_size("800,600"), Some([800, 600]));
        assert_eq!(parse_size("0x600"), None);
        assert_eq!(parse_size("wide"), None);
    }

    #[test]
    fn fires_when_frame_reached() {
        let plan = ScreenshotPlan {
            path: PathBuf::from("shot.png"),
            frame: 5,
            size: None,
            scale: None,
        };
        assert!(!plan.fires_on(4));
        assert!(plan.fires_on(5));
        assert!(plan.fires_on(6));
    }
}
