//! Clipboard writes for the worktrees UI (issue #1585 Phase 3).
//!
//! `arboard` first; when it can't reach a clipboard (no display server, an
//! SSH session) the fallback is an OSC 52 sequence written to the host
//! terminal, which every modern terminal honours and which is exactly what
//! a child's own OSC 52 request would have done had it reached the host
//! directly. Two sources reach this: a child's `Event::ClipboardStore`
//! (passed straight through — no new capability) and the user's explicit
//! `alt-c` on a selection. Nothing here logs the text.

use std::io::Write;

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// Copies `text`, preferring the system clipboard and falling back to
/// OSC 52 on the host terminal.
pub fn copy_text(text: &str) -> Result<()> {
    let via_arboard = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_owned()));
    if via_arboard.is_ok() {
        return Ok(());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(osc52_sequence(text).as_bytes())
        .and_then(|()| stdout.flush())
        .context("failed to write the OSC 52 clipboard sequence")
}

/// `ESC ] 52 ; c ; <base64> BEL` — the xterm clipboard-set sequence.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_base64_encodes_the_payload() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
    }
}
