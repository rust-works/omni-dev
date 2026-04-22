//! Error types for Datadog operations.

use thiserror::Error;

/// Errors that can occur during Datadog operations.
#[derive(Error, Debug)]
pub enum DatadogError {
    /// Datadog credentials are not configured.
    #[error("Datadog credentials not configured. Run `omni-dev datadog auth login`")]
    CredentialsNotFound,

    /// A Datadog API request failed.
    #[error("Datadog API request failed: HTTP {status}: {body}")]
    ApiRequestFailed {
        /// HTTP status code.
        status: u16,
        /// Response body text, optionally suffixed with rate-limit details
        /// when the status was 429.
        body: String,
    },

    /// The configured Datadog site is invalid.
    #[error("Invalid Datadog site: {0}")]
    InvalidSite(String),

    /// A time-range specification could not be parsed.
    #[error("Invalid time range: {0}")]
    InvalidTimeRange(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_not_found_display() {
        let err = DatadogError::CredentialsNotFound;
        assert!(err.to_string().contains("not configured"));
        assert!(err.to_string().contains("datadog auth login"));
    }

    #[test]
    fn api_request_failed_display() {
        let err = DatadogError::ApiRequestFailed {
            status: 401,
            body: "Unauthorized".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(msg.contains("Unauthorized"));
    }

    #[test]
    fn invalid_site_display() {
        let err = DatadogError::InvalidSite("weird.example".to_string());
        assert!(err.to_string().contains("weird.example"));
    }

    #[test]
    fn invalid_time_range_display() {
        let err = DatadogError::InvalidTimeRange("1h30m".to_string());
        assert!(err.to_string().contains("1h30m"));
    }
}
