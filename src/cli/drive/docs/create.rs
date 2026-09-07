//! CLI command for `omni-dev drive docs create`.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::docs::write::read_text;
use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::create::{create, describe, CreateOptions};
use crate::drive::write_gate::FolderPermissionRule;

/// Creates a new Google Doc, optionally seeded with text.
#[derive(Parser)]
pub struct CreateCommand {
    /// The new document's title.
    #[arg(long)]
    pub name: String,

    /// The folder to create it in. Also what the permission gate evaluates.
    #[arg(long, value_name = "FOLDER_ID")]
    pub parent: String,

    /// Initial body text.
    #[arg(long, conflicts_with = "text_file")]
    pub text: Option<String>,

    /// Read the initial body text from a file, or `-` for stdin.
    #[arg(long, value_name = "PATH")]
    pub text_file: Option<String>,

    /// Report what would happen without creating anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CreateCommand {
    /// Runs the command, deriving a Docs client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let text = match (self.text, self.text_file.as_deref()) {
            (Some(text), _) => Some(text),
            (None, Some(source)) => Some(read_text(source)?),
            (None, None) => None,
        };
        let opts = CreateOptions {
            name: self.name,
            parent_folder_id: self.parent,
            text,
            dry_run: self.dry_run,
        };
        let rules = helpers::active_account_rules()?;
        run_create(client, &docs, &opts, &rules, &self.output).await
    }
}

/// Runs the engine and renders the outcome.
///
/// Split from [`CreateCommand::execute`] so tests can inject wiremock
/// clients without touching the filesystem or the credential-loading path.
async fn run_create(
    drive: &DriveClient,
    docs: &DocsClient,
    opts: &CreateOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = create(drive, docs, opts, rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    // `describe` embeds operator-supplied ids raw, so the whole rendered
    // line is sanitized here — the same split `sheets create` uses.
    println!("{}", sanitize_for_terminal(&describe(&outcome)));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn text_and_text_file_are_mutually_exclusive() {
        assert!(CreateCommand::try_parse_from([
            "create",
            "--name",
            "n",
            "--parent",
            "p",
            "--text",
            "x",
            "--text-file",
            "f",
        ])
        .is_err());
    }

    /// Unlike `append`, neither is required: an empty document is a
    /// perfectly good thing to create.
    #[test]
    fn neither_text_nor_text_file_is_required() {
        let cmd =
            CreateCommand::try_parse_from(["create", "--name", "n", "--parent", "p"]).unwrap();
        assert!(cmd.text.is_none());
        assert!(cmd.text_file.is_none());
    }

    #[test]
    fn name_and_parent_are_required() {
        assert!(CreateCommand::try_parse_from(["create", "--name", "n"]).is_err());
        assert!(CreateCommand::try_parse_from(["create", "--parent", "p"]).is_err());
    }
}
