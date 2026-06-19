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

    /// The external-browser SSO flow failed.
    #[error("snowflake external-browser auth error: {0}")]
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
