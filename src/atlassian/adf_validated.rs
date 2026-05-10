//! Type-system enforcement of "validated once before send" for ADF documents.
//!
//! This module ties together:
//! - the upstream-faithful schema validator from
//!   [`crate::atlassian::adf_schema`] (introduced by ADR-0023), and
//! - a [`ValidatedAdfDocument`] newtype whose only fallible constructor runs
//!   that validator.
//!
//! Every API send signature in [`crate::atlassian::client`] and
//! [`crate::atlassian::confluence_api`] accepts `&ValidatedAdfDocument`
//! rather than `&AdfDocument`, which makes "I forgot to validate" a compile
//! error rather than the opaque HTTP 500 from Confluence that motivates
//! issue #714.
//!
//! See ADR-0024 for the wiring rationale and the per-`(parent, child)` hint
//! table surfaced through [`AdfValidationError`]'s `Display` impl.

use std::ops::Deref;

use serde::Serialize;

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::adf_schema::{validate_document, AdfSchemaViolation};

/// One or more nesting violations discovered when validating an
/// [`AdfDocument`] against the upstream content model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdfValidationError {
    /// All violations found, in document order.
    pub violations: Vec<AdfSchemaViolation>,
}

impl std::fmt::Display for AdfValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Build the full message in a String first, then emit it with a
        // single `write!`. That collapses several formatter-`?` branches into
        // one, which keeps coverage tools from flagging each `writeln!` /
        // `write!` site as a partially-covered branch.
        let mut out = String::new();
        for (i, v) in self.violations.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            let path = v
                .path
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("/");
            out.push_str(&format!(
                "invalid ADF nesting — `{}` cannot be a child of `{}` at /{}.\n",
                v.child_type, v.parent_type, path
            ));
            let hint = hint_for(&v.parent_type, &v.child_type).map_or_else(
                || {
                    format!(
                        "hint: restructure the document so `{}` is not a direct child of `{}`.",
                        v.child_type, v.parent_type
                    )
                },
                |h| format!("hint: {h}"),
            );
            out.push_str(&hint);
        }
        f.write_str(&out)
    }
}

impl std::error::Error for AdfValidationError {}

/// Returns the actionable hint for a known forbidden parent → child pair, or
/// `None` when the pair is forbidden by the schema but we have no hand-written
/// remediation guidance for it. The hint table covers the high-traffic
/// combinations called out in issue #714 plus other known-painful cases; the
/// generic fallback in [`AdfValidationError`]'s `Display` impl covers
/// everything else.
fn hint_for(parent: &str, child: &str) -> Option<&'static str> {
    HINTS
        .iter()
        .find(|(p, c, _)| *p == parent && *c == child)
        .map(|(_, _, h)| *h)
}

const HINTS: &[(&str, &str, &str)] = &[
    (
        "panel",
        "expand",
        "invert the nesting (put the panel inside the expand) or use siblings.",
    ),
    (
        "panel",
        "nestedExpand",
        "invert the nesting (put the panel inside the expand) or use siblings.",
    ),
    (
        "panel",
        "panel",
        "panels cannot nest; use siblings or convert one to a blockquote.",
    ),
    (
        "expand",
        "expand",
        "expands cannot nest directly; consider a single expand with sectioned headings.",
    ),
    (
        "expand",
        "nestedExpand",
        "use a plain `expand` at the inner level only inside table cells or layout columns.",
    ),
    (
        "nestedExpand",
        "expand",
        "nestedExpand cannot contain another expand; flatten the structure.",
    ),
    (
        "nestedExpand",
        "nestedExpand",
        "nestedExpand cannot nest; use siblings.",
    ),
    (
        "nestedExpand",
        "panel",
        "move the panel outside the nestedExpand or replace it with a blockquote.",
    ),
    (
        "tableCell",
        "expand",
        "use a `nestedExpand` inside table cells; `expand` is only valid at the top level or inside layout columns.",
    ),
    (
        "tableHeader",
        "expand",
        "use a `nestedExpand` inside table headers; `expand` is only valid at the top level or inside layout columns.",
    ),
    (
        "tableCell",
        "panel",
        "panels are not allowed inside table cells; move the panel outside the table.",
    ),
    (
        "tableHeader",
        "panel",
        "panels are not allowed inside table headers; move the panel outside the table.",
    ),
    (
        "layoutSection",
        "layoutSection",
        "layout sections cannot nest; use sibling sections.",
    ),
    (
        "layoutColumn",
        "layoutSection",
        "a layout column cannot contain another layout section; flatten the structure.",
    ),
    (
        "blockquote",
        "blockquote",
        "blockquotes cannot nest; use a single blockquote with paragraph siblings.",
    ),
    (
        "blockquote",
        "panel",
        "move the panel outside the blockquote.",
    ),
    (
        "blockquote",
        "expand",
        "move the expand outside the blockquote.",
    ),
    (
        "listItem",
        "panel",
        "panels cannot appear inside list items; place the panel outside the list.",
    ),
    (
        "listItem",
        "expand",
        "expands cannot appear inside list items; place the expand outside the list.",
    ),
];

/// Returns `Ok(())` if `doc` has no nesting violations, else an
/// [`AdfValidationError`] listing every violation found.
///
/// Borrows `doc`; use [`ValidatedAdfDocument::try_new`] when the goal is to
/// produce a validated wrapper rather than just check.
///
/// # Errors
///
/// Returns [`AdfValidationError`] when the document violates one or more
/// allowed-children rules in the upstream content model.
pub fn validate(doc: &AdfDocument) -> Result<(), AdfValidationError> {
    let violations = validate_document(doc);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(AdfValidationError { violations })
    }
}

/// An [`AdfDocument`] that has passed nesting validation against the
/// upstream content model.
///
/// Constructing one is the only way to satisfy the type signatures of the
/// API send functions in [`crate::atlassian::client`] and
/// [`crate::atlassian::confluence_api`]. This makes "I forgot to validate"
/// a compile error rather than a runtime opaque-500 error.
///
/// `Deref<Target = AdfDocument>` and a delegated `Serialize` impl let
/// callers continue to use the validated document anywhere a `&AdfDocument`
/// or serialized JSON value is needed.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAdfDocument(AdfDocument);

impl ValidatedAdfDocument {
    /// Validates `doc` against the upstream ADF content model and wraps it
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns [`AdfValidationError`] if `doc` contains any disallowed
    /// nesting per the schema in [`crate::atlassian::adf_schema`].
    pub fn try_new(doc: AdfDocument) -> Result<Self, AdfValidationError> {
        let violations = validate_document(&doc);
        if violations.is_empty() {
            Ok(Self(doc))
        } else {
            Err(AdfValidationError { violations })
        }
    }

    /// Returns a trivially-valid empty document without invoking the
    /// validator. Useful for tests and for code paths that need an
    /// "unset" placeholder.
    #[must_use]
    pub fn empty() -> Self {
        Self(AdfDocument::new())
    }

    /// Test-only constructor that wraps `doc` *without* running the
    /// validator.
    ///
    /// Reserved for tests that need to drive a send function with an
    /// intentionally-invalid document — for example, the HTTP-500 diagnosis
    /// path tests in [`crate::atlassian::confluence_api`] which assert the
    /// post-response diagnoser fires when a violation slips past the local
    /// validator.
    ///
    /// **Never use in production code.** Production callers must go through
    /// [`Self::try_new`] so validation is guaranteed.
    #[cfg(test)]
    #[must_use]
    pub fn trust(doc: AdfDocument) -> Self {
        Self(doc)
    }
}

impl Deref for ValidatedAdfDocument {
    type Target = AdfDocument;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for ValidatedAdfDocument {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::adf::AdfNode;

    fn doc(nodes: Vec<AdfNode>) -> AdfDocument {
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: nodes,
        }
    }

    #[test]
    fn try_new_accepts_clean_document() {
        let d = doc(vec![AdfNode::paragraph(vec![AdfNode::text("ok")])]);
        let v = ValidatedAdfDocument::try_new(d).unwrap();
        assert_eq!(v.content.len(), 1);
    }

    #[test]
    fn try_new_rejects_panel_with_expand() {
        // Issue #714 reproducer.
        let d = doc(vec![AdfNode::panel(
            "info",
            vec![AdfNode::expand(None, vec![])],
        )]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert_eq!(err.violations.len(), 1);
        assert_eq!(err.violations[0].child_type, "expand");
        assert_eq!(err.violations[0].parent_type, "panel");
    }

    #[test]
    fn try_new_rejects_table_cell_with_expand() {
        let d = doc(vec![AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![AdfNode::expand(None, vec![])]),
        ])])]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert!(err
            .violations
            .iter()
            .any(|v| v.child_type == "expand" && v.parent_type == "tableCell"));
    }

    #[test]
    fn try_new_allows_expand_inside_layout_column() {
        let d = doc(vec![AdfNode::layout_section(vec![AdfNode::layout_column(
            100,
            vec![AdfNode::expand(None, vec![])],
        )])]);
        assert!(ValidatedAdfDocument::try_new(d).is_ok());
    }

    #[test]
    fn empty_is_trivially_valid() {
        let v = ValidatedAdfDocument::empty();
        assert!(v.content.is_empty());
    }

    #[test]
    fn serializes_as_inner_adf() {
        let d = doc(vec![AdfNode::paragraph(vec![AdfNode::text("hello")])]);
        let v = ValidatedAdfDocument::try_new(d.clone()).unwrap();
        let v_json = serde_json::to_string(&v).unwrap();
        let d_json = serde_json::to_string(&d).unwrap();
        assert_eq!(v_json, d_json);
    }

    #[test]
    fn deref_exposes_inner_fields() {
        let d = doc(vec![AdfNode::paragraph(vec![])]);
        let v = ValidatedAdfDocument::try_new(d).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.doc_type, "doc");
    }

    #[test]
    fn error_display_includes_path_and_hint_for_known_pair() {
        let d = doc(vec![AdfNode::panel(
            "info",
            vec![AdfNode::expand(None, vec![])],
        )]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
        // adf_schema's path is index-only from the document root; the
        // panel sits at /0 and its expand child at /0/0.
        assert!(msg.contains("at /0/0"));
        assert!(msg.contains("hint: invert the nesting"));
    }

    #[test]
    fn error_display_falls_back_to_generic_hint_for_unknown_pair() {
        // `paragraph → table` is forbidden by the schema but is not in our
        // hand-written hint table; the generic fallback should still give
        // the user something actionable.
        let d = doc(vec![AdfNode::paragraph(vec![AdfNode::table(vec![])])]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF nesting"));
        assert!(msg.contains("`table` cannot be a child of `paragraph`"));
        assert!(msg.contains("hint: restructure the document"));
    }

    #[test]
    fn error_display_separates_multiple_violations() {
        let d = doc(vec![
            AdfNode::panel("info", vec![AdfNode::expand(None, vec![])]),
            AdfNode::blockquote(vec![AdfNode::panel("note", vec![])]),
        ]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert!(err.violations.len() >= 2);
        let msg = err.to_string();
        // Two violations imply a blank-line separator (two consecutive
        // newlines) between them.
        assert!(msg.contains("\n\n"));
    }
}
