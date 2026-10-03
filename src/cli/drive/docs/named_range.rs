//! CLI for scoped named-range metadata and content mutations.

use anyhow::Result;
use clap::{Args, Parser};

use crate::cli::drive::{format::OutputFormat, helpers};
use crate::drive::client::DriveClient;
use crate::drive::docs::{
    client::DocsClient,
    named_range::{Mutation, Scope},
    write::{WriteOptions, WritePayload},
};

/// Required structural scope shared by all three commands.
#[derive(Args)]
pub struct ScopedArgs {
    /// Document ID.
    pub document_id: String,
    /// Server tab ID, or `legacy` for a response without tabs. Never defaults to first tab.
    #[arg(long)]
    pub tab: String,
    /// `body` or an existing header/footer/footnote segment ID.
    #[arg(long)]
    pub segment: String,
    /// Resolve and report the complete effect without mutation or a backup lease.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ScopedArgs {
    async fn execute(self, client: &DriveClient, mutation: Mutation) -> Result<()> {
        let scope = Scope {
            tab_id: if self.tab == "legacy" {
                None
            } else {
                Some(self.tab)
            },
            segment_id: if self.segment == "body" {
                None
            } else {
                Some(self.segment)
            },
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::NamedRange { scope, mutation },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        super::write::run_write(
            client,
            &DocsClient::from_drive_client(client)?,
            &opts,
            &helpers::active_account_rules()?,
            &self.output,
        )
        .await
    }
}

/// Creates a named-range label under docs-structure, without changing text.
#[derive(Parser)]
pub struct CreateNamedRangeCommand {
    #[command(flatten)]
    pub scope: ScopedArgs,
    /// Label, 1–256 UTF-16 units; names need not be unique.
    #[arg(long)]
    pub name: String,
    /// Inclusive start in server UTF-16 units, inside one plain-text paragraph.
    #[arg(long)]
    pub start_index: i64,
    /// Exclusive end in server UTF-16 units; exclude the paragraph newline.
    #[arg(long)]
    pub end_index: i64,
}

impl CreateNamedRangeCommand {
    /// Runs creation through the shared permission and lease engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.scope
            .execute(
                client,
                Mutation::Create {
                    name: self.name,
                    start_index: self.start_index,
                    end_index: self.end_index,
                },
            )
            .await
    }
}

/// Deletes named-range metadata under docs-structure; leaves all content intact.
#[derive(Parser)]
pub struct DeleteNamedRangeCommand {
    #[command(flatten)]
    pub scope: ScopedArgs,
    /// Stable ID from the selected tab's namedRanges in `docs read -o json`.
    #[arg(long)]
    pub id: String,
}

impl DeleteNamedRangeCommand {
    /// Runs metadata deletion without issuing content deletion.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.scope
            .execute(client, Mutation::Delete { id: self.id })
            .await
    }
}

/// Replaces one plain-text named span under docs-write; empty text needs docs-delete.
#[derive(Parser)]
pub struct ReplaceNamedRangeContentCommand {
    #[command(flatten)]
    pub scope: ScopedArgs,
    /// Stable ID, never a name that could select multiple labels.
    #[arg(long)]
    pub id: String,
    /// Replacement text; explicit empty text removes content under docs-delete.
    #[arg(
        long,
        conflicts_with = "text_file",
        required_unless_present = "text_file"
    )]
    pub text: Option<String>,
    /// Read replacement text from a bounded UTF-8 file, or `-` for stdin.
    #[arg(long, value_name = "PATH")]
    pub text_file: Option<String>,
}

impl ReplaceNamedRangeContentCommand {
    /// Replaces through the revision-controlled shared write engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let text = match (self.text, self.text_file.as_deref()) {
            (Some(text), None) => text,
            (None, Some(source)) => super::write::read_text(source)?,
            _ => anyhow::bail!("exactly one of --text or --text-file is required"),
        };
        self.scope
            .execute(client, Mutation::Replace { id: self.id, text })
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_stable_id_are_required_and_text_sources_are_exclusive() {
        let base = [
            "replace-named-range-content",
            "d",
            "--tab",
            "t",
            "--segment",
            "body",
            "--id",
            "nr",
        ];
        assert!(ReplaceNamedRangeContentCommand::try_parse_from(base).is_err());
        assert!(ReplaceNamedRangeContentCommand::try_parse_from(
            base.into_iter().chain(["--text", ""])
        )
        .is_ok());
        assert!(ReplaceNamedRangeContentCommand::try_parse_from(
            base.into_iter().chain(["--text-file", "f"])
        )
        .is_ok());
        assert!(
            ReplaceNamedRangeContentCommand::try_parse_from(base.into_iter().chain([
                "--text",
                "x",
                "--text-file",
                "f"
            ]))
            .is_err()
        );
        assert!(
            DeleteNamedRangeCommand::try_parse_from(["delete-named-range", "d", "--id", "nr"])
                .is_err()
        );
        assert!(DeleteNamedRangeCommand::try_parse_from([
            "delete-named-range",
            "d",
            "--tab",
            "t",
            "--segment",
            "body",
            "--name",
            "label"
        ])
        .is_err());
        assert!(CreateNamedRangeCommand::try_parse_from([
            "create-named-range",
            "d",
            "--tab",
            "t",
            "--segment",
            "body",
            "--name",
            "label",
            "--start-index",
            "1",
            "--end-index",
            "3"
        ])
        .is_ok());
    }
}
