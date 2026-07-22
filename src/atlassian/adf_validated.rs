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

use crate::atlassian::adf::{AdfDocument, AdfNode};
use crate::atlassian::adf_schema::{validate_document, AdfSchemaViolation};
use crate::atlassian::convert::markdown_to_adf;

/// A 1-based position in the original JFM markdown source.
///
/// Columns are counted in Unicode scalar values (chars), not bytes, so the
/// reported column matches what an editor's cursor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column (counted in chars).
    pub column: usize,
}

/// Human-facing context resolved for a single violation: the offending node's
/// type, a short excerpt of its text (for Ctrl-F recovery), and — when the
/// original JFM source is available — the 1-based `line:column` of that text.
///
/// Populated by [`AdfValidationError::enriched`]; a [`ViolationContext`] with
/// all-`None` fields means the violation's path could not be resolved to a
/// node carrying text (so nothing extra is printed for it).
#[derive(Debug, Clone, PartialEq, Default)]
struct ViolationContext {
    /// The offending node's `node_type` (e.g. `"text"`, `"paragraph"`).
    node_type: Option<String>,
    /// A short, display-truncated excerpt of the offending run's text.
    excerpt: Option<String>,
    /// 1-based `line:column` of the offending text in the JFM source, when a
    /// source was supplied and the excerpt could be located in it.
    location: Option<SourceLocation>,
}

/// Maximum number of chars shown in a violation's text excerpt before it is
/// truncated with an ellipsis. Long enough to identify the run, short enough
/// to keep the message on one line.
const EXCERPT_MAX_CHARS: usize = 60;

/// Resolves the [`AdfNode`] at `path` (an index path from the document root),
/// or `None` if the path runs off the tree.
fn node_at_path<'a>(doc: &'a AdfDocument, path: &[usize]) -> Option<&'a AdfNode> {
    let mut children = doc.content.as_slice();
    let mut found: Option<&AdfNode> = None;
    for &idx in path {
        let node = children.get(idx)?;
        found = Some(node);
        children = node.content.as_deref().unwrap_or(&[]);
    }
    found
}

/// Concatenates the text of a node and its inline descendants, stopping once
/// [`EXCERPT_MAX_CHARS`] worth of characters have been collected. Used to give
/// block-level violations (which have no `.text` of their own) a locator drawn
/// from their first inline content.
fn collect_text(node: &AdfNode) -> String {
    let mut out = String::new();
    gather_text(node, &mut out);
    out
}

fn gather_text(node: &AdfNode, out: &mut String) {
    if out.chars().count() >= EXCERPT_MAX_CHARS {
        return;
    }
    if let Some(text) = &node.text {
        out.push_str(text);
    }
    if let Some(children) = &node.content {
        for child in children {
            gather_text(child, out);
            if out.chars().count() >= EXCERPT_MAX_CHARS {
                return;
            }
        }
    }
}

/// Truncates `s` to at most [`EXCERPT_MAX_CHARS`] chars, appending `…` when it
/// was shortened. Operates on char boundaries so it never splits a codepoint.
fn truncate_excerpt(s: &str) -> String {
    if s.chars().count() <= EXCERPT_MAX_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(EXCERPT_MAX_CHARS).collect();
    out.push('…');
    out
}

/// Converts a byte offset in `source` to a 1-based `line:column`.
fn offset_to_line_col(source: &str, byte_offset: usize) -> SourceLocation {
    let mut line = 1;
    let mut column = 1;
    for (idx, ch) in source.char_indices() {
        if idx >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    SourceLocation { line, column }
}

/// Locates `needle` (an ADF text-node value) in the JFM `source` and returns
/// its 1-based `line:column`.
///
/// The ADF text is the source run with inline markup stripped (e.g. a code
/// span's backticks are gone), so it is a substring of the source and a direct
/// search lands on the offending run. Reports the **first** occurrence; the
/// excerpt printed alongside disambiguates when the same text repeats.
fn locate_in_source(source: &str, needle: &str) -> Option<SourceLocation> {
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    let byte_offset = source.find(needle)?;
    Some(offset_to_line_col(source, byte_offset))
}

/// Resolves the display context for one violation against `doc` (offending node
/// + excerpt) and, when `source` is `Some`, its `line:column` in the source.
fn resolve_context(
    violation: &AdfSchemaViolation,
    doc: &AdfDocument,
    source: Option<&str>,
) -> ViolationContext {
    let Some(node) = node_at_path(doc, violation.path()) else {
        return ViolationContext::default();
    };
    let node_type = Some(node.node_type.clone());

    let full_text = match &node.text {
        Some(text) => text.clone(),
        None => collect_text(node),
    };
    let full_text = full_text.trim();
    if full_text.is_empty() {
        return ViolationContext {
            node_type,
            ..ViolationContext::default()
        };
    }

    let location = source.and_then(|src| locate_in_source(src, full_text));
    ViolationContext {
        node_type,
        excerpt: Some(truncate_excerpt(full_text)),
        location,
    }
}

/// One or more nesting violations discovered when validating an
/// [`AdfDocument`] against the upstream content model.
//
// `Eq` is intentionally not derived: `AdfSchemaViolation::InvalidAttr`
// carries an `AttrProblem` whose `OutOfRangeF` variant holds `f64`, which
// does not implement `Eq`. `PartialEq` is sufficient for all uses.
#[derive(Debug, Clone, PartialEq)]
pub struct AdfValidationError {
    /// All violations found, in document order.
    pub violations: Vec<AdfSchemaViolation>,
    /// Resolved per-violation source context, parallel to `violations` when
    /// present. Empty when the error was built without a document to resolve
    /// against (e.g. a hand-constructed error in a test); populated by
    /// [`Self::enriched`] so [`Display`](std::fmt::Display) can point at the
    /// offending source location. See issue #1087.
    contexts: Vec<ViolationContext>,
}

impl AdfValidationError {
    /// Builds an error from `violations` with no resolved source context.
    ///
    /// Use [`Self::enriched`] to attach the offending node/excerpt/location.
    #[must_use]
    pub fn new(violations: Vec<AdfSchemaViolation>) -> Self {
        Self {
            violations,
            contexts: Vec::new(),
        }
    }

    /// Resolves each violation's ADF path against `doc` (capturing the
    /// offending node type and a text excerpt) and, when `source` is `Some`,
    /// maps that excerpt to a 1-based `line:column` in the original JFM.
    ///
    /// This turns an opaque ADF index path (e.g. `/38/4/0/1`) into an
    /// actionable message that names the offending run and, for JFM-sourced
    /// documents, where to find it (issue #1087).
    #[must_use]
    fn enriched(mut self, doc: &AdfDocument, source: Option<&str>) -> Self {
        self.contexts = self
            .violations
            .iter()
            .map(|v| resolve_context(v, doc, source))
            .collect();
        self
    }
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
                .path()
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("/");
            match v {
                AdfSchemaViolation::DisallowedChild {
                    child_type,
                    parent_type,
                    ..
                } => {
                    out.push_str(&format!(
                        "invalid ADF nesting — `{child_type}` cannot be a child of `{parent_type}` at /{path}.\n",
                    ));
                    let hint = hint_for(parent_type, child_type).map_or_else(
                        || {
                            format!(
                                "hint: restructure the document so `{child_type}` is not a direct child of `{parent_type}`.",
                            )
                        },
                        |h| format!("hint: {h}"),
                    );
                    out.push_str(&hint);
                }
                AdfSchemaViolation::Arity { .. } => {
                    out.push_str(&format!("invalid ADF nesting — {v}.\n"));
                    out.push_str(
                        "hint: adjust the number of children to match the schema's quantifier.",
                    );
                }
                AdfSchemaViolation::MissingAttr { .. } | AdfSchemaViolation::InvalidAttr { .. } => {
                    out.push_str(&format!("invalid ADF attribute — {v}.\n"));
                    out.push_str("hint: fix the offending attribute on the node before retrying.");
                }
                AdfSchemaViolation::DisallowedMark { .. }
                | AdfSchemaViolation::InvalidMarkAttr { .. } => {
                    out.push_str(&format!("invalid ADF mark — {v}.\n"));
                    out.push_str("hint: remove or correct the offending mark before retrying.");
                }
                AdfSchemaViolation::ForbiddenMarkCombination { .. } => {
                    out.push_str(&format!("invalid ADF mark combination — {v}.\n"));
                    out.push_str(
                        "hint: ADF does not allow these marks on the same text — the `code` (monospace) mark only combines with a link; split the run so each piece carries a single style (e.g. drop the backticks or the surrounding bold/italic).",
                    );
                }
            }
            if let Some(ctx) = self.contexts.get(i) {
                append_context(&mut out, ctx);
            }
        }
        f.write_str(&out)
    }
}

/// Appends the resolved source location and offending-text excerpt for one
/// violation, each on its own indented line, when they are known.
fn append_context(out: &mut String, ctx: &ViolationContext) {
    if let Some(loc) = &ctx.location {
        out.push_str(&format!("\n  at line {}, column {}", loc.line, loc.column));
    }
    if let Some(excerpt) = &ctx.excerpt {
        out.push_str(&format!("\n  in text: {excerpt:?}"));
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
    validate_with_source(doc, None)
}

/// Like [`validate`], but source-aware.
///
/// Keeps `source` (the JFM markdown `doc` was converted from, when applicable)
/// so any violation is reported with the offending run's 1-based `line:column`.
/// Pass `None` when there is no JFM source (e.g. ADF-format input). See issue
/// #1087.
///
/// # Errors
///
/// Returns [`AdfValidationError`] when the document violates one or more
/// allowed-children rules in the upstream content model.
pub fn validate_with_source(
    doc: &AdfDocument,
    source: Option<&str>,
) -> Result<(), AdfValidationError> {
    let violations = validate_document(doc);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(AdfValidationError::new(violations).enriched(doc, source))
    }
}

/// Converts JFM markdown to a validated ADF document, keeping the source
/// available so any validation failure can be reported with its origin.
///
/// On success this is equivalent to
/// `ValidatedAdfDocument::try_new(markdown_to_adf(source)?)`. On a schema
/// violation the returned error is enriched with the offending text excerpt
/// and its 1-based `line:column` in `source`, so callers get an actionable
/// message instead of a bare ADF index path (issue #1087).
///
/// # Errors
///
/// Returns an error if the document violates the ADF content model. (The
/// markdown-to-ADF step itself is infallible in practice but its `Result` is
/// propagated for forward compatibility.)
pub fn markdown_to_validated_adf(source: &str) -> anyhow::Result<ValidatedAdfDocument> {
    let doc = markdown_to_adf(source)?;
    Ok(ValidatedAdfDocument::try_new_with_source(
        doc,
        Some(source),
    )?)
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
        Self::try_new_with_source(doc, None)
    }

    /// Like [`Self::try_new`], but keeps `source` (the JFM markdown `doc` was
    /// converted from, when applicable) so a validation failure reports the
    /// offending run's 1-based `line:column`. Pass `None` when there is no JFM
    /// source (e.g. ADF-format input). See issue #1087.
    ///
    /// # Errors
    ///
    /// Returns [`AdfValidationError`] if `doc` contains any disallowed nesting
    /// per the schema in [`crate::atlassian::adf_schema`].
    pub fn try_new_with_source(
        doc: AdfDocument,
        source: Option<&str>,
    ) -> Result<Self, AdfValidationError> {
        let violations = validate_document(&doc);
        if violations.is_empty() {
            Ok(Self(doc))
        } else {
            Err(AdfValidationError::new(violations).enriched(&doc, source))
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
        // Issue #714 reproducer. Since arity checking landed in #733, an
        // empty `expand` (and the panel that lacks any valid children)
        // also generate Arity violations — assertion is on the
        // disallowed-child case, the one the user cares about.
        let d = doc(vec![AdfNode::panel(
            "info",
            vec![AdfNode::expand(None, vec![])],
        )]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert!(err.violations.iter().any(|v| matches!(
            v,
            AdfSchemaViolation::DisallowedChild { child_type, parent_type, .. }
                if child_type == "expand" && parent_type == "panel"
        )));
    }

    #[test]
    fn try_new_rejects_table_cell_with_expand() {
        let d = doc(vec![AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![AdfNode::expand(None, vec![])]),
        ])])]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert!(err.violations.iter().any(|v| matches!(
            v,
            AdfSchemaViolation::DisallowedChild { child_type, parent_type, .. }
                if child_type == "expand" && parent_type == "tableCell"
        )));
    }

    #[test]
    fn try_new_allows_expand_inside_layout_column() {
        // layoutSection requires 2..=3 columns (Range quantifier) and the
        // expand needs ≥1 child, so the document is composed accordingly.
        let inner = || AdfNode::paragraph(vec![AdfNode::text("x")]);
        let column = || AdfNode::layout_column(50, vec![AdfNode::expand(None, vec![inner()])]);
        let d = doc(vec![AdfNode::layout_section(vec![column(), column()])]);
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

    // ── Display arms for non-nesting variant kinds ────────────────────
    //
    // Each variant kind in `AdfSchemaViolation` produces a different
    // `AdfValidationError` Display section (nesting / arity / attr / mark).
    // Cover the attr and mark sections directly by constructing the
    // error rather than going through the validator.

    #[test]
    fn error_display_for_missing_attr_violation() {
        let err = AdfValidationError::new(vec![AdfSchemaViolation::MissingAttr {
            node_type: "panel".to_string(),
            attr_name: "panelType".to_string(),
            path: vec![0],
        }]);
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF attribute"), "got: {msg}");
        assert!(msg.contains("'panelType'"), "got: {msg}");
        assert!(msg.contains("hint:"), "got: {msg}");
    }

    #[test]
    fn error_display_for_invalid_attr_violation() {
        use crate::atlassian::adf_attr_schema::AttrProblem;
        let err = AdfValidationError::new(vec![AdfSchemaViolation::InvalidAttr {
            node_type: "heading".to_string(),
            attr_name: "level".to_string(),
            problem: AttrProblem::OutOfRange {
                lo: 1,
                hi: 6,
                actual: 7,
            },
            path: vec![0],
        }]);
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF attribute"), "got: {msg}");
        assert!(msg.contains("'heading.level'"), "got: {msg}");
    }

    #[test]
    fn error_display_for_disallowed_mark_violation() {
        let err = AdfValidationError::new(vec![AdfSchemaViolation::DisallowedMark {
            mark_type: "code".to_string(),
            parent_type: "heading".to_string(),
            inline_index: Some(0),
            path: vec![0],
        }]);
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF mark"), "got: {msg}");
        assert!(msg.contains("'code' mark"), "got: {msg}");
        assert!(msg.contains("hint: remove or correct"), "got: {msg}");
    }

    #[test]
    fn error_display_for_forbidden_mark_combination() {
        let err = AdfValidationError::new(vec![AdfSchemaViolation::ForbiddenMarkCombination {
            mark_type: "strong".to_string(),
            conflicts_with: "code".to_string(),
            parent_type: "paragraph".to_string(),
            inline_index: Some(0),
            path: vec![0, 0],
        }]);
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF mark combination"), "got: {msg}");
        assert!(
            msg.contains("'strong' mark cannot be combined with 'code' mark"),
            "got: {msg}"
        );
        assert!(msg.contains("hint:"), "got: {msg}");
    }

    #[test]
    fn try_new_rejects_strong_plus_code_text() {
        // Issue #1047 reproducer at the validator boundary: a paragraph whose
        // text carries both `strong` and `code` is rejected before send.
        let text = AdfNode {
            node_type: "text".to_string(),
            attrs: None,
            content: None,
            text: Some("x".to_string()),
            marks: Some(vec![
                crate::atlassian::adf::AdfMark::strong(),
                crate::atlassian::adf::AdfMark::code(),
            ]),
            local_id: None,
            parameters: None,
        };
        let d = doc(vec![AdfNode::paragraph(vec![text])]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        assert!(err.violations.iter().any(|v| matches!(
            v,
            AdfSchemaViolation::ForbiddenMarkCombination { mark_type, conflicts_with, .. }
                if mark_type == "strong" && conflicts_with == "code"
        )));
    }

    #[test]
    fn error_display_for_invalid_mark_attr_violation() {
        use crate::atlassian::adf_attr_schema::AttrProblem;
        let err = AdfValidationError::new(vec![AdfSchemaViolation::InvalidMarkAttr {
            mark_type: "link".to_string(),
            attr_name: "href".to_string(),
            problem: AttrProblem::BadFormat {
                reason: "not a valid URL",
            },
            inline_index: Some(0),
            path: vec![0],
        }]);
        let msg = err.to_string();
        assert!(msg.contains("invalid ADF mark"), "got: {msg}");
        assert!(msg.contains("'link' mark"), "got: {msg}");
    }

    // ── Source-location enrichment (issue #1087) ──────────────────────

    #[test]
    fn offset_to_line_col_counts_chars_not_bytes() {
        // "éx" sits on line 3; `é` is two bytes but one column, so `x` must be
        // reported at column 2, not 3.
        let src = "ab\ncd\néx";
        assert_eq!(
            offset_to_line_col(src, 0),
            SourceLocation { line: 1, column: 1 }
        );
        assert_eq!(
            offset_to_line_col(src, 6),
            SourceLocation { line: 3, column: 1 } // é
        );
        assert_eq!(
            offset_to_line_col(src, 8),
            SourceLocation { line: 3, column: 2 } // x
        );
    }

    #[test]
    fn locate_in_source_reports_first_occurrence() {
        let loc = locate_in_source("hello\nworld foo", "foo").unwrap();
        assert_eq!(loc, SourceLocation { line: 2, column: 7 });
        assert!(locate_in_source("no match here", "absent").is_none());
        // A needle that is only whitespace trims to empty and locates nothing.
        assert!(locate_in_source("some text", "   ").is_none());
    }

    #[test]
    fn collect_text_stops_at_the_excerpt_budget() {
        // A block node whose *own* text already fills the budget, plus a child:
        // gathering the child enters `gather_text` with a full buffer (the
        // entry guard), and the post-child loop guard also fires — so the
        // trailing child text is never appended.
        let long = "a".repeat(EXCERPT_MAX_CHARS + 5);
        let mut node = AdfNode::paragraph(vec![AdfNode::text("SHOULD_NOT_APPEAR")]);
        node.text = Some(long);
        let gathered = collect_text(&node);
        assert!(gathered.chars().count() >= EXCERPT_MAX_CHARS);
        assert!(!gathered.contains("SHOULD_NOT_APPEAR"), "got: {gathered}");
    }

    #[test]
    fn resolve_context_returns_empty_for_off_tree_path() {
        // A violation whose path runs off the tree resolves to no node, so no
        // excerpt or location is attached.
        let d = doc(vec![AdfNode::paragraph(vec![AdfNode::text("hi")])]);
        let violation = AdfSchemaViolation::DisallowedChild {
            child_type: "x".to_string(),
            parent_type: "y".to_string(),
            path: vec![9, 9],
        };
        assert_eq!(
            resolve_context(&violation, &d, Some("hi")),
            ViolationContext::default()
        );
    }

    #[test]
    fn node_at_path_resolves_nested_and_rejects_off_tree() {
        let d = doc(vec![AdfNode::paragraph(vec![AdfNode::text("hi")])]);
        assert_eq!(node_at_path(&d, &[0]).unwrap().node_type, "paragraph");
        assert_eq!(
            node_at_path(&d, &[0, 0]).unwrap().text.as_deref(),
            Some("hi")
        );
        assert!(node_at_path(&d, &[5]).is_none());
        assert!(node_at_path(&d, &[0, 9]).is_none());
    }

    #[test]
    fn truncate_excerpt_shortens_long_runs() {
        assert_eq!(truncate_excerpt("abc"), "abc");
        let long = "x".repeat(EXCERPT_MAX_CHARS + 5);
        let t = truncate_excerpt(&long);
        assert!(t.ends_with('…'), "got: {t}");
        assert_eq!(t.chars().count(), EXCERPT_MAX_CHARS + 1);
    }

    #[test]
    fn try_new_error_names_offending_text_without_source() {
        // A strong+code text node. `try_new` has no JFM source, so the message
        // carries the offending run's text but no line:column.
        let text = AdfNode {
            node_type: "text".to_string(),
            attrs: None,
            content: None,
            text: Some("/api/v1/example".to_string()),
            marks: Some(vec![
                crate::atlassian::adf::AdfMark::strong(),
                crate::atlassian::adf::AdfMark::code(),
            ]),
            local_id: None,
            parameters: None,
        };
        let d = doc(vec![AdfNode::paragraph(vec![text])]);
        let err = ValidatedAdfDocument::try_new(d).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("in text: \"/api/v1/example\""), "got: {msg}");
        assert!(
            !msg.contains("at line"),
            "no source ⇒ no line:col, got: {msg}"
        );
    }

    #[test]
    fn markdown_to_validated_adf_reports_line_column_and_excerpt() {
        // Issue #1087: an inline-code run carrying an illegal companion mark
        // is rejected by the validator with the source location. Bold+code no
        // longer reaches the validator (the converter splits it, issue #1391),
        // so an explicit span-syntax `underline` keeps this path covered. The
        // offending run sits on line 5 of the source.
        let src =
            "# Heading\n\nIntro paragraph.\n\nHere is [`/api/v1/example`]{underline} in a sentence.\n";
        let err = markdown_to_validated_adf(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/api/v1/example"), "excerpt, got: {msg}");
        assert!(msg.contains("at line 5"), "line, got: {msg}");
        assert!(msg.contains("column"), "column, got: {msg}");
    }

    #[test]
    fn markdown_to_validated_adf_accepts_clean_document() {
        let v = markdown_to_validated_adf("# Title\n\nA clean paragraph.\n").unwrap();
        assert!(!v.content.is_empty());
    }

    #[test]
    fn markdown_to_validated_adf_accepts_emphasis_around_inline_code() {
        // Issue #1391: bold/italic/strike wrapping inline code used to abort
        // the whole write here; the converter now splits the marks into legal
        // adjacent runs, so validation succeeds end-to-end.
        for src in [
            "Here is **`/api/v1/example`** in a sentence.\n",
            "*`x`* and ~~`y`~~\n",
            "**foo `bar` baz** with **[`x`](https://e.com)**\n",
        ] {
            let v = markdown_to_validated_adf(src)
                .unwrap_or_else(|e| panic!("expected {src:?} to validate, got: {e}"));
            assert!(!v.content.is_empty());
        }
    }
}
