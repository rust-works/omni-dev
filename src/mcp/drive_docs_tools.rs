//! MCP tool handlers for `drive docs info`/`drive docs read`.
//!
//! Split out from `drive_tools.rs` the same way `src/mcp/drive_sheets_tools.rs`
//! is — and mirrors that module one-for-one, since #1614 (Sheets) and this
//! issue (#1641, Docs) share every design question.
//!
//! Both tools are read-only. Per [ADR-0071](../../docs/adrs/adr-0071.md) §11
//! the Docs read path (like Sheets') consults no write gate — unlike `docs
//! replace`/`append`/`create`, which stay CLI-only for now
//! ([ADR-0076](../../docs/adrs/adr-0076.md) §12; see issue #1641). That means
//! these tools need no `FolderPermissionRule`s, no `dry_run` param, no
//! ADR-0080 lease, and can never produce a `RefusedNoLease`-shaped outcome —
//! those concepts belong to the write engines alone.
//!
//! Like every Drive tool, each handler takes an optional `account` parameter
//! (see `drive_tools.rs`'s module doc); the doc string is shared via
//! [`crate::mcp::drive_tools::account_param_doc`] rather than forked.

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock as Content},
    schemars, tool, tool_router, ErrorData as McpError,
};
use serde::{Deserialize, Serialize};

use crate::cli::drive::docs::info::InfoOutcome;
use crate::cli::drive::helpers::create_client_for;
use crate::drive::client::DriveClient;
use crate::drive::docs::api::{DocsApi, SuggestionsViewMode};
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::read::{read, ReadOptions};
use crate::drive::docs::target;
use crate::drive::files_api::FilesApi;
use crate::mcp::drive_tools::account_param_doc;

use super::error::tool_error;
use super::git_tools::build_truncated_result;
use super::output_file;
use super::server::OmniDevServer;

// ── Parameter structs ───────────────────────────────────────────────

/// Parameters for the `drive_docs_info` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveDocsInfoParams {
    /// Document id (the `/d/<ID>/` segment of a Docs URL, e.g.
    /// `1a2B3c4D5e6F7g8H9iJ0kLmNoPqRsTuVwXyZ`). Required.
    pub document_id: String,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

/// Parameters for the `drive_docs_read` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveDocsReadParams {
    /// Document id (the `/d/<ID>/` segment of a Docs URL, e.g.
    /// `1a2B3c4D5e6F7g8H9iJ0kLmNoPqRsTuVwXyZ`). Required.
    pub document_id: String,
    /// Restrict output to one tab id (see `drive_docs_info`'s `tabs[].tab_id`).
    /// Omit to read every tab.
    #[serde(default)]
    pub tab: Option<String>,
    /// Which suggestion view the text and `[start,end)` indices are reported
    /// against: `default` (whatever the caller's access implies — inline for
    /// an editor, accepted for a reader; the default when omitted), `inline`
    /// (suggestions shown as tracked changes), `accepted` (as if every
    /// suggestion were accepted), or `without` (as if every suggestion were
    /// rejected). Indices from one view are only meaningful for a later edit
    /// made under that same view — a different view can shift every index.
    #[serde(default)]
    pub suggestions_view: Option<String>,
    /// When set, writes the result (YAML) to this path and returns a short
    /// summary instead of the inline body — recommended for a large document
    /// that would exceed the response size limit.
    #[serde(default)]
    pub output_file: Option<String>,
    #[doc = account_param_doc!()]
    #[serde(default)]
    pub account: Option<String>,
}

// ── Tool handlers ────────────────────────────────────────────────────

#[allow(missing_docs)] // #[tool_router] generates a pub `drive_docs_tool_router` fn.
#[tool_router(router = drive_docs_tool_router, vis = "pub")]
impl OmniDevServer {
    /// Tool: show a document's title, revision id and structural outline.
    #[tool(
        description = "Show a document's title, revision id and structural outline: named \
                       ranges and, per tab, heading text with its `[start,end)` index range plus \
                       paragraph/table/section-break counts — no full body text. `revision_id` is \
                       what a later `drive docs replace`/`append` presents as \
                       `writeControl.requiredRevisionId`, so a write against a document that moved \
                       underneath it is refused rather than misapplied; it is absent when the \
                       caller lacks edit access. Use `drive_docs_read` for the full element list \
                       with every index. \
                       Read-only. Mirrors `omni-dev drive docs info`. Output is YAML."
    )]
    pub async fn drive_docs_info(
        &self,
        Parameters(params): Parameters<DriveDocsInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let docs = DocsClient::from_drive_client(&client).map_err(tool_error)?;
        let yaml = run_docs_info(&client, &docs, &params)
            .await
            .map_err(tool_error)?;
        Ok(build_truncated_result(yaml))
    }

    /// Tool: read a document's structural elements with their index ranges.
    #[tool(
        description = "Read a document's structural elements — paragraphs, tables, section \
                       breaks and more — each with its `[start,end)` index range in \
                       UTF-16 code units, plus headers/footers/footnotes and the document's \
                       `revision_id`. This is the *model* channel (indices for a later edit); \
                       `drive_file_read`'s markdown export is the *prose* channel and has no way \
                       back to an index. `tab` restricts to one tab id (from `drive_docs_info`); \
                       omit to read every tab. `suggestions_view` controls which view the indices \
                       are reported against — indices from one view only apply to an edit made \
                       under that same view. When `output_file` is set, writes the YAML result to \
                       that path and returns a short summary instead — recommended for a large \
                       document. \
                       Read-only — no write gate, lease or dry-run applies (unlike `docs \
                       replace`/`append`/`create`, which have no MCP equivalent yet). \
                       Mirrors `omni-dev drive docs read`. Output is YAML."
    )]
    pub async fn drive_docs_read(
        &self,
        Parameters(params): Parameters<DriveDocsReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = create_client_for(params.account.as_deref()).map_err(tool_error)?;
        let docs = DocsClient::from_drive_client(&client).map_err(tool_error)?;
        let wrote_to_file = params.output_file.is_some();
        let text = run_docs_read(&client, &docs, &params)
            .await
            .map_err(tool_error)?;
        if wrote_to_file {
            Ok(CallToolResult::success(vec![Content::text(text)]))
        } else {
            Ok(build_truncated_result(text))
        }
    }
}

// ── Internal run_* implementations ──────────────────────────────────
//
// Split out from the tool handlers, taking already-resolved `&DriveClient`/
// `&DocsClient`, so they can be tested against a wiremock-backed client
// without needing real credentials (mirrors `run_sheets_info`/`run_sheets_read`
// in `drive_sheets_tools.rs`, and the CLI's `run_info`/`run_read`).

async fn run_docs_info(
    drive: &DriveClient,
    docs: &DocsClient,
    params: &DriveDocsInfoParams,
) -> Result<String> {
    let document = match DocsApi::new(docs)
        .get_document(&params.document_id, SuggestionsViewMode::default())
        .await
    {
        Ok(document) => document,
        // Only now is a `files.get` worth spending — see
        // `target::explain_failure` for why classification is lazy.
        Err(err) => {
            return Err(target::explain_failure(
                &FilesApi::new(drive),
                &params.document_id,
                "info",
                err,
            )
            .await)
        }
    };
    let outcome = InfoOutcome::of(&params.document_id, &document);
    yaml_result(&outcome)
}

async fn run_docs_read(
    drive: &DriveClient,
    docs: &DocsClient,
    params: &DriveDocsReadParams,
) -> Result<String> {
    let suggestions = parse_suggestions_view(params.suggestions_view.as_deref())?;
    let opts = ReadOptions {
        document_id: params.document_id.clone(),
        tab: params.tab.clone(),
        suggestions,
    };
    let outcome = match read(&DocsApi::new(docs), &opts).await {
        Ok(outcome) => outcome,
        // Only now is a `files.get` worth spending — see
        // `target::explain_failure` for why classification is lazy.
        Err(err) => {
            return Err(target::explain_failure(
                &FilesApi::new(drive),
                &params.document_id,
                "read",
                err,
            )
            .await)
        }
    };
    let yaml = yaml_result(&outcome)?;
    match params.output_file.as_deref() {
        Some(path) => output_file::write_to_file_yaml(path, &yaml, "yaml"),
        None => Ok(yaml),
    }
}

/// Parses an MCP-supplied suggestions-view string. `None` means
/// [`SuggestionsViewMode::DefaultForCurrentAccess`].
fn parse_suggestions_view(raw: Option<&str>) -> Result<SuggestionsViewMode> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("default") => Ok(SuggestionsViewMode::DefaultForCurrentAccess),
        Some("inline") => Ok(SuggestionsViewMode::Inline),
        Some("accepted") => Ok(SuggestionsViewMode::PreviewAccepted),
        Some("without") => Ok(SuggestionsViewMode::PreviewWithoutSuggestions),
        Some(other) => {
            anyhow::bail!(
                "unknown suggestions_view {other:?} (expected 'default', 'inline', 'accepted', \
                 or 'without')"
            )
        }
    }
}

fn yaml_result<T: Serialize>(data: &T) -> Result<String> {
    serde_yaml::to_string(data).context("Failed to serialize result as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use rmcp::handler::server::wrapper::Parameters;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::docs::client::DOCS_API_URL;
    use crate::drive::test_support::EnvGuard;
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    /// Builds a Drive client plus a Docs client derived from it, both pointed
    /// at the same wiremock server — the Drive client serves the `files.get`
    /// classification fallback, the Docs client (via `DOCS_API_URL`) serves
    /// `documents.get`. Mirrors `src/drive/docs/read.rs`'s and
    /// `src/mcp/drive_sheets_tools.rs`'s test helper of the same shape.
    async fn docs_and_drive_clients(server: &MockServer) -> (DriveClient, DocsClient) {
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
        let docs = DocsClient::from_drive_client_with(&env, &drive).unwrap();
        (drive, docs)
    }

    // ── parse_suggestions_view ───────────────────────────────────────

    #[test]
    fn parse_suggestions_view_defaults_to_current_access() {
        assert_eq!(
            parse_suggestions_view(None).unwrap(),
            SuggestionsViewMode::DefaultForCurrentAccess
        );
        assert_eq!(
            parse_suggestions_view(Some("default")).unwrap(),
            SuggestionsViewMode::DefaultForCurrentAccess
        );
    }

    #[test]
    fn parse_suggestions_view_accepts_known_values_case_insensitively() {
        assert_eq!(
            parse_suggestions_view(Some("Inline")).unwrap(),
            SuggestionsViewMode::Inline
        );
        assert_eq!(
            parse_suggestions_view(Some("ACCEPTED")).unwrap(),
            SuggestionsViewMode::PreviewAccepted
        );
        assert_eq!(
            parse_suggestions_view(Some("without")).unwrap(),
            SuggestionsViewMode::PreviewWithoutSuggestions
        );
    }

    #[test]
    fn parse_suggestions_view_rejects_unknown_values() {
        let err = parse_suggestions_view(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("unknown suggestions_view"));
    }

    // ── run_docs_info ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_docs_info_returns_title_revision_and_outline_as_yaml() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "documentId": "d1",
                "title": "Design Doc",
                "revisionId": "rev-abc",
                "body": {"content": []},
            })))
            .expect(1)
            .mount(&server)
            .await;

        let params = DriveDocsInfoParams {
            document_id: "d1".to_string(),
            account: None,
        };
        let yaml = run_docs_info(&drive, &docs, &params).await.unwrap();
        assert!(yaml.contains("Design Doc"), "{yaml}");
        assert!(yaml.contains("rev-abc"), "{yaml}");
    }

    #[tokio::test]
    async fn run_docs_info_explains_a_non_doc_target() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/s1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": 404, "message": "Requested entity was not found.",
                          "status": "NOT_FOUND"},
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "s1",
                "name": "Budget",
                "mimeType": "application/vnd.google-apps.spreadsheet",
            })))
            .mount(&server)
            .await;

        let params = DriveDocsInfoParams {
            document_id: "s1".to_string(),
            account: None,
        };
        let err = run_docs_info(&drive, &docs, &params).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not a Google Doc"), "{text}");
        assert!(text.contains("drive sheets read"), "{text}");
    }

    // ── run_docs_read ────────────────────────────────────────────────

    fn read_params(tab: Option<&str>) -> DriveDocsReadParams {
        DriveDocsReadParams {
            document_id: "d1".to_string(),
            tab: tab.map(str::to_string),
            suggestions_view: None,
            output_file: None,
            account: None,
        }
    }

    fn document_body() -> serde_json::Value {
        serde_json::json!({
            "documentId": "d1",
            "title": "Design Doc",
            "revisionId": "rev-1",
            "body": {"content": [
                {
                    "startIndex": 1, "endIndex": 10,
                    "paragraph": {
                        "elements": [{"textRun": {"content": "Overview\n"}}],
                        "paragraphStyle": {"namedStyleType": "HEADING_1"},
                    },
                },
            ]},
        })
    }

    #[tokio::test]
    async fn run_docs_read_returns_elements_with_revision_id() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document_body()))
            .mount(&server)
            .await;

        let yaml = run_docs_read(&drive, &docs, &read_params(None))
            .await
            .unwrap();
        assert!(yaml.contains("rev-1"), "{yaml}");
        assert!(yaml.contains("Overview"), "{yaml}");
    }

    #[tokio::test]
    async fn run_docs_read_sends_the_requested_suggestions_view_mode() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .and(query_param(
                "suggestionsViewMode",
                "PREVIEW_SUGGESTIONS_ACCEPTED",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(document_body()))
            .expect(1)
            .mount(&server)
            .await;

        let mut params = read_params(None);
        params.suggestions_view = Some("accepted".to_string());
        run_docs_read(&drive, &docs, &params).await.unwrap();
    }

    #[tokio::test]
    async fn run_docs_read_restricts_to_one_tab() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "documentId": "d1",
                "revisionId": "rev-1",
                "tabs": [
                    {
                        "tabProperties": {"tabId": "t.0", "title": "One"},
                        "documentTab": {"body": {"content": [
                            {"startIndex": 1, "endIndex": 5,
                             "paragraph": {"elements": [{"textRun": {"content": "a\n"}}]}},
                        ]}},
                    },
                    {
                        "tabProperties": {"tabId": "t.1", "title": "Two"},
                        "documentTab": {"body": {"content": [
                            {"startIndex": 1, "endIndex": 5,
                             "paragraph": {"elements": [{"textRun": {"content": "b\n"}}]}},
                        ]}},
                    },
                ],
            })))
            .mount(&server)
            .await;

        let yaml = run_docs_read(&drive, &docs, &read_params(Some("t.1")))
            .await
            .unwrap();
        assert!(yaml.contains("t.1"), "{yaml}");
        assert!(!yaml.contains("t.0"), "{yaml}");
    }

    #[tokio::test]
    async fn run_docs_read_rejects_an_unknown_tab_naming_the_real_ones() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "documentId": "d1",
                "tabs": [
                    {
                        "tabProperties": {"tabId": "t.0", "title": "One"},
                        "documentTab": {"body": {"content": []}},
                    },
                ],
            })))
            .mount(&server)
            .await;

        let err = run_docs_read(&drive, &docs, &read_params(Some("missing")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("t.0"), "{err}");
    }

    #[tokio::test]
    async fn run_docs_read_rejects_an_unknown_suggestions_view_before_any_request() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        // No mocks mounted at all: the bad `suggestions_view` value must be
        // rejected client-side, before any request is issued.
        let mut params = read_params(None);
        params.suggestions_view = Some("bogus".to_string());
        let err = run_docs_read(&drive, &docs, &params).await.unwrap_err();
        assert!(
            err.to_string().contains("unknown suggestions_view"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn run_docs_read_writes_output_file_and_returns_summary() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document_body()))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("out.yaml");
        let mut params = read_params(None);
        params.output_file = Some(out_path.to_str().unwrap().to_string());

        let summary = run_docs_read(&drive, &docs, &params).await.unwrap();
        assert!(!summary.contains("Overview"), "{summary}");
        assert!(summary.contains("format: yaml"), "{summary}");
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("Overview"), "{written}");
    }

    #[tokio::test]
    async fn run_docs_read_keeps_the_original_error_when_classification_fails() {
        let server = MockServer::start().await;
        let (drive, docs) = docs_and_drive_clients(&server).await;
        Mock::given(method("GET"))
            .and(path("/v1/documents/d1"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {"code": 500, "message": "backend error", "status": "INTERNAL"},
            })))
            .mount(&server)
            .await;
        // `files.get` fallback also fails, so classification cannot improve
        // the message and the original error must survive untouched.
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/d1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = run_docs_read(&drive, &docs, &read_params(None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("backend error"), "{err}");
    }

    // ── handler-level smoke tests ────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn drive_docs_info_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_docs_info(Parameters(DriveDocsInfoParams {
                document_id: "d1".to_string(),
                account: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drive_docs_read_handler_propagates_credentials_error() {
        let guard = EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = OmniDevServer::new();
        let err = server
            .drive_docs_read(Parameters(read_params(None)))
            .await
            .unwrap_err();
        assert!(err.message.contains("not configured"));
    }
}
