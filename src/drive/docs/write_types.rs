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
//! `deleteContentRange` has no representation here at all — not a variant,
//! not a struct, not a string constant — which is what makes "`omni-dev`
//! cannot delete document content" a property of the build rather than a
//! promise in prose (ADR-0076 §4, adopting ADR-0075 §4's stance).

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
/// **Only the requests the typed verbs need are modelled.** There is no
/// `deleteContentRange` variant, and adding one is a deliberate act guarded
/// by `no_destructive_or_unleased_request_is_reachable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DocsRequest {
    /// Replace every occurrence of some text.
    #[serde(rename = "replaceAllText")]
    ReplaceAllText(ReplaceAllTextRequest),
    /// Insert text at the end of a segment.
    #[serde(rename = "insertText")]
    InsertText(InsertTextRequest),
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
/// Only the `endOfSegmentLocation` arm of the API's location union is
/// modelled. The other arm takes an explicit numeric `index`, and modelling
/// it would put UTF-16 index arithmetic into this crate — wrong for
/// astral-plane characters in a way that corrupts silently — as well as
/// reintroducing the off-by-one at a segment's trailing newline. ADR-0076
/// §5 records that as the entry cost `--index` insertion must pay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InsertTextRequest {
    /// The text to insert.
    pub text: String,
    /// Where it goes: the end of the body segment of the first tab.
    #[serde(rename = "endOfSegmentLocation")]
    pub end_of_segment_location: EndOfSegmentLocation,
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
            end_of_segment_location: EndOfSegmentLocation::default(),
        })
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
    /// The `occurrencesChanged` the server reported, when the single request
    /// was a `replaceAllText`.
    ///
    /// `None` is normal rather than an error: an `insertText` replies with an
    /// empty object. Same absent-is-not-empty lesson as `ValueRange::values`.
    #[must_use]
    pub fn occurrences_changed(&self) -> Option<i64> {
        self.replies
            .first()
            .and_then(|reply| reply.replace_all_text.as_ref())
            .and_then(|reply| reply.occurrences_changed)
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
}
