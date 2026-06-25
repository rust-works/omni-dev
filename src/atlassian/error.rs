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

    /// A JIRA write returned HTTP 400 because one or more fields require
    /// rich-text content in ADF format (e.g. `customfield_19300`) but the
    /// caller submitted a plain string. The multi-line `Display` matches the
    /// format requested in issue #867: a header line naming the offending
    /// field(s), a `To fix:` line pointing at JFM / raw-ADF inputs, and an
    /// `Original API error:` line preserving JIRA's verbatim wording.
    #[error("{}", format_jira_adf_field_required(fields, original_message))]
    JiraAdfFieldRequired {
        /// Stable JIRA field IDs (e.g. `customfield_19300`) whose error
        /// message indicated they require an ADF document.
        fields: Vec<String>,
        /// Verbatim message from JIRA's `errors.<field>` entry — preserved
        /// so the `Original API error:` line shows what JIRA actually said
        /// (and we degrade gracefully if Atlassian changes the wording).
        original_message: String,
        /// Raw response body (kept for callers that want to log it).
        body: String,
    },
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
        AdfSchemaViolation::ForbiddenMarkCombination {
            mark_type,
            conflicts_with,
            ..
        } => format!(
            "Diagnosis: the submitted ADF combines the `{mark_type}` and `{conflicts_with}` marks on one text run, which ADF does not allow."
        ),
    };
    let mut out = format!("{header}\n{diag_line}");
    if let Some(hint) = hint {
        out.push_str("\nHint: ");
        out.push_str(hint);
    }
    out
}

fn format_jira_adf_field_required(fields: &[String], original_message: &str) -> String {
    let header = match fields {
        [] => "JIRA fields require rich-text content in ADF format.".to_string(),
        [one] => format!("Field `{one}` requires rich-text content in ADF format."),
        many => {
            let joined = many
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("Fields {joined} require rich-text content in ADF format.")
        }
    };
    let hint = "\n\nTo fix: pass the value as a JFM markdown string \
                (it will be auto-converted to ADF), or pass a raw ADF \
                document object. See `omni-dev://specs/jfm` for JFM syntax.";
    let original = if original_message.is_empty() {
        String::new()
    } else {
        format!("\n\nOriginal API error: \"{original_message}\"")
    };
    format!("{header}{hint}{original}")
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

    #[test]
    fn api_request_failed_with_diagnosis_display_for_forbidden_mark_combination() {
        let err = AtlassianError::ApiRequestFailedWithDiagnosis {
            body: String::new(),
            diagnosis: AdfSchemaViolation::ForbiddenMarkCombination {
                mark_type: "strong".to_string(),
                conflicts_with: "code".to_string(),
                parent_type: "paragraph".to_string(),
                inline_index: Some(0),
                path: vec![0, 0],
            },
            hint: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("`strong`"), "got: {msg}");
        assert!(msg.contains("`code`"), "got: {msg}");
        assert!(msg.contains("does not allow"), "got: {msg}");
    }

    #[test]
    fn jira_adf_field_required_display_single_field() {
        let err = AtlassianError::JiraAdfFieldRequired {
            fields: vec!["customfield_19300".to_string()],
            original_message:
                "Operation value must be an Atlassian Document (see the Atlassian Document Format)"
                    .to_string(),
            body: "{}".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Field `customfield_19300`"), "got: {msg}");
        assert!(
            msg.contains("requires rich-text content in ADF format"),
            "got: {msg}"
        );
        assert!(msg.contains("To fix:"), "got: {msg}");
        assert!(msg.contains("JFM markdown"), "got: {msg}");
        assert!(msg.contains("omni-dev://specs/jfm"), "got: {msg}");
        assert!(msg.contains("Original API error:"), "got: {msg}");
        assert!(
            msg.contains("Operation value must be an Atlassian Document"),
            "got: {msg}"
        );
    }

    #[test]
    fn jira_adf_field_required_display_multiple_fields() {
        let err = AtlassianError::JiraAdfFieldRequired {
            fields: vec![
                "customfield_19300".to_string(),
                "customfield_42000".to_string(),
            ],
            original_message: "Operation value must be an Atlassian Document".to_string(),
            body: String::new(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Fields `customfield_19300`, `customfield_42000`"),
            "got: {msg}"
        );
        assert!(msg.contains("require rich-text content"), "got: {msg}");
    }

    #[test]
    fn jira_adf_field_required_display_no_fields_uses_generic_header() {
        // The `jira_write_error` helper never constructs the variant with an
        // empty `fields` vec, but the defensive `[]` arm of the formatter is
        // public surface — direct construction must still render sensibly.
        let err = AtlassianError::JiraAdfFieldRequired {
            fields: vec![],
            original_message: "Operation value must be an Atlassian Document".to_string(),
            body: String::new(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("JIRA fields require rich-text content in ADF format."),
            "got: {msg}"
        );
        assert!(!msg.contains("Field `"), "got: {msg}");
    }

    #[test]
    fn jira_adf_field_required_display_omits_original_when_empty() {
        let err = AtlassianError::JiraAdfFieldRequired {
            fields: vec!["customfield_19300".to_string()],
            original_message: String::new(),
            body: String::new(),
        };
        let msg = err.to_string();
        assert!(!msg.contains("Original API error:"), "got: {msg}");
    }
}
