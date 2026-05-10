//! Human-readable hints for resolving ADF schema violations.
//!
//! Surfaces a one-line `Hint:` suggestion suitable for the `Diagnosis:` line
//! emitted by [`crate::atlassian::error::AtlassianError::ApiRequestFailedWithDiagnosis`]
//! when Confluence rejects a write with HTTP 500.
//!
//! Hints are advisory: returning `None` means we have no specific remediation
//! to suggest, and the diagnosis line still names the violation. Hints
//! evolve as new violation variants land — the `match` is split per variant
//! so each sub-PR of #733 only touches the variant it owns.

use crate::atlassian::adf_schema::{AdfSchemaViolation, Quantifier};

/// Returns a human-readable hint for the given violation, if one is known.
#[must_use]
pub fn hint_for(violation: &AdfSchemaViolation) -> Option<&'static str> {
    match violation {
        AdfSchemaViolation::DisallowedChild {
            parent_type,
            child_type,
            ..
        } => disallowed_child_hint(parent_type, child_type),
        AdfSchemaViolation::Arity {
            parent_type,
            expected,
            actual,
            ..
        } => arity_hint(parent_type, expected, *actual),
    }
}

fn disallowed_child_hint(parent: &str, child: &str) -> Option<&'static str> {
    match (parent, child) {
        ("panel", "expand" | "nestedExpand") => {
            Some("invert the nesting (panel inside expand) or make them siblings")
        }
        ("expand", "expand" | "nestedExpand") => {
            Some("expand cannot contain another expand; make them siblings instead")
        }
        ("nestedExpand", "expand" | "nestedExpand") => {
            Some("nestedExpand cannot contain another expand; make them siblings instead")
        }
        ("tableCell" | "tableHeader", "expand") => {
            Some("table cells permit nestedExpand only — replace the expand with nestedExpand")
        }
        ("blockquote", "expand" | "nestedExpand" | "panel" | "table") => {
            Some("blockquote does not allow this child; move it outside the blockquote")
        }
        _ => None,
    }
}

fn arity_hint(parent: &str, expected: &Quantifier, actual: usize) -> Option<&'static str> {
    match (parent, expected, actual) {
        ("bulletList" | "orderedList", Quantifier::OneOrMore, 0) => {
            Some("a list must contain at least one item; remove the empty list or add a list item")
        }
        ("mediaSingle", Quantifier::Exactly(1), 0) => {
            Some("mediaSingle must contain exactly one media child; add the media node or remove the wrapper")
        }
        ("mediaSingle", Quantifier::Exactly(1), n) if n > 1 => {
            Some("mediaSingle holds exactly one media; use mediaGroup to bundle multiple media nodes")
        }
        ("mediaGroup", Quantifier::OneOrMore, 0) => {
            Some("mediaGroup must contain at least one media; remove the empty group or add a media node")
        }
        ("table", Quantifier::OneOrMore, 0) => {
            Some("a table must contain at least one row; remove the empty table or add a row")
        }
        ("tableRow", Quantifier::OneOrMore, 0) => {
            Some("a table row must contain at least one cell; add a tableCell or tableHeader")
        }
        ("layoutSection", Quantifier::Range(2, 3), 0 | 1) => {
            Some("layoutSection requires 2 or 3 columns; add layoutColumn nodes to reach the minimum")
        }
        ("layoutSection", Quantifier::Range(2, 3), _) => {
            Some("layoutSection accepts at most 3 columns; remove the extra layoutColumn nodes")
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn returns_hint_for_panel_expand() {
        let v = AdfSchemaViolation::DisallowedChild {
            child_type: "expand".to_string(),
            parent_type: "panel".to_string(),
            path: vec![0, 0],
        };
        assert!(hint_for(&v).is_some());
    }

    #[test]
    fn returns_no_hint_for_unknown_pair() {
        let v = AdfSchemaViolation::DisallowedChild {
            child_type: "foo".to_string(),
            parent_type: "bar".to_string(),
            path: vec![],
        };
        assert!(hint_for(&v).is_none());
    }

    #[test]
    fn returns_hint_for_empty_bullet_list() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "bulletList".to_string(),
            atoms: vec!["listItem"],
            expected: Quantifier::OneOrMore,
            actual: 0,
            path: vec![0],
        };
        let hint = hint_for(&v).expect("hint expected");
        assert!(hint.contains("at least one item"), "got: {hint}");
    }

    #[test]
    fn returns_hint_for_two_media_in_media_single() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "mediaSingle".to_string(),
            atoms: vec!["media"],
            expected: Quantifier::Exactly(1),
            actual: 2,
            path: vec![0],
        };
        let hint = hint_for(&v).expect("hint expected");
        assert!(hint.contains("mediaGroup"), "got: {hint}");
    }

    #[test]
    fn returns_hint_for_layout_section_too_few_columns() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "layoutSection".to_string(),
            atoms: vec!["layoutColumn"],
            expected: Quantifier::Range(2, 3),
            actual: 1,
            path: vec![0],
        };
        assert!(hint_for(&v).is_some());
    }

    #[test]
    fn returns_hint_for_layout_section_too_many_columns() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "layoutSection".to_string(),
            atoms: vec!["layoutColumn"],
            expected: Quantifier::Range(2, 3),
            actual: 4,
            path: vec![0],
        };
        let hint = hint_for(&v).expect("hint expected");
        assert!(hint.contains("at most"), "got: {hint}");
    }
}
