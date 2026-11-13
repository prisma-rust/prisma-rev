//! Win32 native window backend (`feature = "win32-native"`, Windows only).
//!
//! Goal: keep winit out of Windows release builds. The approach is a minimal Win32
//! window + message loop — the `RegisterClassExW` / `CreateWindowExW` /
//! `PeekMessageW` family, plus a `WndProc` that translates `WM_*` into
//! [`PlatformEvent`].
//!
//! This backend is **frozen**: correctness fixes and new neutral `PlatformEvent`
//! variants only, no feature work (see `AGENTS.md` § Window backends, decision
//! recorded in #1033).
//!
//! # Why the message loop is not a blocking `GetMessageW`
//!
//! The main thread must keep advancing the simulation even when no window message
//! arrives (otherwise a static image freezes; the winit backend uses
//! `ControlFlow::Poll` for this). `GetMessageW` blocks indefinitely on an empty
//! queue, and only `SetTimer` + `WM_TIMER` can give it a timeout — but `WM_TIMER`
//! has the system clock resolution (~15.6 ms), which pins the main loop at ~64 Hz
//! and caps the render frame rate.
//!
//! So this uses the combination MSDN recommends for game loops:
//! [`PlatformWindow::poll_events`] (a non-blocking `PeekMessageW` pump) +
//! [`PlatformWindow::wait_events`] (`MsgWaitForMultipleObjectsEx`: returns
//! immediately when a message arrives, otherwise waits up to `timeout_ms`).
//!
//! # Explicitly out of scope (boundaries traded for size and risk; not bugs)
//!
//! - **No IME / text input**: `WM_CHAR` / `WM_IME_*` are all ignored and [`Key`]
//!   expresses physical keys only.
//! - **No drag and drop**: `DragAcceptFiles` is not registered, so there is no
//!   `WM_DROPFILES`.
//! - **No multi-monitor DPI smoothing**: the process declares Per-Monitor-V2 and
//!   `WM_DPICHANGED` only adjusts the window position/size to the rectangle the
//!   system suggests (`scale_factor()` reads `GetDpiForWindow`), but the engine does
//!   not relayout the ECS UI from it.
//! - **No taskbar progress / thumbnail toolbar / jump list**: `ITaskbarList3` is
//!   untouched.
//! - **No touch**: [`PlatformEvent::Touch`] is never produced; only mouse and
//!   keyboard.
//! - **Left/right modifiers**: `WM_KEYDOWN` reports the generic `VK_SHIFT` /
//!   `VK_CONTROL` / `VK_MENU`, and both sides are treated as the left one (see
//!   [`crate::Key::from_vk`]).
//! - **Pointer lock** is implemented with "hide the cursor + `ClipCursor` + recenter
//!   the cursor in the client area every frame": the `WM_MOUSEMOVE` offset becomes
//!   [`PlatformEvent::MouseMotion`]. There is no `WM_INPUT` (Raw Input), so
//!   `CursorMoved` is no longer produced while locked.
//! - **Fullscreen** is `WS_POPUP` at the primary monitor size, with no exclusive
//!   fullscreen mode switch.
//! - Limits such as `min_width` / `max_width` apply to the **window** size
//!   (`WM_GETMINMAXINFO`), whereas the corresponding winit setting is the **client
//!   area** size — a title bar / border apart.
//!
//! # Verification status
//!
//! This module has been **cross-compile verified only**
//! (`--target x86_64-pc-windows-gnu`); it has never run on a real Windows device.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard};

use raw_window_handle::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle};
use windows_sys::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, InvalidateRect, UpdateWindow, ValidateRect,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetSystemMetrics, GetWindowLongPtrW, LoadCursorW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassExW, SetCursorPos,
    SetWindowLongPtrW, SetWindowPos, ShowCursor, ShowWindow, TranslateMessage, CREATESTRUCTW,
    CS_HREDRAW, CS_OWNDC, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MINMAXINFO, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_HIDE, SW_MAXIMIZE, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT,
    WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW,
    WS_POPUP, WS_THICKFRAME, WS_VISIBLE,
};

use crate::config::WindowConfig;
use crate::event::{Key, Modifiers, PlatformEvent};
use crate::window::PlatformWindow;

const WHEEL_DELTA: f32 = 120.0;
const CLASS_NAME: &str = "PrismaRevWindow";
/// Event-queue cap, so a flood of window messages (high-DPI mouse) cannot exhaust
/// memory.
const MAX_QUEUED_EVENTS: usize = 4096;

/// An action deferred out of `WndProc`: these APIs **re-enter** `WndProc`
/// synchronously, so they must be called after the state lock is released.
enum PostAction {
    Capture,
    ReleaseCapture,
    ApplyDpiRect(RECT),
    /// Recenters the cursor in the client area (pointer-lock "recenter").
    Recenter(POINT),
}

/// Client-area center while pointer-locked: `client` is used to compute the delta
/// (`WM_MOUSEMOVE` reports **client-area** coordinates) and `screen` is used to
/// recenter with `SetCursorPos` (screen coordinates).
#[derive(Clone, Copy)]
struct Grab {
    client: POINT,
    screen: POINT,
}

struct WindowState {
    events: VecDeque<PlatformEvent>,
    close_requested: bool,
    /// Pointer-lock state: `Some(..)` means "hide + recenter" mode.
    grab: Option<Grab>,
    /// Whether the cursor is hidden by us (`ShowCursor` is a counter, called only on
    /// a flip).
    cursor_hidden: bool,
    dpi: u32,
    min_size: Option<(i32, i32)>,
    max_size: Option<(i32, i32)>,
}

impl WindowState {
    fn new(cfg: &WindowConfig) -> Self {
        let min_size = match (cfg.min_width, cfg.min_height) {
            (Some(w), Some(h)) => Some((w as i32, h as i32)),
            _ => None,
        };
        let max_size = match (cfg.max_width, cfg.max_height) {
            (Some(w), Some(h)) => Some((w as i32, h as i32)),
            _ => None,
        };
        Self {
            events: VecDeque::new(),
            close_requested: false,
            grab: None,
            cursor_hidden: false,
            dpi: 96,
            min_size,
            max_size,
        }
    }

    fn push(&mut self, event: PlatformEvent) {
        if self.events.len() < MAX_QUEUED_EVENTS {
            self.events.push_back(event);
        }
    }

    /// Live modifier snapshot from `GetKeyState` (`WM_KEYDOWN` carries no modifiers
    /// itself).
    fn modifiers(&self) -> Modifiers {
        unsafe {
            Modifiers {
                shift: key_down(VK_SHIFT),
                control: key_down(VK_CONTROL),
                alt: key_down(VK_MENU),
            }
        }
    }
}

unsafe fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn lock(state: &Mutex<WindowState>) -> MutexGuard<'_, WindowState> {
    // No logic in `WndProc` can panic; even if the mutex is poisoned, keep going
    // (losing the window is worse).
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn loword(value: usize) -> u16 {
    (value & 0xffff) as u16
}

fn hiword(value: usize) -> u16 {
    ((value >> 16) & 0xffff) as u16
}

/// Signed client-area coordinate in a mouse message (equivalent to `GET_X_LPARAM` /
/// `GET_Y_LPARAM`).
fn signed_x(lparam: isize) -> i32 {
    (lparam as usize & 0xffff) as u16 as i16 as i32
}

fn signed_y(lparam: isize) -> i32 {
    ((lparam as usize >> 16) & 0xffff) as u16 as i16 as i32
}

/// Retrieves the window state from `GWLP_USERDATA` (written during `WM_NCCREATE`).
unsafe fn state_of(hwnd: HWND) -> Option<&'static Mutex<WindowState>> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Mutex<WindowState>;
    unsafe { raw.as_ref() }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Creation: store `CreateWindowExW`'s `lpCreateParams` (a raw pointer to the Arc)
    // in USERDATA.
    if msg == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let Some(state) = (unsafe { state_of(hwnd) }) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };

    let mut post: Option<PostAction> = None;
    let mut handled = true;

    {
        let mut st = lock(state);
        match msg {
            WM_CLOSE => {
                st.close_requested = true;
                st.push(PlatformEvent::CloseRequested);
            }
            WM_DESTROY => unsafe { PostQuitMessage(0) },
            WM_SIZE => {
                let width = loword(lparam as usize) as u32;
                let height = hiword(lparam as usize) as u32;
                st.push(PlatformEvent::Resized { width, height });
                if st.grab.is_some() {
                    st.grab = unsafe { grab_center(hwnd) };
                }
            }
            WM_PAINT => {
                // Vulkan presents directly; GDI only needs to mark the dirty region
                // valid.
                unsafe { ValidateRect(hwnd, std::ptr::null()) };
            }
            WM_ERASEBKGND => {
                // Handled (equivalent to returning 1): do not paint the background,
                // avoiding a white flash.
            }
            WM_SETFOCUS => st.push(PlatformEvent::Focused(true)),
            WM_KILLFOCUS => st.push(PlatformEvent::Focused(false)),
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let modifiers = st.modifiers();
                st.push(PlatformEvent::Modifiers(modifiers));
                st.push(PlatformEvent::Key {
                    key: Key::from_vk(wparam as u32),
                    pressed: msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN,
                });
            }
            WM_MOUSEMOVE => {
                let x = signed_x(lparam);
                let y = signed_y(lparam);
                match st.grab {
                    Some(grab) => {
                        // Locked: the cursor is pinned to the client-area center, so
                        // the offset is the raw delta (both are client-area coords).
                        let (dx, dy) = (x - grab.client.x, y - grab.client.y);
                        if dx != 0 || dy != 0 {
                            st.push(PlatformEvent::MouseMotion {
                                dx: dx as f64,
                                dy: dy as f64,
                            });
                            // Recentring must happen after the state lock is released:
                            // `SetCursorPos` may dispatch a new mouse message, and
                            // re-entering `WndProc` on the same thread would lock the
                            // mutex again. (The recenter itself emits another move with
                            // dx = dy = 0, which the check above swallows.)
                            post = Some(PostAction::Recenter(grab.screen));
                        }
                    }
                    None => st.push(PlatformEvent::CursorMoved {
                        x: x as f64,
                        y: y as f64,
                    }),
                }
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                let button = match msg {
                    WM_LBUTTONDOWN => 0,
                    WM_RBUTTONDOWN => 1,
                    _ => 2,
                };
                st.push(PlatformEvent::MouseButton {
                    button,
                    pressed: true,
                });
                post = Some(PostAction::Capture);
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
                let button = match msg {
                    WM_LBUTTONUP => 0,
                    WM_RBUTTONUP => 1,
                    _ => 2,
                };
                st.push(PlatformEvent::MouseButton {
                    button,
                    pressed: false,
                });
                post = Some(PostAction::ReleaseCapture);
            }
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // XBUTTON1 = back button, XBUTTON2 = forward button.
                let button = if hiword(wparam) == 1 { 3 } else { 4 };
                st.push(PlatformEvent::MouseButton {
                    button,
                    pressed: msg == WM_XBUTTONDOWN,
                });
                post = Some(PostAction::Capture);
            }
            WM_MOUSEWHEEL => {
                // HIWORD(wParam) is a signed multiple of 120 per notch.
                let notches = hiword(wparam) as i16 as f32 / WHEEL_DELTA;
                st.push(PlatformEvent::MouseWheel {
                    delta: notches as f64,
                });
            }
            WM_SETCURSOR => {
                // While the cursor is hidden, do not let the DC paint the class cursor
                // back (return 1 = handled).
                if !st.cursor_hidden {
                    handled = false;
                }
            }
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
                if let Some((w, h)) = st.min_size {
                    info.ptMinTrackSize.x = w;
                    info.ptMinTrackSize.y = h;
                }
                if let Some((w, h)) = st.max_size {
                    info.ptMaxTrackSize.x = w;
                    info.ptMaxTrackSize.y = h;
                }
            }
            WM_DPICHANGED => {
                st.dpi = hiword(wparam) as u32;
                // Follow the system-suggested rectangle (no content scaling; see the
                // module docs).
                post = Some(PostAction::ApplyDpiRect(unsafe {
                    *(lparam as *const RECT)
                }));
            }
            _ => handled = false,
        }
    }

    if !handled {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    match post {
        Some(PostAction::Capture) => {
            unsafe { SetCapture(hwnd) };
        }
        Some(PostAction::ReleaseCapture) => unsafe {
            ReleaseCapture();
        },
        Some(PostAction::ApplyDpiRect(rect)) => unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        },
        Some(PostAction::Recenter(center)) => unsafe {
            SetCursorPos(center.x, center.y);
        },
        None => {}
    }

    // The return value of `WM_SETCURSOR` / `WM_ERASEBKGND` is a convention: 1 =
    // handled.
    if msg == WM_SETCURSOR || msg == WM_ERASEBKGND {
        1
    } else {
        0
    }
}

/// Client-area center, given in both client-area and screen coordinates.
unsafe fn grab_center(hwnd: HWND) -> Option<Grab> {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return None;
    }
    let client = POINT {
        x: rect.right / 2,
        y: rect.bottom / 2,
    };
    let mut screen = client;
    if unsafe { ClientToScreen(hwnd, &mut screen) } == 0 {
        return None;
    }
    Some(Grab { client, screen })
}

/// A created Win32 window.
pub(crate) struct Win32Window {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// Window state shared with `GWLP_USERDATA` (accessed as a raw pointer from
    /// `WndProc`).
    state: Arc<Mutex<WindowState>>,
    /// Pointer from `Arc::into_raw`; `Drop` reclaims this reference with
    /// `Arc::from_raw`.
    state_ptr: *mut Mutex<WindowState>,
}

// HWND is a process-level handle; this type is used only on the main thread that
// created it (only `SendWindowHandles` crosses threads).
unsafe impl Send for Win32Window {}
unsafe impl Sync for Win32Window {}

impl Drop for Win32Window {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() {
                // Destroy the window first (`WndProc` still touches the state), then
                // reclaim the Arc.
                DestroyWindow(self.hwnd);
            }
            drop(Arc::from_raw(self.state_ptr));
        }
    }
}

fn window_styles(cfg: &WindowConfig) -> (u32, u32) {
    // Borderless / fullscreen: title bar and resizing do not apply, so use a popup
    // directly.
    if cfg.fullscreen || !cfg.decorations {
        return (WS_POPUP | WS_VISIBLE, 0);
    }
    let mut style = WS_OVERLAPPEDWINDOW;
    if !cfg.resizable {
        style &= !(WS_THICKFRAME | WS_MAXIMIZEBOX);
    }
    (style, 0)
}

/// Creates a window with the Win32 API (bypassing winit).
pub(crate) fn create_window(cfg: &WindowConfig) -> anyhow::Result<PlatformWindow> {
    unsafe {
        // Per-Monitor-V2: without this declaration the process is treated as
        // DPI-unaware and Windows stretches the whole frame bitmap (blurry). Setting
        // it twice fails, which is fine to ignore.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinstance = GetModuleHandleW(std::ptr::null());
        anyhow::ensure!(!hinstance.is_null(), "GetModuleHandleW failed");

        let class_name = wide(CLASS_NAME);
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            // No background: Vulkan presents the whole frame, and a GDI repaint would
            // only flicker.
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        let atom = RegisterClassExW(&class);
        if atom == 0 {
            let error = GetLastError();
            // ERROR_CLASS_ALREADY_EXISTS = 1410: a second window in the same process
            // is normal.
            anyhow::ensure!(error == 1410, "RegisterClassExW failed ({error})");
        }

        // The configured width/height follow winit's `LogicalSize` semantics (logical
        // pixels); here they are scaled by the system DPI into client-area physical
        // pixels so both backends open a window of the same size.
        let scale = GetDpiForSystem() as f32 / 96.0;
        let client_w = ((cfg.width.max(1) as f32) * scale).round() as i32;
        let client_h = ((cfg.height.max(1) as f32) * scale).round() as i32;

        let (style, ex_style) = window_styles(cfg);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        if cfg.fullscreen {
            // Borderless fullscreen: fill the primary monitor directly (no exclusive
            // fullscreen).
            rect.right = GetSystemMetrics(SM_CXSCREEN);
            rect.bottom = GetSystemMetrics(SM_CYSCREEN);
        } else {
            AdjustWindowRectEx(&mut rect, style, 0, ex_style);
        }

        let title = wide(&cfg.title);
        let state = Arc::new(Mutex::new(WindowState::new(cfg)));
        let state_ptr = Arc::into_raw(Arc::clone(&state)) as *mut Mutex<WindowState>;

        let (x, y) = match (cfg.position_x, cfg.position_y) {
            (Some(x), Some(y)) => (x, y),
            _ => (CW_USEDEFAULT, CW_USEDEFAULT),
        };

        let hwnd = CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            state_ptr as *const c_void,
        );
        if hwnd.is_null() {
            // Window creation failed: reclaim the Arc reference handed out, avoiding a
            // leak.
            drop(Arc::from_raw(state_ptr));
            anyhow::bail!("CreateWindowExW failed ({})", GetLastError());
        }

        lock(&state).dpi = GetDpiForWindow(hwnd);

        if cfg.maximized {
            ShowWindow(hwnd, SW_MAXIMIZE);
        } else if cfg.visible {
            ShowWindow(hwnd, SW_SHOW);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
        UpdateWindow(hwnd);

        let window = Win32Window {
            hwnd,
            hinstance,
            state,
            state_ptr,
        };
        let (w, h) = window.inner_size();
        log::info!(
            "win32: window created ({w}x{h} client px, visible = {})",
            cfg.visible
        );
        Ok(PlatformWindow::from_win32(window))
    }
}

impl Win32Window {
    pub(crate) fn inner_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rect) } == 0 {
            return (0, 0);
        }
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }

    pub(crate) fn scale_factor(&self) -> f64 {
        let dpi = lock(&self.state).dpi.max(96);
        dpi as f64 / 96.0
    }

    pub(crate) fn request_redraw(&self) {
        unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
    }

    pub(crate) fn raw_window_handle(&self) -> RawWindowHandle {
        use std::num::NonZeroIsize;

        let mut handle = Win32WindowHandle::new(
            NonZeroIsize::new(self.hwnd as isize).expect("HWND must be non-zero"),
        );
        // `hinstance` must be set: without it `create_surface` returns
        // `ERROR_INITIALIZATION_FAILED` (a pitfall hit by the editor's native
        // viewport).
        handle.hinstance = NonZeroIsize::new(self.hinstance as isize);
        RawWindowHandle::Win32(handle)
    }

    pub(crate) fn raw_display_handle(&self) -> RawDisplayHandle {
        RawDisplayHandle::Windows(raw_window_handle::WindowsDisplayHandle::new())
    }

    pub(crate) fn set_cursor_visible(&self, visible: bool) {
        // `ShowCursor` is a counter: call it once per state flip. The call is made
        // after releasing the state lock (it may trigger `WM_SETCURSOR`, and
        // re-entering on the same thread would lock the mutex again).
        let flip = {
            let mut st = lock(&self.state);
            let hidden = st.cursor_hidden;
            if hidden == visible {
                st.cursor_hidden = !visible;
                true
            } else {
                false
            }
        };
        if flip {
            unsafe { ShowCursor(if visible { 1 } else { 0 }) };
        }
    }

    pub(crate) fn set_pointer_confined(&self, confined: bool) -> anyhow::Result<()> {
        if !confined {
            lock(&self.state).grab = None;
            unsafe { ClipCursor(std::ptr::null()) };
            return Ok(());
        }

        let grab = unsafe { grab_center(self.hwnd) }
            .ok_or_else(|| anyhow::anyhow!("GetClientRect/ClientToScreen failed"))?;
        let mut rect = RECT::default();
        anyhow::ensure!(
            unsafe { GetClientRect(self.hwnd, &mut rect) } != 0,
            "GetClientRect failed"
        );
        let mut top_left = POINT {
            x: rect.left,
            y: rect.top,
        };
        let mut bottom_right = POINT {
            x: rect.right,
            y: rect.bottom,
        };
        unsafe {
            anyhow::ensure!(
                ClientToScreen(self.hwnd, &mut top_left) != 0
                    && ClientToScreen(self.hwnd, &mut bottom_right) != 0,
                "ClientToScreen failed"
            );
            let clip = RECT {
                left: top_left.x,
                top: top_left.y,
                right: bottom_right.x,
                bottom: bottom_right.y,
            };
            ClipCursor(&clip);
            SetCursorPos(grab.screen.x, grab.screen.y);
        }
        lock(&self.state).grab = Some(grab);
        Ok(())
    }

    /// 光标是否正被 `ClipCursor` 抓在客户区中央（= 指针锁定）。
    pub(crate) fn is_pointer_locked(&self) -> bool {
        lock(&self.state).grab.is_some()
    }

    pub(crate) fn close_requested(&self) -> bool {
        lock(&self.state).close_requested
    }

    /// Drains events queued since the last call (the `PeekMessageW` pump plus
    /// `WndProc` collection).
    pub(crate) fn poll_events(&self) -> Vec<PlatformEvent> {
        loop {
            let mut msg = MSG::default();
            let has = unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
            if has == 0 {
                break;
            }
            // WM_QUIT does not go through `DispatchMessageW`.
            if msg.message == WM_QUIT {
                lock(&self.state).close_requested = true;
                break;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let mut st = lock(&self.state);
        st.events.drain(..).collect()
    }

    /// Blocks until a message arrives or `timeout_ms` elapses (the message is left
    /// for `poll_events` to drain).
    pub(crate) fn wait_events(&self, timeout_ms: u32) {
        unsafe {
            MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                timeout_ms,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_low_and_high_word() {
        assert_eq!(loword(0x0001_0002), 2);
        assert_eq!(hiword(0x0001_0002), 1);
        assert_eq!(signed_x(0x0000_0002), 2);
        assert_eq!(signed_y(0x0001_0002), 1);
        // Negative coordinates (mouse moved outside the client area's top-left).
        assert_eq!(signed_x(0x0000_FFFE), -2);
        assert_eq!(signed_y(0xFFFF_0000u64 as isize), -1);
        // One wheel notch = 1.0 line.
        assert_eq!(hiword(120usize) as i16 as f32 / WHEEL_DELTA, 1.0);
        assert_eq!(
            hiword((-120i16 as u16) as usize) as i16 as f32 / WHEEL_DELTA,
            -1.0
        );
    }

    #[test]
    fn window_styles_drop_resize_frame_when_fixed() {
        let mut cfg = WindowConfig {
            resizable: false,
            ..WindowConfig::default()
        };
        let (style, _) = window_styles(&cfg);
        assert_eq!(style & WS_THICKFRAME, 0);
        assert_eq!(style & WS_MAXIMIZEBOX, 0);
        assert_eq!(
            style,
            WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX)
        );

        cfg.resizable = true;
        let (style, _) = window_styles(&cfg);
        assert_eq!(style, WS_OVERLAPPEDWINDOW);

        cfg.decorations = false;
        let (style, _) = window_styles(&cfg);
        assert_ne!(style & WS_POPUP, 0);
    }
}
