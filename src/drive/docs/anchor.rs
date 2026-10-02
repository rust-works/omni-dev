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
    /// No body text matches.
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

fn paragraphs(document: &Document) -> Result<Vec<Paragraph<'_>>, AnchorError> {
    let mut out = Vec::new();
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
        if let Some(body) = resolved.body {
            collect(
                &body.content,
                tab,
                resolved.tab_id,
                &mut container,
                &mut out,
            )?;
        }
    }
    Ok(out)
}

fn collect<'a>(
    elements: &'a [StructuralElement],
    tab: usize,
    tab_id: Option<&str>,
    next_container: &mut usize,
    out: &mut Vec<Paragraph<'a>>,
) -> Result<(), AnchorError> {
    let container = *next_container;
    *next_container += 1;
    for (position, element) in elements.iter().enumerate() {
        if let Some(paragraph) = &element.paragraph {
            let start = element.start_index.ok_or(AnchorError::InvalidIndices)?;
            let end = element.end_index.ok_or(AnchorError::InvalidIndices)?;
            if start < 1 || end <= start {
                return Err(AnchorError::InvalidIndices);
            }
            let mut runs = Vec::new();
            let mut previous_end = start;
            for inline in &paragraph.elements {
                let run_start = inline.start_index.ok_or(AnchorError::InvalidIndices)?;
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
                    collect(&cell.content, tab, tab_id, next_container, out)?;
                }
            }
        }
        // Tables of contents and non-body segments are deliberately not editable.
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
    Err(AnchorError::InvalidIndices)
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
    let (chars, bytes) = insertion_counts(text);
    if chars == 0 {
        return Err(AnchorError::InvalidInsertionText);
    }
    let paragraphs = paragraphs(document)?;
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
        return Err(AnchorError::UnsafeRange);
    }
    Ok(EditPreview {
        start_index: index,
        end_index: index,
        tab_id: p.tab_id.clone(),
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
    let paragraphs = paragraphs(document)?;
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
            return Err(AnchorError::UnsafeRange);
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
                    return Err(AnchorError::InvalidIndices);
                }
                index = next;
            }
            cursor = hi;
        }
    }
    if cursor != end {
        return Err(AnchorError::UnsafeRange);
    }
    Ok(EditPreview {
        start_index: start,
        end_index: end,
        tab_id: a.tab_id.clone(),
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
