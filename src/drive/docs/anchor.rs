//! Content anchors resolved against the same inline snapshot as the write lease.
//!
//! This is the production UTF-16 arithmetic boundary. Offsets are computed
//! within server-indexed text runs, never by summing document text. Non-text
//! elements break matches; ranges cannot cross containers or structural gaps.

use std::collections::HashSet;

use regex::RegexBuilder;
use serde::Serialize;

use super::types::{Document, StructuralElement};

/// Why an anchor or range cannot safely address an edit. Never contains prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum AnchorError {
    /// The anchor is empty or contains a paragraph separator.
    InvalidAnchor,
    /// No characters would remain after the API strips unsupported input.
    InvalidInsertionText,
    /// The segment selector is empty or inconsistent.
    InvalidSelection,
    /// No selected segment exists.
    SegmentNotFound,
    /// A segment ID identifies more than one segment.
    AmbiguousSegment,
    /// No selected text matches.
    NotFound,
    /// More than one occurrence, including overlapping occurrences.
    Ambiguous {
        /// Number of matches across all tab bodies.
        count: usize,
    },
    /// Missing or inconsistent server indices or tab identity.
    InvalidIndices,
    /// The edit overlaps pending content suggestions.
    SuggestedContent,
    /// The range crosses a structural boundary or includes a protected newline.
    UnsafeRange,
    /// The second anchor precedes the first or is in another tab/container.
    UnorderedRange,
}

/// An explicit existing segment, optionally narrowed to one tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentSelection {
    /// Header, footer or footnote map key; absent selects all tab bodies.
    pub segment_id: Option<String>,
    /// Optional tab identity; requires a segment ID.
    pub tab_id: Option<String>,
}

impl SegmentSelection {
    /// Reject empty identifiers and a tab without a segment before I/O.
    pub fn validate(&self) -> Result<(), AnchorError> {
        if self.segment_id.as_ref().is_some_and(String::is_empty)
            || self.tab_id.as_ref().is_some_and(String::is_empty)
        {
            return Err(AnchorError::InvalidSelection);
        }
        if self.tab_id.is_some() && self.segment_id.is_none() {
            return Err(AnchorError::InvalidSelection);
        }
        Ok(())
    }
}

/// Kind of an explicitly selected non-body segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SegmentKind {
    /// Page header.
    Header,
    /// Page footer.
    Footer,
    /// Footnote content.
    Footnote,
}

/// Whether inserted text goes before or after the unique anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Before the first code unit of the anchor.
    Before,
    /// After the last code unit of the anchor.
    After,
}

/// Metadata shared by a dry-run and a successful edit. No document prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditPreview {
    /// Inclusive start in server UTF-16 code units.
    pub start_index: i64,
    /// Exclusive end; equals start for insertion.
    pub end_index: i64,
    /// The addressed tab; absent only for legacy top-level body responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// Explicit non-body segment identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
    /// Selected segment family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_kind: Option<SegmentKind>,
    /// Number of paragraphs touched.
    pub paragraphs: usize,
    /// Unicode scalar values inserted or removed.
    pub chars: usize,
    /// UTF-8 bytes inserted or removed.
    pub bytes: usize,
}

struct Run<'a> {
    text: &'a str,
    start: i64,
    end: i64,
    suggested: bool,
}

struct Paragraph<'a> {
    tab: usize,
    tab_id: Option<String>,
    container: usize,
    start: i64,
    end: i64,
    // The last newline of a container, or one preceding a structural element.
    protected_newline: bool,
    runs: Vec<Run<'a>>,
}

#[derive(Clone, Copy)]
struct Match {
    paragraph: usize,
    start: i64,
    end: i64,
    suggested: bool,
}

fn utf16_len(text: &str) -> Result<i64, AnchorError> {
    i64::try_from(text.encode_utf16().count()).map_err(|_| AnchorError::InvalidIndices)
}

fn paragraphs<'a>(
    document: &'a Document,
    selection: &SegmentSelection,
) -> Result<(Vec<Paragraph<'a>>, Option<SegmentKind>), AnchorError> {
    selection.validate()?;
    let mut out = Vec::new();
    let mut selected = Vec::new();
    let mut container = 0;
    let mut tab_ids = HashSet::new();
    for (tab, resolved) in document.resolved_tabs().iter().enumerate() {
        if !document.tabs.is_empty() {
            let id = resolved
                .tab_id
                .filter(|id| !id.is_empty())
                .ok_or(AnchorError::InvalidIndices)?;
            if !tab_ids.insert(id) {
                return Err(AnchorError::InvalidIndices);
            }
        }
        if let Some(id) = &selection.segment_id {
            if selection
                .tab_id
                .as_deref()
                .is_some_and(|tab| Some(tab) != resolved.tab_id)
            {
                continue;
            }
            for (kind, segments) in [
                (SegmentKind::Header, resolved.headers()),
                (SegmentKind::Footer, resolved.footers()),
                (SegmentKind::Footnote, resolved.footnotes()),
            ] {
                for segment in segments {
                    if segment.segment_id == id {
                        selected.push((tab, resolved.tab_id, kind, segment));
                    }
                }
            }
        } else if let Some(body) = resolved.body {
            collect(
                &body.content,
                tab,
                resolved.tab_id,
                false,
                &mut container,
                &mut out,
            )?;
        }
    }
    let mut segment_kind = None;
    if selection.segment_id.is_some() {
        let (tab, tab_id, kind, segment) = match selected.as_slice() {
            [] => return Err(AnchorError::SegmentNotFound),
            [one] => *one,
            _ => return Err(AnchorError::AmbiguousSegment),
        };
        segment_kind = Some(kind);
        collect(segment.content, tab, tab_id, true, &mut container, &mut out)?;
    }
    Ok((out, segment_kind))
}

fn collect<'a>(
    elements: &'a [StructuralElement],
    tab: usize,
    tab_id: Option<&str>,
    segment: bool,
    next_container: &mut usize,
    out: &mut Vec<Paragraph<'a>>,
) -> Result<(), AnchorError> {
    let container = *next_container;
    *next_container += 1;
    for (position, element) in elements.iter().enumerate() {
        if let Some(paragraph) = &element.paragraph {
            let start = if segment {
                element.start_index()
            } else {
                element.start_index.ok_or(AnchorError::InvalidIndices)?
            };
            let end = element.end_index.ok_or(AnchorError::InvalidIndices)?;
            if start < i64::from(!segment) || end <= start {
                return Err(AnchorError::InvalidIndices);
            }
            let mut runs = Vec::new();
            let mut previous_end = start;
            for inline in &paragraph.elements {
                let run_start = if segment {
                    inline.start_index.unwrap_or(0)
                } else {
                    inline.start_index.ok_or(AnchorError::InvalidIndices)?
                };
                let run_end = inline.end_index.ok_or(AnchorError::InvalidIndices)?;
                if run_start < previous_end || run_end <= run_start || run_end > end {
                    return Err(AnchorError::InvalidIndices);
                }
                previous_end = run_end;
                if let Some(text) = &inline.text_run {
                    if run_start.checked_add(utf16_len(&text.content)?) != Some(run_end) {
                        return Err(AnchorError::InvalidIndices);
                    }
                    runs.push(Run {
                        text: &text.content,
                        start: run_start,
                        end: run_end,
                        suggested: !text.suggested_insertion_ids.is_empty()
                            || !text.suggested_deletion_ids.is_empty(),
                    });
                }
            }
            if !runs
                .last()
                .is_some_and(|run| run.end == end && run.text.ends_with('\n'))
            {
                return Err(AnchorError::InvalidIndices);
            }
            let protected_newline = elements
                .get(position + 1)
                .is_none_or(|next| next.paragraph.is_none());
            out.push(Paragraph {
                tab,
                tab_id: tab_id.map(str::to_owned),
                container,
                start,
                end,
                protected_newline,
                runs,
            });
        } else if let Some(table) = &element.table {
            for row in &table.table_rows {
                for cell in &row.table_cells {
                    collect(&cell.content, tab, tab_id, segment, next_container, out)?;
                }
            }
        }
        // Tables of contents remain deliberately uneditable.
    }
    Ok(())
}

fn find(
    paragraphs: &[Paragraph<'_>],
    needle: &str,
    match_case: bool,
) -> Result<Match, AnchorError> {
    if needle.is_empty() || needle.contains(['\n', '\r']) {
        return Err(AnchorError::InvalidAnchor);
    }
    let pattern = RegexBuilder::new(&regex::escape(needle))
        .case_insensitive(!match_case)
        .build()
        .map_err(|_| AnchorError::InvalidAnchor)?;
    let mut count = 0;
    let mut unique = None;
    for (paragraph, p) in paragraphs.iter().enumerate() {
        // Group contiguous text runs. A missing index unit (image, break, etc.)
        // must not disappear and manufacture a match over its neighbours.
        let mut first = 0;
        while first < p.runs.len() {
            let mut last = first + 1;
            while last < p.runs.len() && p.runs[last - 1].end == p.runs[last].start {
                last += 1;
            }
            let runs = &p.runs[first..last];
            let text: String = runs.iter().map(|run| run.text).collect();
            let mut offset = 0;
            while let Some(found) = pattern.find_at(&text, offset) {
                let start = map_boundary(runs, found.start())?;
                let end = map_boundary(runs, found.end())?;
                let suggested = runs
                    .iter()
                    .any(|run| run.suggested && run.start < end && run.end > start);
                count += 1;
                unique = Some(Match {
                    paragraph,
                    start,
                    end,
                    suggested,
                });
                // Advance one scalar from the match start, so overlaps count too.
                offset = found.start()
                    + text[found.start()..]
                        .chars()
                        .next()
                        .ok_or(AnchorError::InvalidAnchor)?
                        .len_utf8();
            }
            first = last;
        }
    }
    match count {
        0 => Err(AnchorError::NotFound),
        1 => unique.ok_or(AnchorError::NotFound),
        count => Err(AnchorError::Ambiguous { count }),
    }
}

fn map_boundary(runs: &[Run<'_>], mut byte: usize) -> Result<i64, AnchorError> {
    for run in runs {
        if byte <= run.text.len() {
            let prefix = run.text.get(..byte).ok_or(AnchorError::InvalidIndices)?;
            return run
                .start
                .checked_add(utf16_len(prefix)?)
                .ok_or(AnchorError::InvalidIndices);
        }
        byte -= run.text.len();
    }
    Err(AnchorError::InvalidIndices) // omni-dev: coverage ignore-line reason="map_boundary is only called with a byte offset inside the text its runs concatenate, so the loop always returns first; the Err keeps the function total without a panic"
}

/// Counts the text Google actually inserts, excluding documented stripped
/// control characters and BMP private-use characters. The original text is
/// still sent verbatim; the server performs this filtering.
pub(crate) fn insertion_counts(text: &str) -> (usize, usize) {
    text.chars().filter(|ch| !matches!(ch, '\u{0000}'..='\u{0008}' | '\u{000c}'..='\u{001f}' | '\u{e000}'..='\u{f8ff}'))
        .fold((0, 0), |(chars, bytes), ch| (chars + 1, bytes + ch.len_utf8()))
}

/// Resolve insertion against a body anchor in the leased inline snapshot.
pub fn resolve_insert(
    document: &Document,
    anchor: &str,
    side: Side,
    text: &str,
    match_case: bool,
) -> Result<EditPreview, AnchorError> {
    resolve_insert_in(
        document,
        anchor,
        side,
        text,
        match_case,
        &SegmentSelection::default(),
    )
}

/// Resolve an anchored insert within an explicitly selected segment or tab bodies.
pub fn resolve_insert_in(
    document: &Document,
    anchor: &str,
    side: Side,
    text: &str,
    match_case: bool,
    selection: &SegmentSelection,
) -> Result<EditPreview, AnchorError> {
    let (chars, bytes) = insertion_counts(text);
    if chars == 0 {
        return Err(AnchorError::InvalidInsertionText);
    }
    let (paragraphs, segment_kind) = paragraphs(document, selection)?;
    let found = find(&paragraphs, anchor, match_case)?;
    if found.suggested {
        return Err(AnchorError::SuggestedContent);
    }
    let p = &paragraphs[found.paragraph];
    let index = match side {
        Side::Before => found.start,
        Side::After => found.end,
    };
    if index < p.start || index >= p.end {
        return Err(AnchorError::UnsafeRange); // omni-dev: coverage ignore-line reason="a match never contains the paragraph's closing newline (find rejects a needle with one) and collect verified the last run ends at it, so the index always lies inside the paragraph; kept as defence on the write boundary"
    }
    Ok(EditPreview {
        start_index: index,
        end_index: index,
        tab_id: p.tab_id.clone(),
        segment_id: selection.segment_id.clone(),
        segment_kind,
        paragraphs: 1,
        chars,
        bytes,
    })
}

/// Resolve deletion of one unique match, or an inclusive pair of anchors.
///
/// Cross-paragraph deletion is permitted only through contiguous plain text
/// within the same body or table cell. Protected newlines cannot be removed.
pub fn resolve_delete(
    document: &Document,
    from: &str,
    to: Option<&str>,
    match_case: bool,
) -> Result<EditPreview, AnchorError> {
    resolve_delete_in(document, from, to, match_case, &SegmentSelection::default())
}

/// Resolve an anchored delete within an explicitly selected segment or tab bodies.
pub fn resolve_delete_in(
    document: &Document,
    from: &str,
    to: Option<&str>,
    match_case: bool,
    selection: &SegmentSelection,
) -> Result<EditPreview, AnchorError> {
    let (paragraphs, segment_kind) = paragraphs(document, selection)?;
    let first = find(&paragraphs, from, match_case)?;
    let last = if let Some(to) = to {
        find(&paragraphs, to, match_case)?
    } else {
        first
    };
    let a = &paragraphs[first.paragraph];
    let b = &paragraphs[last.paragraph];
    if a.tab != b.tab
        || a.container != b.container
        || first.start > last.start
        || first.end > last.end
    {
        return Err(AnchorError::UnorderedRange);
    }
    let start = first.start;
    let end = last.end;
    let mut cursor = start;
    let mut chars = 0;
    let mut bytes = 0;
    let mut count = 0;
    for p in &paragraphs[first.paragraph..=last.paragraph] {
        if p.tab != a.tab || p.container != a.container {
            return Err(AnchorError::UnsafeRange); // omni-dev: coverage ignore-line reason="a container switch between two paragraphs of one container is always a table, and the paragraph before it has protected_newline, so the check below refuses the range before the walk reaches a foreign paragraph; kept as defence in depth"
        }
        if p.protected_newline && start < p.end && end >= p.end {
            return Err(AnchorError::UnsafeRange);
        }
        count += 1;
        for run in &p.runs {
            if run.end <= start || run.start >= end {
                continue;
            }
            if run.start > cursor {
                return Err(AnchorError::UnsafeRange);
            }
            if run.suggested {
                return Err(AnchorError::SuggestedContent);
            }
            let lo = cursor.max(run.start);
            let hi = end.min(run.end);
            let mut index = run.start;
            for ch in run.text.chars() {
                let next = index + ch.len_utf16() as i64;
                if index >= lo && next <= hi {
                    chars += 1;
                    bytes += ch.len_utf8();
                } else if index < hi && next > lo {
                    return Err(AnchorError::InvalidIndices); // omni-dev: coverage ignore-line reason="lo and hi are regex match boundaries mapped through whole runs, so neither falls inside a surrogate pair; kept as defence in the UTF-16 arithmetic"
                }
                index = next;
            }
            cursor = hi;
        }
    }
    if cursor != end {
        return Err(AnchorError::UnsafeRange); // omni-dev: coverage ignore-line reason="end is the end of the last anchor, which lies inside a run the loop above visits, and a gap before it already returned UnsafeRange, so the cursor always reaches it; kept as defence in depth"
    }
    Ok(EditPreview {
        start_index: start,
        end_index: end,
        tab_id: a.tab_id.clone(),
        segment_id: selection.segment_id.clone(),
        segment_kind,
        paragraphs: count,
        chars,
        bytes,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    fn paragraph(start: i64, runs: &[&str]) -> Value {
        let mut index = start;
        let elements: Vec<Value> = runs
            .iter()
            .map(|text| {
                let end = index + text.encode_utf16().count() as i64;
                let element =
                    json!({"startIndex": index, "endIndex": end, "textRun": {"content": text}});
                index = end;
                element
            })
            .collect();
        json!({"startIndex": start, "endIndex": index, "paragraph": {"elements": elements}})
    }

    fn doc(content: Vec<Value>) -> Document {
        serde_json::from_value(json!({"body": {"content": content}})).unwrap()
    }

    fn selection(id: &str) -> SegmentSelection {
        SegmentSelection {
            segment_id: Some(id.into()),
            tab_id: None,
        }
    }

    #[test]
    fn all_segment_families_use_zero_based_utf16_and_ignore_other_content() {
        for (map, kind) in [
            ("headers", SegmentKind::Header),
            ("footers", SegmentKind::Footer),
            ("footnotes", SegmentKind::Footnote),
        ] {
            let mut p = paragraph(0, &["😀 a", "nchor\n"]);
            p.as_object_mut().unwrap().remove("startIndex");
            p["paragraph"]["elements"][0]
                .as_object_mut()
                .unwrap()
                .remove("startIndex");
            let document: Document = serde_json::from_value(json!({
                "body": {"content": [paragraph(1, &["anchor anchor\n"])]},
                map: {"s": {"content": [p]}, "other": {"content": [{"paragraph": {}}]}}
            }))
            .unwrap();
            let edit = resolve_delete_in(&document, "anchor", None, true, &selection("s")).unwrap();
            assert_eq!(
                (edit.start_index, edit.end_index, edit.chars, edit.bytes),
                (3, 9, 6, 6)
            );
            assert_eq!(edit.segment_id.as_deref(), Some("s"));
            assert_eq!(edit.segment_kind, Some(kind));
            assert_eq!(
                resolve_insert_in(&document, "😀", Side::Before, "x", true, &selection("s"))
                    .unwrap()
                    .start_index,
                0
            );
            assert_eq!(
                resolve_delete(&document, "anchor", None, true),
                Err(AnchorError::Ambiguous { count: 2 })
            );
        }
    }

    #[test]
    fn segment_identity_is_resolved_before_anchor_uniqueness_in_child_tabs() {
        let document: Document = serde_json::from_value(json!({"tabs": [{
            "tabProperties": {"tabId": "parent"},
            "documentTab": {"headers": {"s": {"content": [paragraph(0, &["other\n"])]}}},
            "childTabs": [{"tabProperties": {"tabId": "child"}, "documentTab": {"footnotes": {"s": {"content": [paragraph(0, &["anchor\n"])]}}}}]
        }]})).unwrap();
        assert_eq!(
            resolve_delete_in(&document, "anchor", None, true, &selection("s")),
            Err(AnchorError::AmbiguousSegment)
        );
        let mut selected = selection("s");
        selected.tab_id = Some("child".into());
        let edit = resolve_delete_in(&document, "anchor", None, true, &selected).unwrap();
        assert_eq!(edit.tab_id.as_deref(), Some("child"));
        assert_eq!(edit.segment_kind, Some(SegmentKind::Footnote));
        selected.tab_id = Some("missing".into());
        assert_eq!(
            resolve_delete_in(&document, "anchor", None, true, &selected),
            Err(AnchorError::SegmentNotFound)
        );
        for invalid in [
            selection(""),
            SegmentSelection {
                segment_id: None,
                tab_id: Some("child".into()),
            },
            SegmentSelection {
                segment_id: Some("s".into()),
                tab_id: Some(String::new()),
            },
        ] {
            assert_eq!(
                resolve_delete_in(&document, "anchor", None, true, &invalid),
                Err(AnchorError::InvalidSelection)
            );
        }
    }

    #[test]
    fn selected_segments_preserve_suggestion_and_structural_guards() {
        let mut suggested = paragraph(0, &["anchor\n"]);
        suggested["paragraph"]["elements"][0]["textRun"]["suggestedDeletionIds"] =
            json!(["suggestion"]);
        let document: Document =
            serde_json::from_value(json!({"headers": {"s": {"content": [suggested]}}})).unwrap();
        assert_eq!(
            resolve_delete_in(&document, "anchor", None, true, &selection("s")),
            Err(AnchorError::SuggestedContent)
        );
        assert_eq!(
            resolve_insert_in(&document, "anchor", Side::After, "x", true, &selection("s")),
            Err(AnchorError::SuggestedContent)
        );
        let document: Document = serde_json::from_value(json!({"footers": {"s": {"content": [
            paragraph(0, &["first\n"]), {"startIndex": 6, "endIndex": 7, "sectionBreak": {}}, paragraph(7, &["last\n"])
        ]}}})).unwrap();
        assert_eq!(
            resolve_delete_in(&document, "first", Some("last"), true, &selection("s")),
            Err(AnchorError::UnsafeRange)
        );
        assert_eq!(
            resolve_delete_in(&document, "last\n", None, true, &selection("s")),
            Err(AnchorError::InvalidAnchor)
        );
        let edit = resolve_delete_in(&document, "last", None, true, &selection("s")).unwrap();
        assert_eq!(edit.end_index, 11); // The final newline at 11 survives.
    }

    #[test]
    fn segment_table_cells_and_inline_gaps_cannot_be_crossed() {
        let gap = json!({"endIndex": 4, "paragraph": {"elements": [
            {"endIndex": 1, "textRun": {"content": "a"}},
            {"startIndex": 1, "endIndex": 2, "footnoteReference": {"footnoteId": "note"}},
            {"startIndex": 2, "endIndex": 4, "textRun": {"content": "b\n"}}
        ]}});
        let document: Document =
            serde_json::from_value(json!({"footnotes": {"s": {"content": [gap]}}})).unwrap();
        assert_eq!(
            resolve_delete_in(&document, "ab", None, true, &selection("s")),
            Err(AnchorError::NotFound)
        );
        assert_eq!(
            resolve_delete_in(&document, "a", Some("b"), true, &selection("s")),
            Err(AnchorError::UnsafeRange)
        );
        let document: Document = serde_json::from_value(json!({"headers": {"s": {"content": [
            paragraph(0, &["before\n"]), {"startIndex": 7, "endIndex": 30, "table": {"tableRows": [{"tableCells": [
                {"content": [paragraph(9, &["first\n"])]}, {"content": [paragraph(20, &["last\n"])]}
            ]}]}}
        ]}}})).unwrap();
        assert_eq!(
            resolve_delete_in(&document, "first", Some("last"), true, &selection("s")),
            Err(AnchorError::UnorderedRange)
        );
        assert_eq!(
            resolve_delete_in(&document, "before", Some("first"), true, &selection("s")),
            Err(AnchorError::UnorderedRange)
        );
        assert_eq!(
            resolve_delete_in(&document, "first", None, true, &selection("s"))
                .unwrap()
                .end_index,
            14
        );
    }

    #[test]
    fn astral_characters_and_split_runs_use_server_utf16_indices() {
        let document = doc(vec![paragraph(1, &["😀 a", "nchor\n"])]);
        let edit = resolve_insert(&document, "anchor", Side::After, "x", true).unwrap();
        assert_eq!((edit.start_index, edit.end_index), (10, 10));
        let edit = resolve_delete(&document, "anchor", None, true).unwrap();
        assert_eq!((edit.start_index, edit.end_index, edit.chars), (4, 10, 6));
    }

    #[test]
    fn unicode_case_folding_preserves_original_boundaries_and_matches_literals() {
        let document = doc(vec![paragraph(1, &["😀 \u{212a}.[İ]\n"])]);
        let edit = resolve_delete(&document, "k.[İ]", None, false).unwrap();
        assert_eq!(
            (edit.start_index, edit.end_index, edit.chars, edit.bytes),
            (4, 9, 5, 8)
        );
        assert_eq!(
            resolve_delete(&document, "k.[İ]", None, true),
            Err(AnchorError::NotFound)
        );
    }

    #[test]
    fn overlapping_matches_are_ambiguous_and_paragraph_breaks_are_not_anchors() {
        let document = doc(vec![paragraph(1, &["aaa\n"])]);
        assert_eq!(
            resolve_delete(&document, "aa", None, true),
            Err(AnchorError::Ambiguous { count: 2 })
        );
        assert_eq!(
            resolve_delete(&document, "a\na", None, true),
            Err(AnchorError::InvalidAnchor)
        );
        assert_eq!(
            resolve_delete(&document, "", None, true),
            Err(AnchorError::InvalidAnchor)
        );
        assert_eq!(
            resolve_delete(&document, "absent", None, true),
            Err(AnchorError::NotFound)
        );
    }

    #[test]
    fn inline_objects_do_not_shift_later_anchors_or_manufacture_matches() {
        let document = doc(vec![
            json!({"startIndex": 1, "endIndex": 5, "paragraph": {"elements": [
                {"startIndex": 1, "endIndex": 2, "textRun": {"content": "a"}},
                {"startIndex": 2, "endIndex": 3, "inlineObjectElement": {"inlineObjectId": "img"}},
                {"startIndex": 3, "endIndex": 5, "textRun": {"content": "b\n"}}
            ]}}),
        ]);
        assert_eq!(
            resolve_insert(&document, "b", Side::Before, "x", true)
                .unwrap()
                .start_index,
            3
        );
        assert_eq!(
            resolve_delete(&document, "ab", None, true),
            Err(AnchorError::NotFound)
        );
        assert_eq!(
            resolve_delete(&document, "a", Some("b"), true),
            Err(AnchorError::UnsafeRange)
        );
    }

    #[test]
    fn incomplete_or_inconsistent_indices_are_refused() {
        let mut p = paragraph(1, &["anchor\n"]);
        p["paragraph"]["elements"][0]
            .as_object_mut()
            .unwrap()
            .remove("startIndex");
        assert_eq!(
            resolve_delete(&doc(vec![p]), "anchor", None, true),
            Err(AnchorError::InvalidIndices)
        );
        let mut p = paragraph(1, &["😀 anchor\n"]);
        p["paragraph"]["elements"][0]["endIndex"] = json!(8);
        assert_eq!(
            resolve_delete(&doc(vec![p]), "anchor", None, true),
            Err(AnchorError::InvalidIndices)
        );
    }

    #[test]
    fn paragraph_and_run_indices_that_do_not_nest_are_refused() {
        type Mutation = fn(&mut Value);
        let cases: [(&str, Mutation); 7] = [
            ("paragraph starts before the body", |p| {
                p["startIndex"] = json!(0);
            }),
            ("paragraph is empty", |p| {
                p["endIndex"] = json!(1);
            }),
            ("run overlaps the previous run", |p| {
                p["paragraph"]["elements"][1]["startIndex"] = json!(2);
            }),
            ("run is empty", |p| {
                p["paragraph"]["elements"][1]["endIndex"] = json!(4);
            }),
            ("run overruns its paragraph", |p| {
                p["endIndex"] = json!(7);
            }),
            ("last run is not newline-terminated", |p| {
                p["paragraph"]["elements"][1]["textRun"]["content"] = json!("def");
                p["paragraph"]["elements"][1]["endIndex"] = json!(7);
                p["endIndex"] = json!(7);
            }),
            ("last run stops short of the paragraph end", |p| {
                p["endIndex"] = json!(9);
            }),
        ];
        for (name, mutate) in cases {
            let mut p = paragraph(1, &["abc", "def\n"]);
            mutate(&mut p);
            assert_eq!(
                resolve_delete(&doc(vec![p]), "abc", None, true),
                Err(AnchorError::InvalidIndices),
                "{name}"
            );
        }
        let mut p = paragraph(1, &["abc\n"]);
        p["paragraph"]["elements"] = json!([]);
        assert_eq!(
            resolve_delete(&doc(vec![p]), "abc", None, true),
            Err(AnchorError::InvalidIndices),
            "a paragraph with no runs"
        );
    }

    #[test]
    fn suggestions_in_anchors_and_inside_ranges_are_refused() {
        for field in ["suggestedInsertionIds", "suggestedDeletionIds"] {
            let mut p = paragraph(1, &["start ", "pending", " finish\n"]);
            p["paragraph"]["elements"][1]["textRun"][field] = json!(["suggestion"]);
            let document = doc(vec![p]);
            assert_eq!(
                resolve_insert(&document, "pending", Side::After, "x", true),
                Err(AnchorError::SuggestedContent)
            );
            assert_eq!(
                resolve_delete(&document, "start", Some("finish"), true),
                Err(AnchorError::SuggestedContent)
            );
            assert!(resolve_delete(&document, "start", None, true).is_ok());
        }
    }

    #[test]
    fn safe_ranges_span_paragraphs_and_preserve_the_last_newline() {
        let document = doc(vec![paragraph(1, &["start\n"]), paragraph(7, &["end\n"])]);
        let edit = resolve_delete(&document, "start", Some("end"), true).unwrap();
        assert_eq!(
            (
                edit.start_index,
                edit.end_index,
                edit.chars,
                edit.paragraphs
            ),
            (1, 10, 9, 2)
        );
        assert_eq!(
            resolve_delete(&document, "end", Some("start"), true),
            Err(AnchorError::UnorderedRange)
        );
        assert_eq!(
            resolve_insert(&document, "end", Side::After, "x", true)
                .unwrap()
                .start_index,
            10
        );
        assert_eq!(
            resolve_insert(&document, "end\n", Side::After, "x", true),
            Err(AnchorError::InvalidAnchor)
        );
    }

    #[test]
    fn tables_cells_and_tabs_have_independent_boundaries() {
        let table = json!({"startIndex": 8, "endIndex": 30, "table": {"tableRows": [{"tableCells": [
            {"content": [paragraph(11, &["cell\n"])]},
            {"content": [paragraph(18, &["other\n"])]}
        ]}]}});
        let document = doc(vec![
            paragraph(1, &["before\n"]),
            table,
            paragraph(30, &["after\n"]),
        ]);
        assert_eq!(
            resolve_insert(&document, "cell", Side::After, "x", true)
                .unwrap()
                .start_index,
            15
        );
        assert_eq!(
            resolve_delete(&document, "cell", Some("other"), true),
            Err(AnchorError::UnorderedRange)
        );
        assert_eq!(
            resolve_delete(&document, "before", Some("after"), true),
            Err(AnchorError::UnsafeRange)
        );
        let tabbed: Document = serde_json::from_value(json!({"tabs": [
            {"tabProperties": {"tabId": "t1"}, "documentTab": {"body": {"content": [paragraph(1, &["start\n"])]}},
             "childTabs": [{"tabProperties": {"tabId": "t2"}, "documentTab": {"body": {"content": [paragraph(1, &["end\n"])]}}}]}
        ]})).unwrap();
        assert_eq!(
            resolve_insert(&tabbed, "end", Side::Before, "x", true)
                .unwrap()
                .tab_id
                .as_deref(),
            Some("t2")
        );
        assert_eq!(
            resolve_delete(&tabbed, "start", Some("end"), true),
            Err(AnchorError::UnorderedRange)
        );
    }

    #[test]
    fn non_body_and_table_of_contents_text_is_not_addressable() {
        let document: Document = serde_json::from_value(json!({
            "headers": {"h1": {"content": [paragraph(1, &["header\n"])]}},
            "body": {"content": [{"tableOfContents": {"content": [paragraph(1, &["toc\n"])]}}]}
        }))
        .unwrap();
        for needle in ["header", "toc"] {
            assert_eq!(
                resolve_delete(&document, needle, None, true),
                Err(AnchorError::NotFound)
            );
        }
    }

    proptest! {
        #[test]
        fn random_unicode_prefixes_and_run_splits_preserve_utf16_offsets(
            prefix in prop::collection::vec(prop_oneof![Just('😀'), Just('𠀀'), Just('é'), Just('x')], 0..80),
            split in 0usize..100,
        ) {
            let prefix: String = prefix.into_iter().collect();
            let text = format!("{prefix}ANCHOR\n");
            let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
            let byte = boundaries[split % boundaries.len()];
            let runs: Vec<&str> = [&text[..byte], &text[byte..]].into_iter().filter(|s| !s.is_empty()).collect();
            let document = doc(vec![paragraph(1, &runs)]);
            let edit = resolve_delete(&document, "ANCHOR", None, true).unwrap();
            prop_assert_eq!(edit.start_index, 1 + prefix.encode_utf16().count() as i64);
            prop_assert_eq!(edit.end_index, edit.start_index + 6);
        }
    }
    #[test]
    fn insertion_counts_exclude_characters_stripped_by_google() {
        let document = doc(vec![paragraph(1, &["anchor\n"])]);
        let edit =
            resolve_insert(&document, "anchor", Side::After, "\0😀\r\u{e000}x\n", true).unwrap();
        assert_eq!((edit.chars, edit.bytes), (3, 6));
        assert_eq!(
            resolve_insert(&document, "anchor", Side::After, "\0\r", true),
            Err(AnchorError::InvalidInsertionText)
        );
    }

    #[test]
    fn missing_empty_or_duplicate_tab_ids_are_refused() {
        for ids in [vec![None], vec![Some("")], vec![Some("same"), Some("same")]] {
            let tabs: Vec<Value> = ids.into_iter().map(|id| json!({
                "tabProperties": {"tabId": id}, "documentTab": {"body": {"content": [paragraph(1, &["anchor\n"])]}}
            })).collect();
            let document: Document = serde_json::from_value(json!({"tabs": tabs})).unwrap();
            assert_eq!(
                resolve_insert(&document, "anchor", Side::Before, "x", true),
                Err(AnchorError::InvalidIndices)
            );
        }
    }
}
