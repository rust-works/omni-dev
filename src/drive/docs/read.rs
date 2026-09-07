//! `drive docs read` engine — one `documents.get`, flattened into an
//! index-ordered element list per tab.
//!
//! **Ungated**, exactly as `sheets read` and the rest of the Drive read
//! surface are. [ADR-0071](../../../docs/adrs/adr-0071.md) §11 records
//! read-path enforcement as a known, scoped gap across the whole
//! integration, pending a batching/caching design; this command inherits
//! that gap rather than opting out of it, and will inherit its resolution.
//!
//! There is deliberately **no per-tab cap** of the kind `sheets read`'s
//! `MAX_SHEETS_PER_READ` imposes. That cap exists because a whole-workbook
//! read fans out into *N* `values.batchGet` requests and could return a
//! partially-fetched workbook. `documents.get` is exactly one request whose
//! result is all-or-nothing, so a cap could only refuse a document already
//! in hand; `MAX_DOCUMENT_BYTES` is the analogous protection and it sits at
//! the right layer.

use anyhow::Result;
use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::docs::api::{DocsApi, SuggestionsViewMode};
use crate::drive::docs::structure::{flatten, DocElement};

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// The document to read.
    pub document_id: String,
    /// Restrict output to one tab id.
    ///
    /// Applied **client-side after the fetch** — `documents.get` has no
    /// per-tab endpoint, and fetching every tab is what makes the tab list
    /// (and so a useful error for a wrong id) available at all.
    pub tab: Option<String>,
    /// Which suggestion view the indices are reported against.
    pub suggestions: SuggestionsViewMode,
}

/// One tab's flattened content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabContent {
    /// The tab's id, absent for a legacy single-body document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    /// The tab's title, likewise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Depth in the tab tree.
    pub nesting_level: i64,
    /// The tab's structural elements, in index order.
    pub elements: Vec<DocElement>,
}

/// The full result of one read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadOutcome {
    /// The document read.
    pub document_id: String,
    /// Its title, when the response carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The document's revision at the moment of the read.
    ///
    /// This is the `writeControl.requiredRevisionId` token a later
    /// `documents.batchUpdate` presents so a write against a document that
    /// moved underneath it is refused rather than silently misapplied.
    /// Absent when the caller lacks edit access, which Google signals by
    /// omitting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// The tabs read, in document order.
    pub tabs: Vec<TabContent>,
}

impl JsonlSerialize for ReadOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

impl ReadOutcome {
    /// Every element across every tab, paired with the tab that holds it.
    ///
    /// The `-o jsonl` shape: a document's element list *is* a record stream,
    /// unlike a sheet's rows, so each line is one element with the document
    /// and tab identity denormalised onto it. That redundancy is the point —
    /// it is what makes a single line self-describing to `jq`.
    #[must_use]
    pub fn flat_elements(&self) -> Vec<(&TabContent, &DocElement)> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.elements.iter().map(move |el| (tab, el)))
            .collect()
    }
}

/// Reads a document and flattens each tab's body.
pub async fn read(api: &DocsApi<'_>, opts: &ReadOptions) -> Result<ReadOutcome> {
    let document = api
        .get_document(&opts.document_id, opts.suggestions)
        .await?;

    let resolved = document.resolved_tabs();
    let mut tabs: Vec<TabContent> = resolved
        .iter()
        .map(|tab| TabContent {
            tab_id: tab.tab_id.map(ToString::to_string),
            title: tab.title.map(ToString::to_string),
            nesting_level: tab.nesting_level,
            elements: tab.body.map(flatten).unwrap_or_default(),
        })
        .collect();

    if let Some(wanted) = &opts.tab {
        // An unknown tab id is an *error* naming the real ones, never an
        // empty result: a typo'd id and a genuinely empty tab must not look
        // alike. Same reasoning as `sheets read`'s unknown-sheet handling.
        let known: Vec<&str> = tabs.iter().filter_map(|t| t.tab_id.as_deref()).collect();
        anyhow::ensure!(
            known.contains(&wanted.as_str()),
            "document '{}' has no tab '{wanted}'; it has {}",
            opts.document_id,
            if known.is_empty() {
                "no tabs (it predates tabs, or was returned in the legacy single-body form)"
                    .to_string()
            } else {
                format!("tabs: {}", known.join(", "))
            }
        );
        tabs.retain(|t| t.tab_id.as_deref() == Some(wanted.as_str()));
    }

    Ok(ReadOutcome {
        document_id: opts.document_id.clone(),
        title: document.title.clone(),
        revision_id: document.revision_id.clone(),
        tabs,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
    use crate::drive::docs::client::{DocsClient, DOCS_API_URL};
    use crate::drive::docs::structure::ElementKind;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    /// Builds a Docs client pointed at wiremock, via the real derivation
    /// path.
    ///
    /// Note the ordering: `replace_session` swaps the Drive client's whole
    /// transport, so it must run **before** the derive. Deriving first would
    /// leave the Docs client holding the original session, pointed at the
    /// real `oauth2.googleapis.com` — a live network call from a unit test.
    async fn docs_client(server: &MockServer) -> DocsClient {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "test-token",
                "expires_in": 3600,
            })))
            .mount(server)
            .await;

        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(DOCS_API_URL, &server.uri());
        DocsClient::from_drive_client_with(&env, &drive).unwrap()
    }

    fn opts(tab: Option<&str>) -> ReadOptions {
        ReadOptions {
            document_id: "d1".to_string(),
            tab: tab.map(str::to_string),
            suggestions: SuggestionsViewMode::default(),
        }
    }

    fn paragraph(start: i64, end: i64, text: &str) -> serde_json::Value {
        serde_json::json!({
            "startIndex": start, "endIndex": end,
            "paragraph": {
                "elements": [{"textRun": {"content": text}}],
                "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"},
            },
        })
    }

    fn mount_document(body: serde_json::Value) -> Mock {
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    }

    #[tokio::test]
    async fn read_calls_documents_get_exactly_once() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1", "title": "Doc", "revisionId": "rev-1",
            "body": {"content": [paragraph(1, 10, "hello\n")]},
        }))
        .expect(1)
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        assert_eq!(outcome.tabs.len(), 1);
        assert_eq!(outcome.tabs[0].elements[0].text, "hello");
    }

    /// The lease token has to survive the read, or the write phase has
    /// nothing to present.
    #[tokio::test]
    async fn read_carries_the_revision_id_through() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1", "revisionId": "rev-abc",
            "body": {"content": []},
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        assert_eq!(outcome.revision_id.as_deref(), Some("rev-abc"));
    }

    /// A reader-only caller gets no `revisionId`; the read still succeeds.
    #[tokio::test]
    async fn read_without_edit_access_has_no_revision_id() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({"documentId": "d1", "body": {"content": []}}))
            .mount(&server)
            .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        assert_eq!(outcome.revision_id, None);
    }

    #[tokio::test]
    async fn read_of_a_tabbed_document_returns_every_tab_in_order() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1", "revisionId": "rev-1",
            "tabs": [
                {
                    "tabProperties": {"tabId": "t.0", "title": "One", "nestingLevel": 0},
                    "documentTab": {"body": {"content": [paragraph(1, 5, "a\n")]}},
                },
                {
                    "tabProperties": {"tabId": "t.1", "title": "Two", "nestingLevel": 0},
                    "documentTab": {"body": {"content": [paragraph(1, 5, "b\n")]}},
                },
            ],
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        let ids: Vec<_> = outcome
            .tabs
            .iter()
            .map(|t| t.tab_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, vec!["t.0", "t.1"]);
        assert_eq!(outcome.tabs[1].elements[0].text, "b");
    }

    #[tokio::test]
    async fn read_of_a_legacy_body_document_returns_one_anonymous_tab() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "body": {"content": [{"endIndex": 1, "sectionBreak": {}}]},
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        assert_eq!(outcome.tabs.len(), 1);
        assert_eq!(outcome.tabs[0].tab_id, None);
        assert_eq!(outcome.tabs[0].elements[0].kind, ElementKind::SectionBreak);
    }

    #[tokio::test]
    async fn read_filters_to_a_single_tab() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "tabs": [
                {
                    "tabProperties": {"tabId": "t.0", "title": "One"},
                    "documentTab": {"body": {"content": [paragraph(1, 5, "a\n")]}},
                },
                {
                    "tabProperties": {"tabId": "t.1", "title": "Two"},
                    "documentTab": {"body": {"content": [paragraph(1, 5, "b\n")]}},
                },
            ],
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(Some("t.1")))
            .await
            .unwrap();
        assert_eq!(outcome.tabs.len(), 1);
        assert_eq!(outcome.tabs[0].elements[0].text, "b");
    }

    /// A typo'd tab id and a genuinely empty tab must not look alike.
    #[tokio::test]
    async fn read_with_an_unknown_tab_id_errors_and_names_the_real_ids() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "tabs": [
                {"tabProperties": {"tabId": "t.0"}, "documentTab": {"body": {"content": []}}},
                {"tabProperties": {"tabId": "t.1"}, "documentTab": {"body": {"content": []}}},
            ],
        }))
        .mount(&server)
        .await;

        let err = read(&DocsApi::new(&client), &opts(Some("nope")))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tab 'nope'"), "{err}");
        assert!(err.contains("t.0"), "{err}");
        assert!(err.contains("t.1"), "{err}");
    }

    /// Asking for a tab of a legacy single-body document explains *why*
    /// there are none, rather than printing an empty id list.
    #[tokio::test]
    async fn read_with_a_tab_filter_on_a_legacy_document_explains_itself() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({"documentId": "d1", "body": {"content": []}}))
            .mount(&server)
            .await;

        let err = read(&DocsApi::new(&client), &opts(Some("t.0")))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tabs"), "{err}");
    }

    #[tokio::test]
    async fn read_surfaces_an_api_error_with_its_status_and_message() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": 404, "message": "Requested entity was not found.",
                          "status": "NOT_FOUND"},
            })))
            .mount(&server)
            .await;

        let err = read(&DocsApi::new(&client), &opts(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Docs API request failed"), "{err}");
        assert!(err.contains("404"), "{err}");
        assert!(err.contains("Requested entity was not found."), "{err}");
    }

    #[tokio::test]
    async fn read_honours_the_suggestions_view_mode() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .and(query_param(
                "suggestionsViewMode",
                "PREVIEW_SUGGESTIONS_ACCEPTED",
            ))
            .and(query_param("includeTabsContent", "true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"documentId": "d1", "body": {"content": []}}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let options = ReadOptions {
            suggestions: SuggestionsViewMode::PreviewAccepted,
            ..opts(None)
        };
        read(&DocsApi::new(&client), &options).await.unwrap();
    }

    #[tokio::test]
    async fn read_outcome_serialises_tabs_as_an_ordered_list() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1", "title": "T", "revisionId": "rev-1",
            "tabs": [
                {"tabProperties": {"tabId": "t.0"}, "documentTab": {"body": {"content": []}}},
                {"tabProperties": {"tabId": "t.1"}, "documentTab": {"body": {"content": []}}},
            ],
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        let json = serde_json::to_value(&outcome).unwrap();
        // A list, never a map: tab order is meaningful and map key order is
        // not guaranteed through serde_json.
        assert!(json["tabs"].is_array());
        assert_eq!(json["tabs"][0]["tab_id"], "t.0");
        assert_eq!(json["revision_id"], "rev-1");
    }

    #[tokio::test]
    async fn read_outcome_omits_absent_optional_fields() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({"documentId": "d1", "body": {"content": []}}))
            .mount(&server)
            .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        let json = serde_json::to_value(&outcome).unwrap();
        assert!(json.get("title").is_none());
        assert!(json.get("revision_id").is_none());
        assert!(json["tabs"][0].get("tab_id").is_none());
    }

    /// The `-o jsonl` shape: one record per element, not one per document.
    #[tokio::test]
    async fn flat_elements_streams_every_element_across_every_tab() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "tabs": [
                {
                    "tabProperties": {"tabId": "t.0"},
                    "documentTab": {"body": {"content": [
                        paragraph(1, 5, "a\n"), paragraph(5, 9, "b\n")]}},
                },
                {
                    "tabProperties": {"tabId": "t.1"},
                    "documentTab": {"body": {"content": [paragraph(1, 5, "c\n")]}},
                },
            ],
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        let flat = outcome.flat_elements();
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].0.tab_id.as_deref(), Some("t.0"));
        assert_eq!(flat[2].0.tab_id.as_deref(), Some("t.1"));
        assert_eq!(flat[2].1.text, "c");
    }
}
