//! Fallback crash dialog for platforms without a native dialog backend.
//!
//! Prints the error to stderr and returns `Exit`. No clipboard.

use super::CrashChoice;

pub fn show(title: &str, message: &str) -> CrashChoice {
    eprintln!("\n=== {title} ===\n{message}\n");
    CrashChoice::Exit
}

pub fn copy_to_clipboard(_text: &str) -> std::io::Result<()> {
    Ok(())
}
