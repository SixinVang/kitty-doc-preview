//! Clipboard delivery for text selections.
//!
//! Two paths are used together: OSC 52, which the terminal forwards to the
//! system clipboard (and which therefore also works over SSH), and a local
//! helper (`pbcopy`, `wl-copy`, `xclip`, `xsel`) for terminals that ignore
//! OSC 52 writes. Both are best effort; copying never fails the view.

use base64::Engine as _;
use std::io::Write;
use std::process::{Command, Stdio};

/// Queue the OSC 52 write into `out` and try a local helper. Returns whether
/// the local helper accepted the text (OSC 52 is always emitted).
pub fn copy(text: &str, out: &mut Vec<u8>) -> bool {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    out.extend_from_slice(format!("\x1b]52;c;{encoded}\x1b\\").as_bytes());

    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        // A remote pbcopy/xclip would write the wrong machine's clipboard.
        return false;
    }
    local_helper(text)
}

fn local_helper(text: &str) -> bool {
    for (cmd, args) in helpers() {
        let child = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn helpers() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    {
        &[("pbcopy", &[])]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["-b"]),
        ]
    }
}
