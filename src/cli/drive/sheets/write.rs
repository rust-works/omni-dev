//! CLI commands for `omni-dev drive sheets write`/`append`/`clear`.
//!
//! Three clap structs over one engine call. They share `run_write`, so the
//! gate wiring, `--dry-run` handling, output rendering and request logging
//! cannot drift between them.

use std::io::Read as _;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::drive::sheets::values::{self, ValuesFormat};
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::ValueInputOption;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::write::{describe, write, WriteOptions, WriteVerb};

/// `--input`'s value set — a thin CLI-layer copy of [`ValueInputOption`],
/// keeping `clap` out of the engine (mirrors
/// `crate::cli::drive::permissions::check::OperationArg`).
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum InputArg {
    /// Parse values as if typed into the UI: `=SUM(A1:A3)` becomes a
    /// formula, `2026-09-06` a date, `1,234` a number.
    #[default]
    UserEntered,
    /// Store every value verbatim as text; a leading `=` stays literal.
    Raw,
}

impl From<InputArg> for ValueInputOption {
    fn from(arg: InputArg) -> Self {
        match arg {
            InputArg::UserEntered => Self::UserEntered,
            InputArg::Raw => Self::Raw,
        }
    }
}

/// Overwrites the cells of a range.
#[derive(Parser)]
pub struct WriteCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to write, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    /// Conflicts with a `--range` that already names a sheet.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Values to write: a local file path, or `-` to read stdin. CSV unless
    /// the path ends in `.json` or `--values-format` says otherwise.
    #[arg(long, value_name = "PATH|-")]
    pub values: String,

    /// How to parse `--values`. `auto` infers from the file extension.
    #[arg(long = "values-format", value_enum, default_value_t = ValuesFormat::Auto)]
    pub values_format: ValuesFormat,

    /// How the API interprets the values. `user-entered` (the default)
    /// parses formulas, dates and numbers the way typing them into the UI
    /// would; `raw` stores every value verbatim as text.
    #[arg(long, value_enum, default_value_t = InputArg::UserEntered)]
    pub input: InputArg,

    /// Reports the gate verdict and the parsed dimensions without calling
    /// the Sheets API.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Appends rows after the last row of a range's table.
#[derive(Parser)]
pub struct AppendCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range identifying the table to append to, optionally carrying its
    /// own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Rows to append: a local file path, or `-` to read stdin.
    #[arg(long, value_name = "PATH|-")]
    pub values: String,

    /// How to parse `--values`. `auto` infers from the file extension.
    #[arg(long = "values-format", value_enum, default_value_t = ValuesFormat::Auto)]
    pub values_format: ValuesFormat,

    /// How the API interprets the values (see `drive sheets write`).
    #[arg(long, value_enum, default_value_t = InputArg::UserEntered)]
    pub input: InputArg,

    /// Reports the gate verdict and the parsed dimensions without calling
    /// the Sheets API.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Clears a range's values, leaving formatting intact.
#[derive(Parser)]
pub struct ClearCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to clear, optionally carrying its own `Sheet!` prefix.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`, or
    /// clears the whole tab on its own.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Reports the gate verdict without calling the Sheets API.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl WriteCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let values = read_values(&self.values, self.values_format)?;
        let opts = WriteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: WriteVerb::Write,
            range: self.range,
            sheet: self.sheet,
            values,
            input: self.input.into(),
            dry_run: self.dry_run,
        };
        run_write(client, &opts, &self.output).await
    }
}

impl AppendCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let values = read_values(&self.values, self.values_format)?;
        let opts = WriteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: WriteVerb::Append,
            range: self.range,
            sheet: self.sheet,
            values,
            input: self.input.into(),
            dry_run: self.dry_run,
        };
        run_write(client, &opts, &self.output).await
    }
}

impl ClearCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = WriteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: WriteVerb::Clear,
            range: self.range,
            sheet: self.sheet,
            values: Vec::new(),
            input: ValueInputOption::default(),
            dry_run: self.dry_run,
        };
        run_write(client, &opts, &self.output).await
    }
}

/// Reads and parses `--values` from a path or stdin.
///
/// Shared with `sheets create`, which takes the same flag.
pub(crate) fn read_values(source: &str, format: ValuesFormat) -> Result<Vec<Vec<String>>> {
    let content = if source == "-" {
        // Bounded at the cap + 1 so an unbounded stream is never buffered
        // past it before being refused — there's no upfront size to stat
        // for a pipe, unlike the file branch below. Mirrors
        // `crate::cli::drive::edit`'s `read_stdin_content`.
        let mut buf = Vec::new();
        std::io::stdin()
            .take(crate::drive::files_api::MAX_UPLOAD_BYTES + 1)
            .read_to_end(&mut buf)
            .context("Failed to read --values from stdin")?;
        anyhow::ensure!(
            buf.len() as u64 <= crate::drive::files_api::MAX_UPLOAD_BYTES,
            "--values from stdin is over the {} byte cap",
            crate::drive::files_api::MAX_UPLOAD_BYTES
        );
        String::from_utf8(buf).context("--values from stdin is not valid UTF-8")?
    } else {
        // Bounded by the same cap `drive upload`/`edit` use, so a stray
        // multi-gigabyte file is refused rather than buffered.
        let metadata = std::fs::metadata(source)
            .with_context(|| format!("Failed to stat --values file {source}"))?;
        anyhow::ensure!(
            metadata.len() <= crate::drive::files_api::MAX_UPLOAD_BYTES,
            "--values file {source} is {} bytes, over the {} byte cap",
            metadata.len(),
            crate::drive::files_api::MAX_UPLOAD_BYTES
        );
        std::fs::read_to_string(source)
            .with_context(|| format!("Failed to read --values file {source}"))?
    };
    values::parse(&content, format.resolve(source))
}

/// Runs the engine and renders the outcome.
///
/// Split from each `execute` so tests can inject a wiremock client and
/// pre-built options directly, without touching the filesystem or the
/// credential-loading path.
///
/// Takes an options struct rather than loose parameters: `clippy.toml` caps
/// arguments at 7 and this would otherwise sit at 8.
async fn run_write(client: &DriveClient, opts: &WriteOptions, output: &OutputFormat) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = write(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    println!("{}", describe(&outcome, opts.verb));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn input_arg_maps_onto_the_engine_option() {
        assert_eq!(
            ValueInputOption::from(InputArg::UserEntered),
            ValueInputOption::UserEntered
        );
        assert_eq!(ValueInputOption::from(InputArg::Raw), ValueInputOption::Raw);
    }

    #[test]
    fn input_arg_defaults_to_user_entered() {
        // The one default whose wrong value silently mangles data: `raw`
        // would store `=SUM(A1:A3)` as literal text.
        assert_eq!(
            ValueInputOption::from(InputArg::default()),
            ValueInputOption::UserEntered
        );
    }

    #[test]
    fn read_values_parses_a_csv_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cells.csv");
        std::fs::write(&path, "a,b\nc,d\n").unwrap();
        let rows = read_values(path.to_str().unwrap(), ValuesFormat::Auto).unwrap();
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn read_values_infers_json_from_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cells.json");
        std::fs::write(&path, r#"[["a",1]]"#).unwrap();
        let rows = read_values(path.to_str().unwrap(), ValuesFormat::Auto).unwrap();
        assert_eq!(rows, vec![vec!["a", "1"]]);
    }

    #[test]
    fn read_values_reports_a_missing_file_clearly() {
        let err = read_values("/definitely/not/here.csv", ValuesFormat::Auto).unwrap_err();
        assert!(err.to_string().contains("Failed to stat"), "{err}");
    }

    #[test]
    fn read_values_refuses_a_file_over_the_upload_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.csv");
        let oversize = vec![b'a'; (crate::drive::files_api::MAX_UPLOAD_BYTES + 1) as usize];
        std::fs::write(&path, oversize).unwrap();
        let err = read_values(path.to_str().unwrap(), ValuesFormat::Auto).unwrap_err();
        assert!(err.to_string().contains("over the"), "{err}");
    }

    // ── `execute`/`run_write`, against a wiremock Drive+Sheets backend ──

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

    /// A `DriveClient` pointed at `server`, whose token endpoint is already
    /// mocked. Callers must additionally point `SHEETS_API_URL` at the same
    /// server, since `run_write` derives its `SheetsClient` internally from
    /// the real process environment.
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

    fn mount_file(id: &str, mime_type: &str, parents: &[&str]) -> wiremock::Mock {
        let parents: Vec<&str> = parents.to_vec();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id, "mimeType": mime_type, "parents": parents,
                })),
            )
    }

    fn mount_folder(id: &str) -> wiremock::Mock {
        mount_file(id, "application/vnd.google-apps.folder", &[])
    }

    fn allow_write_rule_settings() -> serde_json::Value {
        serde_json::json!({
            "rules": [{
                "folder_id": "parent-1",
                "recursive": true,
                "allow": ["sheets-write"],
            }],
        })
    }

    fn write_opts(dry_run: bool) -> WriteOptions {
        WriteOptions {
            spreadsheet_id: "sheet-1".to_string(),
            verb: WriteVerb::Write,
            range: Some("A1:B2".to_string()),
            sheet: None,
            values: vec![vec!["a".to_string(), "b".to_string()]],
            input: InputArg::UserEntered.into(),
            dry_run,
        }
    }

    #[tokio::test]
    async fn run_write_reports_blocked_by_default_policy() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["parent-1"],
        )
        .mount(&server)
        .await;
        mount_folder("parent-1").mount(&server).await;
        // No settings written: the gate defaults to deny, so no Sheets
        // endpoint should ever be called.

        run_write(&client, &write_opts(false), &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_write_json_output_short_circuits_before_the_table_line() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["parent-1"],
        )
        .mount(&server)
        .await;
        mount_folder("parent-1").mount(&server).await;

        run_write(&client, &write_opts(false), &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_command_execute_calls_values_update() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_write_rule_settings())],
        )
        .unwrap();

        let values_path = dir.path().join("cells.csv");
        std::fs::write(&values_path, "a,b\n").unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["parent-1"],
        )
        .mount(&server)
        .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updatedCells": 2})),
            )
            .mount(&server)
            .await;

        let cmd = WriteCommand {
            spreadsheet_id: "sheet-1".to_string(),
            range: Some("A1:B2".to_string()),
            sheet: None,
            values: values_path.to_str().unwrap().to_string(),
            values_format: ValuesFormat::Auto,
            input: InputArg::UserEntered,
            dry_run: false,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn append_command_execute_calls_values_append() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_write_rule_settings())],
        )
        .unwrap();

        let values_path = dir.path().join("cells.csv");
        std::fs::write(&values_path, "a,b\n").unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["parent-1"],
        )
        .mount(&server)
        .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1:B2:append",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "updates": {"updatedRange": "A1:B2", "updatedCells": 2},
                })),
            )
            .mount(&server)
            .await;

        let cmd = AppendCommand {
            spreadsheet_id: "sheet-1".to_string(),
            range: Some("A1:B2".to_string()),
            sheet: None,
            values: values_path.to_str().unwrap().to_string(),
            values_format: ValuesFormat::Auto,
            input: InputArg::UserEntered,
            dry_run: false,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn clear_command_execute_calls_values_clear() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_write_rule_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_file(
            "sheet-1",
            crate::drive::types::GOOGLE_SHEET_MIME_TYPE,
            &["parent-1"],
        )
        .mount(&server)
        .await;
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1:B2:clear",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"clearedRange": "A1:B2"})),
            )
            .mount(&server)
            .await;

        let cmd = ClearCommand {
            spreadsheet_id: "sheet-1".to_string(),
            range: Some("A1:B2".to_string()),
            sheet: None,
            dry_run: false,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();
    }
}
