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
