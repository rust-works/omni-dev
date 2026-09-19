//! Error types for TypeSafe Jev operations.

use thiserror::Error;

/// Errors that can occur while calling TypeSafe's Jev System One API.
#[derive(Error, Debug)]
pub enum JevError {
    /// No Jev API key was found in the environment or `settings.json`.
    #[error(
        "Jev credentials not configured. Set TYPESAFE_API_KEY (or OMNI_DEV_JEV_API_KEY), \
         or add one to ~/.omni-dev/settings.json"
    )]
    CredentialsNotFound,

    /// A Jev API request failed.
    #[error("Jev API request failed: HTTP {status}: {body}")]
    ApiRequestFailed {
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },

    /// A question specification could not be built into a valid request.
    #[error("Invalid question specification: {0}")]
    InvalidQuestionSpec(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_not_found_display() {
        let err = JevError::CredentialsNotFound;
        let msg = err.to_string();
        assert!(msg.contains("TYPESAFE_API_KEY"));
        assert!(msg.contains("OMNI_DEV_JEV_API_KEY"));
        assert!(msg.contains("settings.json"));
    }

    #[test]
    fn api_request_failed_display() {
        let err = JevError::ApiRequestFailed {
            status: 401,
            body: "Unauthorized".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(msg.contains("Unauthorized"));
    }

    #[test]
    fn invalid_question_spec_display() {
        let err = JevError::InvalidQuestionSpec("missing name".to_string());
        assert!(err.to_string().contains("missing name"));
    }
}
