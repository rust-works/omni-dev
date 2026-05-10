//! Error types for Atlassian operations.

use thiserror::Error;

use crate::atlassian::adf_schema::AdfSchemaViolation;
use crate::atlassian::adf_validated::AdfValidationError;

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
    /// line, a `Diagnosis:` line naming the offending nesting or arity error,
    /// and an optional `Hint:` line. The raw response body is intentionally
    /// omitted from the user-facing message — it is already logged at `debug!`
    /// by the call site.
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

    /// The converted ADF document violates Confluence's nesting constraints.
    #[error("{0}")]
    InvalidAdfNesting(#[from] AdfValidationError),
}

fn format_diagnosis(diagnosis: &AdfSchemaViolation, hint: Option<&str>) -> String {
    let header = "Confluence API returned HTTP 500 (Internal Server Error)";
    let diag_line = match diagnosis {
        AdfSchemaViolation::DisallowedChild {
            child_type,
            parent_type,
            ..
        } => format!(
            "Diagnosis: the submitted ADF contains `{child_type}` nested inside `{parent_type}` \
             (not allowed by Confluence's content model)."
        ),
        AdfSchemaViolation::Arity { .. } => {
            format!("Diagnosis: the submitted ADF has an arity violation — {diagnosis}.")
        }
        AdfSchemaViolation::MissingAttr {
            node_type,
            attr_name,
            ..
        } => format!(
            "Diagnosis: the submitted ADF's `{node_type}` is missing required attribute `{attr_name}`."
        ),
        AdfSchemaViolation::InvalidAttr {
            node_type,
            attr_name,
            problem,
            ..
        } => format!(
            "Diagnosis: the submitted ADF's `{node_type}.{attr_name}` is invalid — {problem}."
        ),
        AdfSchemaViolation::DisallowedMark {
            mark_type,
            parent_type,
            ..
        } => format!(
            "Diagnosis: the submitted ADF carries a `{mark_type}` mark on `{parent_type}` which is not permitted in that context."
        ),
        AdfSchemaViolation::InvalidMarkAttr {
            mark_type,
            attr_name,
            problem,
            ..
        } => format!(
            "Diagnosis: the submitted ADF's `{mark_type}` mark has invalid `{attr_name}` — {problem}."
        ),
    };
    let mut out = format!("{header}\n{diag_line}");
    if let Some(hint) = hint {
        out.push_str("\nHint: ");
        out.push_str(hint);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlassian::adf_schema::Quantifier;

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
            diagnosis: AdfSchemaViolation::DisallowedChild {
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
            diagnosis: AdfSchemaViolation::DisallowedChild {
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

    #[test]
    fn invalid_adf_nesting_display_includes_violations() {
        let err = AtlassianError::InvalidAdfNesting(AdfValidationError {
            violations: vec![AdfSchemaViolation::DisallowedChild {
                parent_type: "panel".to_string(),
                child_type: "expand".to_string(),
                path: vec![0, 0],
            }],
        });
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
        assert!(msg.contains("hint: invert the nesting"));
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_for_arity() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::Arity {
                parent_type: "bulletList".to_string(),
                atoms: vec!["listItem"],
                expected: Quantifier::OneOrMore,
                actual: 0,
                path: vec![1],
            },
            hint: Some("a list must contain at least one item".to_string()),
        };
        let msg = err.to_string();
        assert!(msg.contains("Confluence API returned HTTP 500 (Internal Server Error)"));
        assert!(msg.contains("arity violation"), "got: {msg}");
        assert!(msg.contains("'bulletList'"), "got: {msg}");
        assert!(msg.contains("at least one"), "got: {msg}");
        assert!(msg.contains("Hint: a list must contain"), "got: {msg}");
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_for_missing_attr() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::MissingAttr {
                node_type: "panel".to_string(),
                attr_name: "panelType".to_string(),
                path: vec![0],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`panel`"), "got: {msg}");
        assert!(msg.contains("missing required attribute"), "got: {msg}");
        assert!(msg.contains("`panelType`"), "got: {msg}");
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_for_invalid_attr() {
        use crate::atlassian::adf_attr_schema::AttrProblem;
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::InvalidAttr {
                node_type: "heading".to_string(),
                attr_name: "level".to_string(),
                problem: AttrProblem::OutOfRange {
                    lo: 1,
                    hi: 6,
                    actual: 7,
                },
                path: vec![0],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`heading.level`"), "got: {msg}");
        assert!(msg.contains("invalid"), "got: {msg}");
        assert!(msg.contains("[1, 6]"), "got: {msg}");
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_for_disallowed_mark() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::DisallowedMark {
                mark_type: "code".to_string(),
                parent_type: "heading".to_string(),
                inline_index: Some(0),
                path: vec![0],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`code` mark"), "got: {msg}");
        assert!(msg.contains("`heading`"), "got: {msg}");
        assert!(msg.contains("not permitted"), "got: {msg}");
    }

    #[test]
    fn api_request_failed_with_diagnosis_display_for_invalid_mark_attr() {
        use crate::atlassian::adf_attr_schema::AttrProblem;
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::InvalidMarkAttr {
                mark_type: "link".to_string(),
                attr_name: "href".to_string(),
                problem: AttrProblem::BadFormat {
                    reason: "not a valid URL",
                },
                inline_index: Some(0),
                path: vec![0],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`link` mark"), "got: {msg}");
        assert!(msg.contains("`href`"), "got: {msg}");
        assert!(msg.contains("not a valid URL"), "got: {msg}");
    }
}
