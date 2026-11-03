//! 中立窗口句柄——两个后端（winit / Win32）之上同一张脸。
//!
//! `prism-app` 只通过 [`PlatformWindow`] 取窗口尺寸、原始句柄和指针捕获，
//! 所以「关掉 winit」不需要在应用层写第二套窗口概念。

use std::sync::Arc;

#[cfg(feature = "winit-backend")]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::config::WindowConfig;

/// 在窗口创建线程提取、供渲染线程使用的原始句柄。
#[derive(Clone, Copy)]
pub struct SendWindowHandles {
    pub display: RawDisplayHandle,
    pub window: RawWindowHandle,
}

// Win32 HWND 和 Android ANativeWindow 是进程级句柄，窗口生命周期覆盖渲染线程。
unsafe impl Send for SendWindowHandles {}

/// 窗口上下文——启动时创建，关闭时销毁。
///
/// 注意：渲染器不在此创建。为达成「秒开窗口、事件不被初始化阻塞」，
/// 窗口在**主线程**快速创建（~数毫秒），而重量级的 `GraphRenderer`
/// （`prism-render`）构建移到**渲染线程**异步完成（见 `prism_app`）。
pub struct PlatformContext {
    pub(crate) window: Arc<PlatformWindow>,
}

/// 一个已创建的窗口（后端由 feature 决定）。
pub struct PlatformWindow {
    backend: Backend,
}

enum Backend {
    #[cfg(feature = "winit-backend")]
    Winit(winit::window::Window),
    #[cfg(all(windows, feature = "win32-native"))]
    Win32(crate::win32::Win32Window),
}

impl PlatformContext {
    /// 在**主线程**创建 winit 窗口（快速，不触碰 Vulkan / GPU）。
    ///
    /// 仅分配窗口——真正的渲染器在渲染线程通过 `GraphRenderer::new` 构建，
    /// 因此 `resumed` 能立即返回并把窗口事件（关闭/移动/缩放）派发给主线程。
    #[cfg(feature = "winit-backend")]
    pub fn create_window(
        event_loop: &winit::event_loop::ActiveEventLoop,
        window_cfg: &WindowConfig,
    ) -> Self {
        Self {
            window: Arc::new(crate::winit_backend::create_window(event_loop, window_cfg)),
        }
    }

    /// 用 Win32 API 创建原生窗口（不经过 winit）。
    ///
    /// 与 [`Self::create_window`] 语义一致：只开窗口，不做任何 GPU / 引擎工作。
    #[cfg(all(windows, feature = "win32-native"))]
    pub fn create_window_native(window_cfg: &WindowConfig) -> anyhow::Result<Self> {
        Ok(Self {
            window: Arc::new(crate::win32::create_window(window_cfg)?),
        })
    }

    pub fn window(&self) -> &PlatformWindow {
        &self.window
    }

    pub fn window_arc(&self) -> Arc<PlatformWindow> {
        Arc::clone(&self.window)
    }
}

#[cfg(feature = "winit-backend")]
impl PlatformWindow {
    pub(crate) fn from_winit(window: winit::window::Window) -> Self {
        Self {
            backend: Backend::Winit(window),
        }
    }
}

#[cfg(all(windows, feature = "win32-native"))]
impl PlatformWindow {
    pub(crate) fn from_win32(window: crate::win32::Win32Window) -> Self {
        Self {
            backend: Backend::Win32(window),
        }
    }

    fn win32(&self) -> Option<&crate::win32::Win32Window> {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(_) => None,
            Backend::Win32(window) => Some(window),
        }
    }

    /// 取出所有排队中的平台事件（Win32 后端的消息泵）。
    ///
    /// 只有 `win32-native` 构建有这个方法：winit 后端的事件由它的
    /// `ApplicationHandler` 回调直接送达。
    pub fn poll_events(&self) -> Vec<crate::event::PlatformEvent> {
        self.win32().map(|w| w.poll_events()).unwrap_or_default()
    }

    /// 阻塞到有消息到达或 `timeout_ms` 到期。
    pub fn wait_events(&self, timeout_ms: u32) {
        if let Some(window) = self.win32() {
            window.wait_events(timeout_ms);
        }
    }

    /// 用户点了关闭按钮（或收到 `WM_QUIT`）。
    pub fn close_requested(&self) -> bool {
        self.win32().map(|w| w.close_requested()).unwrap_or(false)
    }
}

impl PlatformWindow {
    /// 客户区尺寸（物理像素）。
    pub fn inner_size(&self) -> (u32, u32) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => {
                let size = window.inner_size();
                (size.width, size.height)
            }
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.inner_size(),
        }
    }

    /// 逻辑像素 → 物理像素的比例（1.0 = 96 DPI）。
    pub fn scale_factor(&self) -> f64 {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => window.scale_factor(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.scale_factor(),
        }
    }

    /// 请求重绘（Win32 下是 `InvalidateRect`；winit 下是 `request_redraw`）。
    pub fn request_redraw(&self) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => window.request_redraw(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.request_redraw(),
        }
    }

    fn set_cursor_visible(&self, visible: bool) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => window.set_cursor_visible(visible),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.set_cursor_visible(visible),
        }
    }

    fn set_pointer_confined(&self, confined: bool) -> anyhow::Result<()> {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => {
                let mode = if confined {
                    winit::window::CursorGrabMode::Confined
                } else {
                    winit::window::CursorGrabMode::None
                };
                window
                    .set_cursor_grab(mode)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.set_pointer_confined(confined),
        }
    }

    /// 显示句柄（Vulkan 实例扩展 / 表面创建用）。
    pub fn raw_display_handle(&self) -> RawDisplayHandle {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => window
                .display_handle()
                .expect("get display handle")
                .as_raw(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.raw_display_handle(),
        }
    }

    /// 窗口句柄（供渲染线程创建表面）。
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit(window) => window.window_handle().expect("get window handle").as_raw(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.raw_window_handle(),
        }
    }
}

/// 取一份可跨线程传递的原始句柄。
pub fn raw_window_handles(window: &PlatformWindow) -> anyhow::Result<SendWindowHandles> {
    Ok(SendWindowHandles {
        display: window.raw_display_handle(),
        window: window.raw_window_handle(),
    })
}

/// Build raw Win32 handles for a host-owned child window.
///
/// `hinstance` **必须**给：`create_surface` 在 HWND 带 `hinstance = None` 时
/// 返回 `ERROR_INITIALIZATION_FAILED`（editor 原生视口踩过的坑）。
#[cfg(windows)]
pub fn raw_win32_window_handles(hwnd: isize, hinstance: isize) -> SendWindowHandles {
    use std::num::NonZeroIsize;

    let mut window = raw_window_handle::Win32WindowHandle::new(
        NonZeroIsize::new(hwnd).expect("native viewport HWND must be non-zero"),
    );
    window.hinstance = NonZeroIsize::new(hinstance);
    SendWindowHandles {
        display: RawDisplayHandle::Windows(raw_window_handle::WindowsDisplayHandle::new()),
        window: RawWindowHandle::Win32(window),
    }
}

/// 计算窗口所需的 Vulkan 实例扩展名。
///
/// 依赖窗口的 display handle，开销很低；在主线程（`resumed`）调用即可，
/// 结果随 [`SendWindowHandles`] 一起交给渲染线程用于 `GraphRenderer::new`。
pub fn required_vulkan_extensions(window: &PlatformWindow) -> Vec<String> {
    let ext_ptrs = ash_window::enumerate_required_extensions(window.raw_display_handle())
        .expect("enumerate required extensions");
    ext_ptrs
        .iter()
        .map(|p| {
            unsafe { std::ffi::CStr::from_ptr(*p) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Grab the cursor (hide + confine) for FPS-style pointer lock.
pub fn grab_pointer(window: &PlatformWindow) {
    window.set_cursor_visible(false);
    if let Err(e) = window.set_pointer_confined(true) {
        log::warn!("failed to grab cursor (pointer lock): {e}");
    }
}

/// Release the cursor (show + unconfine).
pub fn release_pointer(window: &PlatformWindow) {
    window.set_cursor_visible(true);
    if let Err(e) = window.set_pointer_confined(false) {
        log::warn!("failed to release cursor grab: {e}");
    }
}
