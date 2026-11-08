//! Thread scheduling hints for the game loop.
//!
//! The render thread owns the frame deadline; physics, audio decoding and
//! asset polling are assistants that must not compete with it. Raising the
//! render thread (Windows above-normal, macOS user-interactive QoS) and
//! lowering the assistants (Linux/Android `nice + 10`) is enough on a machine
//! with spare cores. With four cores or fewer the threads already share the
//! same cores, so moving priorities around only shuffles the delay and every
//! helper below turns into a no-op.
//!
//! Everything here is best effort: Linux cannot raise a thread's priority
//! without privileges, and lowering one is pointless on Windows and macOS, so
//! a helper returns `false` when it did not ask the OS for anything. Real-time
//! classes (`SCHED_FIFO`, `THREAD_PRIORITY_TIME_CRITICAL`) are deliberately
//! avoided: they need extra privileges and mobile kernels may throttle or kill
//! the process for using them.

/// Cores at which priority hints start to pay off.
///
/// At four cores or fewer the render thread and its assistants are already
/// contending for the same cores, so the hints are skipped.
const MIN_CORES_FOR_HINTS: usize = 5;

/// Number of cores usable by this process, never zero.
pub fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get())
}

/// Ask the OS to schedule the calling thread ahead of ordinary threads.
///
/// Returns `true` when the OS accepted the request. Returns `false` on
/// platforms that cannot raise a thread (Linux, Android), on machines with
/// four cores or fewer, and when the OS rejects the call.
pub fn favour_current() -> bool {
    if !should_adjust(available_parallelism()) {
        return false;
    }
    raise_current()
}

/// Ask the OS to schedule the calling thread behind ordinary threads.
///
/// Returns `true` when the OS accepted the request. Returns `false` on
/// platforms that cannot lower a thread (Windows, macOS), on machines with
/// four cores or fewer, and when the OS rejects the call.
pub fn put_behind_current() -> bool {
    if !should_adjust(available_parallelism()) {
        return false;
    }
    lower_current()
}

fn should_adjust(cores: usize) -> bool {
    cores >= MIN_CORES_FOR_HINTS
}

#[cfg(windows)]
fn raise_current() -> bool {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };

    // SAFETY: `GetCurrentThread` returns a pseudo handle for the calling
    // thread; `SetThreadPriority` only changes that thread's scheduling
    // priority.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) != 0 }
}

#[cfg(target_os = "macos")]
fn raise_current() -> bool {
    use std::ffi::{c_int, c_uint};

    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: c_uint, relative_priority: c_int) -> c_int;
    }

    /// `QOS_CLASS_USER_INTERACTIVE` from `<pthread/qos.h>`.
    const QOS_CLASS_USER_INTERACTIVE: c_uint = 0x21;

    // SAFETY: the function only changes the QoS class of the calling thread.
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) == 0 }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn raise_current() -> bool {
    // Linux and Android keep the default priority: raising it needs
    // CAP_SYS_NICE or a matching RLIMIT_NICE.
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn lower_current() -> bool {
    use std::ffi::c_int;

    extern "C" {
        fn nice(increment: c_int) -> c_int;
    }

    // A relative increment keeps a thread that is already nicer (for example
    // because the shell started the process that way) relatively behind,
    // instead of silently raising it back to an absolute value.
    // SAFETY: `nice` only adjusts the calling thread's nice value.
    unsafe { nice(10) >= 0 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn lower_current() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_parallelism_is_never_zero() {
        assert!(available_parallelism() >= 1);
    }

    #[test]
    fn hints_are_skipped_on_small_machines() {
        for cores in 1..MIN_CORES_FOR_HINTS {
            assert!(!should_adjust(cores), "{cores} cores must not adjust");
        }
        assert!(should_adjust(MIN_CORES_FOR_HINTS));
        assert!(should_adjust(MIN_CORES_FOR_HINTS * 2));
    }

    #[test]
    fn hints_can_be_requested_repeatedly() {
        let first = favour_current();
        let second = favour_current();
        assert_eq!(first, second, "favour_current must be idempotent");
        #[cfg(windows)]
        assert_eq!(
            first,
            available_parallelism() >= MIN_CORES_FOR_HINTS,
            "Windows must accept the above-normal request when there are enough cores"
        );
        let _ = put_behind_current();
        let _ = put_behind_current();
    }
}
