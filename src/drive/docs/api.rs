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
}
