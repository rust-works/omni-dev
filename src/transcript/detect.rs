//! Source auto-detection: map a locator to the source that recognises it.
//!
//! Backs the provider-less `omni-dev transcript fetch <url>` path (#1187).
//! Each registered source's static [`TranscriptSource::matches`] is probed in
//! registration order; the first match is constructed and returned behind a
//! `Box<dyn TranscriptSource>` for runtime dispatch. Detection is pure string
//! inspection — no network — so an unrecognised locator fails fast before any
//! HTTP.

use crate::transcript::error::{Result, TranscriptError};
use crate::transcript::source::TranscriptSource;
use crate::transcript::sources::youtube::Youtube;

/// Probe every registered source in priority order and construct the first
/// whose [`TranscriptSource::matches`] recognises `url`.
///
/// Returns [`TranscriptError::InvalidLocator`] when no registered source claims
/// the locator.
///
/// Registering a new source is one arm here plus a `pub mod` in
/// [`crate::transcript::sources`] — see the "Adding a new source" recipe in
/// `docs/transcript.md`.
pub fn detect(url: &str) -> Result<Box<dyn TranscriptSource>> {
    if Youtube::matches(url) {
        return Ok(Box::new(Youtube::new()?));
    }

    Err(TranscriptError::InvalidLocator(format!(
        "no transcript source recognises `{url}`"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn detects_youtube_watch_url() {
        let source = detect("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(source.name(), "youtube");
    }

    #[test]
    fn detects_youtube_short_url() {
        let source = detect("https://youtu.be/dQw4w9WgXcQ").unwrap();
        assert_eq!(source.name(), "youtube");
    }

    #[test]
    fn detects_bare_youtube_id() {
        let source = detect("dQw4w9WgXcQ").unwrap();
        assert_eq!(source.name(), "youtube");
    }

    #[test]
    fn unrecognised_locator_is_invalid_locator() {
        // `Box<dyn TranscriptSource>` is not `Debug`, so use `matches!` rather
        // than `unwrap_err` (which needs the `Ok` type to be `Debug`).
        assert!(matches!(
            detect("https://vimeo.com/76979871"),
            Err(TranscriptError::InvalidLocator(msg)) if msg.contains("vimeo.com")
        ));
    }
}
