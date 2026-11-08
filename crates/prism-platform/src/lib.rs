//! Platform abstraction for PrismaRev.
//!
//! Provides the window-system interface, Vulkan surface creation and input event
//! routing. Contains no application-specific logic — the game loop lives in
//! `prism-app`.
//!
//! ## Window backends (features)
//!
//! - `winit-backend` (default): winit window + event loop for desktop / Android.
//! - `win32-native` (off by default, Windows only): opens a window directly with the
//!   Win32 API (`RegisterClassExW` / `CreateWindowExW` / message loop) and does
//!   **not** link winit, saving its binary size in Windows release builds.
//!   Size-sensitive distributions pick one:
//!   `prism-app = { default-features = false, features = ["win32-native", ...] }`.
//!
//! [`PlatformWindow`] is the neutral handle over both backends: `prism-app` only
//! sees it and [`PlatformEvent`], never a window library type.

#![deny(warnings)]

mod config;
mod event;
mod window;

#[cfg(feature = "winit-backend")]
mod winit_backend;

#[cfg(all(windows, feature = "win32-native"))]
mod win32;

#[cfg(not(any(feature = "winit-backend", all(windows, feature = "win32-native"))))]
compile_error!(
    "prism-platform: 至少启用一个窗口后端（`winit-backend`，或 Windows 上的 `win32-native`）"
);

pub use config::WindowConfig;
pub use event::{
    scroll_lines_from_pixels, Key, Modifiers, PlatformEvent, TouchPhase, PIXELS_PER_SCROLL_LINE,
};
#[cfg(windows)]
pub use window::raw_win32_window_handles;
pub use window::{
    grab_pointer, raw_window_handles, release_pointer, required_vulkan_extensions, PlatformContext,
    PlatformWindow, SendWindowHandles,
};
#[cfg(feature = "winit-backend")]
pub use winit_backend::to_platform_event;

/// Creates the platform event loop on Android.
///
/// Android's `AndroidApp` is provided by the user project's JNI entry; building the
/// event loop belongs to the platform layer, so the app layer only hands its own
/// `App` to winit.
#[cfg(all(target_os = "android", feature = "winit-backend"))]
pub fn build_android_event_loop(
    android_app: winit::platform::android::activity::AndroidApp,
) -> anyhow::Result<winit::event_loop::EventLoop<()>> {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    Ok(winit::event_loop::EventLoop::builder()
        .with_android_app(android_app)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build Android event loop: {e}"))?)
}
