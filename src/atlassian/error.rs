//! Error types for Atlassian operations.

use thiserror::Error;

/// Errors that can occur during Atlassian operations.
#[derive(Error, Debug)]
pub enum AtlassianError {
    /// Atlassian credentials are not configured.
    #[error("Atlassian credentials not configured. Run `omni-dev atlassian auth login`")]
    CredentialsNotFound,

    /// An Atlassian API request failed.
    #[error("Atlassian API request failed: HTTP {status}: {body}")]
    ApiRequestFailed {
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },

    /// The JFM document is invalid or malformed.
    #[error("Invalid JFM document: {0}")]
    InvalidDocument(String),

    /// An error occurred during ADF conversion.
    #[error("ADF conversion error: {0}")]
    ConversionError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_not_found_display() {
        let err = AtlassianError::CredentialsNotFound;
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn api_request_failed_display() {
        let err = AtlassianError::ApiRequestFailed {
            status: 404,
            body: "Not Found".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("404"));
        assert!(msg.contains("Not Found"));
    }

    #[test]
    fn invalid_document_display() {
        let err = AtlassianError::InvalidDocument("bad format".to_string());
        assert!(err.to_string().contains("bad format"));
    }

    #[test]
    fn conversion_error_display() {
        let err = AtlassianError::ConversionError("oops".to_string());
        assert!(err.to_string().contains("oops"));
    }
}
