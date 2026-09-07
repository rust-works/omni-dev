//! Docs v1 API façade — typed wrappers over the endpoints the CLI needs,
//! mirroring `crate::drive::sheets::api::SheetsApi`'s shape.
//!
//! Free `build_*_url` functions take a literal `base_url` so they are
//! unit-testable without a client, exactly like `sheets/api.rs`'s and
//! `files_api.rs`'s.
//!
//! # Why there is no `fields` mask
//!
//! `spreadsheets.get` is always masked, because unmasked it embeds every
//! cell of every sheet. The mirror-image decision here is the opposite one,
//! and it is deliberate rather than an omission — there is a test pinning
//! it. Three reasons:
//!
//! 1. **A Docs mask cannot be made recursion-safe.** A `fields` mask spells
//!    nesting depth out literally, and a table may contain a table to
//!    arbitrary depth. Any fixed-depth mask therefore *silently drops
//!    document text* below its deepest named level — partial content
//!    indistinguishable from complete content, which is exactly the failure
//!    mode `MAX_SHEETS_PER_READ` refuses rather than tolerates.
//! 2. **It would have to be written twice.** With `includeTabsContent=true`
//!    the content lives under `tabs.documentTab.body.content…`, not
//!    `body.content…`, so the mask's root depends on a query parameter. Two
//!    masks that must agree is two masks that will diverge.
//! 3. **The payload is bounded in a way a spreadsheet's is not.** Google
//!    caps a Doc at roughly a million characters, so the worst-case response
//!    is large but bounded; a spreadsheet's cell count has no comparable
//!    ceiling.
//!
//! [`MAX_DOCUMENT_BYTES`] is the guard that replaces it. If payload size
//! ever does become a problem, a mask belongs on the `info` path **only**
//! (which needs a shallow skeleton and no recursion) and must never be
//! applied to `read`.

use anyhow::{Context, Result};
use url::Url;

use crate::drive::api_client::GoogleApiClient;
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::types::Document;
use crate::drive::docs::write_types::{
    BatchUpdateDocumentRequest, BatchUpdateDocumentResponse, DocsRequest,
};
use crate::drive::error::DriveError;
use crate::drive::files_api::{append_write_scope_hint, WriteCapability};

/// Maximum `documents.get` response accepted into memory.
///
/// Best-effort, and honestly so: Google gzips and frequently uses chunked
/// transfer encoding, so `Content-Length` is often absent and there is then
/// nothing to check up front — the same caveat
/// `files_api.rs::check_download_size` already carries. The real bound is
/// Google's own document size cap; this catches the case it can see.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

/// How the API should render pending suggestions.
///
/// Engine-layer, deliberately free of any `clap` derive — the CLI keeps its
/// own `ValueEnum` mirror, the same split `ValueRenderOption` and
/// `DriveOperation` use.
///
/// This is a correctness knob, not decoration: a document with pending
/// suggestions has a different index space depending on the view, so which
/// view a read reports against determines whether its indices mean anything
/// to a subsequent edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SuggestionsViewMode {
    /// Whatever the caller's access level implies — suggestions inline for
    /// an editor, accepted for a reader.
    #[default]
    DefaultForCurrentAccess,
    /// Suggestions shown inline, as tracked changes.
    Inline,
    /// The document as it would be with every suggestion accepted.
    PreviewAccepted,
    /// The document as it would be with every suggestion rejected.
    PreviewWithoutSuggestions,
}

impl SuggestionsViewMode {
    /// The wire value for the `suggestionsViewMode` query parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultForCurrentAccess => "DEFAULT_FOR_CURRENT_ACCESS",
            Self::Inline => "SUGGESTIONS_INLINE",
            Self::PreviewAccepted => "PREVIEW_SUGGESTIONS_ACCEPTED",
            Self::PreviewWithoutSuggestions => "PREVIEW_WITHOUT_SUGGESTIONS",
        }
    }
}

/// Docs API façade.
#[derive(Debug)]
pub struct DocsApi<'a> {
    client: &'a DocsClient,
}

impl<'a> DocsApi<'a> {
    /// Wraps an existing [`DocsClient`].
    #[must_use]
    pub fn new(client: &'a DocsClient) -> Self {
        Self { client }
    }

    /// Fetches a document's full structural model.
    ///
    /// Always sends `includeTabsContent=true`; see
    /// [`build_document_get_url`]. Never `fields`-masked; see the module
    /// docs.
    pub async fn get_document(
        &self,
        document_id: &str,
        suggestions: SuggestionsViewMode,
    ) -> Result<Document> {
        let url = build_document_get_url(self.client.base_url(), document_id, suggestions)?;
        let response = self.client.transport().get_json(url.as_str()).await?;
        check_document_size(response.content_length())?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Docs document")
            .await
    }

    /// Applies **one** request to a document, under a revision lease.
    ///
    /// The single mutating entry point for Docs, and every part of the
    /// signature is load-bearing (ADR-0076 §3, §4):
    ///
    /// - It takes one [`DocsRequest`], not a `Vec`, so "one verb, one
    ///   request, one batch, one log record" is a signature rather than a
    ///   convention — and the batch-ordering hazard, where an insertion
    ///   shifts every later request's indices, cannot arise.
    /// - `required_revision_id` is `&str`, not `Option<&str>`, so there is
    ///   no unleased path to reach for.
    /// - `pub(in crate::drive)` keeps it behind the same visibility fence as
    ///   `FilesApi::create` and `SheetsApi::values_update`, so nothing
    ///   outside `crate::drive` — where every engine runs the gate first —
    ///   can even compile a call to it.
    // No caller yet, by design: ADR-0076 §3's "no escape hatch" guarantee is
    // landed and reviewed on a diff that is purely about the lease, before
    // the verbs that depend on it exist. The engines land in the next commit.
    #[allow(dead_code)]
    pub(in crate::drive) async fn batch_update(
        &self,
        document_id: &str,
        request: DocsRequest,
        required_revision_id: &str,
    ) -> Result<BatchUpdateDocumentResponse> {
        let url = build_batch_update_url(self.client.base_url(), document_id)?;
        let body = BatchUpdateDocumentRequest::new(request, required_revision_id);
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Docs batchUpdate response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }
}

/// Whether `err` is Google refusing a `batchUpdate` because the revision
/// lease no longer matches.
///
/// **Confirmed against the live API** (ADR-0076 §6): a stale
/// `requiredRevisionId` returns HTTP 400 with the `google.rpc` envelope,
/// `error.status` of `INVALID_ARGUMENT` and the message
/// `"The required revision ID '<id>' does not match the latest revision."`.
///
/// `INVALID_ARGUMENT` is **not** Docs-specific — it is the same status a
/// malformed request carries — which is exactly why this matches the status
/// code in conjunction with a message substring rather than trusting the
/// status alone. Matching on the status would classify every malformed
/// request as a lost lease and tell the user to re-run something that can
/// never succeed.
///
/// The failure direction is safe either way. A false negative degrades to
/// `WriteResult::Failed`, which still carries the server's own message
/// verbatim; nothing was written on either path, because a one-request
/// `batchUpdate` is atomic. There is no input for which this returns `true`
/// and a write has happened.
#[allow(dead_code)] // Caller lands with the write engines; see `batch_update`.
pub(in crate::drive) fn is_stale_revision(err: &anyhow::Error) -> bool {
    /// The distinctive part of Google's message, lowercased for comparison.
    const STALE_MARKER: &str = "does not match the latest revision";

    matches!(
        err.downcast_ref::<DriveError>(),
        Some(DriveError::ApiRequestFailed { status: 400, body, .. })
            if body.to_ascii_lowercase().contains(STALE_MARKER)
    )
}

/// Refuses a `documents.get` whose declared `Content-Length` exceeds
/// [`MAX_DOCUMENT_BYTES`]. A missing length is allowed through — there is
/// nothing to check up front in that case.
fn check_document_size(content_length: Option<u64>) -> Result<()> {
    if let Some(len) = content_length {
        anyhow::ensure!(
            len <= MAX_DOCUMENT_BYTES,
            "refusing to load {len} bytes into memory (limit: {MAX_DOCUMENT_BYTES} bytes); \
             this document is too large for `drive docs read`"
        );
    }
    Ok(())
}

/// Builds the `documents.get` URL.
///
/// `includeTabsContent=true` is unconditional, and that is the load-bearing
/// choice. Without it a three-tab document returns only the first tab's
/// content, in a response *shaped identically* to a one-tab document — so
/// silently reading a third of a document is indistinguishable from reading
/// all of a small one. This is the same failure `drive read --content`
/// already has on a Sheet (first sheet only), and the reason `sheets read`
/// exists. Narrowing is a client-side `--tab` filter applied after the
/// fetch, so the full tab list is always known.
fn build_document_get_url(
    base_url: &str,
    document_id: &str,
    suggestions: SuggestionsViewMode,
) -> Result<Url> {
    let mut url =
        GoogleApiClient::api_url(base_url, "/v1/documents").context("Invalid Docs base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[document_id])?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("includeTabsContent", "true");
        pairs.append_pair("suggestionsViewMode", suggestions.as_str());
    }
    Ok(url)
}

/// Builds the `documents.batchUpdate` URL.
///
/// `:batchUpdate` is a suffix on the id path segment, so it is appended to
/// the id *before* the segment is pushed — pushing it separately would
/// percent-encode the `:` into its own segment.
#[allow(dead_code)] // Reached via `batch_update`; see its note.
fn build_batch_update_url(base_url: &str, document_id: &str) -> Result<Url> {
    let mut url =
        GoogleApiClient::api_url(base_url, "/v1/documents").context("Invalid Docs base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[&format!("{document_id}:batchUpdate")])?;
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = "https://docs.googleapis.com";

    /// The deliberate inverse of `sheets/api.rs`'s
    /// `spreadsheet_get_url_masks_fields`. A Docs mask cannot be made
    /// recursion-safe, so its absence is a decision — pinned here so it is
    /// not "fixed" later by someone reasoning from the Sheets precedent.
    #[test]
    fn document_get_url_sends_no_fields_mask() {
        let url = build_document_get_url(BASE, "d1", SuggestionsViewMode::DefaultForCurrentAccess)
            .unwrap();
        assert!(
            url.query_pairs().all(|(k, _)| k != "fields"),
            "a fields mask would silently truncate nested tables: {url}"
        );
    }

    /// Without this a multi-tab document silently reads as its first tab.
    #[test]
    fn document_get_url_always_includes_tabs_content() {
        let url = build_document_get_url(BASE, "d1", SuggestionsViewMode::DefaultForCurrentAccess)
            .unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(k, _)| k == "includeTabsContent")
                .map(|(_, v)| v.to_string()),
            Some("true".to_string())
        );
    }

    #[test]
    fn document_get_url_sends_every_suggestions_view_mode() {
        for (mode, wire) in [
            (
                SuggestionsViewMode::DefaultForCurrentAccess,
                "DEFAULT_FOR_CURRENT_ACCESS",
            ),
            (SuggestionsViewMode::Inline, "SUGGESTIONS_INLINE"),
            (
                SuggestionsViewMode::PreviewAccepted,
                "PREVIEW_SUGGESTIONS_ACCEPTED",
            ),
            (
                SuggestionsViewMode::PreviewWithoutSuggestions,
                "PREVIEW_WITHOUT_SUGGESTIONS",
            ),
        ] {
            let url = build_document_get_url(BASE, "d1", mode).unwrap();
            assert_eq!(
                url.query_pairs()
                    .find(|(k, _)| k == "suggestionsViewMode")
                    .map(|(_, v)| v.to_string()),
                Some(wire.to_string()),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn suggestions_view_mode_defaults_to_current_access() {
        assert_eq!(
            SuggestionsViewMode::default(),
            SuggestionsViewMode::DefaultForCurrentAccess
        );
    }

    #[test]
    fn document_get_url_keeps_the_id_as_one_path_segment() {
        let url =
            build_document_get_url(BASE, "1AbC_dEf-Gh", SuggestionsViewMode::default()).unwrap();
        assert_eq!(url.path(), "/v1/documents/1AbC_dEf-Gh");
    }

    /// A document id is opaque today, but the façade still goes through
    /// `push_path_segments` rather than `format!` so no second precedent for
    /// interpolating into a path exists. This pins that an id containing a
    /// URL-meaningful character stays in the path instead of reshaping it.
    #[test]
    fn a_url_meaningful_character_in_an_id_is_percent_encoded() {
        let url = build_document_get_url(BASE, "a/b?c#d", SuggestionsViewMode::default()).unwrap();
        assert_eq!(url.path(), "/v1/documents/a%2Fb%3Fc%23d");
        assert!(url.fragment().is_none(), "{url}");
    }

    #[test]
    fn urls_respect_a_wiremock_style_base_with_a_port() {
        let url = build_document_get_url(
            "http://127.0.0.1:8080",
            "d1",
            SuggestionsViewMode::default(),
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(8080));
        assert_eq!(url.path(), "/v1/documents/d1");
    }

    #[test]
    fn check_document_size_refuses_a_response_over_the_cap() {
        let err = check_document_size(Some(MAX_DOCUMENT_BYTES + 1)).unwrap_err();
        assert!(err.to_string().contains("refusing to load"), "{err}");
    }

    #[test]
    fn check_document_size_allows_a_response_at_the_cap() {
        assert!(check_document_size(Some(MAX_DOCUMENT_BYTES)).is_ok());
    }

    /// Google gzips and chunks, so an absent length is the common case and
    /// must not be treated as a refusal.
    #[test]
    fn check_document_size_allows_a_missing_content_length() {
        assert!(check_document_size(None).is_ok());
    }

    #[test]
    fn batch_update_url_appends_the_method_to_the_id_segment() {
        let url = build_batch_update_url(BASE, "d1").unwrap();
        // One segment, with a literal `:` — not `d1/%3AbatchUpdate`.
        assert_eq!(url.path(), "/v1/documents/d1:batchUpdate");
    }

    #[test]
    fn batch_update_url_respects_a_wiremock_style_base() {
        let url = build_batch_update_url("http://127.0.0.1:8080", "d1").unwrap();
        assert_eq!(url.port(), Some(8080));
        assert_eq!(url.path(), "/v1/documents/d1:batchUpdate");
    }

    fn api_error(status: u16, body: &str) -> anyhow::Error {
        DriveError::ApiRequestFailed {
            api: "Docs",
            status,
            body: body.to_string(),
            reason: Some("INVALID_ARGUMENT".to_string()),
        }
        .into()
    }

    /// The exact shape observed live (ADR-0076 §6).
    #[test]
    fn is_stale_revision_matches_the_confirmed_400() {
        let err = api_error(
            400,
            "The required revision ID 'ALm37BXk3nQ' does not match the latest revision.",
        );
        assert!(is_stale_revision(&err));
    }

    /// The whole reason this matches a message substring rather than the
    /// status: `INVALID_ARGUMENT` is what a *malformed* request carries too,
    /// and telling the user to re-run one would be advice that can never
    /// work.
    #[test]
    fn is_stale_revision_ignores_an_unrelated_invalid_argument_400() {
        let err = api_error(400, "Invalid requests[0].insertText: index out of bounds");
        assert!(!is_stale_revision(&err));
    }

    #[test]
    fn is_stale_revision_ignores_a_403_and_a_500() {
        for status in [403, 500] {
            let err = api_error(status, "does not match the latest revision");
            assert!(!is_stale_revision(&err), "status {status}");
        }
    }

    #[test]
    fn is_stale_revision_ignores_a_non_drive_error() {
        assert!(!is_stale_revision(&anyhow::anyhow!(
            "does not match the latest revision"
        )));
    }

    /// The surface-level half of ADR-0076 §3's guarantee, modelled on
    /// ADR-0061's `no_force_escape_hatch_exists_in_the_ui_surface`.
    ///
    /// The type fence in `write_types.rs` already stops `writeControl` being
    /// omitted or made optional. It does **not** stop someone adding a
    /// `targetRevisionId` field, or hand-building a `serde_json::json!` body
    /// that bypasses the typed struct entirely — and that second route is
    /// not hypothetical, since `sheets/api.rs` builds request bodies exactly
    /// that way. This closes both, and also pins §4's claim that no
    /// destructive request is constructible.
    #[test]
    fn no_destructive_or_unleased_request_is_reachable() {
        let sources = [
            ("api.rs", include_str!("api.rs")),
            ("write_types.rs", include_str!("write_types.rs")),
        ];
        for (name, source) in sources {
            // Production code only: the tests and docs below deliberately
            // name the things they assert the absence of.
            let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);
            for (number, line) in code_only.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue; // prose may discuss a rebase; code may not request one
                }
                let bypasses = code.contains("targetRevisionId")
                    || code.contains("target_revision_id")
                    || code.contains("--force")
                    || code.contains("force: true");
                assert!(
                    !bypasses,
                    "{name}:{}: the Docs write path must never bypass the lease: {line}",
                    number + 1
                );
                let destroys = code.contains("deleteContentRange")
                    || code.contains("deletePositionedObject")
                    || code.contains("deleteTableRow")
                    || code.contains("deleteTableColumn");
                assert!(
                    !destroys,
                    "{name}:{}: no destructive Docs request may be constructible: {line}",
                    number + 1
                );
            }
        }
    }
}
