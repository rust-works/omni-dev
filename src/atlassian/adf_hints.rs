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

use crate::atlassian::adf_attr_schema::AttrProblem;
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
        AdfSchemaViolation::MissingAttr {
            node_type,
            attr_name,
            ..
        } => missing_attr_hint(node_type, attr_name),
        AdfSchemaViolation::InvalidAttr {
            node_type,
            attr_name,
            problem,
            ..
        } => invalid_attr_hint(node_type, attr_name, problem),
        AdfSchemaViolation::DisallowedMark {
            mark_type,
            parent_type,
            ..
        } => disallowed_mark_hint(mark_type, parent_type),
        AdfSchemaViolation::InvalidMarkAttr {
            mark_type,
            attr_name,
            problem,
            ..
        } => invalid_mark_attr_hint(mark_type, attr_name, problem),
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

fn missing_attr_hint(node_type: &str, attr_name: &str) -> Option<&'static str> {
    match (node_type, attr_name) {
        ("panel", "panelType") => {
            Some("set panelType to one of: info, note, warning, success, error, custom")
        }
        ("heading", "level") => Some("set level to an integer in 1..=6"),
        ("taskItem", "state") => Some("set state to TODO or DONE"),
        ("decisionItem", "state") => Some("set state to DECIDED or UNDECIDED"),
        ("media", "type") => Some("set type to file, link, or external"),
        ("status", "color") => {
            Some("set color to one of: neutral, purple, blue, red, yellow, green")
        }
        (_, "localId") => {
            Some("set localId to a unique identifier (typically a UUID) for this node")
        }
        _ => None,
    }
}

fn invalid_attr_hint(
    node_type: &str,
    attr_name: &str,
    problem: &AttrProblem,
) -> Option<&'static str> {
    match (node_type, attr_name, problem) {
        ("panel", "panelType", AttrProblem::NotInEnum { .. }) => {
            Some("valid panelType values are: info, note, warning, success, error, custom")
        }
        ("heading", "level", AttrProblem::OutOfRange { .. }) => {
            Some("heading.level must be an integer in 1..=6")
        }
        ("media", "type", AttrProblem::NotInEnum { .. }) => {
            Some("media.type must be one of: file, link, external")
        }
        ("layoutColumn", "width", _) => {
            Some("layoutColumn.width must be a number in 0..=100 (percentage)")
        }
        (_, _, AttrProblem::BadFormat { reason }) if reason.contains("URL") => {
            Some("URL attributes must be absolute (e.g. https://…)")
        }
        _ => None,
    }
}

fn disallowed_mark_hint(mark_type: &str, parent_type: &str) -> Option<&'static str> {
    match (mark_type, parent_type) {
        ("code", "heading") => {
            Some("headings cannot carry the `code` mark — use a paragraph if you need code styling")
        }
        (_, "codeBlock") => Some("codeBlock text is literal and accepts no marks; remove the mark"),
        ("border" | "backgroundColor", _)
            if parent_type != "tableCell" && parent_type != "tableHeader" =>
        {
            Some("border/backgroundColor block marks are accepted on tableCell/tableHeader only")
        }
        ("alignment" | "indentation", _)
            if parent_type != "paragraph" && parent_type != "heading" =>
        {
            Some("alignment/indentation block marks are accepted on paragraph/heading only")
        }
        _ => None,
    }
}

fn invalid_mark_attr_hint(
    mark_type: &str,
    attr_name: &str,
    problem: &AttrProblem,
) -> Option<&'static str> {
    match (mark_type, attr_name, problem) {
        ("link", "href", AttrProblem::BadFormat { .. }) => {
            Some("link.href must be an absolute URL (e.g. https://example.com/page)")
        }
        ("subsup", "type", AttrProblem::NotInEnum { .. }) => {
            Some("subsup.type must be either 'sub' or 'sup'")
        }
        ("border", "size", AttrProblem::OutOfRange { .. }) => {
            Some("border.size must be an integer in 1..=3")
        }
        ("indentation", "level", AttrProblem::OutOfRange { .. }) => {
            Some("indentation.level must be an integer in 1..=6")
        }
        ("alignment", "align", AttrProblem::NotInEnum { .. }) => {
            Some("alignment.align must be one of: start, end, center, right, left")
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
