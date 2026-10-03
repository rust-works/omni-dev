//! Request and response types for `documents.batchUpdate`.
//!
//! Kept apart from [`crate::drive::docs::types`] — which models what
//! `documents.get` *returns* — because these types are where this module's
//! safety guarantees are enforced structurally rather than by convention,
//! and that is easier to review in one place.
//!
//! Two shapes carry that weight, both per
//! [ADR-0076](../../../docs/adrs/adr-0076.md) §3 and §4:
//!
//! - [`WriteControl`] is a **struct with one field**, not an enum over the
//!   API's two-arm union, so the rebasing arm cannot be expressed.
//! - [`BatchUpdateDocumentRequest`] holds **one** [`DocsRequest`], not a
//!   `Vec`, and its `write_control` is **not** `Option`.
//!
//! [`DocsRequest`] models only the requests the typed verbs need.
//! Anchored insertion and deletion use indices resolved from the leased snapshot
//! (ADR-0094). Unmodelled destructive requests remain guarded by a source test.

use serde::{Deserialize, Serialize};

/// The `writeControl` field of a `documents.batchUpdate`.
///
/// **One field, deliberately not an enum.** The API's `writeControl` is a
/// union: `requiredRevisionId` refuses the write if the document has moved,
/// while `targetRevisionId` applies the edit against that old revision,
/// rebasing over whatever landed since.
///
/// The second arm is this API's `--force`, and worse than one: a force-push
/// at least tells you that you overwrote something, whereas a rebasing
/// `replaceAllText` reports plain success on a document containing a
/// collaborator's paragraph the edit was never computed against and nobody
/// looked at. The dangerous outcome is indistinguishable from the safe one.
///
/// So it is made **unrepresentable** rather than merely undocumented,
/// following [ADR-0061](../../../docs/adrs/adr-0061.md) §2's "not an engine
/// option, not a CLI flag, not a wire field" stance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteControl {
    /// The revision the document must still be at for the write to apply.
    #[serde(rename = "requiredRevisionId")]
    pub required_revision_id: String,
}

/// A `documents.batchUpdate` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchUpdateDocumentRequest {
    /// Exactly one request — see [`Self::new`].
    pub requests: Vec<DocsRequest>,
    /// The lease. **Not** `Option`: there is no unleased path.
    #[serde(rename = "writeControl")]
    pub write_control: WriteControl,
}

impl BatchUpdateDocumentRequest {
    /// Builds a one-request, always-leased batch.
    ///
    /// Taking a single [`DocsRequest`] rather than a `Vec` makes "one verb,
    /// one request, one batch, one `drivemutation` record" a *signature*
    /// rather than a convention (ADR-0076 §4). It also removes this API's
    /// signature hazard from v1 entirely: ordering within a batch, where an
    /// insertion shifts every subsequent request's indices, is where Docs
    /// integrations break, and a one-request batch cannot exhibit it.
    #[must_use]
    pub fn new(request: DocsRequest, required_revision_id: &str) -> Self {
        Self {
            requests: vec![request],
            write_control: WriteControl {
                required_revision_id: required_revision_id.to_string(),
            },
        }
    }
}

/// One `Request` of a `documents.batchUpdate`.
///
/// Externally tagged, matching the API's own wire shape: a `Request` is an
/// object with exactly one populated field naming the operation.
///
/// Only requests consumed by the gated engines are modelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DocsRequest {
    /// Replace every occurrence of some text.
    #[serde(rename = "replaceAllText")]
    ReplaceAllText(ReplaceAllTextRequest),
    /// Insert text at the end of a segment.
    #[serde(rename = "insertText")]
    InsertText(InsertTextRequest),
    /// Delete an anchor-resolved content range under `DocsDelete`.
    #[serde(rename = "deleteContentRange")]
    DeleteContentRange(DeleteContentRangeRequest),
    /// Apply a concrete list preset to resolved paragraphs under DocsWrite.
    #[serde(rename = "createParagraphBullets")]
    CreateParagraphBullets(CreateParagraphBulletsRequest),
    /// Remove list formatting, preserving prose, under DocsWrite.
    #[serde(rename = "deleteParagraphBullets")]
    DeleteParagraphBullets(DeleteParagraphBulletsRequest),
}

/// A `replaceAllText` request.
///
/// `tabsCriteria` is deliberately **absent**, not optional. Omitting it
/// applies the replacement to every tab, which is what the verb means; the
/// alternative — silently narrowing to one tab — would be wrong in the worse
/// direction. Confirmed live and in the discovery schema (ADR-0076 §11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplaceAllTextRequest {
    /// What to find.
    #[serde(rename = "containsText")]
    pub contains_text: SubstringMatchCriteria,
    /// What to put in its place.
    #[serde(rename = "replaceText")]
    pub replace_text: String,
}

/// The match criteria of a `replaceAllText`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubstringMatchCriteria {
    /// The literal substring to match. Never a regex.
    pub text: String,
    /// Whether the match is case-sensitive.
    ///
    /// The API defaults this to `false`; the CLI defaults it to `true`. See
    /// ADR-0076 §7 — under Google's default, `--search it` also rewrites
    /// `It` and `IT`, in a verb with no undo.
    #[serde(rename = "matchCase")]
    pub match_case: bool,
}

/// An `insertText` request.
///
/// Location is a union, preserving the existing append wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InsertTextRequest {
    /// The text to insert.
    pub text: String,
    /// Exactly one location arm.
    #[serde(flatten)]
    pub location: InsertLocation,
}

/// The two supported insert locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum InsertLocation {
    /// Append to the first tab's body.
    #[serde(rename = "endOfSegmentLocation")]
    EndOfSegment(EndOfSegmentLocation),
    /// Anchor-resolved UTF-16 index relative to a body or explicit segment.
    #[serde(rename = "location")]
    At(Location),
}

/// A segment-relative insertion point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    /// Server UTF-16 index.
    pub index: i64,
    /// Omitted only for legacy top-level bodies.
    #[serde(rename = "tabId", skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// Explicit header, footer or footnote identity; absent for bodies.
    #[serde(rename = "segmentId", skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
}

/// A segment-relative content range with explicit tab identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentRange {
    /// Inclusive server UTF-16 start.
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    /// Exclusive server UTF-16 end.
    #[serde(rename = "endIndex")]
    pub end_index: i64,
    /// Omitted only for legacy top-level bodies.
    #[serde(rename = "tabId", skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// Explicit header, footer or footnote identity; absent for bodies.
    #[serde(rename = "segmentId", skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
}

/// A single typed deletion request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteContentRangeRequest {
    /// Anchor-resolved range with segment and tab identity.
    pub range: ContentRange,
}

/// Concrete Google list presets. Unspecified and arbitrary strings are excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BulletPreset {
    /// Google `BULLET_DISC_CIRCLE_SQUARE` preset.
    #[serde(rename = "BULLET_DISC_CIRCLE_SQUARE")]
    BulletDiscCircleSquare,
    /// Google `BULLET_DIAMONDX_ARROW3D_SQUARE` preset.
    #[serde(rename = "BULLET_DIAMONDX_ARROW3D_SQUARE")]
    BulletDiamondxArrow3DSquare,
    /// Google `BULLET_CHECKBOX` preset.
    #[serde(rename = "BULLET_CHECKBOX")]
    BulletCheckbox,
    /// Google `BULLET_ARROW_DIAMOND_DISC` preset.
    #[serde(rename = "BULLET_ARROW_DIAMOND_DISC")]
    BulletArrowDiamondDisc,
    /// Google `BULLET_STAR_CIRCLE_SQUARE` preset.
    #[serde(rename = "BULLET_STAR_CIRCLE_SQUARE")]
    BulletStarCircleSquare,
    /// Google `BULLET_ARROW3D_CIRCLE_SQUARE` preset.
    #[serde(rename = "BULLET_ARROW3D_CIRCLE_SQUARE")]
    BulletArrow3DCircleSquare,
    /// Google `BULLET_LEFTTRIANGLE_DIAMOND_DISC` preset.
    #[serde(rename = "BULLET_LEFTTRIANGLE_DIAMOND_DISC")]
    BulletLefttriangleDiamondDisc,
    /// Google `BULLET_DIAMONDX_HOLLOWDIAMOND_SQUARE` preset.
    #[serde(rename = "BULLET_DIAMONDX_HOLLOWDIAMOND_SQUARE")]
    BulletDiamondxHollowdiamondSquare,
    /// Google `BULLET_DIAMOND_CIRCLE_SQUARE` preset.
    #[serde(rename = "BULLET_DIAMOND_CIRCLE_SQUARE")]
    BulletDiamondCircleSquare,
    /// Google `NUMBERED_DECIMAL_ALPHA_ROMAN` preset.
    #[serde(rename = "NUMBERED_DECIMAL_ALPHA_ROMAN")]
    NumberedDecimalAlphaRoman,
    /// Google `NUMBERED_DECIMAL_ALPHA_ROMAN_PARENS` preset.
    #[serde(rename = "NUMBERED_DECIMAL_ALPHA_ROMAN_PARENS")]
    NumberedDecimalAlphaRomanParens,
    /// Google `NUMBERED_DECIMAL_NESTED` preset.
    #[serde(rename = "NUMBERED_DECIMAL_NESTED")]
    NumberedDecimalNested,
    /// Google `NUMBERED_UPPERALPHA_ALPHA_ROMAN` preset.
    #[serde(rename = "NUMBERED_UPPERALPHA_ALPHA_ROMAN")]
    NumberedUpperalphaAlphaRoman,
    /// Google `NUMBERED_UPPERROMAN_UPPERALPHA_DECIMAL` preset.
    #[serde(rename = "NUMBERED_UPPERROMAN_UPPERALPHA_DECIMAL")]
    NumberedUpperromanUpperalphaDecimal,
    /// Google `NUMBERED_ZERODECIMAL_ALPHA_ROMAN` preset.
    #[serde(rename = "NUMBERED_ZERODECIMAL_ALPHA_ROMAN")]
    NumberedZerodecimalAlphaRoman,
}

/// Apply list formatting in a single request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreateParagraphBulletsRequest {
    /// Complete paragraph range in the leased snapshot.
    pub range: ContentRange,
    /// Explicit preset; no server default.
    #[serde(rename = "bulletPreset")]
    pub bullet_preset: BulletPreset,
}

/// Remove bullets without deleting paragraph content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteParagraphBulletsRequest {
    /// Complete paragraph range in the leased snapshot.
    pub range: ContentRange,
}

/// The end of a segment.
///
/// Serialises as `{}`. An absent `segmentId` means the document body, and an
/// absent `tabId` means the **first** tab — documented in the API's own
/// discovery schema and confirmed live (ADR-0076 §11). That asymmetry with
/// `replaceAllText`, which spans every tab, is real and is why `docs append`
/// documents itself as first-tab-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EndOfSegmentLocation {}

impl DocsRequest {
    /// A `replaceAllText` over every tab.
    #[must_use]
    pub fn replace_all_text(search: &str, replace: &str, match_case: bool) -> Self {
        Self::ReplaceAllText(ReplaceAllTextRequest {
            contains_text: SubstringMatchCriteria {
                text: search.to_string(),
                match_case,
            },
            replace_text: replace.to_string(),
        })
    }

    /// An `insertText` at the end of the first tab's body.
    #[must_use]
    pub fn insert_text_at_end(text: &str) -> Self {
        Self::InsertText(InsertTextRequest {
            text: text.to_string(),
            location: InsertLocation::EndOfSegment(EndOfSegmentLocation::default()),
        })
    }
    /// Insert at an anchor-resolved point.
    #[must_use]
    pub(in crate::drive) fn insert_text_at(text: &str, edit: &super::anchor::EditPreview) -> Self {
        Self::InsertText(InsertTextRequest {
            text: text.to_owned(),
            location: InsertLocation::At(Location {
                index: edit.start_index,
                tab_id: edit.tab_id.clone(),
                segment_id: edit.segment_id.clone(),
            }),
        })
    }

    /// Delete the range resolved from this invocation's snapshot.
    #[must_use]
    pub(in crate::drive) fn delete_range(edit: &super::anchor::EditPreview) -> Self {
        Self::DeleteContentRange(DeleteContentRangeRequest {
            range: ContentRange {
                start_index: edit.start_index,
                end_index: edit.end_index,
                tab_id: edit.tab_id.clone(),
                segment_id: edit.segment_id.clone(),
            },
        })
    }
    /// Build one list request from the same snapshot used for its revision.
    #[must_use]
    pub(in crate::drive) fn list_bullets(
        edit: &super::anchor::ListPreview,
        preset: Option<BulletPreset>,
    ) -> Self {
        let range = ContentRange {
            start_index: edit.start_index,
            end_index: edit.end_index,
            tab_id: edit.tab_id.clone(),
            segment_id: None,
        };
        match preset {
            Some(bullet_preset) => Self::CreateParagraphBullets(CreateParagraphBulletsRequest {
                range,
                bullet_preset,
            }),
            None => Self::DeleteParagraphBullets(DeleteParagraphBulletsRequest { range }),
        }
    }
}

/// A `documents.batchUpdate` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchUpdateDocumentResponse {
    /// The document acted on.
    #[serde(default, rename = "documentId")]
    pub document_id: Option<String>,
    /// One reply per request, in request order.
    #[serde(default)]
    pub replies: Vec<DocsReply>,
}

impl BatchUpdateDocumentResponse {
    /// The `occurrencesChanged` the server reported, exactly as sent.
    ///
    /// `None` means the field was absent, which happens in **two** unrelated
    /// cases — which is why callers that know they sent a `replaceAllText`
    /// should use [`Self::occurrences_changed_for_replace`] instead:
    ///
    /// - an `insertText` replies with an empty object, so there is genuinely
    ///   no count; and
    /// - a `replaceAllText` that matched **nothing** also omits it, because
    ///   Docs serialises proto3 JSON and proto3 omits zero-valued integers.
    ///
    /// Observed live (2026-09-07): a replace matching 2 occurrences returns
    /// `occurrencesChanged: 2`, and one matching 0 returns the field not at
    /// all. Same rule as `StructuralElement::start_index`.
    #[must_use]
    pub fn occurrences_changed(&self) -> Option<i64> {
        self.replies
            .first()
            .and_then(|reply| reply.replace_all_text.as_ref())
            .and_then(|reply| reply.occurrences_changed)
    }

    /// The count for a request the caller knows was a `replaceAllText`,
    /// resolving proto3's omitted zero to `0`.
    ///
    /// A replace always has an answer — "nothing matched" is a count, not an
    /// absence of one — so this returns `i64` rather than `Option<i64>`. The
    /// caller supplies the missing piece of information that disambiguates
    /// [`Self::occurrences_changed`]'s two `None` cases: that the request was
    /// a replace, so an absent field can only mean zero.
    #[must_use]
    pub fn occurrences_changed_for_replace(&self) -> i64 {
        self.occurrences_changed().unwrap_or(0)
    }
}

/// One reply of a `documents.batchUpdate`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DocsReply {
    /// Present only for a `replaceAllText` request.
    #[serde(default, rename = "replaceAllText")]
    pub replace_all_text: Option<ReplaceAllTextResponse>,
}

/// The reply to a `replaceAllText`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReplaceAllTextResponse {
    /// How many occurrences the server actually changed.
    #[serde(default, rename = "occurrencesChanged")]
    pub occurrences_changed: Option<i64>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The wire-shape half of ADR-0076 §3 and §4: every batch carries a
    /// lease, carries exactly one request, and never carries the rebasing
    /// arm of the `writeControl` union.
    #[test]
    fn every_list_preset_has_a_single_exact_tab_scoped_leased_wire_shape() {
        let edit = super::super::anchor::ListPreview {
            start_index: 11,
            end_index: 29,
            tab_id: Some("nested-tab".into()),
            paragraphs: 2,
            leading_tabs_removed: 3,
        };
        let expected = [
            "BULLET_DISC_CIRCLE_SQUARE",
            "BULLET_DIAMONDX_ARROW3D_SQUARE",
            "BULLET_CHECKBOX",
            "BULLET_ARROW_DIAMOND_DISC",
            "BULLET_STAR_CIRCLE_SQUARE",
            "BULLET_ARROW3D_CIRCLE_SQUARE",
            "BULLET_LEFTTRIANGLE_DIAMOND_DISC",
            "BULLET_DIAMONDX_HOLLOWDIAMOND_SQUARE",
            "BULLET_DIAMOND_CIRCLE_SQUARE",
            "NUMBERED_DECIMAL_ALPHA_ROMAN",
            "NUMBERED_DECIMAL_ALPHA_ROMAN_PARENS",
            "NUMBERED_DECIMAL_NESTED",
            "NUMBERED_UPPERALPHA_ALPHA_ROMAN",
            "NUMBERED_UPPERROMAN_UPPERALPHA_DECIMAL",
            "NUMBERED_ZERODECIMAL_ALPHA_ROMAN",
        ];
        let presets = [
            BulletPreset::BulletDiscCircleSquare,
            BulletPreset::BulletDiamondxArrow3DSquare,
            BulletPreset::BulletCheckbox,
            BulletPreset::BulletArrowDiamondDisc,
            BulletPreset::BulletStarCircleSquare,
            BulletPreset::BulletArrow3DCircleSquare,
            BulletPreset::BulletLefttriangleDiamondDisc,
            BulletPreset::BulletDiamondxHollowdiamondSquare,
            BulletPreset::BulletDiamondCircleSquare,
            BulletPreset::NumberedDecimalAlphaRoman,
            BulletPreset::NumberedDecimalAlphaRomanParens,
            BulletPreset::NumberedDecimalNested,
            BulletPreset::NumberedUpperalphaAlphaRoman,
            BulletPreset::NumberedUpperromanUpperalphaDecimal,
            BulletPreset::NumberedZerodecimalAlphaRoman,
        ];
        assert_eq!(presets.len(), expected.len());
        for (preset, wire) in presets.iter().zip(expected) {
            let body = serde_json::to_value(BatchUpdateDocumentRequest::new(
                DocsRequest::list_bullets(&edit, Some(*preset)),
                "r1",
            ))
            .unwrap();
            assert_eq!(
                body,
                serde_json::json!({"requests": [{"createParagraphBullets": {
                "range": {"startIndex": 11, "endIndex": 29, "tabId": "nested-tab"}, "bulletPreset": wire
            }}], "writeControl": {"requiredRevisionId": "r1"}})
            );
        }
        let body = serde_json::to_value(BatchUpdateDocumentRequest::new(
            DocsRequest::list_bullets(&edit, None),
            "r1",
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"requests": [{"deleteParagraphBullets": {
            "range": {"startIndex": 11, "endIndex": 29, "tabId": "nested-tab"}
        }}], "writeControl": {"requiredRevisionId": "r1"}})
        );
    }

    #[test]
    fn a_batch_update_body_always_carries_the_lease_and_exactly_one_request() {
        let body = BatchUpdateDocumentRequest::new(DocsRequest::insert_text_at_end("hi"), "rev-1");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["writeControl"]["requiredRevisionId"], "rev-1");
        assert!(json["writeControl"].get("targetRevisionId").is_none());
        assert_eq!(json["requests"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn replace_all_text_serialises_to_the_api_shape() {
        let body = BatchUpdateDocumentRequest::new(
            DocsRequest::replace_all_text("Q3", "Q4", true),
            "rev-1",
        );
        let json = serde_json::to_value(&body).unwrap();
        let request = &json["requests"][0]["replaceAllText"];
        assert_eq!(request["containsText"]["text"], "Q3");
        assert_eq!(request["containsText"]["matchCase"], true);
        assert_eq!(request["replaceText"], "Q4");
    }

    /// Omitting `tabsCriteria` is what makes a replace span every tab
    /// (ADR-0076 §11) — so its absence from the wire body is asserted, not
    /// assumed.
    #[test]
    fn replace_all_text_sends_no_tabs_criteria_so_it_spans_every_tab() {
        let body =
            BatchUpdateDocumentRequest::new(DocsRequest::replace_all_text("a", "b", false), "r");
        let json = serde_json::to_value(&body).unwrap();
        assert!(json["requests"][0]["replaceAllText"]
            .get("tabsCriteria")
            .is_none());
    }

    /// The append half of §5: an insert is addressed positionally, and the
    /// numeric-index arm never appears on the wire.
    #[test]
    fn insert_text_sends_end_of_segment_location_and_never_an_index() {
        let body = BatchUpdateDocumentRequest::new(DocsRequest::insert_text_at_end("hi"), "r");
        let json = serde_json::to_value(&body).unwrap();
        let request = &json["requests"][0]["insertText"];
        assert_eq!(request["text"], "hi");
        assert_eq!(request["endOfSegmentLocation"], serde_json::json!({}));
        assert!(request.get("location").is_none());
    }

    #[test]
    fn occurrences_changed_reads_the_first_reply() {
        let response: BatchUpdateDocumentResponse = serde_json::from_value(serde_json::json!({
            "documentId": "d1",
            "replies": [{"replaceAllText": {"occurrencesChanged": 7}}],
        }))
        .unwrap();
        assert_eq!(response.occurrences_changed(), Some(7));
    }

    /// The shape Google actually returns for a replace that matched
    /// **nothing**, observed live on 2026-09-07: proto3 omits the
    /// zero-valued integer, so the field is simply absent.
    ///
    /// Reading that as "unknown" is the bug this pins — a replace that
    /// matched nothing has an answer, and it is zero. The mock in the
    /// original test sent `occurrencesChanged: 0`, which Google never does,
    /// which is exactly why only a live run caught it.
    #[test]
    fn a_replace_that_matched_nothing_omits_the_count_and_reads_as_zero() {
        let response: BatchUpdateDocumentResponse = serde_json::from_value(serde_json::json!({
            "documentId": "d1",
            "replies": [{"replaceAllText": {}}],
        }))
        .unwrap();
        assert_eq!(response.occurrences_changed(), None, "absent on the wire");
        assert_eq!(
            response.occurrences_changed_for_replace(),
            0,
            "but a replace knows absent means zero"
        );
    }

    /// A reply carrying no `replaceAllText` section at all resolves the same
    /// way for a replace: still zero, never "unknown".
    #[test]
    fn a_bare_reply_object_also_reads_as_zero_for_a_replace() {
        let response: BatchUpdateDocumentResponse = serde_json::from_value(serde_json::json!({
            "documentId": "d1", "replies": [{}],
        }))
        .unwrap();
        assert_eq!(response.occurrences_changed_for_replace(), 0);
    }

    /// An `insertText` replies with an empty object; that is normal.
    #[test]
    fn an_empty_reply_yields_no_occurrence_count_rather_than_an_error() {
        let response: BatchUpdateDocumentResponse = serde_json::from_value(serde_json::json!({
            "documentId": "d1", "replies": [{}],
        }))
        .unwrap();
        assert_eq!(response.occurrences_changed(), None);
    }

    #[test]
    fn a_response_with_no_replies_key_parses() {
        let response: BatchUpdateDocumentResponse =
            serde_json::from_value(serde_json::json!({"documentId": "d1"})).unwrap();
        assert!(response.replies.is_empty());
        assert_eq!(response.occurrences_changed(), None);
    }
    #[test]
    fn anchored_wire_shapes_include_tab_identity_and_exactly_one_leased_request() {
        let edit = super::super::anchor::EditPreview {
            segment_id: None,
            segment_kind: None,
            start_index: 4,
            end_index: 10,
            tab_id: Some("child-tab".into()),
            paragraphs: 1,
            chars: 6,
            bytes: 6,
        };
        let insert = serde_json::to_value(BatchUpdateDocumentRequest::new(
            DocsRequest::insert_text_at("hi", &edit),
            "rev",
        ))
        .unwrap();
        assert_eq!(
            insert["requests"][0]["insertText"],
            serde_json::json!({"text": "hi", "location": {"index": 4, "tabId": "child-tab"}})
        );
        let delete = serde_json::to_value(BatchUpdateDocumentRequest::new(
            DocsRequest::delete_range(&edit),
            "rev",
        ))
        .unwrap();
        assert_eq!(
            delete,
            serde_json::json!({"requests": [{"deleteContentRange": {"range": {"startIndex": 4, "endIndex": 10, "tabId": "child-tab"}}}], "writeControl": {"requiredRevisionId": "rev"}})
        );
    }
}
