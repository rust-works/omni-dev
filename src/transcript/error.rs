//! Error types for transcript operations.

use thiserror::Error;

/// Result type alias for transcript operations.
pub type Result<T> = std::result::Result<T, TranscriptError>;

/// Errors that can occur during transcript fetching, parsing, or formatting.
#[derive(Error, Debug)]
pub enum TranscriptError {
    /// The supplied locator (URL, ID) could not be parsed by any source.
    #[error("invalid transcript locator: {0}")]
    InvalidLocator(String),

    /// The media platform returned a response that did not parse as expected.
    #[error("failed to parse response from media platform: {0}")]
    ParseError(String),

    /// No caption track matched the requested language.
    #[error(
        "no caption track for language `{requested}`; available: {}",
        if available.is_empty() { "(none)".to_string() } else { available.join(", ") }
    )]
    LanguageNotFound {
        /// The language code the caller asked for.
        requested: String,
        /// Language codes that *are* available on the media item.
        available: Vec<String>,
    },

    /// An auto-generated (`asr`) track was the only match but the caller did
    /// not opt in via `allow_auto`.
    #[error("only auto-generated captions are available for `{0}`; pass --auto to accept them")]
    AutoCaptionsRequireOptIn(String),

    /// The media platform refused playback (e.g. age-gated, region-locked,
    /// removed, or login-required). Carries the platform's status string so
    /// callers can react to the specific reason rather than a generic HTTP
    /// failure.
    #[error("media platform refused playback: status={status}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    PlayabilityRefused {
        /// Platform-specific status code (e.g. YouTube `LOGIN_REQUIRED`,
        /// `AGE_VERIFICATION_REQUIRED`, `UNPLAYABLE`).
        status: String,
        /// Optional human-readable reason from the platform.
        reason: Option<String>,
    },

    /// An I/O error occurred (e.g. writing transcript to a file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_locator_display() {
        let err = TranscriptError::InvalidLocator("not a youtube url".to_string());
        let msg = err.to_string();
        assert!(msg.contains("invalid transcript locator"));
        assert!(msg.contains("not a youtube url"));
    }

    #[test]
    fn parse_error_display() {
        let err = TranscriptError::ParseError("missing field `videoId`".to_string());
        assert!(err.to_string().contains("missing field"));
    }

    #[test]
    fn language_not_found_with_available() {
        let err = TranscriptError::LanguageNotFound {
            requested: "fr".to_string(),
            available: vec!["en".to_string(), "es".to_string()],
        };
        let msg = err.to_string();
        assert!(msg.contains("`fr`"));
        assert!(msg.contains("en, es"));
    }

    #[test]
    fn language_not_found_with_empty_available() {
        let err = TranscriptError::LanguageNotFound {
            requested: "en".to_string(),
            available: vec![],
        };
        let msg = err.to_string();
        assert!(msg.contains("`en`"));
        assert!(msg.contains("(none)"));
    }

    #[test]
    fn auto_captions_require_opt_in_display() {
        let err = TranscriptError::AutoCaptionsRequireOptIn("en".to_string());
        let msg = err.to_string();
        assert!(msg.contains("auto-generated"));
        assert!(msg.contains("--auto"));
    }

    #[test]
    fn playability_refused_with_reason() {
        let err = TranscriptError::PlayabilityRefused {
            status: "LOGIN_REQUIRED".to_string(),
            reason: Some("Sign in to confirm your age".to_string()),
        };
        let msg = err.to_string();
        assert!(msg.contains("LOGIN_REQUIRED"));
        assert!(msg.contains("Sign in to confirm your age"));
    }

    #[test]
    fn playability_refused_without_reason() {
        let err = TranscriptError::PlayabilityRefused {
            status: "UNPLAYABLE".to_string(),
            reason: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("UNPLAYABLE"));
        assert!(!msg.contains("()"));
    }

    #[test]
    fn io_error_from_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: TranscriptError = io_err.into();
        assert!(matches!(err, TranscriptError::Io(_)));
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn debug_impl_present() {
        let err = TranscriptError::InvalidLocator("x".to_string());
        let dbg = format!("{err:?}");
        assert!(dbg.contains("InvalidLocator"));
    }
}
