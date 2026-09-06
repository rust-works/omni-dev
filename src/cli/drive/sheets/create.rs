//! CLI command for `omni-dev drive sheets create`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::drive::sheets::values::ValuesFormat;
use crate::cli::drive::sheets::write::{read_values, InputArg};
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::create::{create, describe, CreateOptions};

/// Creates a new Google Sheet, optionally seeded with values.
///
/// Shorthand for `drive create --mime-type
/// application/vnd.google-apps.spreadsheet`, which does the same thing; the
/// difference is `--values`, and being discoverable inside the `sheets`
/// tree.
#[derive(Parser)]
pub struct CreateCommand {
    /// The new spreadsheet's title.
    #[arg(long)]
    pub name: String,

    /// The folder id to create it in — what the write-permission gate is
    /// evaluated against.
    #[arg(long, value_name = "FOLDER_ID")]
    pub parent: String,

    /// Optional initial values, written to `Sheet1!A1` onwards: a local file
    /// path, or `-` to read stdin.
    #[arg(long, value_name = "PATH|-")]
    pub values: Option<String>,

    /// How to parse `--values`. `auto` infers from the file extension.
    #[arg(long = "values-format", value_enum, default_value_t = ValuesFormat::Auto)]
    pub values_format: ValuesFormat,

    /// How the API interprets the values (see `drive sheets write`).
    #[arg(long, value_enum, default_value_t = InputArg::UserEntered)]
    pub input: InputArg,

    /// Reports the gate verdict without creating anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CreateCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let values = match &self.values {
            Some(source) => read_values(source, self.values_format)?,
            None => Vec::new(),
        };
        let opts = CreateOptions {
            name: self.name,
            parent_folder_id: self.parent,
            values,
            input: self.input.into(),
            dry_run: self.dry_run,
        };
        run_create(client, &opts, &self.output).await
    }
}

/// Runs the engine and renders the outcome.
///
/// Split from [`CreateCommand::execute`] so tests can inject a wiremock
/// client and pre-built options, without touching the filesystem or the
/// credential-loading path.
async fn run_create(
    client: &DriveClient,
    opts: &CreateOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = create(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    println!("{}", describe(&outcome));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
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
    /// server, since `run_create` derives its `SheetsClient` internally from
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

    fn mount_folder(id: &str) -> wiremock::Mock {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/drive/v3/files/{id}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": id, "name": id,
                    "mimeType": "application/vnd.google-apps.folder", "parents": [],
                })),
            )
    }

    fn opts(dry_run: bool) -> CreateOptions {
        CreateOptions {
            name: "Budget".to_string(),
            parent_folder_id: "parent-1".to_string(),
            values: Vec::new(),
            input: InputArg::UserEntered.into(),
            dry_run,
        }
    }

    fn allow_create_rule_settings() -> serde_json::Value {
        serde_json::json!({
            "rules": [{
                "folder_id": "parent-1",
                "recursive": true,
                "allow": ["create"],
            }],
        })
    }

    #[tokio::test]
    async fn run_create_reports_blocked_by_default_policy() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_folder("parent-1").mount(&server).await;
        // No settings written, so there is no configured account: the gate
        // defaults to deny and `files.create` must never be called.

        run_create(&client, &opts(false), &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_create_allowed_creates_and_renders_table_output() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_create_rule_settings())],
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-sheet", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                })),
            )
            .mount(&server)
            .await;

        run_create(&client, &opts(false), &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_create_json_output_short_circuits_before_the_table_line() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_folder("parent-1").mount(&server).await;

        run_create(&client, &opts(false), &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_command_execute_with_no_values_dispatches_through_run_create() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_folder("parent-1").mount(&server).await;

        let cmd = CreateCommand {
            name: "Budget".to_string(),
            parent: "parent-1".to_string(),
            values: None,
            values_format: ValuesFormat::Auto,
            input: InputArg::UserEntered,
            dry_run: false,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn create_command_execute_reads_values_from_a_file() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        crate::utils::settings::Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("write_permissions", allow_create_rule_settings())],
        )
        .unwrap();

        let values_path = dir.path().join("cells.csv");
        std::fs::write(&values_path, "a,b\n").unwrap();

        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        std::env::set_var(SHEETS_API_URL, server.uri());
        mount_folder("parent-1").mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/drive/v3/files"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "new-sheet", "name": "Budget",
                    "mimeType": "application/vnd.google-apps.spreadsheet",
                })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"updatedCells": 2})),
            )
            .mount(&server)
            .await;

        let cmd = CreateCommand {
            name: "Budget".to_string(),
            parent: "parent-1".to_string(),
            values: Some(values_path.to_str().unwrap().to_string()),
            values_format: ValuesFormat::Auto,
            input: InputArg::UserEntered,
            dry_run: false,
            output: OutputFormat::Table,
        };
        cmd.execute(&client).await.unwrap();
    }
}
