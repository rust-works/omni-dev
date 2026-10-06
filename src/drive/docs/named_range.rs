//! Bounded named-range mutations resolved against the revision-controlled snapshot.

use serde::Serialize;

use super::anchor::insertion_counts;
use super::types::{Document, Range, StructuralElement};
use super::write_types::{
    CreateNamedRangeRequest, DocsRequest, NamedRangeIdRequest, ReplaceNamedRangeContentRequest,
    TabsCriteria,
};

/// Explicit scope: absent tab is legacy-only; absent segment is the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// A server tab ID, or legacy single-body scope.
    pub tab_id: Option<String>,
    /// Header/footer/footnote ID, or body scope.
    pub segment_id: Option<String>,
}

/// A typed operation with no name-based fan-out selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Create metadata over a bounded UTF-16 span.
    Create {
        /// Label, 1–256 UTF-16 units.
        name: String,
        /// Inclusive start in the snapshot.
        start_index: i64,
        /// Exclusive end in the snapshot.
        end_index: i64,
    },
    /// Remove only the label and its span metadata.
    Delete {
        /// Stable named-range ID.
        id: String,
    },
    /// Replace one plain-text span; empty text removes its content.
    Replace {
        /// Stable named-range ID.
        id: String,
        /// Replacement text.
        text: String,
    },
}

impl Mutation {
    /// Pure validation before any network read.
    pub(crate) fn validate(&self, scope: &Scope) -> Result<(), String> {
        if scope.tab_id.as_ref().is_some_and(String::is_empty)
            || scope.segment_id.as_ref().is_some_and(String::is_empty)
        {
            return Err("scope IDs cannot be empty".into());
        }
        match self {
            Self::Create {
                name,
                start_index,
                end_index,
            } => {
                if !(1..=256).contains(&name.encode_utf16().count()) {
                    return Err("named-range name must be 1–256 UTF-16 units".into());
                }
                if *start_index < 0 || end_index <= start_index {
                    return Err("named-range indices must describe a nonempty bounded span".into());
                }
            }
            Self::Delete { id } | Self::Replace { id, .. } if id.is_empty() => {
                return Err("named-range ID cannot be empty".into());
            }
            Self::Replace { text, .. } if !text.is_empty() && insertion_counts(text).0 == 0 => {
                return Err("replacement becomes empty after Docs strips unsupported characters; use explicit empty text for deletion".into());
            }
            _ => {}
        }
        Ok(())
    }
}

/// Refusal metadata containing no document prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Error {
    /// No unique tab/segment matching the explicit scope.
    InvalidScope,
    /// The ID is absent or occurs more than once within the selected tab.
    UnresolvedId,
    /// Some spans escape the selected tab or segment.
    ScopeMismatch,
    /// Replacement would remove additional discontinuous spans.
    DiscontinuousRange,
    /// Indices do not identify safe contiguous plain text within one paragraph.
    UnsafeRange,
    /// The selected text has pending content suggestions.
    SuggestedContent,
}

/// Distinguishes removing a label from removing its text in structured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    /// Adds only named-range metadata.
    CreateMetadata,
    /// Removes only named-range metadata.
    DeleteMetadata,
    /// Replaces existing text with nonempty content.
    ReplaceContent,
    /// Removes content with an explicitly empty replacement.
    DeleteContent,
}

/// Metadata-only description of the complete selected effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    /// Explicit metadata/content distinction, independent of the generic status.
    pub effect: Effect,
    /// Stable ID; unknown before a create reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub named_range_id: Option<String>,
    /// Complete span set; never elided in structured output.
    pub ranges: Vec<Range>,
    /// Unicode scalars whose content is removed by replacement.
    pub removed_chars: usize,
    /// UTF-8 bytes whose content is removed by replacement.
    pub removed_bytes: usize,
    /// Unicode scalars inserted by replacement.
    pub inserted_chars: usize,
    /// UTF-8 bytes inserted by replacement.
    pub inserted_bytes: usize,
}

/// Build the wire request and preview together from the same inline snapshot.
pub(crate) fn resolve(
    document: &Document,
    scope: &Scope,
    mutation: &Mutation,
) -> Result<(DocsRequest, Preview), Error> {
    let tabs = document.resolved_tabs();
    // Modern responses must have unique, nonempty identities; never silently
    // reinterpret a missing tab ID as the API's first-tab default.
    let mut ids = std::collections::HashSet::new();
    if !document.tabs.is_empty()
        && tabs
            .iter()
            .any(|tab| tab.tab_id.is_none_or(|id| id.is_empty() || !ids.insert(id)))
    {
        return Err(Error::InvalidScope);
    }
    let selected: Vec<_> = tabs
        .iter()
        .filter(|tab| tab.tab_id == scope.tab_id.as_deref())
        .collect();
    let [tab] = selected.as_slice() else {
        return Err(Error::InvalidScope);
    };
    let content = if let Some(segment_id) = scope.segment_id.as_deref() {
        let segments: Vec<_> = tab
            .headers()
            .into_iter()
            .chain(tab.footers())
            .chain(tab.footnotes())
            .filter(|segment| segment.segment_id == segment_id)
            .collect();
        let [segment] = segments.as_slice() else {
            return Err(Error::InvalidScope);
        };
        segment.content
    } else {
        &tab.body.ok_or(Error::InvalidScope)?.content
    };
    let mut preview = Preview {
        effect: match mutation {
            Mutation::Create { .. } => Effect::CreateMetadata,
            Mutation::Delete { .. } => Effect::DeleteMetadata,
            Mutation::Replace { text, .. } if text.is_empty() => Effect::DeleteContent,
            Mutation::Replace { .. } => Effect::ReplaceContent,
        },
        named_range_id: None,
        ranges: vec![],
        removed_chars: 0,
        removed_bytes: 0,
        inserted_chars: 0,
        inserted_bytes: 0,
    };
    if let Mutation::Create {
        name,
        start_index,
        end_index,
    } = mutation
    {
        validate_text_range(
            content,
            *start_index,
            *end_index,
            scope.segment_id.is_some(),
        )?;
        let range = Range {
            start_index: Some(*start_index),
            end_index: Some(*end_index),
            tab_id: scope.tab_id.clone(),
            segment_id: scope.segment_id.clone(),
        };
        preview.ranges.push(range.clone());
        return Ok((
            DocsRequest::CreateNamedRange(CreateNamedRangeRequest {
                name: name.clone(),
                range,
            }),
            preview,
        ));
    }
    let id = match mutation {
        Mutation::Delete { id } | Mutation::Replace { id, .. } => id,
        Mutation::Create { .. } => return Err(Error::UnsafeRange), // patchcov: coverage ignore-line reason="the Create branch above always returns, so a Create mutation never reaches the ID lookup; the arm exists solely for exhaustiveness"
    };
    let matches: Vec<_> = tab
        .named_ranges
        .values()
        .flat_map(|group| &group.named_ranges)
        .filter(|range| range.named_range_id.as_deref() == Some(id))
        .collect();
    let [named] = matches.as_slice() else {
        return Err(Error::UnresolvedId);
    };
    if named.ranges.is_empty() {
        return Err(Error::UnsafeRange);
    }
    for range in &named.ranges {
        // With includeTabsContent, a range's missing tabId inherits its
        // DocumentTab owner. An explicit conflicting ID is never accepted.
        if range
            .tab_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .is_some_and(|id| Some(id) != tab.tab_id)
            || range.segment_id.as_deref().filter(|id| !id.is_empty())
                != scope.segment_id.as_deref()
        {
            return Err(Error::ScopeMismatch);
        }
        let start = range.start_index.unwrap_or(0); // proto3 omits zero
        let end = range.end_index.ok_or(Error::UnsafeRange)?;
        if start < 0 || end <= start {
            return Err(Error::UnsafeRange);
        }
        preview.ranges.push(Range {
            tab_id: scope.tab_id.clone(),
            segment_id: scope.segment_id.clone(),
            start_index: Some(start),
            end_index: Some(end),
        });
    }
    preview.named_range_id = Some(id.clone());
    let target = NamedRangeIdRequest {
        named_range_id: id.clone(),
        tabs_criteria: scope.tab_id.as_ref().map(|id| TabsCriteria {
            tab_ids: [id.clone()],
        }),
    };
    match mutation {
        Mutation::Delete { .. } => Ok((DocsRequest::DeleteNamedRange(target), preview)),
        Mutation::Replace { text, .. } => {
            let [range] = preview.ranges.as_slice() else {
                return Err(Error::DiscontinuousRange);
            };
            let (chars, bytes) = validate_text_range(
                content,
                range.start_index.ok_or(Error::UnsafeRange)?,
                range.end_index.ok_or(Error::UnsafeRange)?,
                scope.segment_id.is_some(),
            )?;
            preview.removed_chars = chars;
            preview.removed_bytes = bytes;
            (preview.inserted_chars, preview.inserted_bytes) = insertion_counts(text);
            Ok((
                DocsRequest::ReplaceNamedRangeContent(ReplaceNamedRangeContentRequest {
                    target,
                    text: text.clone(),
                }),
                preview,
            ))
        }
        Mutation::Create { .. } => Err(Error::UnsafeRange), // patchcov: coverage ignore-line reason="the Create branch above always returns, so a Create mutation never reaches this match; the arm exists solely for exhaustiveness"
    }
}

// Deliberately narrower than anchor deletion: a named replacement must not
// merge paragraphs or remove non-text objects. Traverse tables only to find
// an individual cell paragraph; generated TOCs remain excluded.
fn validate_text_range(
    elements: &[StructuralElement],
    start: i64,
    end: i64,
    non_body: bool,
) -> Result<(usize, usize), Error> {
    if start < i64::from(!non_body) || end <= start {
        return Err(Error::UnsafeRange);
    }
    let mut candidates = Vec::new();
    matching_paragraphs(elements, start, end, &mut candidates);
    let [element] = candidates.as_slice() else {
        return Err(Error::UnsafeRange);
    };
    let paragraph = element.paragraph.as_ref().ok_or(Error::UnsafeRange)?;
    let p_start = element.start_index.unwrap_or(0);
    let p_end = element.end_index.ok_or(Error::UnsafeRange)?;
    if p_start < i64::from(!non_body) || p_end <= p_start {
        return Err(Error::UnsafeRange);
    }
    let mut previous = p_start;
    let mut cursor = start;
    let mut chars = 0;
    let mut bytes = 0;
    for inline in &paragraph.elements {
        let lo = inline.start_index.unwrap_or(0);
        let hi = inline.end_index.ok_or(Error::UnsafeRange)?;
        if lo < previous || hi <= lo || hi > p_end {
            return Err(Error::UnsafeRange);
        }
        previous = hi;
        let Some(run) = &inline.text_run else {
            if lo < end && hi > start {
                return Err(Error::UnsafeRange);
            }
            continue;
        };
        if lo.checked_add(
            i64::try_from(run.content.encode_utf16().count()).map_err(|_| Error::UnsafeRange)?,
        ) != Some(hi)
        {
            return Err(Error::UnsafeRange);
        }
        if hi <= start || lo >= end {
            continue;
        }
        if lo > cursor {
            return Err(Error::UnsafeRange);
        }
        if !run.suggested_insertion_ids.is_empty() || !run.suggested_deletion_ids.is_empty() {
            return Err(Error::SuggestedContent);
        }
        let mut index = lo;
        for ch in run.content.chars() {
            let next = index + ch.len_utf16() as i64;
            if index < end && next > start {
                if index < start || next > end || ch == '\n' {
                    return Err(Error::UnsafeRange);
                }
                chars += 1;
                bytes += ch.len_utf8();
            }
            index = next;
        }
        cursor = hi.min(end);
    }
    if cursor == end
        && paragraph
            .elements
            .last()
            .and_then(|e| e.text_run.as_ref())
            .is_some_and(|run| previous == p_end && run.content.ends_with('\n'))
    {
        return Ok((chars, bytes));
    }
    Err(Error::UnsafeRange)
}

fn matching_paragraphs<'a>(
    elements: &'a [StructuralElement],
    start: i64,
    end: i64,
    out: &mut Vec<&'a StructuralElement>,
) {
    for element in elements {
        if element.paragraph.is_some()
            && element.start_index.unwrap_or(0) <= start
            && element.end_index.is_some_and(|p_end| end < p_end)
        {
            out.push(element);
        }
        if let Some(table) = &element.table {
            for row in &table.table_rows {
                for cell in &row.table_cells {
                    matching_paragraphs(&cell.content, start, end, out);
                }
            }
        }
    }
}

impl Preview {
    /// Bounded human rendering; structured outcomes retain the complete span list.
    pub(crate) fn describe_spans(&self) -> String {
        let mut spans = self
            .ranges
            .iter()
            .take(50)
            .map(|range| {
                format!(
                    "[{}, {}) UTF-16, tab {}, segment {}",
                    range.start_index.unwrap_or(0),
                    range.end_index.unwrap_or(0),
                    range.tab_id.as_deref().unwrap_or("legacy"),
                    range.segment_id.as_deref().unwrap_or("body")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if self.ranges.len() > 50 {
            spans.push_str(&format!("; {} more span(s)", self.ranges.len() - 50));
        }
        spans
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn paragraph(start: i64, text: &str) -> Value {
        let end = start + text.encode_utf16().count() as i64;
        json!({"startIndex": start, "endIndex": end, "paragraph": {"elements": [
            {"startIndex": start, "endIndex": end, "textRun": {"content": text}}
        ]}})
    }

    fn fixture(segment: Option<&str>, spans: Vec<Value>) -> (Document, Scope) {
        let mut tab = json!({"body": {"content": [paragraph(1, "😀 label\n")]},
        "namedRanges": {"label": {"name": "label", "namedRanges": [
            {"namedRangeId": "nr", "name": "label", "ranges": spans}
        ]}}});
        if let Some(segment) = segment {
            tab["headers"] = json!({segment: {"content": [paragraph(0, "😀 label\n")]}});
        }
        let document = serde_json::from_value(json!({"tabs": [{"tabProperties": {"tabId": "parent"},
            "documentTab": {}, "childTabs": [{"tabProperties": {"tabId": "child"}, "documentTab": tab}]}]})).unwrap();
        (
            document,
            Scope {
                tab_id: Some("child".into()),
                segment_id: segment.map(str::to_owned),
            },
        )
    }

    fn replace() -> Mutation {
        Mutation::Replace {
            id: "nr".into(),
            text: "😀".into(),
        }
    }

    #[test]
    fn nested_tab_and_segment_wire_shapes_are_bounded_and_leased() {
        for segment in [None, Some("h")] {
            let start = i64::from(segment.is_none());
            let (document, scope) = fixture(
                segment,
                vec![json!({"startIndex": start, "endIndex": start + 2, "segmentId": segment})],
            );
            for mutation in [
                Mutation::Create {
                    name: "label".into(),
                    start_index: start,
                    end_index: start + 2,
                },
                Mutation::Delete { id: "nr".into() },
                replace(),
            ] {
                let (request, preview) = resolve(&document, &scope, &mutation).unwrap();
                let wire = serde_json::to_value(
                    super::super::write_types::BatchUpdateDocumentRequest::new(request, "r"),
                )
                .unwrap();
                assert_eq!(wire["writeControl"], json!({"requiredRevisionId": "r"}));
                assert_eq!(wire["requests"].as_array().unwrap().len(), 1);
                assert_eq!(preview.ranges[0].tab_id.as_deref(), Some("child"));
                match mutation {
                    Mutation::Create { .. } => {
                        assert_eq!(
                            wire["requests"][0]["createNamedRange"]["range"]["tabId"],
                            "child"
                        );
                        assert_eq!(
                            wire["requests"][0]["createNamedRange"]["range"]["segmentId"],
                            json!(segment)
                        );
                    }
                    Mutation::Delete { .. } | Mutation::Replace { .. } => {
                        let op = if matches!(mutation, Mutation::Delete { .. }) {
                            "deleteNamedRange"
                        } else {
                            "replaceNamedRangeContent"
                        };
                        assert_eq!(wire["requests"][0][op]["namedRangeId"], "nr");
                        assert_eq!(
                            wire["requests"][0][op]["tabsCriteria"],
                            json!({"tabIds": ["child"]})
                        );
                        assert!(wire["requests"][0][op].get("name").is_none());
                        assert!(wire["requests"][0][op].get("namedRangeName").is_none());
                    }
                }
            }
            let (_, preview) = resolve(&document, &scope, &replace()).unwrap();
            assert_eq!(
                (
                    preview.removed_chars,
                    preview.removed_bytes,
                    preview.inserted_chars,
                    preview.inserted_bytes
                ),
                (1, 4, 1, 4)
            );
        }
    }

    #[test]
    fn name_limit_is_utf16_and_stripped_text_cannot_hide_deletion() {
        let scope = Scope {
            tab_id: None,
            segment_id: None,
        };
        for (name, valid) in [
            (String::new(), false),
            ("😀".repeat(128), true),
            ("😀".repeat(129), false),
        ] {
            assert_eq!(
                Mutation::Create {
                    name,
                    start_index: 1,
                    end_index: 2
                }
                .validate(&scope)
                .is_ok(),
                valid
            );
        }
        assert!(Mutation::Replace {
            id: "nr".into(),
            text: "\0\u{e000}".into()
        }
        .validate(&scope)
        .is_err());
        assert!(Mutation::Replace {
            id: "nr".into(),
            text: String::new()
        }
        .validate(&scope)
        .is_ok());
        assert!(Mutation::Delete { id: String::new() }
            .validate(&scope)
            .is_err());
    }

    #[test]
    fn missing_duplicate_or_conflicting_scope_and_ids_are_refused() {
        let (document, scope) = fixture(None, vec![json!({"startIndex": 1, "endIndex": 3})]);
        for bad in [
            Scope {
                tab_id: None,
                segment_id: None,
            },
            Scope {
                tab_id: Some("missing".into()),
                segment_id: None,
            },
            Scope {
                tab_id: Some("child".into()),
                segment_id: Some("missing".into()),
            },
        ] {
            assert_eq!(
                resolve(&document, &bad, &replace()),
                Err(Error::InvalidScope)
            );
        }
        let mut duplicate = document.clone();
        duplicate.tabs.push(document.tabs[0].clone());
        assert_eq!(
            resolve(&duplicate, &scope, &replace()),
            Err(Error::InvalidScope)
        );
        let mut no_identity = document.clone();
        no_identity.tabs[0].tab_properties.as_mut().unwrap().tab_id = None;
        assert_eq!(
            resolve(&no_identity, &scope, &replace()),
            Err(Error::InvalidScope)
        );
        let mut duplicate = document.clone();
        let groups = &mut duplicate.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .named_ranges;
        groups.insert("duplicate".into(), groups["label"].clone());
        assert_eq!(
            resolve(&duplicate, &scope, &replace()),
            Err(Error::UnresolvedId)
        );
        assert_eq!(
            resolve(
                &document,
                &scope,
                &Mutation::Delete {
                    id: "absent".into()
                }
            ),
            Err(Error::UnresolvedId)
        );
        for span in [
            json!({"startIndex": 1, "endIndex": 3, "tabId": "parent"}),
            json!({"startIndex": 1, "endIndex": 3, "segmentId": "h"}),
        ] {
            let (document, scope) = fixture(None, vec![span]);
            assert_eq!(
                resolve(&document, &scope, &Mutation::Delete { id: "nr".into() }),
                Err(Error::ScopeMismatch)
            );
        }
    }

    #[test]
    fn discontinuous_metadata_deletion_is_not_content_deletion() {
        let (document, scope) = fixture(
            None,
            vec![
                json!({"startIndex": 1, "endIndex": 3}),
                json!({"startIndex": 4, "endIndex": 9}),
            ],
        );
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::DiscontinuousRange)
        );
        let (request, preview) =
            resolve(&document, &scope, &Mutation::Delete { id: "nr".into() }).unwrap();
        assert!(matches!(request, DocsRequest::DeleteNamedRange(_)));
        assert_eq!(preview.ranges.len(), 2);
        assert_eq!(preview.removed_chars, 0);
    }

    #[test]
    fn numeric_spans_reject_surrogate_splits_newlines_and_invalid_bounds() {
        for (start, end) in [(1, 2), (2, 3), (0, 3), (1, 10), (1, 20), (5, 4)] {
            let (document, scope) =
                fixture(None, vec![json!({"startIndex": start, "endIndex": end})]);
            assert_eq!(
                resolve(&document, &scope, &replace()),
                Err(Error::UnsafeRange),
                "{start}..{end}"
            );
        }
        let (document, scope) = fixture(None, vec![json!({"startIndex": 1})]);
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::UnsafeRange)
        );
    }

    #[test]
    fn split_runs_keep_indices_but_objects_suggestions_and_gaps_are_refused() {
        for kind in ["split", "object", "gap", "suggested", "bad-index", "toc"] {
            let (mut document, scope) =
                fixture(None, vec![json!({"startIndex": 4, "endIndex": 9})]);
            let body = document.tabs[0].child_tabs[0]
                .document_tab
                .as_mut()
                .unwrap()
                .body
                .as_mut()
                .unwrap();
            let first = json!({"startIndex": 1, "endIndex": 6, "textRun": {"content": "😀 la"}});
            let mut second =
                json!({"startIndex": 6, "endIndex": 10, "textRun": {"content": "bel\n"}});
            match kind {
                "suggested" => second["textRun"]["suggestedInsertionIds"] = json!(["s"]),
                "gap" => second["startIndex"] = json!(7),
                "bad-index" => second["endIndex"] = json!(11),
                "object" => {
                    second = json!({"startIndex": 6, "endIndex": 10, "inlineObjectElement": {"inlineObjectId": "o"}});
                }
                _ => {}
            }
            body.content = serde_json::from_value(json!([{"startIndex": 1, "endIndex": 10, "paragraph": {"elements": [first, second]}}])).unwrap();
            if kind == "toc" {
                body.content = serde_json::from_value(json!([{"startIndex": 1, "endIndex": 10, "tableOfContents": {"content": [paragraph(1, "😀 label\n")]}}])).unwrap();
            }
            match kind {
                "split" => assert!(resolve(&document, &scope, &replace()).is_ok()),
                "suggested" => assert_eq!(
                    resolve(&document, &scope, &replace()),
                    Err(Error::SuggestedContent)
                ),
                _ => assert_eq!(
                    resolve(&document, &scope, &replace()),
                    Err(Error::UnsafeRange)
                ),
            }
        }
    }

    #[test]
    fn legacy_scope_omits_tab_criteria_and_nonbody_proto3_zero_is_valid() {
        let document: Document = serde_json::from_value(json!({"body": {"content": [paragraph(1, "abc\n")]},
            "footnotes": {"f": {"content": [paragraph(0, "abc\n")]}},
            "namedRanges": {"label": {"namedRanges": [{"namedRangeId": "nr", "ranges": [{"endIndex": 3, "segmentId": "f"}]}]}}})).unwrap();
        let scope = Scope {
            tab_id: None,
            segment_id: Some("f".into()),
        };
        let (request, preview) = resolve(&document, &scope, &replace()).unwrap();
        assert_eq!(preview.ranges[0].start_index, Some(0));
        let wire = serde_json::to_value(request).unwrap();
        assert!(wire["replaceNamedRangeContent"]
            .get("tabsCriteria")
            .is_none());
    }
    #[test]
    fn table_cells_preserve_suggestion_refusals_and_overlapping_indices_fail_closed() {
        let (mut document, scope) = fixture(None, vec![json!({"startIndex": 3, "endIndex": 5})]);
        let mut p = paragraph(3, "😀 label\n");
        let body = document.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .body
            .as_mut()
            .unwrap();
        body.content = serde_json::from_value(json!([{"startIndex": 1, "endIndex": 14,
            "table": {"tableRows": [{"tableCells": [{"content": [p.clone()]}]}]}}]))
        .unwrap();
        assert!(resolve(&document, &scope, &replace()).is_ok());
        p["paragraph"]["elements"][0]["textRun"]["suggestedDeletionIds"] = json!(["suggestion"]);
        document.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .body
            .as_mut()
            .unwrap()
            .content = serde_json::from_value(json!([{"startIndex": 1, "endIndex": 14,
                "table": {"tableRows": [{"tableCells": [{"content": [p.clone()]}]}]}}]))
        .unwrap();
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::SuggestedContent)
        );
        let mut plain = paragraph(3, "😀 label\n");
        document.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .body
            .as_mut()
            .unwrap()
            .content = serde_json::from_value(json!([plain.clone(), plain.clone()])).unwrap();
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::UnsafeRange)
        );
        plain.as_object_mut().unwrap().remove("startIndex");
        document.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .body
            .as_mut()
            .unwrap()
            .content = serde_json::from_value(json!([plain])).unwrap();
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::UnsafeRange)
        );
    }

    /// Replaces the child tab's body with `content`, so a test states only the
    /// paragraph shape it exercises.
    fn set_body(document: &mut Document, content: Value) {
        document.tabs[0].child_tabs[0]
            .document_tab
            .as_mut()
            .unwrap()
            .body
            .as_mut()
            .unwrap()
            .content = serde_json::from_value(content).unwrap();
    }

    #[test]
    fn empty_scope_ids_and_unbounded_create_spans_are_refused_before_any_read() {
        let delete = Mutation::Delete { id: "nr".into() };
        for scope in [
            Scope {
                tab_id: Some(String::new()),
                segment_id: None,
            },
            Scope {
                tab_id: None,
                segment_id: Some(String::new()),
            },
        ] {
            assert_eq!(
                delete.validate(&scope).unwrap_err(),
                "scope IDs cannot be empty"
            );
        }
        let scope = Scope {
            tab_id: None,
            segment_id: None,
        };
        for (start_index, end_index) in [(-1, 2), (3, 3), (4, 3)] {
            let create = Mutation::Create {
                name: "label".into(),
                start_index,
                end_index,
            };
            assert_eq!(
                create.validate(&scope).unwrap_err(),
                "named-range indices must describe a nonempty bounded span",
                "{start_index}..{end_index}"
            );
        }
    }

    #[test]
    fn create_refuses_a_span_that_is_not_safe_plain_text() {
        let (document, scope) = fixture(None, vec![json!({"startIndex": 1, "endIndex": 3})]);
        // [1, 10) would swallow the paragraph's newline.
        let create = Mutation::Create {
            name: "label".into(),
            start_index: 1,
            end_index: 10,
        };
        assert_eq!(resolve(&document, &scope, &create), Err(Error::UnsafeRange));
    }

    #[test]
    fn an_existing_range_with_no_spans_is_unsafe_to_mutate() {
        let (document, scope) = fixture(None, vec![]);
        for mutation in [Mutation::Delete { id: "nr".into() }, replace()] {
            assert_eq!(
                resolve(&document, &scope, &mutation),
                Err(Error::UnsafeRange)
            );
        }
    }

    /// Runs and non-text elements wholly outside the span are skipped rather
    /// than refused: only what the span touches has to be plain text.
    #[test]
    fn elements_outside_the_span_are_skipped() {
        let (mut document, scope) = fixture(None, vec![json!({"startIndex": 3, "endIndex": 5})]);
        set_body(
            &mut document,
            json!([{"startIndex": 1, "endIndex": 10, "paragraph": {"elements": [
                {"startIndex": 1, "endIndex": 2, "inlineObjectElement": {"inlineObjectId": "o"}},
                {"startIndex": 2, "endIndex": 10, "textRun": {"content": "abcdefg\n"}},
            ]}}]),
        );
        let (_, preview) = resolve(&document, &scope, &replace()).unwrap();
        assert_eq!((preview.removed_chars, preview.removed_bytes), (2, 2));

        let (mut document, scope) = fixture(None, vec![json!({"startIndex": 4, "endIndex": 6})]);
        set_body(
            &mut document,
            json!([{"startIndex": 1, "endIndex": 10, "paragraph": {"elements": [
                {"startIndex": 1, "endIndex": 3, "textRun": {"content": "ab"}},
                {"startIndex": 3, "endIndex": 7, "textRun": {"content": "cdef"}},
                {"startIndex": 7, "endIndex": 10, "textRun": {"content": "gh\n"}},
            ]}}]),
        );
        let (_, preview) = resolve(&document, &scope, &replace()).unwrap();
        assert_eq!((preview.removed_chars, preview.removed_bytes), (2, 2));
    }

    #[test]
    fn an_index_gap_inside_the_span_is_refused() {
        let (mut document, scope) = fixture(None, vec![json!({"startIndex": 4, "endIndex": 9})]);
        // Index 6 belongs to no run, though both runs are internally consistent.
        set_body(
            &mut document,
            json!([{"startIndex": 1, "endIndex": 11, "paragraph": {"elements": [
                {"startIndex": 1, "endIndex": 6, "textRun": {"content": "😀 la"}},
                {"startIndex": 7, "endIndex": 11, "textRun": {"content": "bel\n"}},
            ]}}]),
        );
        assert_eq!(
            resolve(&document, &scope, &replace()),
            Err(Error::UnsafeRange)
        );
    }

    /// The span must end inside a paragraph whose final run is its
    /// newline-terminated end, or the replacement could join paragraphs.
    #[test]
    fn a_paragraph_that_does_not_end_in_its_newline_run_is_refused() {
        for (span_start, span_end, paragraph_end, content) in [
            // The last run has no newline.
            (1, 3, 10, "abcdefghi"),
            // The runs stop short of the paragraph's end index.
            (1, 3, 12, "abcdefgh\n"),
            // No run reaches the span at all.
            (6, 8, 12, "abcd\n"),
        ] {
            let (mut document, scope) = fixture(
                None,
                vec![json!({"startIndex": span_start, "endIndex": span_end})],
            );
            let run_end = 1 + content.encode_utf16().count();
            set_body(
                &mut document,
                json!([{"startIndex": 1, "endIndex": paragraph_end, "paragraph": {"elements": [
                    {"startIndex": 1, "endIndex": run_end, "textRun": {"content": content}},
                ]}}]),
            );
            assert_eq!(
                resolve(&document, &scope, &replace()),
                Err(Error::UnsafeRange),
                "{content:?} in {span_start}..{span_end}"
            );
        }
    }

    #[test]
    fn effects_and_bounded_rendering_distinguish_metadata_from_content_deletion() {
        let (document, scope) = fixture(None, vec![json!({"startIndex": 1, "endIndex": 3})]);
        let (_, metadata) =
            resolve(&document, &scope, &Mutation::Delete { id: "nr".into() }).unwrap();
        let (_, content) = resolve(
            &document,
            &scope,
            &Mutation::Replace {
                id: "nr".into(),
                text: String::new(),
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&metadata).unwrap()["effect"],
            "delete-metadata"
        );
        assert_eq!(
            serde_json::to_value(&content).unwrap()["effect"],
            "delete-content"
        );
        assert!(metadata
            .describe_spans()
            .contains("[1, 3) UTF-16, tab child, segment body"));
        let mut many = metadata;
        many.ranges = vec![many.ranges[0].clone(); 51];
        assert!(many.describe_spans().ends_with("1 more span(s)"));
        assert_eq!(
            serde_json::to_value(many).unwrap()["ranges"]
                .as_array()
                .unwrap()
                .len(),
            51
        );
    }
}
