//! Error type for the clean-room Snowflake client.

/// Result alias for the clean-room Snowflake client.
pub type Result<T> = std::result::Result<T, Error>;

/// An error from the clean-room Snowflake client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP transport failure.
    #[error("snowflake transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server returned an unsuccessful response (`success: false`).
    #[error("snowflake server error ({code}): {message}")]
    Server {
        /// The Snowflake response `code` (empty string when absent).
        code: String,
        /// The Snowflake response `message`.
        message: String,
    },

    /// The session token has expired; renew it or re-authenticate.
    #[error("snowflake session expired")]
    SessionExpired,

    /// A request or response body could not be (de)serialized as expected.
    #[error("snowflake protocol error: {0}")]
    Protocol(String),

    /// Authentication failed (external-browser SSO, PAT, or key-pair JWT).
    #[error("snowflake auth error: {0}")]
    Auth(String),

    /// A result feature the client does not yet implement was encountered.
    #[error("unsupported snowflake feature: {0}")]
    Unsupported(String),

    /// Local I/O failure (e.g. the SSO callback listener or browser launch).
    #[error("snowflake io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether this is a session-expiry error (drives evict + re-auth).
    #[must_use]
    pub fn is_session_expired(&self) -> bool {
        matches!(self, Self::SessionExpired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_session_expired_only_for_session_expired() {
        assert!(Error::SessionExpired.is_session_expired());
        assert!(!Error::Auth("x".into()).is_session_expired());
        assert!(!Error::Unsupported("arrow".into()).is_session_expired());
        assert!(!Error::Protocol("p".into()).is_session_expired());
        assert!(!Error::Server {
            code: "390112".into(),
            message: "m".into(),
        }
        .is_session_expired());
    }

    #[test]
    fn display_renders_code_and_message() {
        assert_eq!(
            Error::SessionExpired.to_string(),
            "snowflake session expired"
        );
        assert_eq!(
            Error::Server {
                code: "001003".into(),
                message: "boom".into(),
            }
            .to_string(),
            "snowflake server error (001003): boom"
        );
        assert_eq!(
            Error::Unsupported("arrow".into()).to_string(),
            "unsupported snowflake feature: arrow"
        );
    }
}
