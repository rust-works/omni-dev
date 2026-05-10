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

    // ── disallowed_child_hint coverage ──────────────────────────────────
    //
    // One assertion per match arm so a future arm-removal regression fails
    // here loudly instead of silently dropping user-facing guidance.

    fn dc(parent: &str, child: &str) -> AdfSchemaViolation {
        AdfSchemaViolation::DisallowedChild {
            parent_type: parent.to_string(),
            child_type: child.to_string(),
            path: vec![0],
        }
    }

    #[test]
    fn disallowed_child_hint_arms_all_match() {
        let cases: &[(&str, &str, &str)] = &[
            ("panel", "expand", "invert the nesting"),
            ("panel", "nestedExpand", "invert the nesting"),
            ("expand", "expand", "expand cannot contain another expand"),
            (
                "expand",
                "nestedExpand",
                "expand cannot contain another expand",
            ),
            (
                "nestedExpand",
                "expand",
                "nestedExpand cannot contain another expand",
            ),
            (
                "nestedExpand",
                "nestedExpand",
                "nestedExpand cannot contain another expand",
            ),
            (
                "tableCell",
                "expand",
                "table cells permit nestedExpand only",
            ),
            (
                "tableHeader",
                "expand",
                "table cells permit nestedExpand only",
            ),
            (
                "blockquote",
                "expand",
                "blockquote does not allow this child",
            ),
            (
                "blockquote",
                "nestedExpand",
                "blockquote does not allow this child",
            ),
            (
                "blockquote",
                "panel",
                "blockquote does not allow this child",
            ),
            (
                "blockquote",
                "table",
                "blockquote does not allow this child",
            ),
        ];
        for (parent, child, needle) in cases {
            let h = hint_for(&dc(parent, child))
                .unwrap_or_else(|| panic!("expected hint for ({parent}, {child})"));
            assert!(h.contains(needle), "({parent},{child}) got: {h}");
        }
    }

    // ── arity_hint coverage ────────────────────────────────────────────

    fn ar(parent: &str, q: Quantifier, actual: usize) -> AdfSchemaViolation {
        AdfSchemaViolation::Arity {
            parent_type: parent.to_string(),
            atoms: vec!["x"],
            expected: q,
            actual,
            path: vec![0],
        }
    }

    #[test]
    fn arity_hint_arms_all_match() {
        let cases: &[(&str, Quantifier, usize, &str)] = &[
            ("bulletList", Quantifier::OneOrMore, 0, "at least one item"),
            ("orderedList", Quantifier::OneOrMore, 0, "at least one item"),
            (
                "mediaSingle",
                Quantifier::Exactly(1),
                0,
                "exactly one media child",
            ),
            ("mediaSingle", Quantifier::Exactly(1), 2, "mediaGroup"),
            ("mediaGroup", Quantifier::OneOrMore, 0, "at least one media"),
            ("table", Quantifier::OneOrMore, 0, "at least one row"),
            ("tableRow", Quantifier::OneOrMore, 0, "at least one cell"),
            (
                "layoutSection",
                Quantifier::Range(2, 3),
                1,
                "2 or 3 columns",
            ),
            (
                "layoutSection",
                Quantifier::Range(2, 3),
                4,
                "at most 3 columns",
            ),
        ];
        for (parent, q, actual, needle) in cases {
            let h = hint_for(&ar(parent, q.clone(), *actual))
                .unwrap_or_else(|| panic!("expected hint for ({parent}, {q:?}, {actual})"));
            assert!(h.contains(needle), "({parent},{q:?},{actual}) got: {h}");
        }
    }

    #[test]
    fn arity_hint_returns_none_for_unknown_parent() {
        assert!(hint_for(&ar("madeUp", Quantifier::OneOrMore, 0)).is_none());
    }

    // ── missing_attr_hint coverage ─────────────────────────────────────

    fn ma(node: &str, attr: &str) -> AdfSchemaViolation {
        AdfSchemaViolation::MissingAttr {
            node_type: node.to_string(),
            attr_name: attr.to_string(),
            path: vec![0],
        }
    }

    #[test]
    fn missing_attr_hint_arms_all_match() {
        let cases: &[(&str, &str, &str)] = &[
            ("panel", "panelType", "panelType to one of"),
            ("heading", "level", "1..=6"),
            ("taskItem", "state", "TODO or DONE"),
            ("decisionItem", "state", "DECIDED or UNDECIDED"),
            ("media", "type", "file, link, or external"),
            ("status", "color", "neutral, purple, blue"),
            ("anyNode", "localId", "unique identifier"),
        ];
        for (node, attr, needle) in cases {
            let h = hint_for(&ma(node, attr))
                .unwrap_or_else(|| panic!("expected hint for ({node}, {attr})"));
            assert!(h.contains(needle), "({node},{attr}) got: {h}");
        }
    }

    #[test]
    fn missing_attr_hint_returns_none_for_unknown_pair() {
        assert!(hint_for(&ma("future", "unknown")).is_none());
    }

    // ── invalid_attr_hint coverage ─────────────────────────────────────

    fn ia(node: &str, attr: &str, problem: AttrProblem) -> AdfSchemaViolation {
        AdfSchemaViolation::InvalidAttr {
            node_type: node.to_string(),
            attr_name: attr.to_string(),
            problem,
            path: vec![0],
        }
    }

    #[test]
    fn invalid_attr_hint_panel_panel_type_enum() {
        let v = ia(
            "panel",
            "panelType",
            AttrProblem::NotInEnum {
                allowed: vec!["info"],
                actual: "purple".to_string(),
            },
        );
        assert!(hint_for(&v).unwrap().contains("info, note, warning"));
    }

    #[test]
    fn invalid_attr_hint_heading_level_range() {
        let v = ia(
            "heading",
            "level",
            AttrProblem::OutOfRange {
                lo: 1,
                hi: 6,
                actual: 7,
            },
        );
        assert!(hint_for(&v).unwrap().contains("1..=6"));
    }

    #[test]
    fn invalid_attr_hint_media_type_enum() {
        let v = ia(
            "media",
            "type",
            AttrProblem::NotInEnum {
                allowed: vec!["file"],
                actual: "video".to_string(),
            },
        );
        assert!(hint_for(&v).unwrap().contains("file, link, external"));
    }

    #[test]
    fn invalid_attr_hint_layout_column_width_any_problem() {
        // The wildcard `_` for problem means any problem matches. Try one
        // shape just to confirm the arm fires.
        let v = ia(
            "layoutColumn",
            "width",
            AttrProblem::OutOfRangeF {
                lo: 0.0,
                hi: 100.0,
                actual: 200.0,
            },
        );
        assert!(hint_for(&v).unwrap().contains("0..=100"));
    }

    #[test]
    fn invalid_attr_hint_url_bad_format_fallback() {
        // BadFormat with reason containing "URL" hits the catch-all arm.
        let v = ia(
            "anyNode",
            "anyAttr",
            AttrProblem::BadFormat {
                reason: "not a valid URL",
            },
        );
        assert!(hint_for(&v).unwrap().contains("absolute"));
    }

    #[test]
    fn invalid_attr_hint_returns_none_for_unknown_combination() {
        assert!(hint_for(&ia(
            "future",
            "unknown",
            AttrProblem::WrongType { expected: "string" }
        ))
        .is_none());
    }

    // ── disallowed_mark_hint coverage ──────────────────────────────────

    fn dm(mark: &str, parent: &str) -> AdfSchemaViolation {
        AdfSchemaViolation::DisallowedMark {
            mark_type: mark.to_string(),
            parent_type: parent.to_string(),
            inline_index: None,
            path: vec![0],
        }
    }

    #[test]
    fn disallowed_mark_hint_code_on_heading() {
        assert!(hint_for(&dm("code", "heading"))
            .unwrap()
            .contains("headings cannot carry"));
    }

    #[test]
    fn disallowed_mark_hint_any_mark_on_code_block() {
        assert!(hint_for(&dm("strong", "codeBlock"))
            .unwrap()
            .contains("codeBlock text is literal"));
    }

    #[test]
    fn disallowed_mark_hint_border_on_paragraph() {
        // border on a non-table-cell parent → the per-mark guard arm.
        assert!(hint_for(&dm("border", "paragraph"))
            .unwrap()
            .contains("tableCell/tableHeader only"));
    }

    #[test]
    fn disallowed_mark_hint_background_color_on_paragraph() {
        assert!(hint_for(&dm("backgroundColor", "paragraph"))
            .unwrap()
            .contains("tableCell/tableHeader only"));
    }

    #[test]
    fn disallowed_mark_hint_alignment_on_table_cell() {
        // alignment on something other than paragraph/heading → block-mark arm.
        assert!(hint_for(&dm("alignment", "tableCell"))
            .unwrap()
            .contains("paragraph/heading only"));
    }

    #[test]
    fn disallowed_mark_hint_indentation_on_table_cell() {
        assert!(hint_for(&dm("indentation", "tableCell"))
            .unwrap()
            .contains("paragraph/heading only"));
    }

    #[test]
    fn disallowed_mark_hint_returns_none_for_unknown_combination() {
        assert!(hint_for(&dm("madeUpMark", "paragraph")).is_none());
    }

    // ── invalid_mark_attr_hint coverage ─────────────────────────────────

    fn ima(mark: &str, attr: &str, problem: AttrProblem) -> AdfSchemaViolation {
        AdfSchemaViolation::InvalidMarkAttr {
            mark_type: mark.to_string(),
            attr_name: attr.to_string(),
            problem,
            inline_index: None,
            path: vec![0],
        }
    }

    #[test]
    fn invalid_mark_attr_hint_link_href_bad_format() {
        let v = ima(
            "link",
            "href",
            AttrProblem::BadFormat {
                reason: "not a valid URL",
            },
        );
        assert!(hint_for(&v).unwrap().contains("absolute URL"));
    }

    #[test]
    fn invalid_mark_attr_hint_subsup_type_enum() {
        let v = ima(
            "subsup",
            "type",
            AttrProblem::NotInEnum {
                allowed: vec!["sub", "sup"],
                actual: "side".to_string(),
            },
        );
        assert!(hint_for(&v).unwrap().contains("sub' or 'sup"));
    }

    #[test]
    fn invalid_mark_attr_hint_border_size_range() {
        let v = ima(
            "border",
            "size",
            AttrProblem::OutOfRange {
                lo: 1,
                hi: 3,
                actual: 5,
            },
        );
        assert!(hint_for(&v).unwrap().contains("1..=3"));
    }

    #[test]
    fn invalid_mark_attr_hint_indentation_level_range() {
        let v = ima(
            "indentation",
            "level",
            AttrProblem::OutOfRange {
                lo: 1,
                hi: 6,
                actual: 9,
            },
        );
        assert!(hint_for(&v).unwrap().contains("1..=6"));
    }

    #[test]
    fn invalid_mark_attr_hint_alignment_align_enum() {
        let v = ima(
            "alignment",
            "align",
            AttrProblem::NotInEnum {
                allowed: vec!["start"],
                actual: "middle".to_string(),
            },
        );
        assert!(hint_for(&v).unwrap().contains("start, end, center"));
    }

    #[test]
    fn invalid_mark_attr_hint_returns_none_for_unknown_combination() {
        assert!(hint_for(&ima(
            "futureMark",
            "futureAttr",
            AttrProblem::WrongType { expected: "string" }
        ))
        .is_none());
    }
}
