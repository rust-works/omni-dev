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
    let mut stdout = std::io::stdout().lock();
    copy_text_with(
        text,
        |t| {
            arboard::Clipboard::new()
                .and_then(|mut c| c.set_text(t.to_owned()))
                .map_err(anyhow::Error::from)
        },
        &mut stdout,
    )
}

/// [`copy_text`] with the system clipboard (`primary`) and the OSC 52 sink
/// (`fallback`) injected — so the fallback path is testable without a
/// display server, and the success path without touching the real
/// clipboard.
pub(crate) fn copy_text_with(
    text: &str,
    primary: impl FnOnce(&str) -> Result<()>,
    fallback: &mut impl Write,
) -> Result<()> {
    if primary(text).is_ok() {
        return Ok(());
    }
    fallback
        .write_all(osc52_sequence(text).as_bytes())
        .and_then(|()| fallback.flush())
        .context("failed to write the OSC 52 clipboard sequence")
}

/// `ESC ] 52 ; c ; <base64> BEL` — the xterm clipboard-set sequence.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_base64_encodes_the_payload() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn a_working_clipboard_means_no_osc52_is_written() {
        let mut sink = Vec::new();
        copy_text_with("hello", |_| Ok(()), &mut sink).unwrap();
        assert!(sink.is_empty());
    }

    #[test]
    fn a_failing_clipboard_falls_back_to_osc52_on_the_sink() {
        let mut sink = Vec::new();
        copy_text_with("hello", |_| Err(anyhow::anyhow!("no display")), &mut sink).unwrap();
        assert_eq!(String::from_utf8(sink).unwrap(), osc52_sequence("hello"));
    }

    struct BrokenSink;

    impl Write for BrokenSink {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("tty gone"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failing_clipboard_and_sink_is_an_error() {
        let err = copy_text_with(
            "hello",
            |_| Err(anyhow::anyhow!("no display")),
            &mut BrokenSink,
        )
        .unwrap_err();
        assert!(err.to_string().contains("OSC 52"));
    }

    /// This module only ever *writes* to the clipboard (`arboard::set_text`,
    /// and the OSC 52 fallback above). That is a deliberate decision, not an
    /// oversight: issue #1602 left *Paste* out of the terminal-grid context
    /// menu specifically because adding a clipboard read would open a
    /// clipboard→PTY byte path that does not exist today — a real
    /// capability change that deserves its own decision rather than a free
    /// ride on a menu item. The emulator is likewise left at
    /// `Osc52::OnlyCopy` (ADR-0072 §4) so a child cannot read the clipboard
    /// either; this guard is the other half of that same guarantee — the UI
    /// itself must not be able to read it either. Modeled on
    /// `no_force_escape_hatch_exists_in_the_ui_surface` in `actions.rs`.
    #[test]
    fn no_host_clipboard_read_exists_in_the_ui_surface() {
        let sources = [
            ("clipboard.rs", include_str!("clipboard.rs")),
            ("app.rs", include_str!("app.rs")),
            ("menu.rs", include_str!("menu.rs")),
            ("popup.rs", include_str!("popup.rs")),
            ("terminal/mod.rs", include_str!("terminal/mod.rs")),
        ];
        for (name, source) in sources {
            // Only production code: tests (including this one) are exempt,
            // since a guard that can't name what it forbids can't test
            // itself, and prose below may discuss the absence of a read.
            let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);
            for (number, line) in code_only.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with("///") {
                    continue; // prose may discuss clipboard reads; code may not perform one
                }
                // `get_text`/`get_contents` are arboard's actual read APIs.
                // `.get(` alone is far too broad (HashMap/Vec lookups), so
                // it only counts when paired with `arboard::Clipboard` on
                // the same line.
                let reads_the_clipboard = code.contains("get_text")
                    || code.contains("get_contents")
                    || code.contains("Clipboard::get")
                    || (code.contains("arboard::Clipboard") && code.contains(".get("));
                assert!(
                    !reads_the_clipboard,
                    "{name}:{}: the UI must never read the host clipboard: {line}",
                    number + 1
                );
            }
        }
    }
}
