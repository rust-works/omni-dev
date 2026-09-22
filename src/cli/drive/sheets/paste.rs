//! CLI commands for `omni-dev drive sheets cut-paste`/`copy-paste`/
//! `paste-data` (issue #1839, [ADR-0083](../../../../docs/adrs/adr-0083.md)
//! §4).
//!
//! `--paste-type` defaults to `normal` on `cut-paste`/`copy-paste` (the
//! API's own default and what "paste" means to a Sheets user) and to
//! `values` on `paste-data` (its delimited-text input carries no formats,
//! merges or validation for `normal` to add) — ADR-0083 §4 fixes both
//! defaults so this module doesn't pick its own. See
//! `crate::drive::sheets::paste`'s module doc for the gate composition per
//! paste type and the curated four-of-seven `PasteType` surface.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::paste::{describe_lines, paste, read_data_text, PasteOptions, PasteVerb};
use crate::drive::sheets::types::{PasteOrientation, PasteType};

/// The `--paste-type` values this crate models — mirrors [`PasteType`]
/// with a `clap::ValueEnum` derive, since the engine type deliberately
/// carries no `clap` dependency (the same split `banding.rs`'s
/// `BandingAxisArg` uses). Curated: four of the API's seven variants — see
/// [`PasteType`]'s doc comment for the deferred three.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PasteTypeArg {
    /// Values, formulas, formats and merges.
    Normal,
    /// Cell content only, evaluated formulas rendered to their values.
    Values,
    /// Cell content only, formulas preserved.
    Formula,
    /// The cell format only, excluding data validation.
    Format,
}

impl From<PasteTypeArg> for PasteType {
    fn from(value: PasteTypeArg) -> Self {
        match value {
            PasteTypeArg::Normal => Self::Normal,
            PasteTypeArg::Values => Self::Values,
            PasteTypeArg::Formula => Self::Formula,
            PasteTypeArg::Format => Self::Format,
        }
    }
}

/// `copy-paste`'s `--orientation` — mirrors [`PasteOrientation`].
#[derive(Debug, Clone, Copy, clap::ValueEnum, Default)]
pub enum OrientationArg {
    /// Rows stay rows, columns stay columns (the API's own default).
    #[default]
    Normal,
    /// Rows and columns are swapped.
    Transpose,
}

impl From<OrientationArg> for PasteOrientation {
    fn from(value: OrientationArg) -> Self {
        match value {
            OrientationArg::Normal => Self::Normal,
            OrientationArg::Transpose => Self::Transpose,
        }
    }
}

/// Moves a range to a destination cell, clearing the source. Gated by
/// **both** the `sheets-write` and `sheets-structure` write-permission
/// operations, whatever `--paste-type` names: the source is cleared in
/// full regardless of what is pasted (ADR-0083 §4).
#[derive(Parser)]
pub struct CutPasteCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--source`/`--destination` when either lacks its
    /// own `Sheet!` prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The range to move. May carry its own `Sheet!` prefix. Must be a
    /// bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub source: String,

    /// The single-cell destination. May carry its own `Sheet!` prefix.
    #[arg(long, value_name = "[SHEET!]A1")]
    pub destination: String,

    /// What to carry into the destination. The source is cleared in full
    /// either way.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Normal)]
    pub paste_type: PasteTypeArg,

    /// Reports the gate verdict, the region that would be overwritten at
    /// the destination, and the cells that would be cleared at the
    /// source, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CutPasteCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::CutPaste {
                sheet: self.sheet,
                source: self.source,
                destination: self.destination,
                paste_type: self.paste_type.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

/// Copies a range to a destination, spilling a larger source past the
/// destination's end or repeating a smaller one to fill it (the API's own
/// rule; see `crate::drive::sheets::paste::copy_paste_extent`). Gated by
/// `--paste-type` (ADR-0083 §4): a value-only type needs `sheets-write`
/// alone, a presentation-only type `sheets-structure` alone, and `normal`
/// (the default) needs both.
#[derive(Parser)]
pub struct CopyPasteCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--source`/`--destination` when either lacks its
    /// own `Sheet!` prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The range to copy from. May carry its own `Sheet!` prefix. Must be
    /// a bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub source: String,

    /// The destination range (a single cell is a valid anchor). May carry
    /// its own `Sheet!` prefix. Must be a bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub destination: String,

    /// What to carry over. `normal` (the default) needs both the
    /// `sheets-write` and `sheets-structure` operations — a bare
    /// `copy-paste` under a `sheets-write`-only grant is refused; pass
    /// `--paste-type values` for a values-only copy that grant already
    /// covers.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Normal)]
    pub paste_type: PasteTypeArg,

    /// Swaps rows and columns before pasting.
    #[arg(long, value_enum, default_value_t = OrientationArg::Normal)]
    pub orientation: OrientationArg,

    /// Reports the gate verdict and the region that would be overwritten
    /// at the destination, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CopyPasteCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::CopyPaste {
                sheet: self.sheet,
                source: self.source,
                destination: self.destination,
                paste_type: self.paste_type.into(),
                orientation: self.orientation.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

/// Pastes delimited text into a range anchored at a destination cell, as
/// if pasted from the clipboard. `delimiter`-form only — the API's `html`
/// alternative is a documented cut (ADR-0083 §4); see
/// `crate::drive::sheets::paste`'s module doc.
#[derive(Parser)]
pub struct PasteDataCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--destination` when it lacks its own `Sheet!`
    /// prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The single-cell destination. May carry its own `Sheet!` prefix.
    #[arg(long, value_name = "[SHEET!]A1")]
    pub destination: String,

    /// The delimited text to paste, given literally. Sent verbatim —
    /// never parsed into cells locally. Mutually exclusive with
    /// `--data-file`; exactly one of the two is required.
    #[arg(long, value_name = "TEXT", required_unless_present = "data_file")]
    pub data: Option<String>,

    /// The delimited text to paste, read from a local file, or `-` to read
    /// stdin. Sent verbatim — never parsed into cells locally.
    #[arg(long, value_name = "PATH|-", conflicts_with = "data")]
    pub data_file: Option<String>,

    /// The delimiter splitting each row of the pasted block into columns.
    #[arg(long, default_value = "\t")]
    pub delimiter: String,

    /// What to carry over. Defaults to `values`, not `normal`: delimited
    /// text carries no formats, merges or validation for `normal` to add.
    /// `normal` is still selectable — the API does not document a normal
    /// paste of delimited text as doing anything beyond values — and
    /// resolves both operations.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Values)]
    pub paste_type: PasteTypeArg,

    /// Reports the gate verdict and the region that would be overwritten
    /// at the destination, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl PasteDataCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        // clap's `required_unless_present`/`conflicts_with` pair makes
        // exactly one of the two reachable, so the `None`/`None` arm is
        // `unreachable` in practice — an `anyhow` error rather than a
        // panic, since a clap attribute is a weaker guarantee than the
        // type system.
        let data = match (self.data, self.data_file) {
            (Some(text), _) => text,
            (None, Some(path)) => read_data_text(&path)?,
            (None, None) => anyhow::bail!("one of --data or --data-file is required"),
        };
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::PasteData {
                sheet: self.sheet,
                destination: self.destination,
                data,
                delimiter: self.delimiter,
                paste_type: self.paste_type.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

async fn run_paste(client: &DriveClient, opts: &PasteOptions, output: &OutputFormat) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = paste(client, &sheets, opts, &rules).await;
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

    #[test]
    fn paste_type_arg_converts_to_the_engine_type() {
        assert!(matches!(
            PasteType::from(PasteTypeArg::Normal),
            PasteType::Normal
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Values),
            PasteType::Values
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Formula),
            PasteType::Formula
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Format),
            PasteType::Format
        ));
    }

    #[test]
    fn orientation_arg_converts_to_the_engine_type() {
        assert!(matches!(
            PasteOrientation::from(OrientationArg::Normal),
            PasteOrientation::Normal
        ));
        assert!(matches!(
            PasteOrientation::from(OrientationArg::Transpose),
            PasteOrientation::Transpose
        ));
    }

    // ── `execute`, against a wiremock Drive+Sheets backend ──

    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::sheets::client::SHEETS_API_URL;
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

    /// No write-permission rules are configured (an unconfigured account —
    /// see `client_with_bootstrapped_token`'s `EnvGuard::clear_credentials`
    /// caller), so the gate refuses by default policy. That's enough to
    /// drive each `execute` (and thus `run_paste`) through its full
    /// CLI-level path — building `PasteOptions`, calling `paste`, and
    /// rendering the `describe_lines` output — without needing a lease or a
    /// workbook fetch, which a `Blocked` verdict never reaches.
    async fn mount_ungated_target(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "sheet-1",
                    "name": "Budget",
                    "mimeType": crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
                    "parents": ["folder-1"],
                })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/drive/v3/files/folder-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "folder-1",
                    "name": "folder-1",
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": [],
                })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn cut_paste_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = CutPasteCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            source: "A1:B2".to_string(),
            destination: "D1".to_string(),
            paste_type: PasteTypeArg::Normal,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn copy_paste_command_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = CopyPasteCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            source: "A1:B2".to_string(),
            destination: "D1:E2".to_string(),
            paste_type: PasteTypeArg::Normal,
            orientation: OrientationArg::Transpose,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Yaml,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn paste_data_command_reads_data_from_a_file_and_runs_end_to_end() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.tsv");
        std::fs::write(&path, "1\t2").unwrap();

        let cmd = PasteDataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            destination: "A1".to_string(),
            data: None,
            data_file: Some(path.to_str().unwrap().to_string()),
            delimiter: "\t".to_string(),
            paste_type: PasteTypeArg::Values,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[tokio::test]
    async fn paste_data_command_takes_literal_text_from_data() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_ungated_target(&server).await;

        let cmd = PasteDataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            destination: "A1".to_string(),
            data: Some("1\t2".to_string()),
            data_file: None,
            delimiter: "\t".to_string(),
            paste_type: PasteTypeArg::Values,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        assert!(cmd.execute(&client).await.is_ok());
    }

    #[test]
    fn the_data_flags_are_mutually_exclusive_and_one_is_required() {
        use clap::CommandFactory as _;
        let cmd = || PasteDataCommand::command().no_binary_name(true);
        assert!(cmd()
            .try_get_matches_from(["sheet-1", "--destination", "A1", "--data", "x"])
            .is_ok());
        assert!(cmd()
            .try_get_matches_from(["sheet-1", "--destination", "A1", "--data-file", "-"])
            .is_ok());
        // Neither: refused, rather than pasting an empty block.
        assert!(cmd()
            .try_get_matches_from(["sheet-1", "--destination", "A1"])
            .is_err());
        // Both: refused, rather than one silently winning.
        let err = cmd()
            .try_get_matches_from([
                "sheet-1",
                "--destination",
                "A1",
                "--data",
                "x",
                "--data-file",
                "clip.tsv",
            ])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[tokio::test]
    async fn paste_data_command_execute_surfaces_a_missing_data_file_before_any_network_call() {
        let server = wiremock::MockServer::start().await;
        // No mocks at all — `read_data_text` must fail before any network
        // call is ever attempted.
        let client = client_with_bootstrapped_token(&server).await;

        let cmd = PasteDataCommand {
            spreadsheet_id: "sheet-1".to_string(),
            sheet: Some("Q1".to_string()),
            destination: "A1".to_string(),
            data: None,
            data_file: Some("/definitely/not/here.tsv".to_string()),
            delimiter: "\t".to_string(),
            paste_type: PasteTypeArg::Values,
            dry_run: true,
            lease: crate::cli::drive::helpers::LeaseTokenArg { lease: None },
            output: crate::cli::drive::format::OutputFormat::Table,
        };
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("Failed to stat"), "{err}");
    }
}
