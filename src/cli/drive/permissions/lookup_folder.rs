//! CLI command for `omni-dev drive permissions lookup-folder`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::files_api::FilesApi;
use crate::drive::folder_ancestry::{self, AncestorChain};
use crate::drive::types::GOOGLE_FOLDER_MIME_TYPE;

/// Default cap on how many search hits to resolve a full path for — each
/// candidate costs its own ancestor-chain walk on top of the search call
/// itself, so this stays modest.
const DEFAULT_LIMIT: usize = 20;

/// Searches Drive for candidate folders and prints their ids and full
/// paths, for pasting into `write_permissions.rules` config.
#[derive(Parser)]
pub struct LookupFolderCommand {
    /// Folder name (or fragment) to search for.
    pub query: String,

    /// Maximum candidates to resolve full paths for.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl LookupFolderCommand {
    /// Runs the command against the shared client resolved by
    /// `PermissionsCommand::execute`.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        run_lookup_folder(client, &self.query, self.limit, &self.output).await
    }
}

/// One candidate folder: id, name, and full root-to-leaf path.
#[derive(Debug, Clone, Serialize)]
pub struct FolderCandidate {
    /// The Drive folder id — paste this into `write_permissions.rules`.
    pub id: String,
    /// The folder's own display name.
    pub name: String,
    /// The full path from Drive's root down to this folder, `/`-joined,
    /// for disambiguating same-named folders in different locations.
    pub path: String,
}

/// Searches for folders matching `query`, resolves each hit's full path,
/// and emits the results in the requested format.
///
/// Split from [`LookupFolderCommand::execute`] so tests can inject a
/// wiremock client without going through the credential-loading path.
async fn run_lookup_folder(
    client: &DriveClient,
    query: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let files_api = FilesApi::new(client);
    let drive_query = format!(
        "mimeType = '{GOOGLE_FOLDER_MIME_TYPE}' and name contains '{}'",
        escape_query_literal(query)
    );
    let hits = files_api.search_all(Some(&drive_query), limit).await?;

    let mut candidates = Vec::with_capacity(hits.files.len());
    for file in &hits.files {
        let chain = folder_ancestry::resolve_ancestor_chain(&files_api, &file.id).await?;
        candidates.push(FolderCandidate {
            id: file.id.clone(),
            name: file.name.clone(),
            path: render_path(&chain),
        });
    }

    if output_as(&candidates, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_candidates_table(&candidates, &mut handle)
}

/// Escapes a user-supplied string for embedding in a Drive query string
/// literal (backslash and single-quote, per Drive's query syntax) — unlike
/// `drive search`'s raw pass-through of an operator-authored query, this
/// command programmatically builds one around caller input, so it must
/// escape it to avoid the input breaking out of the string literal.
fn escape_query_literal(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Renders a root-to-leaf path string from an [`AncestorChain`] (which
/// walks leaf-to-root, `folders[0]` = the target itself).
fn render_path(chain: &AncestorChain) -> String {
    chain
        .folders
        .iter()
        .rev()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Renders candidates as an aligned text table: `ID | NAME | PATH`. An
/// empty input prints `No matching folders found.`.
fn render_candidates_table(candidates: &[FolderCandidate], out: &mut dyn Write) -> Result<()> {
    if candidates.is_empty() {
        writeln!(out, "No matching folders found.")
            .context("Failed to write empty-table message")?;
        return Ok(());
    }

    let ids: Vec<String> = candidates
        .iter()
        .map(|c| sanitize_for_terminal(&c.id))
        .collect();
    let id_width = "ID"
        .len()
        .max(ids.iter().map(String::len).max().unwrap_or(0));
    let names: Vec<String> = candidates
        .iter()
        .map(|c| sanitize_for_terminal(&c.name))
        .collect();
    let name_width = "NAME"
        .len()
        .max(names.iter().map(String::len).max().unwrap_or(0));

    writeln!(out, "{:<id_width$}  {:<name_width$}  PATH", "ID", "NAME")
        .context("Failed to write header row")?;
    for (i, candidate) in candidates.iter().enumerate() {
        writeln!(
            out,
            "{:<id_width$}  {:<name_width$}  {}",
            ids[i],
            names[i],
            sanitize_for_terminal(&candidate.path),
        )
        .context("Failed to write candidate row")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::types::DriveFile;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> DriveClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut client = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    // ── escape_query_literal ────────────────────────────────────────

    #[test]
    fn escape_query_literal_escapes_single_quotes_and_backslashes() {
        assert_eq!(escape_query_literal(r"o'brien"), r"o\'brien");
        assert_eq!(escape_query_literal(r"back\slash"), r"back\\slash");
        assert_eq!(escape_query_literal("plain"), "plain");
    }

    // ── render_path ──────────────────────────────────────────────────

    fn folder(id: &str, name: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: GOOGLE_FOLDER_MIME_TYPE.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn render_path_joins_root_to_leaf() {
        let chain = AncestorChain {
            folders: vec![
                folder("c", "Child"),
                folder("p", "Parent"),
                folder("r", "Root"),
            ],
        };
        assert_eq!(render_path(&chain), "Root/Parent/Child");
    }

    #[test]
    fn render_path_single_folder_is_just_its_name() {
        let chain = AncestorChain {
            folders: vec![folder("r", "Root")],
        };
        assert_eq!(render_path(&chain), "Root");
    }

    // ── render_candidates_table ──────────────────────────────────────

    #[test]
    fn render_table_empty_reports_no_matches() {
        let mut buf = Vec::new();
        render_candidates_table(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("No matching folders found"));
    }

    #[test]
    fn render_table_writes_header_and_rows() {
        let candidates = [FolderCandidate {
            id: "f1".to_string(),
            name: "Reports".to_string(),
            path: "Root/Reports".to_string(),
        }];
        let mut buf = Vec::new();
        render_candidates_table(&candidates, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("NAME"));
        assert!(out.contains("PATH"));
        assert!(out.contains("f1"));
        assert!(out.contains("Reports"));
        assert!(out.contains("Root/Reports"));
    }

    // ── run_lookup_folder (wiremock) ──────────────────────────────────

    #[tokio::test]
    async fn run_lookup_folder_resolves_full_path_per_hit() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .and(wiremock::matchers::query_param(
                "q",
                "mimeType = 'application/vnd.google-apps.folder' and name contains 'Reports'",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [
                        {"id": "f1", "name": "Reports", "mimeType": GOOGLE_FOLDER_MIME_TYPE, "parents": ["root"]},
                    ],
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/f1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "f1", "name": "Reports", "mimeType": GOOGLE_FOLDER_MIME_TYPE, "parents": ["root"],
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/root"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "root", "name": "My Drive", "mimeType": GOOGLE_FOLDER_MIME_TYPE,
                })),
            )
            .mount(&server)
            .await;

        run_lookup_folder(&client, "Reports", 10, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_lookup_folder_no_hits_returns_ok() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"files": []})),
            )
            .mount(&server)
            .await;

        run_lookup_folder(&client, "Nonexistent", 10, &OutputFormat::Table)
            .await
            .unwrap();
    }
}
