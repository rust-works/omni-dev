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

use crate::cli::drive::format::{write_items_jsonl, JsonlSerialize};
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

/// One element, with the document and tab identity denormalised onto it.
///
/// The `-o jsonl` record. A document's element list *is* a record stream,
/// unlike a sheet's rows, so each line is one element rather than one line
/// per document. The repetition of `document_id`/`revision_id`/`tab_id` on
/// every line is the point — it is what makes a single line self-describing
/// to `jq`, which is the whole reason to reach for `jsonl` over `json`.
///
/// The element's own fields are `flatten`ed to the top level rather than
/// nested under a key, so the natural filter is `jq -r '.text'` and every
/// field name matches the one `-o json` uses at its own depth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlatElement<'a> {
    /// The document these elements came from.
    pub document_id: &'a str,
    /// The revision they were read at, when the caller has edit access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<&'a str>,
    /// The holding tab's id, absent for a legacy single-body document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<&'a str>,
    /// The element itself, inlined.
    #[serde(flatten)]
    pub element: &'a DocElement,
}

impl JsonlSerialize for ReadOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_items_jsonl(self.flat_elements().iter(), out)
    }
}

impl ReadOutcome {
    /// Every element across every tab, carrying the identity of the document
    /// and tab that hold it.
    ///
    /// This is the `-o jsonl` projection; see [`FlatElement`].
    #[must_use]
    pub fn flat_elements(&self) -> Vec<FlatElement<'_>> {
        self.tabs
            .iter()
            .flat_map(|tab| {
                tab.elements.iter().map(move |element| FlatElement {
                    document_id: &self.document_id,
                    revision_id: self.revision_id.as_deref(),
                    tab_id: tab.tab_id.as_deref(),
                    element,
                })
            })
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

    /// A two-tab document whose `-o jsonl` rendering the next few tests
    /// assert against.
    async fn two_tab_outcome(server: &MockServer) -> ReadOutcome {
        let client = docs_client(server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "revisionId": "rev-1",
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
        .mount(server)
        .await;

        read(&DocsApi::new(&client), &opts(None)).await.unwrap()
    }

    /// Renders `outcome` the way `-o jsonl` does, as the bytes a caller pipes
    /// into `jq`.
    ///
    /// Asserting on the rendered output rather than on `flat_elements()` is
    /// deliberate: the helper existed and was correct while `write_jsonl`
    /// still emitted one line for the whole document, so a test of the
    /// projection alone cannot see the bug that mattered.
    fn render_jsonl(outcome: &ReadOutcome) -> String {
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// The `-o jsonl` shape: one record per element, not one per document.
    #[tokio::test]
    async fn jsonl_emits_one_line_per_element_across_every_tab() {
        let server = MockServer::start().await;
        let outcome = two_tab_outcome(&server).await;

        let rendered = render_jsonl(&outcome);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);

        let texts: Vec<String> = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["text"].to_string()
            })
            .collect();
        assert_eq!(texts, vec!["\"a\"", "\"b\"", "\"c\""]);
    }

    /// Each line carries the document and tab identity, which is what makes
    /// it self-describing once it is separated from its siblings.
    #[tokio::test]
    async fn jsonl_denormalises_document_and_tab_identity_onto_every_line() {
        let server = MockServer::start().await;
        let outcome = two_tab_outcome(&server).await;
        let rendered = render_jsonl(&outcome);

        let lines: Vec<serde_json::Value> = rendered
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        for line in &lines {
            assert_eq!(line["document_id"], "d1");
            assert_eq!(line["revision_id"], "rev-1");
        }
        assert_eq!(lines[0]["tab_id"], "t.0");
        assert_eq!(lines[1]["tab_id"], "t.0");
        assert_eq!(lines[2]["tab_id"], "t.1");
    }

    /// The element's own fields sit at the top level, not nested under a
    /// key — `jq -r '.text'` is the documented filter, and a wrapper object
    /// would silently make it yield `null`.
    #[tokio::test]
    async fn jsonl_flattens_element_fields_to_the_top_level() {
        let server = MockServer::start().await;
        let outcome = two_tab_outcome(&server).await;
        let rendered = render_jsonl(&outcome);

        let first: serde_json::Value =
            serde_json::from_str(rendered.lines().next().unwrap()).unwrap();
        assert_eq!(first["text"], "a");
        assert_eq!(first["kind"], "paragraph");
        assert_eq!(first["start_index"], 1);
        assert_eq!(first["end_index"], 5);
        assert!(first.get("element").is_none());
    }

    /// A read-only caller has no `revisionId`, and the field is omitted
    /// rather than rendered as `null` — the same contract `-o json` keeps.
    #[tokio::test]
    async fn jsonl_omits_revision_and_tab_when_absent() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({
            "documentId": "d1",
            "body": {"content": [paragraph(1, 5, "a\n")]},
        }))
        .mount(&server)
        .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        let rendered = render_jsonl(&outcome);
        let line: serde_json::Value =
            serde_json::from_str(rendered.lines().next().unwrap()).unwrap();

        assert_eq!(line["document_id"], "d1");
        assert!(line.get("revision_id").is_none());
        assert!(line.get("tab_id").is_none());
    }

    /// An empty document renders no lines at all, rather than one line
    /// describing a document with nothing in it.
    #[tokio::test]
    async fn jsonl_emits_nothing_for_a_document_with_no_elements() {
        let server = MockServer::start().await;
        let client = docs_client(&server).await;
        mount_document(serde_json::json!({"documentId": "d1", "body": {"content": []}}))
            .mount(&server)
            .await;

        let outcome = read(&DocsApi::new(&client), &opts(None)).await.unwrap();
        assert_eq!(render_jsonl(&outcome), "");
    }
}
