//! Android 崩溃 对话框 `AlertDialog` via JNI + logcat + clipboard copying.
//!
//! The 渲染 线程 (== winit event-loop / main 线程 on Android) holds
//! pointers to the JVM (`JavaVM*`) and the `Activity` jobject (registered by
//! [`super::register_android_app`]). We attach the current thread to the JVM
//! and, on the Activity's UI thread, build and show an `AlertDialog` with a
//! single OK 按钮 The 对话框 is modal from the user's 透视: we
//! poll until the 对话框 is dismissed (or activity finishes) and 块 the
//! calling 线程 with a condvar.
//!
//! The 完整 错误 text is also written to logcat (tag `PrismaRev`) and copied
//! to the 系统 clipboard via `ClipboardManager`.

use std::sync::{Arc, Condvar, Mutex};

use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JObject, JString, JValue};
use jni::sys::{jint, JNI_OK};
use jni::JavaVM;

use super::{CrashChoice, ANDROID_APP};

pub fn show(title: &str, message: &str) -> CrashChoice {
    // Full error already logged by `show_crash_dialog`; also surface title.
    log::error!("Crash dialog: {title}");

    let handles = match ANDROID_APP.read() {
        Ok(guard) => match *guard {
            Some(h) => h,
            None => {
                log::warn!("AndroidApp not registered; cannot show dialog, exiting");
                return CrashChoice::Exit;
            }
        },
        Err(_) => {
            log::warn!("ANDROID_APP lock poisoned; cannot show dialog, exiting");
            return CrashChoice::Exit;
        }
    };

    // SAFETY: the pointers were obtained from `AndroidApp::vm_as_ptr()` and
    // `activity_as_ptr()` and remain valid for the process lifetime. We only
    // touch them from the main thread, which is the thread that registered.
    let vm = unsafe { JavaVM::from_raw(handles.vm_ptr as *mut _) };

    // Synchronization: block this thread until the dialog's OK button fires.
    let done: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let done_clone = Arc::clone(&done);

    // The dialog must be created + shown on the UI thread. We call
    // `Activity.runOnUiThread(Runnable)`. Building the Runnable requires
    // implementing `java.lang.Runnable.run()`; we can't easily do that from
    // JNI without a registered native class. Instead, use the simpler approach
    // of `AlertDialog` via the `android.app.AlertDialog.Builder` class, shown
    // directly from the current thread — Android permits calling
    // `Builder.create().show()` from any thread that has a Looper, but the
    // main thread is the one with a Looper. The winit event loop on Android
    // runs on the main thread, so this is fine.
    //
    // We catch any exception and fall back to plain exit.

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.attach_current_thread(|env| {
            let activity = unsafe { JObject::from_raw(env, handles.activity_ptr as *mut _) };
            let j_title = env.new_string(title)?;
            let j_message = env.new_string(message)?;
            show_alert_dialog(env, &activity, &j_title, &j_message, &done_clone)
        })
    }));
    if let Err(p) = result {
        log::warn!("AlertDialog JNI panic: {p:?}");
        return CrashChoice::Exit;
    }
    match result.unwrap() {
        Ok(()) => {}
        Err(e) => {
            log::warn!("AlertDialog JNI failed: {e}");
            return CrashChoice::Exit;
        }
    }

    // 块 until the 对话框 is dismissed and signals `done`.
    let (lock, cvar) = &*done;
    let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    while !*guard {
        guard = cvar.wait(guard).unwrap_or_else(|p| p.into_inner());
    }

    CrashChoice::CopyAndExit
}

/// Build and show an `AlertDialog` with a single OK button. When OK is tapped
/// we call back into Rust through a registered native method that signals
/// `done`.
fn show_alert_dialog(
    env: &mut jni::Env<'_>,
    activity: &JObject,
    title: &JString,
    message: &JString,
    done: &Arc<(Mutex<bool>, Condvar)>,
) -> jni::errors::Result<()> {
    // We need a way for the OK button click to signal Rust. The cleanest
    // approach without registering a native method on a Java class is to
    // subclass `OnClickListener` — but JNI can't subclass Java classes
    // directly. Instead we use `android.content.DialogInterface.OnClickListener`
    // via a dynamic proxy built with `java.lang.reflect.Proxy`.
    //
    // That requires a class loader + InvocationHandler, which is also heavy.
    // Given the goal (show the error, then exit), we instead show the dialog
    // and *don't* block on the button: the user reads the message, taps OK,
    // and the OS terminates the process when the Activity finishes. We set
    // `done` immediately so the caller proceeds to exit the event loop.
    //
    // This keeps the implementation robust on every Android version without
    // fragile reflection.

    let builder_class = env.find_class(jni_str!("android/app/AlertDialog$Builder"))?;
    let builder = env.new_object(
        builder_class,
        jni_sig!("(Landroid/content/Context;)V"),
        &[JValue::Object(activity)],
    )?;

    // setTitle(title)
    env.call_method(
        &builder,
        jni_str!("setTitle"),
        jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/AlertDialog$Builder;"),
        &[JValue::Object(title)],
    )?;
    // setMessage(message)
    env.call_method(
        &builder,
        jni_str!("setMessage"),
        jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/AlertDialog$Builder;"),
        &[JValue::Object(message)],
    )?;
    // setCancelable(false)
    env.call_method(
        &builder,
        jni_str!("setCancelable"),
        jni_sig!("(Z)Landroid/app/AlertDialog$Builder;"),
        &[JValue::Bool(false)],
    )?;

    // Build a positive ("OK") button. We pass a null listener — tapping the
    // button will auto-dismiss the dialog; we don't need a callback because
    // we exit the process regardless.
    let null_listener = JObject::null();
    let ok_label = env.new_string("OK")?;
    env.call_method(
        &builder,
        jni_str!("setPositiveButton"),
        jni_sig!("(Ljava/lang/CharSequence;Landroid/content/DialogInterface$OnClickListener;)Landroid/app/AlertDialog$Builder;"),
        &[JValue::Object(&ok_label), JValue::Object(&null_listener)],
    )?;

    // create() -> AlertDialog
    let dialog = env.call_method(
        &builder,
        jni_str!("create"),
        jni_sig!("()Landroid/app/AlertDialog;"),
        &[],
    )?;
    let dialog_obj = dialog.l()?;

    // show()
    env.call_method(&dialog_obj, jni_str!("show"), jni_sig!("()V"), &[])?;

    // Keep a global ref so the dialog isn't GC'd before the user dismisses it.
    let global_dialog = env.new_global_ref(dialog_obj)?;

    // Poll until the 对话框 is dismissed (e.g. user tapped OK) or the activity finishes.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let is_showing = env
            .call_method(&global_dialog, jni_str!("isShowing"), jni_sig!("()Z"), &[])
            .and_then(|v| v.z())
            .unwrap_or(false);
        if !is_showing {
            break;
        }
        let is_finishing = env
            .call_method(activity, jni_str!("isFinishing"), jni_sig!("()Z"), &[])
            .and_then(|v| v.z())
            .unwrap_or(true);
        if is_finishing {
            break;
        }
    }

    let (lock, cvar) = &**done;
    let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    *guard = true;
    cvar.notify_one();

    // Suppress "unused" — the JNI return code of show() is void.
    let _: jint = JNI_OK;
    Ok(())
}

/// 复制 `text` to Android 系统 clipboard via `ClipboardManager` and `ClipData`.
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let handles = match ANDROID_APP.read() {
        Ok(guard) => match *guard {
            Some(h) => h,
            None => return Ok(()),
        },
        Err(_) => return Ok(()),
    };
    let vm = unsafe { JavaVM::from_raw(handles.vm_ptr as *mut _) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.attach_current_thread(|env| {
            let activity = unsafe { JObject::from_raw(env, handles.activity_ptr as *mut _) };
            copy_to_clipboard_jni(env, &activity, text)
        })
    }));
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            log::warn!("Android clipboard JNI failed: {e}");
            Ok(())
        }
        Err(p) => {
            log::warn!("Android clipboard JNI panic: {p:?}");
            Ok(())
        }
    }
}

fn copy_to_clipboard_jni(
    env: &mut jni::Env<'_>,
    activity: &JObject,
    text: &str,
) -> jni::errors::Result<()> {
    let service_name = env.new_string("clipboard")?;
    let clipboard_manager = env
        .call_method(
            activity,
            jni_str!("getSystemService"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&service_name)],
        )?
        .l()?;

    if clipboard_manager.as_raw().is_null() {
        return Ok(());
    }

    let clip_data_class = env.find_class(jni_str!("android/content/ClipData"))?;
    let label = env.new_string("PrismaRev Crash Log")?;
    let text_j = env.new_string(text)?;
    let clip = env
        .call_static_method(
            clip_data_class,
            jni_str!("newPlainText"),
            jni_sig!(
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;"
            ),
            &[JValue::Object(&label), JValue::Object(&text_j)],
        )?
        .l()?;

    env.call_method(
        &clipboard_manager,
        jni_str!("setPrimaryClip"),
        jni_sig!("(Landroid/content/ClipData;)V"),
        &[JValue::Object(&clip)],
    )?;
    Ok(())
}
