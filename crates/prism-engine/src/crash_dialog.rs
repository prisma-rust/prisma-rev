//! 原生致命错误崩溃对话框
//!
//! 当渲染器遇到不可恢复的错误（设备丢失、验证失败、
//! 无法重建交换链等）时，我们想一次性向用户呈现错误信息并停止，
//! 而不是每帧刷日志。此模块显示一个**原生模态对话框**，提供两个操作：
//!
//! - **复制并退出**（同时将完整错误文本复制到剪贴板，方便用户粘贴到错误报告中）
//! - **退出**
//!
//! 每个平台使用其原生对话框 API，避免引入笨重的跨平台对话框 crate：
//!
//! | 平台 | 对话框 | 剪贴板 |
//! |----------|--------|-----------|
//! | Windows | `MessageBoxW` (`MB_YESNO`) | `OpenClipboard` / `SetClipboardData` |
//! | macOS    | `osascript` (`display dialog`) | `pbcopy` |
//! | Linux    | `zenity --question` (fallback: text on stderr) | `xclip`/`xsel` |
//! | Android | `AlertDialog` via JNI (模态轮询等待) | `ClipboardManager` via JNI |
//!
//! The dialog blocks the calling thread (the winit event-loop / main thread)
//! 直到用户确认，这自然"暂停"了渲染循环。
//! confirmation the caller tears down the event loop.
//!
//! ## Android
//!
//! The 错误 text is logged to logcat (tag `PrismaRev`), an `AlertDialog` is
//! shown on the UI 线程 via JNI against the `Activity` exposed by
//! [`android_activity::AndroidApp`], and the calling 线程 waits modally until
//! the 对话框 is dismissed. The 错误 text is also copied to the Android 系统
//! clipboard via `ClipboardManager` on dismissal.

/// The user's choice in the crash dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashChoice {
    /// "Copy & Exit": copy the error text to the clipboard, then exit.
    CopyAndExit,
    /// "Exit": just exit.
    Exit,
}

// ---------------------------------------------------------------------------
// Android app registration (only compiled on Android
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "android", test))]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct AndroidAppHandles {
    vm_ptr: *mut std::ffi::c_void,
    activity_ptr: *mut std::ffi::c_void,
}

#[cfg(any(target_os = "android", test))]
// SAFETY: the handles are raw pointers into the JVM/Activity, which live for
// the entire process. They are only dereferenced from the main thread (the
// winit event loop thread), which is also the thread that registered them.
unsafe impl Send for AndroidAppHandles {}
#[cfg(any(target_os = "android", test))]
unsafe impl Sync for AndroidAppHandles {}

#[cfg(any(target_os = "android", test))]
static ANDROID_APP: std::sync::RwLock<Option<AndroidAppHandles>> = std::sync::RwLock::new(None);

/// Register the `AndroidApp` so the crash dialog can reach the JVM/Activity
/// for showing an `AlertDialog`. Called from `android_main` before the
/// 事件 循环 starts. Overwrites any previous registration so re-entering the
/// Android activity always replaces any stale Activity handle.
///
/// No-op on non-Android platforms (the function simply isn't compiled there).
#[cfg(target_os = "android")]
pub fn register_android_app(app: &android_activity::AndroidApp) {
    let handles = AndroidAppHandles {
        vm_ptr: app.vm_as_ptr(),
        activity_ptr: app.activity_as_ptr(),
    };
    if let Ok(mut guard) = ANDROID_APP.write() {
        *guard = Some(handles);
    }
}

/// Unregister the `AndroidApp` on activity exit to avoid retaining dangling
/// pointers to a destroyed Activity.
#[cfg(any(target_os = "android", test))]
pub fn unregister_android_app() {
    if let Ok(mut guard) = ANDROID_APP.write() {
        *guard = None;
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Show the native crash dialog with `title` and `message`.
///
/// Blocks the calling thread until the user confirms. Returns the chosen
/// action. If showing the dialog fails on a platform that uses a subprocess
/// (macOS/Linux) or JNI (Android), the error text is logged and `Exit` is
/// returned so the caller still terminates.
pub fn show_crash_dialog(title: &str, message: &str) -> CrashChoice {
    // Always log the full error first so it's in the log even if the dialog
    // backend fails (e.g. zenity not installed).
    log::error!("FATAL: {title}\n{message}");

    let choice = show_native(title, message);
    if matches!(choice, CrashChoice::CopyAndExit) {
        if let Err(e) = copy_to_clipboard(message) {
            log::warn!("failed to copy crash text to clipboard: {e}");
        }
    }
    choice
}

// ---------------------------------------------------------------------------
// Platform dispatch
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[path = "crash_dialog_windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "crash_dialog_macos.rs"]
mod platform;

#[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
#[path = "crash_dialog_linux.rs"]
mod platform;

#[cfg(target_os = "android")]
#[path = "crash_dialog_android.rs"]
mod platform;

#[cfg(not(any(
    windows,
    target_os = "macos",
    target_os = "android",
    all(unix, not(target_os = "android"), not(target_os = "macos"))
)))]
#[path = "crash_dialog_fallback.rs"]
mod platform;

fn show_native(title: &str, message: &str) -> CrashChoice {
    platform::show(title, message)
}

/// 复制 `text` to the 系统 clipboard. Platform-specific.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    platform::copy_to_clipboard(text)
}

#[cfg(test)]
#[path = "crash_dialog_android.rs"]
mod test_android_syntax;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_clipboard_noop_when_not_registered() {
        let res = test_android_syntax::copy_to_clipboard("test message");
        assert!(res.is_ok());
    }

    #[test]
    fn android_show_exits_when_not_registered() {
        let choice = test_android_syntax::show("title", "message");
        assert_eq!(choice, CrashChoice::Exit);
    }

    #[test]
    fn android_app_registration_is_reentrant() {
        let ptr1 = 0x1234 as *mut std::ffi::c_void;
        let ptr2 = 0x5678 as *mut std::ffi::c_void;

        // Register initial handle
        if let Ok(mut guard) = ANDROID_APP.write() {
            *guard = Some(AndroidAppHandles {
                vm_ptr: ptr1,
                activity_ptr: ptr1,
            });
        }
        assert_eq!(ANDROID_APP.read().unwrap().unwrap().activity_ptr, ptr1);

        // Re-entry replaces the previous handle
        if let Ok(mut guard) = ANDROID_APP.write() {
            *guard = Some(AndroidAppHandles {
                vm_ptr: ptr2,
                activity_ptr: ptr2,
            });
        }
        assert_eq!(ANDROID_APP.read().unwrap().unwrap().activity_ptr, ptr2);

        // Exit / unregister clears the reference so it never dangles
        unregister_android_app();
        assert!(ANDROID_APP.read().unwrap().is_none());
    }
}
