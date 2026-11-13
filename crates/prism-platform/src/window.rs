//! Neutral window handle — one face over both backends (winit / Win32).
//!
//! `prism-app` reads the window size, raw handles and pointer capture only through
//! [`PlatformWindow`], so turning off winit does not require a second window
//! abstraction in the app layer.

use std::sync::Arc;

#[cfg(feature = "winit-backend")]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::config::WindowConfig;

/// Raw handles extracted on the window-creation thread for use by the render thread.
#[derive(Clone, Copy)]
pub struct SendWindowHandles {
    pub display: RawDisplayHandle,
    pub window: RawWindowHandle,
}

// Win32 HWND and Android ANativeWindow are process-level handles; the window
// outlives the render thread.
unsafe impl Send for SendWindowHandles {}

/// Window context — created at startup and destroyed on shutdown.
///
/// The renderer is not created here. To open the window in milliseconds without
/// blocking the event loop on initialization, the window is created quickly on the
/// **main thread** while the heavyweight `GraphRenderer` (`prism-render`) is built
/// asynchronously on the **render thread** (see `prism_app`).
pub struct PlatformContext {
    pub(crate) window: Arc<PlatformWindow>,
}

/// A created window; the backend is selected by feature.
pub struct PlatformWindow {
    backend: Backend,
}

enum Backend {
    /// winit 没有「光标是否被抓住」的查询接口，所以这里自己记一份（抓 / 放只发生在
    /// `grab_pointer` / `release_pointer`，写窗口后原子更新）。
    #[cfg(feature = "winit-backend")]
    Winit {
        window: winit::window::Window,
        pointer_locked: std::sync::atomic::AtomicBool,
    },
    #[cfg(all(windows, feature = "win32-native"))]
    Win32(crate::win32::Win32Window),
}

impl PlatformContext {
    /// Creates the winit window on the **main thread** (fast, no Vulkan / GPU work).
    ///
    /// It only allocates the window; the actual renderer is built on the render
    /// thread by `GraphRenderer::new`, so `resumed` can return immediately and
    /// dispatch window events (close / move / resize) on the main thread.
    #[cfg(feature = "winit-backend")]
    pub fn create_window(
        event_loop: &winit::event_loop::ActiveEventLoop,
        window_cfg: &WindowConfig,
    ) -> Self {
        Self {
            window: Arc::new(crate::winit_backend::create_window(event_loop, window_cfg)),
        }
    }

    /// Creates a native window with the Win32 API (bypassing winit).
    ///
    /// Same contract as [`Self::create_window`]: only opens the window, doing no GPU
    /// or engine work.
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
            backend: Backend::Winit {
                window,
                pointer_locked: std::sync::atomic::AtomicBool::new(false),
            },
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
            Backend::Winit { .. } => None,
            Backend::Win32(window) => Some(window),
        }
    }

    /// Drains all queued platform events (the Win32 backend's message pump).
    ///
    /// Only `win32-native` builds have this method; the winit backend delivers
    /// events directly through its `ApplicationHandler` callbacks.
    pub fn poll_events(&self) -> Vec<crate::event::PlatformEvent> {
        self.win32().map(|w| w.poll_events()).unwrap_or_default()
    }

    /// Blocks until a message arrives or `timeout_ms` elapses.
    pub fn wait_events(&self, timeout_ms: u32) {
        if let Some(window) = self.win32() {
            window.wait_events(timeout_ms);
        }
    }

    /// The user clicked the close button (or `WM_QUIT` was received).
    pub fn close_requested(&self) -> bool {
        self.win32().map(|w| w.close_requested()).unwrap_or(false)
    }
}

impl PlatformWindow {
    /// Client-area size in physical pixels.
    pub fn inner_size(&self) -> (u32, u32) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => {
                let size = window.inner_size();
                (size.width, size.height)
            }
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.inner_size(),
        }
    }

    /// Logical-pixel to physical-pixel ratio (1.0 = 96 DPI).
    pub fn scale_factor(&self) -> f64 {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => window.scale_factor(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.scale_factor(),
        }
    }

    /// Requests a redraw (`InvalidateRect` on Win32, `request_redraw` under winit).
    pub fn request_redraw(&self) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => window.request_redraw(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.request_redraw(),
        }
    }

    fn set_cursor_visible(&self, visible: bool) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => window.set_cursor_visible(visible),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.set_cursor_visible(visible),
        }
    }

    fn set_pointer_confined(&self, confined: bool) -> anyhow::Result<()> {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => {
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

    /// 光标是否正被抓住（= 指针锁定的**平台侧**真值）。
    ///
    /// `prism-app` 另有一份 `InputManager::pointer_locked`，但帧钩子（宿主 UI）拿不到
    /// `InputManager`；查窗口比让钩子自己再存一份状态更可靠——Esc、失去焦点、点回
    /// 窗口这些路径都会经过 `grab_pointer` / `release_pointer`。
    pub fn is_pointer_locked(&self) -> bool {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { pointer_locked, .. } => {
                pointer_locked.load(std::sync::atomic::Ordering::Relaxed)
            }
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.is_pointer_locked(),
        }
    }

    /// 记录锁定状态（只在 `grab_pointer` / `release_pointer` 里调用）。
    fn set_pointer_locked(&self, locked: bool) {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { pointer_locked, .. } => {
                pointer_locked.store(locked, std::sync::atomic::Ordering::Relaxed);
            }
            // Win32 后端在 `set_pointer_confined` 里同步维护 `state.grab`，无需再记。
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(_) => {}
        }
    }

    /// Display handle (used for Vulkan instance extensions / surface creation).
    pub fn raw_display_handle(&self) -> RawDisplayHandle {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => window
                .display_handle()
                .expect("get display handle")
                .as_raw(),
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.raw_display_handle(),
        }
    }

    /// Window handle (used by the render thread to create the surface).
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        match &self.backend {
            #[cfg(feature = "winit-backend")]
            Backend::Winit { window, .. } => {
                window.window_handle().expect("get window handle").as_raw()
            }
            #[cfg(all(windows, feature = "win32-native"))]
            Backend::Win32(window) => window.raw_window_handle(),
        }
    }
}

/// Returns a copy of the raw handles that can cross threads.
pub fn raw_window_handles(window: &PlatformWindow) -> anyhow::Result<SendWindowHandles> {
    Ok(SendWindowHandles {
        display: window.raw_display_handle(),
        window: window.raw_window_handle(),
    })
}

/// Build raw Win32 handles for a host-owned child window.
///
/// `hinstance` **must** be set: `create_surface` returns
/// `ERROR_INITIALIZATION_FAILED` when the HWND has `hinstance = None` (a pitfall
/// hit by the editor's native viewport).
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

/// Computes the Vulkan instance extensions the window requires.
///
/// Depends only on the window's display handle and is cheap; call it on the main
/// thread (`resumed`) and pass the result together with [`SendWindowHandles`] to
/// the render thread for `GraphRenderer::new`.
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
    window.set_pointer_locked(true);
}

/// Release the cursor (show + unconfine).
pub fn release_pointer(window: &PlatformWindow) {
    window.set_cursor_visible(true);
    if let Err(e) = window.set_pointer_confined(false) {
        log::warn!("failed to release cursor grab: {e}");
    }
    window.set_pointer_locked(false);
}
