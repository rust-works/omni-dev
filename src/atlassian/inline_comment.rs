//! Inline-comment drift auditing and re-anchoring for Confluence pages.
//!
//! Confluence inline comments are anchored to a run of characters in the page
//! ADF via an `annotation` mark carrying an `id`. When the underlying text is
//! edited, Confluence keeps the mark on whatever *original characters* survive —
//! it does not follow the *meaning* of the annotated text. Substantive rewrites
//! therefore leave inline comments "torn" across disjoint fragments, slid onto
//! unrelated text, or dropped entirely.
//!
//! This module provides:
//!
//! - [`audit_inline_comments`] — read-only: for every inline comment on a page,
//!   compare the text currently bearing its annotation mark against the
//!   reviewer's original highlight (`inlineOriginalSelection`, stored durably on
//!   the comment) and classify the drift ([`DriftStatus`]).
//! - [`reanchor_inline_comment`] — write: move a comment's annotation mark to a
//!   new run in the current-version ADF and PUT the page back in one update.
//!
//! Both operate directly on the typed ADF (`atlas_doc_format`) — never via JFM,
//! which would lose the mark structure the anchor depends on.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::atlassian::adf::{AdfDocument, AdfMark, AdfNode};
use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::atlassian::convert::adf_to_plain_text;

// ── Drift classification ────────────────────────────────────────────

/// How an inline comment's anchor relates to the reviewer's original highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftStatus {
    /// A single run still bears the mark and its text matches the original
    /// highlight — the anchor is healthy.
    Ok,
    /// The mark survives and the joined text still matches the original, but it
    /// is split across multiple disjoint runs (a "torn" anchor). Cosmetic; the
    /// comment still points at the right words.
    Torn,
    /// No run in the current ADF bears the mark — Confluence dropped it during
    /// an edit. The comment no longer highlights anything.
    MarkLost,
    /// One or more runs bear the mark but their joined text no longer matches
    /// the original highlight — the anchor has drifted onto different text.
    Drifted,
}

/// Per-comment drift report produced by [`audit_inline_comments`].
#[derive(Debug, Clone, Serialize)]
pub struct CommentDrift {
    /// The inline comment's ID.
    pub comment_id: String,
    /// Comment author account ID.
    pub author: String,
    /// ISO 8601 creation timestamp.
    pub created: String,
    /// The `annotation`-mark `id` this comment is anchored to.
    pub marker_ref: String,
    /// Drift classification.
    pub status: DriftStatus,
    /// The plaintext the reviewer originally highlighted.
    pub original_selection: String,
    /// The text currently bearing the annotation mark (joined across runs).
    /// `None` when the mark was lost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_anchored_text: Option<String>,
    /// A suggested new anchor for a drifted/lost comment: the original
    /// selection, when it still appears verbatim in the current page text.
    /// `None` when it no longer appears (low confidence — a human should pick).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_new_anchor: Option<String>,
    /// How many times `suggested_new_anchor` appears in the current page text
    /// (so the caller can pass a match index to [`reanchor_inline_comment`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_match_count: Option<usize>,
}

/// Result of a successful [`reanchor_inline_comment`] call.
#[derive(Debug, Clone, Serialize)]
pub struct ReanchorOutcome {
    /// The re-anchored comment's ID.
    pub comment_id: String,
    /// The annotation-mark `id` that was moved.
    pub marker_ref: String,
    /// The text the mark was moved to.
    pub new_anchor_text: String,
    /// The text that previously bore the mark (joined across runs), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_anchored_text: Option<String>,
    /// The reviewer's original highlight, for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_selection: Option<String>,
    /// True when this was a dry run — the page was validated and the move
    /// computed, but no write was performed.
    pub dry_run: bool,
}

// ── Orchestration ───────────────────────────────────────────────────

/// Audits every inline comment on `page_id` for anchor drift.
///
/// Read-only. Fetches the current ADF and the inline comments (which carry the
/// durable `inlineMarkerRef` / `inlineOriginalSelection` properties), then for
/// each comment compares the currently-annotated text against the original
/// highlight. Comments with no marker reference are skipped.
pub async fn audit_inline_comments(
    api: &ConfluenceApi,
    page_id: &str,
) -> Result<Vec<CommentDrift>> {
    let doc = fetch_page_adf(api, page_id).await?;
    let plain = adf_to_plain_text(&doc);
    let comments = api.get_page_inline_comments(page_id).await?;

    let mut reports = Vec::new();
    for comment in comments {
        let Some(marker) = comment.inline_marker_ref.as_deref() else {
            continue;
        };
        let original = comment
            .inline_original_selection
            .clone()
            .unwrap_or_default();

        let runs = collect_annotation_runs(&doc, marker);
        let joined = runs.concat();
        let status = classify(&runs, &joined, &original);

        let (suggested_new_anchor, suggested_match_count) = match status {
            DriftStatus::Ok | DriftStatus::Torn => (None, None),
            DriftStatus::MarkLost | DriftStatus::Drifted => {
                let count = count_non_overlapping(&plain, &original);
                if !original.is_empty() && count > 0 {
                    (Some(original.clone()), Some(count))
                } else {
                    (None, None)
                }
            }
        };

        reports.push(CommentDrift {
            comment_id: comment.id,
            author: comment.author,
            created: comment.created,
            marker_ref: marker.to_string(),
            status,
            original_selection: original,
            current_anchored_text: if runs.is_empty() { None } else { Some(joined) },
            suggested_new_anchor,
            suggested_match_count,
        });
    }

    Ok(reports)
}

/// Moves the annotation mark of inline comment `comment_id` to a new run of
/// text (`anchor_text`) in the current-version ADF and writes the page back.
///
/// `match_index` (1-based) disambiguates when `anchor_text` occurs more than
/// once. The mark is removed from every run currently bearing it and re-applied
/// to the chosen run in a single mutated document, so the page is written in one
/// PUT — the server never observes a half-applied state.
///
/// When `dry_run` is true, the page is fetched, the move is computed, and the
/// resulting ADF is validated, but no write is performed — so callers can
/// preview (and surface anchor/validation errors) without mutating the page.
///
/// Operates entirely on ADF (`atlas_doc_format`); it never round-trips through
/// JFM, which would discard the annotation marks the anchor depends on.
pub async fn reanchor_inline_comment(
    api: &ConfluenceApi,
    page_id: &str,
    comment_id: &str,
    anchor_text: &str,
    match_index: Option<usize>,
    dry_run: bool,
) -> Result<ReanchorOutcome> {
    let comments = api.get_page_inline_comments(page_id).await?;
    let comment = comments
        .iter()
        .find(|c| c.id == comment_id)
        .with_context(|| format!("inline comment {comment_id} not found on page {page_id}"))?;
    let marker = comment.inline_marker_ref.clone().with_context(|| {
        format!("comment {comment_id} has no inline marker reference (is it an inline comment?)")
    })?;
    let original_selection = comment.inline_original_selection.clone();

    let mut doc = fetch_page_adf(api, page_id).await?;

    let previous_runs = collect_annotation_runs(&doc, &marker);
    let previous_anchored_text = if previous_runs.is_empty() {
        None
    } else {
        Some(previous_runs.concat())
    };

    remove_annotation(&mut doc, &marker);
    apply_annotation(&mut doc, anchor_text, match_index, &marker)?;

    let validated =
        ValidatedAdfDocument::try_new(doc).context("re-anchored ADF failed schema validation")?;
    if !dry_run {
        api.update_content(page_id, &validated, None).await?;
    }

    Ok(ReanchorOutcome {
        comment_id: comment_id.to_string(),
        marker_ref: marker,
        new_anchor_text: anchor_text.to_string(),
        previous_anchored_text,
        original_selection,
        dry_run,
    })
}

/// Fetches the current-version ADF of `page_id` as a typed [`AdfDocument`].
async fn fetch_page_adf(api: &ConfluenceApi, page_id: &str) -> Result<AdfDocument> {
    let page = api.get_content(page_id).await?;
    match page.body_adf {
        Some(value) => serde_json::from_value(value).context("failed to parse page ADF"),
        None => Ok(AdfDocument::new()),
    }
}

/// Classifies a comment's anchor from its current runs vs. the original highlight.
fn classify(runs: &[String], joined: &str, original: &str) -> DriftStatus {
    if runs.is_empty() {
        DriftStatus::MarkLost
    } else if normalize(joined) == normalize(original) {
        if runs.len() == 1 {
            DriftStatus::Ok
        } else {
            DriftStatus::Torn
        }
    } else {
        DriftStatus::Drifted
    }
}

// ── ADF mark helpers ────────────────────────────────────────────────

/// Returns the text of each run that bears an `annotation` mark with `marker_id`,
/// in document order.
fn collect_annotation_runs(doc: &AdfDocument, marker_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    for node in &doc.content {
        collect_runs_in_node(node, marker_id, &mut out);
    }
    out
}

fn collect_runs_in_node(node: &AdfNode, marker_id: &str, out: &mut Vec<String>) {
    if is_text_node(node) && has_annotation(node, marker_id) {
        if let Some(text) = &node.text {
            out.push(text.clone());
        }
    }
    if let Some(content) = &node.content {
        for child in content {
            collect_runs_in_node(child, marker_id, out);
        }
    }
}

/// Removes the `annotation` mark with `marker_id` from every run in the document.
/// Runs left with no marks have their `marks` collapsed to `None`.
fn remove_annotation(doc: &mut AdfDocument, marker_id: &str) {
    for node in &mut doc.content {
        remove_annotation_in_node(node, marker_id);
    }
}

fn remove_annotation_in_node(node: &mut AdfNode, marker_id: &str) {
    if let Some(marks) = &mut node.marks {
        marks.retain(|m| !is_target_annotation(m, marker_id));
        if marks.is_empty() {
            node.marks = None;
        }
    }
    if let Some(content) = &mut node.content {
        for child in content {
            remove_annotation_in_node(child, marker_id);
        }
    }
}

/// State threaded through the mutating anchor-application walk.
struct ApplyState {
    anchor: String,
    marker_id: String,
    /// 1-based occurrence to annotate.
    target: usize,
    /// Occurrences seen so far, in document order.
    current: usize,
    /// Set once the target occurrence has been annotated.
    applied: bool,
}

/// Adds an `annotation` mark (`marker_id`) to the run of text matching
/// `anchor_text`.
///
/// Supports **cross-run** anchors: a phrase split across sibling text runs by
/// formatting (e.g. `the **bold** word`) is matched within its inline container
/// and the covered runs are split at the match boundaries and re-marked. A
/// selection that crosses a non-text inline node or a block boundary is not
/// matched (it is not a contiguous run) and yields a "not found" error.
///
/// `match_index` (1-based) selects which occurrence to annotate; `None` requires
/// the anchor to be unique.
fn apply_annotation(
    doc: &mut AdfDocument,
    anchor_text: &str,
    match_index: Option<usize>,
    marker_id: &str,
) -> Result<()> {
    if anchor_text.is_empty() {
        bail!("anchor text must not be empty");
    }

    let total = count_anchor_occurrences(doc, anchor_text);
    if total == 0 {
        bail!(
            "anchor text {anchor_text:?} was not found as a contiguous run of text on the page; \
             it may span a formatting or block boundary — choose a phrase that appears intact"
        );
    }

    let target = match match_index {
        Some(i) if i == 0 || i > total => bail!(
            "match index {i} out of range: anchor text {anchor_text:?} appears \
             {total} time(s) on the page (valid range: 1..={total})"
        ),
        Some(i) => i,
        None if total > 1 => bail!(
            "anchor text {anchor_text:?} appears {total} times on the page; \
             specify a 1-based match index to choose which occurrence to anchor to"
        ),
        None => 1,
    };

    let mut state = ApplyState {
        anchor: anchor_text.to_string(),
        marker_id: marker_id.to_string(),
        target,
        current: 0,
        applied: false,
    };
    apply_in_children(&mut doc.content, &mut state);

    if !state.applied {
        bail!("internal error: failed to apply annotation for occurrence {target}");
    }
    Ok(())
}

fn apply_in_node(node: &mut AdfNode, state: &mut ApplyState) {
    if state.applied {
        return;
    }
    if let Some(content) = &mut node.content {
        apply_in_children(content, state);
    }
}

fn apply_in_children(children: &mut Vec<AdfNode>, state: &mut ApplyState) {
    if state.applied {
        return;
    }
    let old = std::mem::take(children);
    let mut rebuilt: Vec<AdfNode> = Vec::with_capacity(old.len());
    let mut span: Vec<AdfNode> = Vec::new();

    for mut child in old {
        if is_text_node(&child) {
            span.push(child);
        } else {
            flush_span(&mut span, state, &mut rebuilt);
            apply_in_node(&mut child, state);
            rebuilt.push(child);
        }
    }
    flush_span(&mut span, state, &mut rebuilt);

    *children = rebuilt;
}

/// Processes a maximal run of consecutive text-node siblings: searches for the
/// anchor within their concatenated text and, if the target occurrence falls
/// here, splits the covered runs and applies the annotation mark.
fn flush_span(span: &mut Vec<AdfNode>, state: &mut ApplyState, out: &mut Vec<AdfNode>) {
    if span.is_empty() {
        return;
    }
    let nodes = std::mem::take(span);

    if state.applied {
        out.extend(nodes);
        return;
    }

    // Concatenate the span and record each node's byte range within it.
    let mut combined = String::new();
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(nodes.len());
    for node in &nodes {
        let start = combined.len();
        combined.push_str(node.text.as_deref().unwrap_or(""));
        ranges.push((start, combined.len()));
    }

    // Count occurrences in document order until we reach the target.
    let mut target_range: Option<(usize, usize)> = None;
    let mut search_from = 0;
    while let Some(pos) = combined[search_from..].find(&state.anchor) {
        let match_start = search_from + pos;
        let match_end = match_start + state.anchor.len();
        state.current += 1;
        if state.current == state.target {
            target_range = Some((match_start, match_end));
            state.applied = true;
            break;
        }
        search_from = match_end;
    }

    match target_range {
        Some((s, e)) => {
            for (idx, node) in nodes.iter().enumerate() {
                let (a, b) = ranges[idx];
                slice_node(node, a, b, s, e, &state.marker_id, out);
            }
        }
        None => out.extend(nodes),
    }
}

/// Emits `node`, split so the portion overlapping the global byte range `[s, e)`
/// bears the annotation mark. `[a, b)` is `node`'s byte range within the span.
fn slice_node(
    node: &AdfNode,
    a: usize,
    b: usize,
    s: usize,
    e: usize,
    marker_id: &str,
    out: &mut Vec<AdfNode>,
) {
    let text = node.text.as_deref().unwrap_or("");
    if text.is_empty() {
        out.push(node.clone());
        return;
    }

    let s_c = s.clamp(a, b);
    let e_c = e.clamp(a, b);
    for (lo, hi, annotate) in [(a, s_c, false), (s_c, e_c, true), (e_c, b, false)] {
        if lo >= hi {
            continue;
        }
        let segment = &text[(lo - a)..(hi - a)];
        out.push(make_segment(node, segment, annotate, marker_id));
    }
}

/// Clones `base` as a text node carrying `text`, optionally adding the
/// annotation mark on top of `base`'s existing marks.
fn make_segment(base: &AdfNode, text: &str, annotate: bool, marker_id: &str) -> AdfNode {
    let mut node = base.clone();
    node.text = Some(text.to_string());
    if annotate {
        let mark = AdfMark::annotation(marker_id, "inlineComment");
        match &mut node.marks {
            Some(marks) => {
                if !marks.iter().any(|m| is_target_annotation(m, marker_id)) {
                    marks.push(mark);
                }
            }
            None => node.marks = Some(vec![mark]),
        }
    }
    node
}

/// Counts occurrences of `anchor` across all contiguous text-run spans in the
/// document, in the same order [`apply_in_children`] visits them.
fn count_anchor_occurrences(doc: &AdfDocument, anchor: &str) -> usize {
    let mut total = 0;
    count_in_children(&doc.content, anchor, &mut total);
    total
}

fn count_in_children(children: &[AdfNode], anchor: &str, total: &mut usize) {
    let mut span = String::new();
    for child in children {
        if is_text_node(child) {
            span.push_str(child.text.as_deref().unwrap_or(""));
        } else {
            if !span.is_empty() {
                *total += count_non_overlapping(&span, anchor);
                span.clear();
            }
            count_in_children_of(child, anchor, total);
        }
    }
    if !span.is_empty() {
        *total += count_non_overlapping(&span, anchor);
    }
}

fn count_in_children_of(node: &AdfNode, anchor: &str, total: &mut usize) {
    if let Some(content) = &node.content {
        count_in_children(content, anchor, total);
    }
}

// ── Small predicates / string utilities ─────────────────────────────

fn is_text_node(node: &AdfNode) -> bool {
    node.node_type == "text" && node.text.is_some()
}

fn has_annotation(node: &AdfNode, marker_id: &str) -> bool {
    node.marks
        .iter()
        .flatten()
        .any(|m| is_target_annotation(m, marker_id))
}

fn is_target_annotation(mark: &AdfMark, marker_id: &str) -> bool {
    mark.mark_type == "annotation"
        && mark
            .attrs
            .as_ref()
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())
            == Some(marker_id)
}

/// Collapses all runs of whitespace to a single space and trims, so anchored
/// text and the original highlight compare equal despite incidental whitespace
/// differences (e.g. a line wrap re-flow).
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
fn count_non_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        count += 1;
        start += pos + needle.len();
    }
    count
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn strong() -> AdfMark {
        AdfMark {
            mark_type: "strong".to_string(),
            attrs: None,
        }
    }

    fn annotated(text: &str, id: &str) -> AdfNode {
        AdfNode::text_with_marks(text, vec![AdfMark::annotation(id, "inlineComment")])
    }

    fn doc(paras: Vec<Vec<AdfNode>>) -> AdfDocument {
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: paras.into_iter().map(AdfNode::paragraph).collect(),
        }
    }

    // ── collect_annotation_runs ─────────────────────────────────────

    #[test]
    fn collect_runs_single() {
        let d = doc(vec![vec![
            AdfNode::text("The cache TTL is "),
            annotated("200ms", "aaa"),
            AdfNode::text(" by default."),
        ]]);
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["200ms"]);
    }

    #[test]
    fn collect_runs_torn_across_fragments_in_order() {
        let d = doc(vec![vec![
            AdfNode::text("The cache TTL is "),
            annotated("200ms", "aaa"),
            AdfNode::text(". Downstream services "),
            annotated("depend on this", "aaa"),
            AdfNode::text(" value."),
        ]]);
        assert_eq!(
            collect_annotation_runs(&d, "aaa"),
            vec!["200ms", "depend on this"]
        );
    }

    #[test]
    fn collect_runs_ignores_other_ids() {
        let d = doc(vec![vec![annotated("x", "bbb")]]);
        assert!(collect_annotation_runs(&d, "aaa").is_empty());
    }

    // ── classify ────────────────────────────────────────────────────

    #[test]
    fn classify_ok_torn_lost_drifted() {
        assert_eq!(
            classify(&["hello".into()], "hello", "hello"),
            DriftStatus::Ok
        );
        assert_eq!(
            classify(&["hel".into(), "lo".into()], "hello", "hello"),
            DriftStatus::Torn
        );
        assert_eq!(classify(&[], "", "hello"), DriftStatus::MarkLost);
        assert_eq!(
            classify(&["goodbye".into()], "goodbye", "hello"),
            DriftStatus::Drifted
        );
    }

    #[test]
    fn classify_normalizes_whitespace() {
        assert_eq!(
            classify(&["a  b\n c".into()], "a  b\n c", "a b c"),
            DriftStatus::Ok
        );
    }

    // ── remove_annotation ───────────────────────────────────────────

    #[test]
    fn remove_annotation_strips_mark_and_collapses_empty_marks() {
        let mut d = doc(vec![vec![annotated("200ms", "aaa")]]);
        remove_annotation(&mut d, "aaa");
        assert!(collect_annotation_runs(&d, "aaa").is_empty());
        // The run had only the annotation mark, so marks collapses to None.
        let run = &d.content[0].content.as_ref().unwrap()[0];
        assert!(run.marks.is_none());
        assert_eq!(run.text.as_deref(), Some("200ms"));
    }

    #[test]
    fn remove_annotation_preserves_other_marks() {
        let run = AdfNode::text_with_marks(
            "x",
            vec![strong(), AdfMark::annotation("aaa", "inlineComment")],
        );
        let mut d = doc(vec![vec![run]]);
        remove_annotation(&mut d, "aaa");
        let run = &d.content[0].content.as_ref().unwrap()[0];
        let marks = run.marks.as_ref().unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].mark_type, "strong");
    }

    // ── apply_annotation ────────────────────────────────────────────

    fn first_para_runs(d: &AdfDocument) -> &Vec<AdfNode> {
        d.content[0].content.as_ref().unwrap()
    }

    #[test]
    fn apply_annotation_splits_substring_within_single_run() {
        let mut d = doc(vec![vec![AdfNode::text(
            "The cache TTL is 200ms by default.",
        )]]);
        apply_annotation(&mut d, "200ms", None, "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["200ms"]);
        // Round-trips: three runs "The cache TTL is " / "200ms" / " by default.".
        let runs = first_para_runs(&d);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text.as_deref(), Some("The cache TTL is "));
        assert_eq!(runs[1].text.as_deref(), Some("200ms"));
        assert_eq!(runs[2].text.as_deref(), Some(" by default."));
    }

    #[test]
    fn apply_annotation_cross_run_phrase_split_by_formatting() {
        // "the bold word" is split across three runs by a strong mark.
        let mut d = doc(vec![vec![
            AdfNode::text("say the "),
            AdfNode::text_with_marks("bold", vec![strong()]),
            AdfNode::text(" word now"),
        ]]);
        apply_annotation(&mut d, "the bold word", None, "aaa").unwrap();
        // The joined annotated text spans the phrase.
        assert_eq!(collect_annotation_runs(&d, "aaa").concat(), "the bold word");
        // The strong mark is preserved on the "bold" run alongside the annotation.
        let bold_run = first_para_runs(&d)
            .iter()
            .find(|n| n.text.as_deref() == Some("bold"))
            .unwrap();
        let marks = bold_run.marks.as_ref().unwrap();
        assert!(marks.iter().any(|m| m.mark_type == "strong"));
        assert!(marks.iter().any(|m| is_target_annotation(m, "aaa")));
    }

    #[test]
    fn apply_annotation_respects_match_index() {
        let mut d = doc(vec![vec![AdfNode::text("foo bar foo baz foo")]]);
        apply_annotation(&mut d, "foo", Some(2), "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["foo"]);
        // The annotated "foo" is the middle occurrence: preceded by "foo bar ".
        let runs = first_para_runs(&d);
        let annotated_pos = runs.iter().position(|n| has_annotation(n, "aaa")).unwrap();
        let before: String = runs[..annotated_pos]
            .iter()
            .filter_map(|n| n.text.clone())
            .collect();
        assert_eq!(before, "foo bar ");
    }

    #[test]
    fn apply_annotation_ambiguous_without_index_errors() {
        let mut d = doc(vec![vec![AdfNode::text("foo and foo")]]);
        let err = apply_annotation(&mut d, "foo", None, "aaa").unwrap_err();
        assert!(err.to_string().contains("appears 2 times"));
    }

    #[test]
    fn apply_annotation_not_found_errors() {
        let mut d = doc(vec![vec![AdfNode::text("hello world")]]);
        let err = apply_annotation(&mut d, "missing", None, "aaa").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn apply_annotation_index_out_of_range_errors() {
        let mut d = doc(vec![vec![AdfNode::text("foo foo")]]);
        let err = apply_annotation(&mut d, "foo", Some(3), "aaa").unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn apply_annotation_cross_block_not_matched() {
        // The phrase spans two paragraphs — not a contiguous run.
        let mut d = doc(vec![
            vec![AdfNode::text("end of first")],
            vec![AdfNode::text("second begins")],
        ]);
        let err = apply_annotation(&mut d, "first second", None, "aaa").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn apply_annotation_matches_occurrence_in_second_block() {
        let mut d = doc(vec![
            vec![AdfNode::text("alpha here")],
            vec![AdfNode::text("beta there")],
        ]);
        apply_annotation(&mut d, "beta", None, "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["beta"]);
    }

    // ── remove + apply round trip (the reanchor core) ───────────────

    #[test]
    fn reanchor_core_moves_mark_from_old_to_new_run() {
        // Mark starts torn across two fragments; reanchor should move it whole.
        let mut d = doc(vec![vec![
            AdfNode::text("The cache TTL is "),
            annotated("200ms", "aaa"),
            AdfNode::text(". Downstream services "),
            annotated("depend on this", "aaa"),
            AdfNode::text(" value, so it is fixed."),
        ]]);
        remove_annotation(&mut d, "aaa");
        assert!(collect_annotation_runs(&d, "aaa").is_empty());
        apply_annotation(
            &mut d,
            "Downstream services depend on this value",
            None,
            "aaa",
        )
        .unwrap();
        assert_eq!(
            collect_annotation_runs(&d, "aaa").concat(),
            "Downstream services depend on this value"
        );
    }

    #[test]
    fn apply_annotation_handles_multibyte_text() {
        let mut d = doc(vec![vec![AdfNode::text("café serves 200ms résumé")]]);
        apply_annotation(&mut d, "200ms", None, "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["200ms"]);
        // Surrounding multibyte text is preserved intact.
        let joined: String = first_para_runs(&d)
            .iter()
            .filter_map(|n| n.text.clone())
            .collect();
        assert_eq!(joined, "café serves 200ms résumé");
    }

    // ── audit / reanchor over a mocked Confluence API ───────────────

    use crate::atlassian::client::AtlassianClient;

    fn mock_api(server: &wiremock::MockServer) -> ConfluenceApi {
        let client = AtlassianClient::new(&server.uri(), "user@test.com", "token").unwrap();
        ConfluenceApi::new(client)
    }

    /// Mounts the page-read endpoints (`get_content` + its space lookup) so the
    /// page returns `page` as its ADF body.
    async fn mount_page(server: &wiremock::MockServer, id: &str, page: &AdfDocument) {
        let value = serde_json::to_string(page).unwrap();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/wiki/api/v2/pages/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id,
                    "title": "Mock",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1},
                    "body": {"atlas_doc_format": {"value": value}}
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    async fn mount_inline_comments(
        server: &wiremock::MockServer,
        id: &str,
        body: serde_json::Value,
    ) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/wiki/api/v2/pages/{id}/inline-comments"
            )))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn audit_classifies_ok_mark_lost_and_drifted() {
        let server = wiremock::MockServer::start().await;
        let page = doc(vec![vec![
            AdfNode::text("The cache TTL is "),
            annotated("200ms", "aaa"),
            AdfNode::text(" by default. "),
            annotated("wrong", "bbb"),
            AdfNode::text(" end."),
        ]]);
        mount_page(&server, "12345", &page).await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "200ms"}},
                    {"id": "ic2", "version": {"authorId": "b", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "zzz", "inlineOriginalSelection": "missing text"}},
                    {"id": "ic3", "version": {"authorId": "c", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "bbb", "inlineOriginalSelection": "200ms"}}
                ]
            }),
        )
        .await;

        let api = mock_api(&server);
        let drifts = audit_inline_comments(&api, "12345").await.unwrap();
        assert_eq!(drifts.len(), 3);

        let ic1 = drifts.iter().find(|d| d.comment_id == "ic1").unwrap();
        assert_eq!(ic1.status, DriftStatus::Ok);
        assert_eq!(ic1.current_anchored_text.as_deref(), Some("200ms"));

        let ic2 = drifts.iter().find(|d| d.comment_id == "ic2").unwrap();
        assert_eq!(ic2.status, DriftStatus::MarkLost);
        assert!(ic2.current_anchored_text.is_none());
        assert!(ic2.suggested_new_anchor.is_none());

        let ic3 = drifts.iter().find(|d| d.comment_id == "ic3").unwrap();
        assert_eq!(ic3.status, DriftStatus::Drifted);
        assert_eq!(ic3.current_anchored_text.as_deref(), Some("wrong"));
        assert_eq!(ic3.suggested_new_anchor.as_deref(), Some("200ms"));
        assert_eq!(ic3.suggested_match_count, Some(1));
    }

    #[tokio::test]
    async fn reanchor_moves_mark_and_puts_bumped_version() {
        let server = wiremock::MockServer::start().await;
        let page = doc(vec![vec![
            AdfNode::text("Before "),
            annotated("old", "aaa"),
            AdfNode::text(" and 200ms here."),
        ]]);
        mount_page(&server, "12345", &page).await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "old"}}
                ]
            }),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"version": {"number": 2}}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let outcome = reanchor_inline_comment(&api, "12345", "ic1", "200ms", None, false)
            .await
            .unwrap();
        assert_eq!(outcome.new_anchor_text, "200ms");
        assert_eq!(outcome.previous_anchored_text.as_deref(), Some("old"));
        assert!(!outcome.dry_run);

        // Inspect the PUT body: the mark must now sit on "200ms", not "old".
        let requests = server.received_requests().await.unwrap();
        let put = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::PUT)
            .unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&put.body).unwrap();
        let adf_value = sent["body"]["value"].as_str().unwrap();
        let updated: AdfDocument = serde_json::from_str(adf_value).unwrap();
        assert_eq!(collect_annotation_runs(&updated, "aaa"), vec!["200ms"]);
    }

    #[tokio::test]
    async fn reanchor_dry_run_does_not_put() {
        let server = wiremock::MockServer::start().await;
        let page = doc(vec![vec![
            AdfNode::text("Before "),
            annotated("old", "aaa"),
            AdfNode::text(" and 200ms here."),
        ]]);
        mount_page(&server, "12345", &page).await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "old"}}
                ]
            }),
        )
        .await;
        // No PUT is mounted: if reanchor tried to write, update_content would 404.

        let api = mock_api(&server);
        let outcome = reanchor_inline_comment(&api, "12345", "ic1", "200ms", None, true)
            .await
            .unwrap();
        assert!(outcome.dry_run);

        let requests = server.received_requests().await.unwrap();
        assert!(!requests
            .iter()
            .any(|r| r.method == wiremock::http::Method::PUT));
    }

    #[tokio::test]
    async fn reanchor_unknown_comment_errors() {
        let server = wiremock::MockServer::start().await;
        mount_inline_comments(&server, "12345", serde_json::json!({ "results": [] })).await;
        let api = mock_api(&server);
        let err = reanchor_inline_comment(&api, "12345", "nope", "x", None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn reanchor_comment_without_marker_ref_errors() {
        let server = wiremock::MockServer::start().await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"}}
                ]
            }),
        )
        .await;
        let api = mock_api(&server);
        let err = reanchor_inline_comment(&api, "12345", "ic1", "x", None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no inline marker reference"));
    }

    #[tokio::test]
    async fn reanchor_lost_mark_reports_no_previous_text() {
        let server = wiremock::MockServer::start().await;
        // The page no longer bears marker "aaa" anywhere — the mark was lost.
        let page = doc(vec![vec![AdfNode::text("Plain text with 200ms here.")]]);
        mount_page(&server, "12345", &page).await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "old"}}
                ]
            }),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let api = mock_api(&server);
        let outcome = reanchor_inline_comment(&api, "12345", "ic1", "200ms", None, false)
            .await
            .unwrap();
        assert!(outcome.previous_anchored_text.is_none());
        assert_eq!(outcome.new_anchor_text, "200ms");
    }

    #[tokio::test]
    async fn audit_skips_comments_without_marker_ref() {
        let server = wiremock::MockServer::start().await;
        let page = doc(vec![vec![annotated("200ms", "aaa")]]);
        mount_page(&server, "12345", &page).await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"}},
                    {"id": "ic2", "version": {"authorId": "b", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "200ms"}}
                ]
            }),
        )
        .await;

        let api = mock_api(&server);
        let drifts = audit_inline_comments(&api, "12345").await.unwrap();
        // ic1 has no marker reference and is skipped.
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].comment_id, "ic2");
    }

    #[tokio::test]
    async fn audit_page_without_adf_body_reports_mark_lost() {
        let server = wiremock::MockServer::start().await;
        // Page response carries no `body` — `fetch_page_adf` yields an empty doc.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "12345",
                    "title": "Mock",
                    "status": "current",
                    "spaceId": "98",
                    "version": {"number": 1}
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        mount_inline_comments(
            &server,
            "12345",
            serde_json::json!({
                "results": [
                    {"id": "ic1", "version": {"authorId": "a", "createdAt": "t"},
                     "properties": {"inlineMarkerRef": "aaa", "inlineOriginalSelection": "200ms"}}
                ]
            }),
        )
        .await;

        let api = mock_api(&server);
        let drifts = audit_inline_comments(&api, "12345").await.unwrap();
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].status, DriftStatus::MarkLost);
        // The empty page contains no occurrence of the original selection.
        assert!(drifts[0].suggested_new_anchor.is_none());
    }

    // ── edge branches in the anchor-application walk ────────────────

    #[test]
    fn apply_annotation_empty_anchor_errors() {
        let mut d = doc(vec![vec![AdfNode::text("hello")]]);
        let err = apply_annotation(&mut d, "", None, "aaa").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn apply_annotation_match_in_first_block_leaves_later_blocks_untouched() {
        let mut d = doc(vec![
            vec![AdfNode::text("alpha here")],
            vec![AdfNode::text("beta there")],
        ]);
        apply_annotation(&mut d, "alpha", None, "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["alpha"]);
        // The second paragraph is untouched (a single unsplit run).
        let second = d.content[1].content.as_ref().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].text.as_deref(), Some("beta there"));
    }

    fn hard_break() -> AdfNode {
        AdfNode {
            node_type: "hardBreak".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        }
    }

    #[test]
    fn apply_annotation_spans_reset_at_inline_non_text_nodes() {
        // "foo" appears once before and once after a hard break: two separate
        // spans, so the anchor is ambiguous and match_index selects the first.
        let mut d = doc(vec![vec![
            AdfNode::text("see foo here"),
            hard_break(),
            AdfNode::text("and foo there"),
        ]]);
        apply_annotation(&mut d, "foo", Some(1), "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["foo"]);
        // The annotated occurrence is the one before the break.
        let runs = first_para_runs(&d);
        let pos = runs.iter().position(|n| has_annotation(n, "aaa")).unwrap();
        let break_pos = runs
            .iter()
            .position(|n| n.node_type == "hardBreak")
            .unwrap();
        assert!(pos < break_pos);
    }

    #[test]
    fn apply_annotation_preserves_empty_text_runs() {
        let mut d = doc(vec![vec![AdfNode::text(""), AdfNode::text("foo bar")]]);
        apply_annotation(&mut d, "foo", None, "aaa").unwrap();
        assert_eq!(collect_annotation_runs(&d, "aaa"), vec!["foo"]);
        // The empty run survives the split untouched.
        let runs = first_para_runs(&d);
        assert_eq!(runs[0].text.as_deref(), Some(""));
    }

    #[test]
    fn count_non_overlapping_empty_needle_is_zero() {
        assert_eq!(count_non_overlapping("abc", ""), 0);
    }
}
