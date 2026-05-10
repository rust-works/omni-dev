//! Error types for Atlassian operations.

use thiserror::Error;

use crate::atlassian::adf_schema::AdfSchemaViolation;

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

    /// A Confluence write/update/create returned HTTP 500 and the submitted ADF
    /// payload contains a known schema violation that is the likely cause.
    ///
    /// Multi-line `Display` matches the format requested in issue #715: a header
    /// line, a `Diagnosis:` line naming the offending nesting, and an optional
    /// `Hint:` line. The raw response body is intentionally omitted from the
    /// user-facing message — it is already logged at `debug!` by the call site.
    #[error("{}", format_diagnosis(diagnosis, hint.as_deref()))]
    ApiRequestFailedWithDiagnosis {
        /// Raw response body (kept for callers that want to log it).
        body: String,
        /// The first ADF schema violation found in the submitted document.
        diagnosis: AdfSchemaViolation,
        /// Optional human-readable suggestion for resolving the violation.
        hint: Option<String>,
    },

    /// The JFM document is invalid or malformed.
    #[error("Invalid JFM document: {0}")]
    InvalidDocument(String),

    /// An error occurred during ADF conversion.
    #[error("ADF conversion error: {0}")]
    ConversionError(String),
}

fn format_diagnosis(diagnosis: &AdfSchemaViolation, hint: Option<&str>) -> String {
    let mut out = format!(
        "Confluence API returned HTTP 500 (Internal Server Error)\n\
         Diagnosis: the submitted ADF contains `{child}` nested inside `{parent}` \
         (not allowed by Confluence's content model).",
        child = diagnosis.child_type,
        parent = diagnosis.parent_type,
    );
    if let Some(hint) = hint {
        out.push_str("\nHint: ");
        out.push_str(hint);
    }
    out
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

    #[test]
    fn api_request_failed_with_diagnosis_display_with_hint() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: "{}".to_string(),
            diagnosis: AdfSchemaViolation {
                child_type: "expand".to_string(),
                parent_type: "panel".to_string(),
                path: vec![0, 0],
            },
            hint: Some(
                "invert the nesting (panel inside expand) or make them siblings".to_string(),
            ),
        };
        let msg = err.to_string();
        assert!(msg.contains("Confluence API returned HTTP 500 (Internal Server Error)"));
        assert!(msg.contains("Diagnosis:"));
        assert!(msg.contains("`expand`"));
        assert!(msg.contains("`panel`"));
        assert!(msg.contains("Hint: invert the nesting"));
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_without_hint() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation {
                child_type: "table".to_string(),
                parent_type: "nestedExpand".to_string(),
                path: vec![1],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`table`"));
        assert!(msg.contains("`nestedExpand`"));
        assert!(!msg.contains("Hint:"));
    }
}
