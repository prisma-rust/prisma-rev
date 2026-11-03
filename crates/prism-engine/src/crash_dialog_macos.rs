//! macOS crash dialog: `osascript` (AppleScript `display dialog`) + `pbcopy`.
//!
//! Invokes `/usr/bin/osascript` with a `display dialog ... buttons {"Copy & Exit", "Exit"}`
//! script. `display dialog` is modal and blocks until the user picks a button.
//! `pbcopy` fills the pasteboard with the error text.

use std::io::Write;
use std::process::Command;

use super::CrashChoice;

pub fn show(title: &str, message: &str) -> CrashChoice {
    // Escape double-quotes and backslashes for the AppleScript string literal.
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let title_e = esc(title);
    // Replace literal newlines with AppleScript newline concatenation so the
    // dialog shows real line breaks instead of a single run-on line.
    let msg_e = esc(message).replace('\n', "\" & return & \"");

    // "Copy & Exit" is the default (first button). `giving up after 0` means
    // "no timeout" here is not used; we let it block.
    let script = format!(
        "display dialog \"{msg_e}\" with title \"{title_e}\" \
         buttons {{\"Copy & Exit\", \"Exit\"}} default button \"Copy & Exit\" \
         with icon stop"
    );

    match run_osascript(&script) {
        Ok(output) => {
            // `osascript` prints `button returned:Copy & Exit` to stdout.
            if output.contains("Copy & Exit") {
                CrashChoice::CopyAndExit
            } else {
                CrashChoice::Exit
            }
        }
        Err(e) => {
            log::warn!("osascript dialog failed ({e}); falling back to plain exit");
            CrashChoice::Exit
        }
    }
}

fn run_osascript(script: &str) -> std::io::Result<String> {
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(std::io::Error::other("pbcopy exited non-zero"));
    }
    Ok(())
}
