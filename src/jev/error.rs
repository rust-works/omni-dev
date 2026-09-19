//! Error types for TypeSafe Jev operations.

use thiserror::Error;

/// Whether `err` is Jev rejecting the credentials (HTTP 401/403).
///
/// No later call in the same run can get past either — shared by `route`
/// and `verify-decision` so both end a batch run at the first auth failure
/// instead of paying for every remaining call.
#[must_use]
pub fn is_auth_failure(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<JevError>(),
        Some(JevError::ApiRequestFailed {
            status: 401 | 403,
            ..
        })
    )
}

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
    fn is_auth_failure_matches_401_and_403() {
        for status in [401, 403] {
            let err = anyhow::Error::new(JevError::ApiRequestFailed {
                status,
                body: "nope".to_string(),
            });
            assert!(is_auth_failure(&err), "status {status}");
        }
    }

    #[test]
    fn is_auth_failure_rejects_other_statuses_and_errors() {
        let err = anyhow::Error::new(JevError::ApiRequestFailed {
            status: 500,
            body: "boom".to_string(),
        });
        assert!(!is_auth_failure(&err));
        assert!(!is_auth_failure(&anyhow::anyhow!("unrelated")));
    }

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
