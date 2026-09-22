//! CLI surface for `omni-dev drive sheets text-to-columns` (issue #1843,
//! [ADR-0083](../../../../../docs/adrs/adr-0083.md) §1).
//!
//! Gated by
//! [`DriveOperation::SheetsWrite`](crate::drive::write_gate::DriveOperation::SheetsWrite)
//! alone — see that variant's doc comment and `text_to_columns.rs`'s
//! module docs for the live-verification item that could move it to a
//! two-operation gate.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::text_to_columns::{
    describe_lines, text_to_columns, Delimiter, TextToColumnsOptions,
};

/// `--delimiter`'s value set. No variant for the API's
/// `DELIMITER_TYPE_UNSPECIFIED`: its behaviour is undocumented, so this
/// flag is required and has no default.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DelimiterArg {
    Comma,
    Semicolon,
    Period,
    Space,
    Auto,
    Custom,
}

/// Splits a single column's delimited text across the adjacent columns to
/// its right.
///
/// `--source` must resolve to a fully bounded, single-column range (e.g.
/// `A2:A100`) — the API's own "must span exactly one column" constraint,
/// plus this v1's own requirement that it not be open-ended. **How many
/// columns the split needs, and the values it writes, can never be
/// previewed, before or after the request** — `textToColumns` carries no
/// response object, and the split is entirely Sheets' own splitting
/// heuristic. `--dry-run` (and the real run) instead report a local
/// upper-bound width and the count and A1 locations of the non-blank
/// cells within that upper bound that would be (or were) overwritten —
/// never their values or the split pieces.
#[derive(Parser)]
pub struct TextToColumnsCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--source`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The column to split, optionally carrying its own `Sheet!` prefix.
    /// Must resolve to a fully bounded single column (e.g. `A2:A100`).
    #[arg(long, value_name = "A1")]
    pub source: String,

    /// Which separator to split on.
    #[arg(long, value_enum)]
    pub delimiter: DelimiterArg,

    /// The separator text. Required with `--delimiter custom`, and
    /// refused with any other `--delimiter`.
    #[arg(
        long,
        value_name = "TEXT",
        required_if_eq("delimiter", "custom"),
        allow_hyphen_values = true
    )]
    pub custom_delimiter: Option<String>,

    /// Reports the gate verdict, a local upper-bound split width, and the
    /// count and A1 locations of the non-blank cells that would be
    /// overwritten — never the values Sheets would write, which cannot be
    /// previewed.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl TextToColumnsCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        if !matches!(self.delimiter, DelimiterArg::Custom) && self.custom_delimiter.is_some() {
            anyhow::bail!("--custom-delimiter is only used with --delimiter custom");
        }
        let delimiter = match self.delimiter {
            DelimiterArg::Comma => Delimiter::Comma,
            DelimiterArg::Semicolon => Delimiter::Semicolon,
            DelimiterArg::Period => Delimiter::Period,
            DelimiterArg::Space => Delimiter::Space,
            DelimiterArg::Auto => Delimiter::Auto,
            // `required_if_eq` guarantees `--custom-delimiter` is present
            // whenever `--delimiter custom` is; the engine still refuses
            // an empty value (issue #1843's own defense-in-depth for a
            // caller that reaches this options struct some other way).
            DelimiterArg::Custom => Delimiter::Custom(self.custom_delimiter.unwrap_or_default()),
        };
        let opts = TextToColumnsOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            source: Some(self.source),
            delimiter,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_text_to_columns(client, &opts, &self.output).await
    }
}

async fn run_text_to_columns(
    client: &DriveClient,
    opts: &TextToColumnsOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = text_to_columns(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let lines: Vec<String> = describe_lines(&outcome)
        .into_iter()
        .map(|line| sanitize_for_terminal(&line))
        .collect();
    println!("{}", lines.join("\n"));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
    use crate::utils::secret::Secret;
    use clap::CommandFactory;

    #[test]
    fn custom_delimiter_is_required_with_delimiter_custom() {
        let err = TextToColumnsCommand::command()
            .try_get_matches_from([
                "text-to-columns",
                "sheet-1",
                "--source",
                "A1:A10",
                "--delimiter",
                "custom",
            ])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn a_fixed_delimiter_needs_no_custom_text() {
        TextToColumnsCommand::command()
            .try_get_matches_from([
                "text-to-columns",
                "sheet-1",
                "--source",
                "A1:A10",
                "--delimiter",
                "comma",
            ])
            .unwrap();
    }

    #[test]
    fn delimiter_is_required() {
        let err = TextToColumnsCommand::command()
            .try_get_matches_from(["text-to-columns", "sheet-1", "--source", "A1:A10"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn source_is_required() {
        let err = TextToColumnsCommand::command()
            .try_get_matches_from(["text-to-columns", "sheet-1", "--delimiter", "comma"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn custom_delimiter_parses_with_delimiter_custom() {
        let matches = TextToColumnsCommand::command()
            .try_get_matches_from([
                "text-to-columns",
                "sheet-1",
                "--source",
                "A1:A10",
                "--delimiter",
                "custom",
                "--custom-delimiter",
                "|",
            ])
            .unwrap();
        assert_eq!(
            matches
                .get_one::<String>("custom_delimiter")
                .map(String::as_str),
            Some("|")
        );
    }

    async fn client(server: &wiremock::MockServer) -> DriveClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token", "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;
        let credentials = DriveCredentials {
            client_id: "client-1".into(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        };
        let mut client = DriveClient::new(&server.uri(), &credentials).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut client,
            &credentials,
            &format!("{}/token", server.uri()),
        );
        client
    }

    async fn mount_dry_run_prerequisites(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                    "parents": ["parent-1"], "version": "1",
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/parent-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "parent-1", "name": "parent-1",
                    "mimeType": "application/vnd.google-apps.folder", "parents": [],
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "sheets": [{"properties": {"sheetId": 0, "title": "Q1",
                        "gridProperties": {"rowCount": 1000, "columnCount": 26}}}],
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!A1:A10",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [["a,b"]]})),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'Q1'!B1:B10",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": []})),
            )
            .mount(server)
            .await;
    }

    fn allow_settings() -> serde_json::Value {
        serde_json::json!({
            "rules": [{
                "folder_id": "parent-1",
                "recursive": true,
                "allow": ["sheets-write"],
                "require_lease": false,
            }],
        })
    }

    #[tokio::test]
    async fn command_execute_builds_options_and_reaches_the_dry_run_engine() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        TextToColumnsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            source: "A1:A10".to_string(),
            delimiter: DelimiterArg::Comma,
            custom_delimiter: None,
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Table,
        }
        .execute(&client)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn json_output_short_circuits_the_table_renderer() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_dry_run_prerequisites(&server).await;

        TextToColumnsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            source: "A1:A10".to_string(),
            delimiter: DelimiterArg::Comma,
            custom_delimiter: None,
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Json,
        }
        .execute(&client)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_fixed_delimiter_with_stray_custom_text_is_refused() {
        let server = wiremock::MockServer::start().await;
        let client = client(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());

        let err = TextToColumnsCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            source: "A1:A10".to_string(),
            delimiter: DelimiterArg::Comma,
            custom_delimiter: Some("|".to_string()),
            dry_run: true,
            lease: helpers::LeaseTokenArg { lease: None },
            output: OutputFormat::Table,
        }
        .execute(&client)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--custom-delimiter"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
